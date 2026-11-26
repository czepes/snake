//! Ввод-вывод по UDP.
//!
//! Два сокета, как требует спецификация: один только принимает multicast на
//! 239.192.0.4:9193, второй (порт выбирает ОС) делает всё остальное, включая
//! отправку multicast-анонсов через каждый интерфейс. Недоступность multicast
//! или ошибка одного интерфейса не мешают работе: остаются unicast и сервер.

use std::collections::HashSet;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockRef, Socket, Type};

use crate::reliable::Out;
use crate::wire::MAX_DATAGRAM;

pub const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 192, 0, 4);
pub const MULTICAST_PORT: u16 = 9193;
pub const DEFAULT_SERVER: &str = "snakes.ippolitov.me:9193";
const REFRESH_PERIOD: Duration = Duration::from_secs(5);

pub struct Datagram {
    pub from: SocketAddrV4,
    pub multicast: bool,
    pub bytes: Vec<u8>,
}

pub struct Net {
    unicast: UdpSocket,
    mcast: Option<UdpSocket>,
    ifaces: Vec<Ipv4Addr>,
    joined: HashSet<Ipv4Addr>,
    last_refresh: Instant,
}

impl Net {
    /// `port` — порт игрового сокета (`None` — выбирает ОС), `multicast` — принимать ли multicast.
    pub fn open(port: Option<u16>, multicast: bool, now: Instant) -> io::Result<Net> {
        let unicast = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        unicast.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port.unwrap_or(0))).into())?;
        unicast.set_nonblocking(true)?;
        unicast.set_multicast_loop_v4(true)?;
        unicast.set_multicast_ttl_v4(1)?;
        let unicast: UdpSocket = unicast.into();

        let mcast = if multicast { open_multicast().ok() } else { None };
        let mut net = Net {
            unicast,
            mcast,
            ifaces: Vec::new(),
            joined: HashSet::new(),
            last_refresh: now,
        };
        net.rescan();
        Ok(net)
    }

    pub fn local_port(&self) -> u16 {
        self.unicast.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn local_ips(&self) -> Vec<Ipv4Addr> {
        self.ifaces.clone()
    }

    /// Принимается ли multicast хотя бы на одном интерфейсе.
    pub fn lan_available(&self) -> bool {
        self.mcast.is_some() && !self.joined.is_empty()
    }

    fn rescan(&mut self) {
        let mut ips: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
            .map(|v| {
                v.into_iter()
                    .filter_map(|i| match i.ip() {
                        std::net::IpAddr::V4(ip) => Some(ip),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        ips.sort_unstable();
        ips.dedup();
        if let Some(m) = &self.mcast {
            for ip in &ips {
                if !self.joined.contains(ip) && m.join_multicast_v4(&MULTICAST_GROUP, ip).is_ok() {
                    self.joined.insert(*ip);
                }
            }
        }
        self.joined.retain(|ip| ips.contains(ip));
        self.ifaces = ips;
    }

    /// Раз в несколько секунд пересматривает интерфейсы. `true` — список изменился.
    pub fn refresh(&mut self, now: Instant) -> bool {
        if now.duration_since(self.last_refresh) < REFRESH_PERIOD {
            return false;
        }
        self.last_refresh = now;
        let before = self.ifaces.clone();
        self.rescan();
        before != self.ifaces
    }

    /// Забирает все накопившиеся датаграммы обоих сокетов.
    pub fn recv(&mut self, mut f: impl FnMut(Datagram)) {
        let mut buf = [0u8; 2048];
        drain(&self.unicast, &mut buf, false, &mut f);
        if let Some(m) = &self.mcast {
            drain(m, &mut buf, true, &mut f);
        }
    }

    pub fn send(&self, out: &Out) {
        match out {
            Out::Unicast(to, bytes) => {
                let _ = self.unicast.send_to(bytes, SocketAddr::V4(*to));
            }
            Out::Multicast(bytes) => {
                let dest = SocketAddr::V4(SocketAddrV4::new(MULTICAST_GROUP, MULTICAST_PORT));
                let sock = SockRef::from(&self.unicast);
                for ip in &self.ifaces {
                    // Ошибка одного интерфейса не должна мешать остальным.
                    if sock.set_multicast_if_v4(ip).is_ok() {
                        let _ = self.unicast.send_to(bytes, dest);
                    }
                }
            }
        }
    }
}

fn open_multicast() -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    // На BSD и macOS совместное использование порта требует SO_REUSEPORT. На Linux его
    // включать нельзя: ядро разделит датаграммы между сокетами, и каждый экземпляр
    // получит лишь часть анонсов. Там хватает SO_REUSEADDR.
    #[cfg(all(unix, not(target_os = "linux"), not(target_os = "solaris"), not(target_os = "illumos")))]
    s.set_reuse_port(true)?;
    #[cfg(target_os = "linux")]
    let _ = s.set_multicast_all_v4(false);
    s.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MULTICAST_PORT)).into())?;
    s.set_nonblocking(true)?;
    Ok(s.into())
}

fn drain(sock: &UdpSocket, buf: &mut [u8], multicast: bool, f: &mut impl FnMut(Datagram)) {
    // Не больше разумного числа датаграмм за один заход, чтобы интерфейс не «залипал» при потоке мусора.
    for _ in 0..512 {
        match sock.recv_from(buf) {
            Ok((n, SocketAddr::V4(from))) => {
                if n <= MAX_DATAGRAM {
                    f(Datagram {
                        from,
                        multicast,
                        bytes: buf[..n].to_vec(),
                    });
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Разбор адреса сервера: сначала как готовый `ip:port`, иначе через DNS.
pub fn resolve_once(spec: &str) -> Option<SocketAddrV4> {
    if let Ok(SocketAddr::V4(a)) = spec.parse::<SocketAddr>() {
        return Some(a);
    }
    spec.to_socket_addrs().ok()?.find_map(|a| match a {
        SocketAddr::V4(v4) => Some(v4),
        _ => None,
    })
}

/// Фоновый поток DNS: пока адрес не получен, повторяет попытку, затем
/// периодически перепроверяет. Поток один и не растёт с числом игроков.
pub fn spawn_resolver(spec: String) -> mpsc::Receiver<Option<SocketAddrV4>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let r = resolve_once(&spec);
            if tx.send(r).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_secs(if r.is_some() { 60 } else { 5 }));
        }
    });
    rx
}

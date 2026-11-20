//! UDP socket setup: dual-socket scheme for multicast discovery (one
//! receive-only multicast socket, one general-purpose unicast socket that
//! also sends multicast announcements on every multicast-capable IPv4
//! interface), as required by `snakes_task.md`.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

use socket2::{Domain, Protocol, Socket, Type};

use crate::model::{MULTICAST_ADDR, MULTICAST_PORT};

pub fn local_multicast_interfaces() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            // Loopback is included deliberately: it is a valid
            // multicast-capable interface (per `snakes_task.md`, "на всех
            // доступных IPv4-интерфейсах с поддержкой multicast") and
            // excluding it made locally-hosted games invisible to other
            // instances running on the same machine.
            if let std::net::IpAddr::V4(ip) = iface.ip() {
                out.push(ip);
            }
        }
    }
    if out.is_empty() {
        out.push(Ipv4Addr::UNSPECIFIED);
    }
    out
}

/// Binds the general-purpose socket used for unicast traffic and for
/// sending multicast announcements. Binds to an OS-assigned port.
pub fn bind_unicast() -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    let addr: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
    socket.bind(&addr.into())?;
    socket.set_nonblocking(true)?;
    let std_socket: UdpSocket = socket.into();
    Ok(std_socket)
}

/// Binds the multicast-receive-only socket on the well-known discovery port,
/// joining the multicast group on every multicast-capable IPv4 interface and
/// allowing several local instances to share the port.
pub fn bind_multicast_receiver() -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    let addr: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MULTICAST_PORT).into();
    socket.bind(&addr.into())?;
    for iface in local_multicast_interfaces() {
        let _ = socket.join_multicast_v4(&MULTICAST_ADDR, &iface);
    }
    socket.set_nonblocking(true)?;
    let std_socket: UdpSocket = socket.into();
    Ok(std_socket)
}

/// Sends `bytes` as a multicast datagram on every multicast-capable
/// interface, using the unicast socket re-bound for outgoing multicast with
/// the right interface each time.
pub fn send_multicast_all_interfaces(socket: &UdpSocket, bytes: &[u8]) {
    let dest = SocketAddr::V4(SocketAddrV4::new(MULTICAST_ADDR, MULTICAST_PORT));
    let sock_ref = socket2::SockRef::from(socket);
    for iface in local_multicast_interfaces() {
        let _ = sock_ref.set_multicast_if_v4(&iface);
        let _ = socket.send_to(bytes, dest);
    }
}

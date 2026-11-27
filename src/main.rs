//! Многопользовательская «Змейка» по сети с терминальным интерфейсом.
//!
//! Потоков два и их число не растёт с числом игроков: основной (интерфейс, сеть,
//! таймеры в одном цикле) и фоновый DNS-резолвер.
//!
//! Здесь находится «оболочка» ядра: она даёт [`Node`] время и UDP-пакеты,
//! отправляет его исходящие датаграммы и передаёт [`Ui`] события. Ни сетевые
//! повторы, ни передача роли ведущего не блокируют цикл; выход тоже проходит
//! через команды узла, пока тот не разрешит завершить программу.

use std::fs::OpenOptions;
use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{self, Event as TermEvent, KeyEventKind};

use snakes::net::{DEFAULT_SERVER, Net, spawn_resolver};
use snakes::node::Node;
use snakes::ui::Ui;

const HELP: &str = "\
Змейка по сети (протокол v2)

Использование: snakes [параметры]

  --name ИМЯ          имя игрока (по умолчанию из $SNAKE_NAME или $USER)
  --server АДРЕС      сервер курса host:port (по умолчанию snakes.ippolitov.me:9193)
  --no-server         не обращаться к серверу курса
  --no-multicast      не использовать multicast (только сервер и прямые адреса)
  --no-animations     отключить все анимации интерфейса
  --port ПОРТ         фиксированный UDP-порт игрового сокета (по умолчанию выбирает ОС)
  --log ФАЙЛ          писать журнал протокола в файл (или переменная $SNAKE_LOG)
  -h, --help          эта справка
";

struct Args {
    server: Option<String>,
    multicast: bool,
    animations: bool,
    port: Option<u16>,
    name: String,
    log: Option<String>,
}

fn parse_args() -> Result<Option<Args>, String> {
    parse_args_from(std::env::args().skip(1))
}

/// `Ok(None)` означает запрос справки, а не ошибку разбора.
/// Отдельный источник аргументов позволяет проверять флаги без запуска TUI и сети.
fn parse_args_from(args: impl IntoIterator<Item = String>) -> Result<Option<Args>, String> {
    let mut a = Args {
        server: Some(DEFAULT_SERVER.to_owned()),
        multicast: true,
        animations: true,
        port: None,
        name: std::env::var("SNAKE_NAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_default(),
        log: std::env::var("SNAKE_LOG").ok(),
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |what: &str| {
            it.next()
                .ok_or_else(|| format!("параметру {arg} нужно значение ({what})"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--no-server" => a.server = None,
            "--no-multicast" => a.multicast = false,
            "--no-animations" => a.animations = false,
            "--server" => a.server = Some(value("host:port")?),
            "--name" => a.name = value("имя")?,
            "--log" => a.log = Some(value("файл")?),
            "--port" => {
                a.port = Some(
                    value("порт")?
                        .parse()
                        .map_err(|_| "порт должен быть числом 1–65535".to_owned())?,
                )
            }
            other => return Err(format!("неизвестный параметр {other}; см. --help")),
        }
    }
    Ok(Some(a))
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("Ошибка: {e}");
            return ExitCode::FAILURE;
        }
    };

    let start = Instant::now();
    let mut net = match Net::open(args.port, args.multicast, start) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Не удалось открыть UDP-сокет: {e}");
            return ExitCode::FAILURE;
        }
    };

    let seed =
        unix_secs() ^ u64::from(std::process::id()).rotate_left(32) ^ u64::from(net.local_port());
    // Для нового сеанса на прежнем порту счётчик msg_seq должен превышать всё, что слали раньше.
    let first_seq = if args.port.is_some() {
        (unix_secs() as i64) * 100
    } else {
        1
    };
    let mut node = Node::new(start, seed, first_seq);
    node.set_local(net.local_ips(), net.local_port());

    let mut log = args
        .log
        .as_ref()
        .and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok());
    node.set_trace(log.is_some());

    let resolver = args.server.clone().map(spawn_resolver);
    let mut ui = Ui::with_animations(args.name.trim().to_owned(), args.animations);

    let mut terminal = ratatui::init();
    let mut last_draw = start - Duration::from_secs(1);
    let mut quit_seen: Option<Instant> = None;

    let result: std::io::Result<()> = (|| {
        loop {
            let now = Instant::now();

            // 1. Ввод: ждём не дольше 5 мс, затем забираем всё накопленное.
            let mut dirty = false;
            let mut timeout = Duration::from_millis(5);
            while event::poll(timeout)? {
                timeout = Duration::ZERO;
                match event::read()? {
                    TermEvent::Key(k) if k.kind != KeyEventKind::Release => {
                        ui.on_key(k, &mut node, now);
                        dirty = true;
                    }
                    TermEvent::Resize(..) => dirty = true,
                    _ => {}
                }
            }

            // 2. Сеть: входящие, таймеры, исходящие.
            net.recv(|d| node.on_datagram(now, d.from, d.multicast, &d.bytes));
            node.on_tick(now);
            for out in node.drain_out() {
                net.send(&out);
            }

            // 3. Фоновые задачи.
            if net.refresh(now) {
                node.set_local(net.local_ips(), net.local_port());
            }
            if let Some(rx) = &resolver {
                while let Ok(r) = rx.try_recv() {
                    if r.is_some() {
                        node.set_server(r);
                    }
                }
            }

            // 4. События узла → интерфейс; журнал.
            for ev in node.drain_events() {
                ui.on_event(ev, now);
                dirty = true;
            }
            if let Some(f) = log.as_mut() {
                for line in node.drain_trace() {
                    let _ = writeln!(
                        f,
                        "[{:>9.3}] {line}",
                        now.duration_since(start).as_secs_f64()
                    );
                }
            }

            if ui.quit {
                return Ok(());
            }
            // Выход не должен зависать: передача управления и повторы Leave ограничены по времени.
            if node.is_quitting()
                && quit_seen.get_or_insert(now).elapsed() > Duration::from_millis(2000)
            {
                return Ok(());
            }

            // 5. Отрисовка не чаще ~30 кадров в секунду.
            if dirty || now.duration_since(last_draw) >= Duration::from_millis(33) {
                let lan_ok = net.lan_available();
                terminal.draw(|f| ui.draw(f, &node, now, lan_ok))?;
                last_draw = now;
            }
        }
    })();

    ratatui::restore();
    if let Err(e) = result {
        eprintln!("Ошибка терминала: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animations_are_enabled_by_default() {
        assert!(parse_args_from(Vec::new()).unwrap().unwrap().animations);
    }

    #[test]
    fn no_animations_can_be_combined_with_other_options() {
        let args = [
            "--no-animations",
            "--name",
            "Игрок",
            "--no-server",
            "--no-multicast",
        ];
        let args = parse_args_from(args.map(str::to_owned)).unwrap().unwrap();
        assert!(!args.animations && !args.multicast && args.server.is_none());
        assert_eq!(args.name, "Игрок");
    }

    #[test]
    fn help_accepts_no_animations_without_starting_the_game() {
        assert!(
            parse_args_from(["--no-animations", "--help"].map(str::to_owned))
                .unwrap()
                .is_none()
        );
        assert!(HELP.contains("--no-animations"));
    }
}

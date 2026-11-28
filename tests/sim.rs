//! Сценарные тесты протокола в симуляторе сети: потери, дубли, перестановки,
//! «смерть» узлов, смена ведущего. Время виртуальное, сокетов нет.
//!
//! `cmd` подаёт действие узлу, `pump_outputs` заменяет транспорт, а `step`
//! двигает часы, вызывает таймеры и доставляет созревшие пакеты. Seed задаёт
//! генератор для потерь, дублей и задержек; ожидания по реальным часам здесь нет.
//! Для журнала обмена можно запустить
//! `SIM_TRACE=1 cargo test --test sim -- --nocapture`.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

use snakes::catalog::GameKey;
use snakes::node::{Command, Event, GameView, Node, PhaseKind};
use snakes::reliable::Out;
use snakes::rules::{Rng, Snake};
use snakes::wire::*;

fn addr_of(i: usize) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, (i + 1) as u8), 4000 + i as u16)
}

fn index_of(a: SocketAddrV4) -> Option<usize> {
    let o = a.ip().octets();
    if o[0] != 10 || o[1] != 0 || o[2] != 0 || o[3] == 0 {
        return None;
    }
    let i = (o[3] - 1) as usize;
    (a.port() == 4000 + i as u16).then_some(i)
}

struct Packet {
    at: Instant,
    to: usize,
    from: SocketAddrV4,
    mc: bool,
    bytes: Vec<u8>,
}

struct SimNode {
    node: Node,
    alive: bool,
    events: Vec<Event>,
}

/// Модель сети вокруг настоящих `Node`: меняет доставку, но не игровые правила.
struct Sim {
    t0: Instant,
    now: Instant,
    nodes: Vec<SimNode>,
    queue: Vec<Packet>,
    rng: Rng,
    loss: f64,
    dup: f64,
    latency_ms: u64,
    jitter_ms: u64,
    /// Все доставленные датаграммы: (получатель, отправитель, байты) — для повторной подачи.
    history: Vec<(usize, SocketAddrV4, Vec<u8>)>,
    /// Узел-«сервер»: при Join с неизвестным именем создаёт игру (как сервер курса).
    server: Option<usize>,
}

impl Sim {
    fn new(n: usize, seed: u64) -> Sim {
        let now = Instant::now();
        let nodes = (0..n)
            .map(|i| {
                let mut node = Node::new(now, seed + i as u64 * 7919, 1);
                node.set_local(vec![], 0);
                SimNode {
                    node,
                    alive: true,
                    events: Vec::new(),
                }
            })
            .collect();
        Sim {
            t0: now,
            now,
            nodes,
            queue: Vec::new(),
            rng: Rng::new(seed),
            loss: 0.0,
            dup: 0.0,
            latency_ms: 5,
            jitter_ms: 0,
            history: Vec::new(),
            server: None,
        }
    }

    fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && (self.rng.next_u64() % 10_000) as f64 / 10_000.0 < p
    }

    /// Задаёт потерю и число копий; независимый jitter копий допускает перестановку.
    fn schedule(&mut self, to: usize, from: SocketAddrV4, mc: bool, bytes: Vec<u8>) {
        if self.chance(self.loss) {
            return;
        }
        let copies = if self.chance(self.dup) { 2 } else { 1 };
        for _ in 0..copies {
            let jitter = if self.jitter_ms > 0 {
                self.rng.next_u64() % (self.jitter_ms + 1)
            } else {
                0
            };
            self.queue.push(Packet {
                at: self.now + Duration::from_millis(self.latency_ms + jitter),
                to,
                from,
                mc,
                bytes: bytes.clone(),
            });
        }
    }

    fn pump_outputs(&mut self) {
        if std::env::var("SIM_TRACE").is_ok() {
            for i in 0..self.nodes.len() {
                self.nodes[i].node.set_trace(true);
                for line in self.nodes[i].node.drain_trace() {
                    eprintln!(
                        "[{:>6}ms n{i}] {line}",
                        self.now.duration_since(self.t0).as_millis()
                    );
                }
            }
        }
        for i in 0..self.nodes.len() {
            let outs = self.nodes[i].node.drain_out();
            let events = self.nodes[i].node.drain_events();
            self.nodes[i].events.extend(events);
            if !self.nodes[i].alive {
                continue;
            }
            let from = addr_of(i);
            for out in outs {
                match out {
                    Out::Unicast(to, bytes) => {
                        if let Some(j) = index_of(to)
                            && j < self.nodes.len()
                        {
                            self.schedule(j, from, false, bytes);
                        }
                    }
                    Out::Multicast(bytes) => {
                        for j in 0..self.nodes.len() {
                            if j != i {
                                self.schedule(j, from, true, bytes.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    fn deliver(&mut self, p: Packet) {
        if !self.nodes[p.to].alive {
            return;
        }
        // Сервер курса создаёт игру по Join с новым именем.
        if self.server == Some(p.to)
            && self.nodes[p.to].node.phase() == PhaseKind::Lobby
            && let Ok(m) = decode(&p.bytes)
            && let Body::Join { game_name, .. } = m.body
        {
            let cfg = Config {
                width: 20,
                height: 15,
                food_static: 2,
                state_delay_ms: 200,
            };
            self.nodes[p.to].node.command(
                self.now,
                Command::Host {
                    game: game_name,
                    player: "Сервер".into(),
                    cfg,
                },
            );
        }
        self.history.push((p.to, p.from, p.bytes.clone()));
        let now = self.now;
        self.nodes[p.to]
            .node
            .on_datagram(now, p.from, p.mc, &p.bytes);
    }

    fn step(&mut self, ms: u64) {
        self.now += Duration::from_millis(ms);
        let now = self.now;
        for n in &mut self.nodes {
            if n.alive {
                n.node.on_tick(now);
            }
        }
        self.pump_outputs();

        let mut due: Vec<Packet> = Vec::new();
        let mut rest = Vec::new();
        for p in self.queue.drain(..) {
            if p.at <= now {
                due.push(p);
            } else {
                rest.push(p);
            }
        }
        self.queue = rest;
        due.sort_by_key(|p| p.at);
        for p in due {
            self.deliver(p);
        }
        self.pump_outputs();
    }

    fn run(&mut self, ms: u64) {
        for _ in 0..ms / 5 {
            self.step(5);
        }
    }

    fn cmd(&mut self, i: usize, c: Command) {
        let now = self.now;
        self.nodes[i].node.command(now, c);
        self.pump_outputs();
    }

    fn kill(&mut self, i: usize) {
        self.nodes[i].alive = false;
    }

    fn node(&self, i: usize) -> &Node {
        &self.nodes[i].node
    }

    fn view(&self, i: usize) -> GameView<'_> {
        self.nodes[i]
            .node
            .game_view()
            .unwrap_or_else(|| panic!("узел {i} не в игре"))
    }

    fn in_game(&self, i: usize) -> bool {
        self.nodes[i].node.phase() == PhaseKind::InGame
    }

    fn events(&self, i: usize) -> &[Event] {
        &self.nodes[i].events
    }

    fn failures(&self, i: usize) -> Vec<String> {
        self.events(i)
            .iter()
            .filter_map(|e| {
                if let Event::Failure(t) = e {
                    Some(t.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    fn host(&mut self, i: usize, name: &str, player: &str) {
        let cfg = Config {
            width: 20,
            height: 15,
            food_static: 2,
            state_delay_ms: 200,
        };
        self.cmd(
            i,
            Command::Host {
                game: name.into(),
                player: player.into(),
                cfg,
            },
        );
    }

    fn join(&mut self, i: usize, host: usize, name: &str, player: &str, status: PlayerStatus) {
        let key = GameKey {
            name: name.into(),
            addr: addr_of(host),
        };
        self.cmd(
            i,
            Command::Join {
                key,
                player: player.into(),
                status,
            },
        );
    }
}

fn ids(v: &GameView) -> Vec<i32> {
    let mut ids: Vec<i32> = v.players.iter().map(|p| p.id).collect();
    ids.sort_unstable();
    ids
}

fn assert_consistent(sim: &Sim, nodes: &[usize]) {
    let reference = sim.view(nodes[0]);
    for &i in &nodes[1..] {
        let v = sim.view(i);
        assert!(
            (v.order - reference.order).abs() <= 2,
            "порядок состояний расходится: {} и {}",
            v.order,
            reference.order
        );
        assert_eq!(
            ids(&v),
            ids(&reference),
            "составы участников расходятся у узла {i}"
        );
    }
}

// ──────────────────────────────────── Тесты ──────────────────────────────────

#[test]
fn lan_discovery_then_join_playing_and_viewing() {
    let mut sim = Sim::new(3, 1);
    sim.host(0, "Практика", "Вася");
    sim.run(1200);
    // Анонсы дошли до лобби остальных узлов.
    assert_eq!(sim.node(1).catalog_entries().len(), 1);
    let key = sim.node(1).catalog_entries()[0].key.clone();
    sim.cmd(
        1,
        Command::Join {
            key: key.clone(),
            player: "Петя".into(),
            status: PlayerStatus::Playing,
        },
    );
    sim.cmd(
        2,
        Command::Join {
            key,
            player: "Маша".into(),
            status: PlayerStatus::Viewing,
        },
    );
    sim.run(1500);

    for i in 0..3 {
        assert!(
            sim.in_game(i),
            "узел {i} должен быть в игре; события: {:?}",
            sim.events(i)
        );
    }
    let host = sim.view(0);
    assert_eq!(host.role, NodeRole::Master);
    assert_eq!(host.players.len(), 3);
    assert_eq!(ids(&host), vec![1, 2, 3]);
    assert_eq!(
        host.world.snakes.len(),
        2,
        "змейки у Васи и Пети, Маша наблюдает"
    );
    assert_eq!(
        host.players
            .iter()
            .filter(|p| p.role == NodeRole::Deputy)
            .count(),
        1
    );
    assert_eq!(sim.view(1).status, PlayerStatus::Playing);
    assert_eq!(sim.view(2).status, PlayerStatus::Viewing);
    assert_consistent(&sim, &[0, 1, 2]);
    assert!(sim.view(1).order > 0);
}

#[test]
fn steer_command_changes_direction_and_reverse_is_ignored() {
    let mut sim = Sim::new(2, 2);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.run(800);
    assert!(sim.in_game(1));

    // Ставим змейку B в известную позицию: голова (10,7), тело слева, курс вправо.
    {
        let w = sim.nodes[0].node.debug_world_mut().unwrap();
        w.snakes.retain(|s| s.owner == 1);
        w.snakes.push(Snake {
            owner: 2,
            cells: [(10, 7), (9, 7)].into_iter().collect(),
            dir: Dir::Right,
        });
        w.foods.clear();
    }
    sim.cmd(1, Command::Steer(Dir::Left)); // разворот в шею — игнорируется
    sim.cmd(1, Command::Steer(Dir::Up));
    sim.run(450);
    let w = sim.view(1).world;
    let s = w.snake(2).expect("змейка B жива");
    assert_eq!(s.dir, Dir::Up);
}

#[test]
fn status_change_to_viewing_and_back() {
    let mut sim = Sim::new(2, 3);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.run(900);
    assert_eq!(sim.view(0).world.snakes.len(), 2);

    sim.cmd(1, Command::ToggleWatch);
    sim.run(900);
    assert_eq!(sim.view(1).status, PlayerStatus::Viewing);
    assert_eq!(sim.view(0).world.snakes.len(), 1);

    sim.cmd(1, Command::ToggleWatch);
    sim.run(900);
    assert_eq!(sim.view(1).status, PlayerStatus::Playing);
    assert_eq!(sim.view(0).world.snakes.len(), 2);
}

#[test]
fn dead_player_becomes_viewer_and_keeps_score_and_role() {
    let mut sim = Sim::new(2, 4);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.run(900);
    {
        // Змейка B упирается головой в собственное тело.
        let w = sim.nodes[0].node.debug_world_mut().unwrap();
        w.snakes.retain(|s| s.owner == 1);
        w.snakes.push(Snake {
            owner: 2,
            cells: [(10, 7), (10, 8), (11, 8), (11, 7), (11, 6), (10, 6)]
                .into_iter()
                .collect(),
            dir: Dir::Up,
        });
        w.foods.clear();
    }
    sim.cmd(1, Command::Steer(Dir::Right));
    sim.run(500);
    let b = sim.view(1);
    assert_eq!(b.status, PlayerStatus::Viewing);
    assert_ne!(b.role, NodeRole::Master);
    assert!(sim.view(0).world.snake(2).is_none());
}

#[test]
fn join_succeeds_despite_loss_duplicates_and_reordering() {
    let mut sim = Sim::new(4, 5);
    sim.loss = 0.25;
    sim.dup = 0.15;
    sim.jitter_ms = 60;
    sim.host(0, "g", "A");
    sim.run(2500);
    for i in 1..4 {
        sim.join(i, 0, "g", &format!("P{i}"), PlayerStatus::Playing);
    }
    sim.run(10_000);
    for i in 0..4 {
        assert!(sim.in_game(i), "узел {i}: {:?}", sim.events(i));
    }
    let host = sim.view(0);
    assert_eq!(
        host.players.len(),
        4,
        "участники: {:?}; события ведущего: {:?}",
        host.players
            .iter()
            .map(|p| (&p.name, p.id))
            .collect::<Vec<_>>(),
        sim.events(0)
    );
    assert_eq!(ids(&host), vec![1, 2, 3, 4]);
    // Инвариант: змейка есть ровно у тех, кто играет (кто-то мог погибнуть за 10 секунд).
    let playing = host
        .players
        .iter()
        .filter(|p| p.status == PlayerStatus::Playing)
        .count();
    assert_eq!(host.world.snakes.len(), playing);
    assert_consistent(&sim, &[0, 1, 2, 3]);
}

#[test]
fn replayed_join_does_not_create_another_participant() {
    let mut sim = Sim::new(2, 6);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.run(1000);
    assert_eq!(sim.view(0).players.len(), 2);

    // Повторяем самый первый Join, пришедший ведущему.
    let join = sim
        .history
        .iter()
        .find(|(to, _, b)| *to == 0 && matches!(decode(b).map(|m| m.body), Ok(Body::Join { .. })))
        .cloned()
        .expect("Join был доставлен");
    for _ in 0..3 {
        let now = sim.now;
        sim.nodes[0].node.on_datagram(now, join.1, false, &join.2);
        sim.pump_outputs();
        sim.run(100);
    }
    assert_eq!(sim.view(0).players.len(), 2);
    assert_eq!(sim.view(0).world.snakes.len(), 2);
}

#[test]
fn unfinished_join_recovers_after_master_dies() {
    let mut sim = Sim::new(3, 700);
    sim.host(0, "recovery", "master");
    sim.run(600);
    sim.join(1, 0, "recovery", "deputy", PlayerStatus::Viewing);
    sim.run(500);
    assert_eq!(sim.view(1).role, NodeRole::Deputy);

    sim.join(2, 0, "recovery", "newcomer", PlayerStatus::Viewing);
    sim.kill(0);
    sim.run(7500);

    assert_eq!(sim.view(1).role, NodeRole::Master);
    assert!(sim.in_game(2), "{:?}", sim.events(2));
    assert_eq!(sim.view(2).master, Some(addr_of(1)));
    assert_eq!(sim.view(2).status, PlayerStatus::Viewing);
    assert!(sim.view(1).players.iter().any(|p| p.name == "newcomer"));
    let new_join = sim.history.iter().find(|(to, from, bytes)| {
        *to == 1 && *from == addr_of(2) && matches!(decode(bytes).unwrap().body, Body::Join { .. })
    });
    assert!(
        new_join.is_some(),
        "Join должен быть отправлен новому ведущему"
    );
}

#[test]
fn joining_search_uses_a_separate_three_second_deadline() {
    let now = Instant::now();
    let mut client = Node::new(now, 1, 1);
    client.command(
        now,
        Command::Join {
            key: GameKey {
                name: "target".into(),
                addr: addr_of(0),
            },
            player: "client".into(),
            status: PlayerStatus::Viewing,
        },
    );
    client.drain_out();
    client.on_tick(now + Duration::from_millis(5000));
    assert_eq!(client.phase(), PhaseKind::Joining);
    client.drain_out();

    let announcement = |name: &str| Message {
        seq: 50,
        sender_id: None,
        receiver_id: None,
        game_name: None,
        body: Body::Announcement(vec![Announcement {
            game_name: name.into(),
            config: Config::default(),
            players: vec![],
            can_join: true,
        }]),
    };
    client.on_datagram(
        now + Duration::from_millis(5100),
        addr_of(0),
        true,
        &encode(&announcement("target")).unwrap(),
    );
    client.on_datagram(
        now + Duration::from_millis(5200),
        addr_of(1),
        true,
        &encode(&announcement("other")).unwrap(),
    );
    assert_eq!(client.phase(), PhaseKind::Joining);
    assert!(client.drain_out().is_empty());
    client.on_tick(now + Duration::from_millis(7999));
    assert_eq!(client.phase(), PhaseKind::Joining);
    client.on_datagram(
        now + Duration::from_millis(8000),
        addr_of(1),
        true,
        &encode(&announcement("target")).unwrap(),
    );
    assert_eq!(client.phase(), PhaseKind::Lobby);
    assert!(client.drain_out().is_empty());
    assert!(
        client
            .drain_events()
            .iter()
            .any(|event| matches!(event, Event::Failure(reason) if reason.contains("анонс")))
    );
}

#[test]
fn joining_deadline_is_enforced_when_packets_arrive_without_a_tick() {
    for at in [4999, 5000] {
        let now = Instant::now();
        let mut master = Node::new(now, 1, 1);
        let mut client = Node::new(now, 2, 1);
        master.command(
            now,
            Command::Host {
                game: "g".into(),
                player: "host".into(),
                cfg: Config::default(),
            },
        );
        client.set_server(Some(addr_of(0)));
        client.command(
            now,
            Command::HostOnServer {
                game: "g".into(),
                player: "client".into(),
            },
        );
        let join = client
            .drain_out()
            .into_iter()
            .find_map(|out| match out {
                Out::Unicast(_, bytes) => Some(bytes),
                _ => None,
            })
            .unwrap();
        master.on_datagram(now, addr_of(1), false, &join);
        master.on_tick(now);
        let packets: Vec<_> = master
            .drain_out()
            .into_iter()
            .map(|out| match out {
                Out::Unicast(_, bytes) | Out::Multicast(bytes) => bytes,
            })
            .collect();
        let get = |predicate: fn(&Body) -> bool| {
            packets
                .iter()
                .find(|bytes| predicate(&decode(bytes).unwrap().body))
                .unwrap()
        };
        client.on_datagram(now, addr_of(0), false, get(|b| matches!(b, Body::Ack)));
        client.on_datagram(
            now,
            addr_of(0),
            true,
            get(|b| matches!(b, Body::Announcement(_))),
        );
        client.on_datagram(
            now + Duration::from_millis(at),
            addr_of(0),
            false,
            get(|b| matches!(b, Body::State(_))),
        );
        if at == 4999 {
            assert_eq!(client.phase(), PhaseKind::InGame);
        } else {
            assert_eq!(client.phase(), PhaseKind::Lobby);
            assert!(
                client
                    .drain_events()
                    .iter()
                    .any(|e| matches!(e, Event::Failure(reason) if reason.contains("состояние")))
            );
        }
    }
}

#[test]
fn joining_search_polls_course_server_for_new_announcements() {
    let now = Instant::now();
    let mut client = Node::new(now, 1, 1);
    client.set_server(Some(addr_of(3)));
    client.command(
        now,
        Command::Join {
            key: GameKey {
                name: "target".into(),
                addr: addr_of(0),
            },
            player: "client".into(),
            status: PlayerStatus::Viewing,
        },
    );
    client.drain_out();
    client.on_tick(now + Duration::from_millis(5000));
    assert!(client.drain_out().iter().any(|out| match out {
        Out::Unicast(to, bytes) if *to == addr_of(3) =>
            matches!(decode(bytes).unwrap().body, Body::Discover),
        _ => false,
    }));
    client.on_tick(now + Duration::from_millis(5999));
    assert!(client.drain_out().is_empty());
    client.on_tick(now + Duration::from_millis(6000));
    assert!(client.drain_out().iter().any(|out| match out {
        Out::Unicast(to, bytes) if *to == addr_of(3) =>
            matches!(decode(bytes).unwrap().body, Body::Discover),
        _ => false,
    }));
}

#[test]
fn acknowledged_join_restarts_cleanly_after_master_timeout() {
    let now = Instant::now();
    let mut old_master = Node::new(now, 1, 1);
    let mut new_master = Node::new(now, 2, 1);
    let mut client = Node::new(now, 3, 1);
    for master in [&mut old_master, &mut new_master] {
        master.command(
            now,
            Command::Host {
                game: "g".into(),
                player: "host".into(),
                cfg: Config::default(),
            },
        );
    }
    client.set_server(Some(addr_of(0)));
    client.command(
        now,
        Command::HostOnServer {
            game: "g".into(),
            player: "client".into(),
        },
    );
    let old_join = client
        .drain_out()
        .into_iter()
        .find_map(|out| match out {
            Out::Unicast(_, bytes) => Some(bytes),
            _ => None,
        })
        .unwrap();
    old_master.on_datagram(now, addr_of(2), false, &old_join);
    let old_packets: Vec<_> = old_master
        .drain_out()
        .into_iter()
        .filter_map(|out| match out {
            Out::Unicast(_, bytes) => Some(bytes),
            _ => None,
        })
        .collect();
    let ack = old_packets
        .iter()
        .find(|bytes| matches!(decode(bytes).unwrap().body, Body::Ack))
        .unwrap();
    let old_state = old_packets
        .iter()
        .find(|bytes| matches!(decode(bytes).unwrap().body, Body::State(_)))
        .unwrap();
    client.on_datagram(now, addr_of(0), false, ack);
    assert!(client.joining_view().unwrap().acked);
    client.on_tick(now + Duration::from_millis(1001));
    assert_eq!(client.phase(), PhaseKind::Joining);
    client.drain_out();

    client.on_datagram(
        now + Duration::from_millis(1010),
        addr_of(0),
        false,
        old_state,
    );
    assert_eq!(client.phase(), PhaseKind::Joining);
    assert!(
        client.drain_out().is_empty(),
        "старое состояние игнорируется при поиске"
    );
    new_master.on_tick(now + Duration::from_millis(1100));
    let announcement = new_master
        .drain_out()
        .into_iter()
        .find_map(|out| match out {
            Out::Multicast(bytes)
                if matches!(decode(&bytes).unwrap().body, Body::Announcement(_)) =>
            {
                Some(bytes)
            }
            _ => None,
        })
        .unwrap();
    client.on_datagram(
        now + Duration::from_millis(1100),
        addr_of(1),
        true,
        &announcement,
    );
    let restarted = client.joining_view().unwrap();
    assert!(!restarted.acked);
    assert!(restarted.has_config);
    assert_eq!(restarted.started, now + Duration::from_millis(1100));
    let new_join = client
        .drain_out()
        .into_iter()
        .find_map(|out| match out {
            Out::Unicast(to, bytes)
                if to == addr_of(1)
                    && matches!(decode(&bytes).unwrap().body, Body::Join { .. }) =>
            {
                Some(bytes)
            }
            _ => None,
        })
        .unwrap();
    assert!(decode(&new_join).unwrap().seq > decode(&old_join).unwrap().seq);

    client.on_datagram(now + Duration::from_millis(1100), addr_of(0), false, ack);
    client.on_datagram(
        now + Duration::from_millis(1100),
        addr_of(0),
        false,
        old_state,
    );
    assert!(!client.joining_view().unwrap().acked);
    new_master.on_datagram(
        now + Duration::from_millis(1100),
        addr_of(2),
        false,
        &new_join,
    );
    let mut packets: Vec<_> = new_master
        .drain_out()
        .into_iter()
        .filter_map(|out| match out {
            Out::Unicast(_, bytes) => Some(bytes),
            _ => None,
        })
        .collect();
    packets.sort_by_key(|bytes| u8::from(matches!(decode(bytes).unwrap().body, Body::Ack)));
    for bytes in packets {
        client.on_datagram(now + Duration::from_millis(1100), addr_of(1), false, &bytes);
    }
    assert_eq!(client.game_view().unwrap().master, Some(addr_of(1)));
}

#[test]
fn joining_sends_ping_after_ack_before_receiving_config() {
    let now = Instant::now();
    let mut client = Node::new(now, 1, 1);
    client.set_server(Some(addr_of(0)));
    client.command(
        now,
        Command::HostOnServer {
            game: "g".into(),
            player: "client".into(),
        },
    );
    let join = client
        .drain_out()
        .into_iter()
        .find_map(|out| match out {
            Out::Unicast(_, bytes) => Some(decode(&bytes).unwrap()),
            _ => None,
        })
        .unwrap();
    let ack = Message {
        seq: join.seq,
        sender_id: Some(1),
        receiver_id: Some(2),
        game_name: Some("g".into()),
        body: Body::Ack,
    };
    client.on_datagram(now, addr_of(0), false, &encode(&ack).unwrap());
    client.on_tick(now + Duration::from_millis(100));
    assert!(client.drain_out().iter().any(|out| match out {
        Out::Unicast(to, bytes) if *to == addr_of(0) => {
            let msg = decode(bytes).unwrap();
            matches!(msg.body, Body::Ping) && msg.sender_id == Some(2) && msg.receiver_id == Some(1)
        }
        _ => false,
    }));
}

#[test]
fn join_to_unreachable_game_fails_with_reason() {
    let mut sim = Sim::new(2, 7);
    let key = GameKey {
        name: "нет такой".into(),
        addr: addr_of(1),
    };
    sim.kill(1);
    sim.cmd(
        0,
        Command::Join {
            key,
            player: "A".into(),
            status: PlayerStatus::Playing,
        },
    );
    sim.run(4900);
    assert_eq!(sim.node(0).phase(), PhaseKind::Joining);
    sim.run(400);
    assert_eq!(sim.node(0).phase(), PhaseKind::Joining);
    sim.run(2700);
    assert_eq!(sim.node(0).phase(), PhaseKind::Lobby);
    let f = sim.failures(0);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].contains("ACK не получен"), "{}", f[0]);
}

#[test]
fn seventh_participant_is_rejected_with_error_text() {
    let mut sim = Sim::new(7, 8);
    sim.host(0, "g", "A");
    sim.run(600);
    for i in 1..6 {
        sim.join(i, 0, "g", &format!("P{i}"), PlayerStatus::Viewing);
        sim.run(400);
    }
    assert_eq!(sim.view(0).players.len(), 6);
    sim.join(6, 0, "g", "late", PlayerStatus::Viewing);
    sim.run(800);
    assert_eq!(sim.node(6).phase(), PhaseKind::Lobby);
    assert!(
        sim.failures(6)
            .iter()
            .any(|f| f.contains("нет свободных мест")),
        "{:?}",
        sim.failures(6)
    );
}

#[test]
fn fifth_player_gets_error_but_can_watch() {
    let mut sim = Sim::new(6, 9);
    sim.host(0, "g", "A");
    sim.run(600);
    for i in 1..4 {
        sim.join(i, 0, "g", &format!("P{i}"), PlayerStatus::Playing);
        sim.run(500);
    }
    assert_eq!(sim.view(0).world.snakes.len(), 4);
    sim.join(4, 0, "g", "five", PlayerStatus::Playing);
    sim.run(900);
    assert_eq!(sim.node(4).phase(), PhaseKind::Lobby);
    assert!(
        sim.failures(4).iter().any(|f| f.contains("Нет места")),
        "{:?}",
        sim.failures(4)
    );

    sim.join(4, 0, "g", "five", PlayerStatus::Viewing);
    sim.run(900);
    assert!(sim.in_game(4));
    // Попытка вернуться к игре без свободной змейки — ошибка, статус не меняется.
    sim.cmd(4, Command::ToggleWatch);
    sim.run(900);
    assert_eq!(sim.view(4).status, PlayerStatus::Viewing);
    assert!(
        sim.failures(4).iter().any(|f| f.contains("4 змейки")),
        "{:?}",
        sim.failures(4)
    );
}

#[test]
fn silent_participant_is_removed_by_master() {
    let mut sim = Sim::new(3, 10);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.join(2, 0, "g", "C", PlayerStatus::Playing);
    sim.run(1500);
    assert_eq!(sim.view(0).players.len(), 3);
    sim.kill(2);
    sim.run(1600);
    let host = sim.view(0);
    assert_eq!(host.players.len(), 2);
    assert!(
        host.world.snake(3).is_none(),
        "змейка удалённого участника исчезает"
    );
    assert_eq!(sim.view(1).players.len(), 2);
}

#[test]
fn deputy_takes_over_when_master_dies() {
    let mut sim = Sim::new(3, 11);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.join(2, 0, "g", "C", PlayerStatus::Playing);
    sim.run(2000);
    let deputy = (1..3)
        .find(|&i| sim.view(i).role == NodeRole::Deputy)
        .expect("заместитель назначен");
    let other = 3 - deputy;
    let order_before = sim.view(deputy).order;

    sim.kill(0);
    sim.run(3000);

    assert_eq!(
        sim.view(deputy).role,
        NodeRole::Master,
        "события: {:?}",
        sim.events(deputy)
    );
    let v = sim.view(other);
    assert_eq!(
        v.master,
        Some(addr_of(deputy)),
        "второй узел переключился на нового ведущего"
    );
    assert!(v.order > order_before, "состояния продолжают публиковаться");
    assert!(sim.in_game(other), "{:?}", sim.failures(other));
    // Прежний ведущий и его змейка удалены, остальные на месте.
    let nv = sim.view(deputy);
    assert_eq!(nv.players.len(), 2);
    assert!(nv.players.iter().all(|p| p.name != "A"));
    assert!(nv.world.snake(1).is_none());
    // Новый ведущий назначил заместителя.
    assert!(nv.players.iter().any(|p| p.role == NodeRole::Deputy));
}

#[test]
fn newly_joined_normal_fails_over_without_any_further_master_packets() {
    let mut sim = Sim::new(3, 93);
    sim.host(0, "g", "A");
    sim.join(1, 0, "g", "B", PlayerStatus::Viewing);
    sim.run(600);
    assert_eq!(sim.view(1).role, NodeRole::Deputy);

    sim.join(2, 0, "g", "C", PlayerStatus::Viewing);
    for _ in 0..100 {
        if sim.in_game(2) {
            break;
        }
        sim.step(5);
    }
    assert_eq!(sim.view(2).role, NodeRole::Normal);
    sim.kill(0);
    sim.queue.retain(|packet| packet.from != addr_of(0));
    sim.run(1500);

    assert_eq!(sim.view(1).role, NodeRole::Master);
    assert_eq!(sim.view(2).master, Some(addr_of(1)));
    assert_eq!(sim.view(2).players.len(), 2);
}

#[test]
fn commands_still_work_after_failover() {
    let mut sim = Sim::new(3, 12);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.join(2, 0, "g", "C", PlayerStatus::Playing);
    sim.run(2000);
    let deputy = (1..3)
        .find(|&i| sim.view(i).role == NodeRole::Deputy)
        .unwrap();
    let other = 3 - deputy;
    sim.kill(0);
    sim.run(3000);

    // Змейка могла погибнуть раньше, поэтому проверяем переход в противоположный статус.
    let before = sim.view(other).status;
    let snakes_before = sim.view(deputy).world.snakes.len();
    sim.cmd(other, Command::ToggleWatch);
    sim.run(1000);
    let expected = match before {
        PlayerStatus::Playing => PlayerStatus::Viewing,
        PlayerStatus::Viewing => PlayerStatus::Playing,
    };
    assert_eq!(sim.view(other).status, expected);
    let snakes_after = sim.view(deputy).world.snakes.len();
    assert_eq!(
        snakes_after as i32 - snakes_before as i32,
        if expected == PlayerStatus::Playing {
            1
        } else {
            -1
        }
    );
}

#[test]
fn graceful_master_exit_hands_over_without_timeout() {
    let mut sim = Sim::new(3, 13);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.join(2, 0, "g", "C", PlayerStatus::Playing);
    sim.run(2000);
    let deputy = (1..3)
        .find(|&i| sim.view(i).role == NodeRole::Deputy)
        .unwrap();

    sim.cmd(0, Command::Leave);
    sim.run(500); // заметно быстрее тайм-аута связи в 1000 мс
    assert_eq!(sim.node(0).phase(), PhaseKind::Lobby);
    assert_eq!(
        sim.view(deputy).role,
        NodeRole::Master,
        "{:?}",
        sim.events(deputy)
    );
    assert!(sim.view(deputy).players.iter().all(|p| p.name != "A"));
    sim.run(1500);
    assert!(sim.in_game(1) && sim.in_game(2));
}

#[test]
fn master_handover_waits_for_each_ack_in_order() {
    let mut sim = Sim::new(2, 53);
    sim.host(0, "g", "A");
    sim.join(1, 0, "g", "B", PlayerStatus::Viewing);
    sim.run(600);
    assert_eq!(sim.view(1).role, NodeRole::Deputy);

    let now = sim.now;
    let master = &mut sim.nodes[0].node;
    master.command(now, Command::Leave);
    let state = master
        .drain_out()
        .into_iter()
        .find_map(|out| match out {
            Out::Unicast(to, bytes) if to == addr_of(1) => {
                let msg = decode(&bytes).unwrap();
                matches!(msg.body, Body::State(_)).then_some(msg)
            }
            _ => None,
        })
        .expect("ожидался снимок для заместителя");

    let ack = |seq| Message {
        seq,
        sender_id: Some(2),
        receiver_id: Some(1),
        game_name: Some("g".into()),
        body: Body::Ack,
    };
    master.on_datagram(now, addr_of(2), false, &encode(&ack(state.seq)).unwrap());
    assert!(master.drain_out().is_empty());
    let retry_time = now + Duration::from_millis(100);
    master.on_tick(retry_time);
    assert!(master.drain_out().iter().any(|out| match out {
        Out::Unicast(to, bytes) => *to == addr_of(1) && decode(bytes).unwrap() == state,
        _ => false,
    }));

    master.on_datagram(
        retry_time,
        addr_of(1),
        false,
        &encode(&ack(state.seq)).unwrap(),
    );
    let role = master
        .drain_out()
        .into_iter()
        .find_map(|out| match out {
            Out::Unicast(to, bytes) if to == addr_of(1) => {
                let msg = decode(&bytes).unwrap();
                matches!(
                    msg.body,
                    Body::RoleChange {
                        sender: Some(NodeRole::Normal),
                        receiver: Some(NodeRole::Master),
                        ..
                    }
                )
                .then_some(msg)
            }
            _ => None,
        })
        .expect("после ACK снимка должна передаваться роль");
    master.on_datagram(
        retry_time,
        addr_of(1),
        false,
        &encode(&ack(state.seq)).unwrap(),
    );
    master.on_tick(retry_time);
    assert_eq!(master.phase(), PhaseKind::InGame);

    master.on_datagram(
        retry_time,
        addr_of(1),
        false,
        &encode(&ack(role.seq)).unwrap(),
    );
    master.on_tick(retry_time);
    assert_eq!(master.phase(), PhaseKind::Lobby);
}

#[test]
fn master_handover_does_not_wait_forever_for_ack() {
    let mut sim = Sim::new(2, 54);
    sim.host(0, "g", "A");
    sim.join(1, 0, "g", "B", PlayerStatus::Viewing);
    sim.run(600);
    let now = sim.now;
    let master = &mut sim.nodes[0].node;
    master.command(now, Command::Leave);
    master.on_tick(now + Duration::from_millis(999));
    assert_eq!(master.phase(), PhaseKind::InGame);
    master.on_tick(now + Duration::from_millis(1000));
    assert_eq!(master.phase(), PhaseKind::Lobby);
}

#[test]
fn normal_participant_leaving_is_removed_immediately() {
    let mut sim = Sim::new(3, 14);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.join(2, 0, "g", "C", PlayerStatus::Playing);
    sim.run(2000);
    sim.cmd(2, Command::Leave);
    sim.run(400);
    assert_eq!(sim.node(2).phase(), PhaseKind::Lobby);
    assert_eq!(sim.view(0).players.len(), 2);
    assert!(sim.view(0).world.snake(3).is_none());
}

#[test]
fn deputy_is_reassigned_after_deputy_leaves() {
    let mut sim = Sim::new(3, 15);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Viewing);
    sim.join(2, 0, "g", "C", PlayerStatus::Viewing);
    sim.run(2000);
    let deputy = (1..3)
        .find(|&i| sim.view(i).role == NodeRole::Deputy)
        .unwrap();
    let other = 3 - deputy;
    sim.cmd(deputy, Command::Leave);
    sim.run(1200);
    assert_eq!(
        sim.view(other).role,
        NodeRole::Deputy,
        "наблюдатель тоже может стать заместителем"
    );
}

#[test]
fn everyone_lost_gives_error_with_reason() {
    let mut sim = Sim::new(3, 16);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.join(2, 0, "g", "C", PlayerStatus::Playing);
    sim.run(2000);
    // Недоступны и ведущий, и заместитель: оставшийся участник сообщает причину.
    let deputy = (1..3)
        .find(|&i| sim.view(i).role == NodeRole::Deputy)
        .unwrap();
    let other = 3 - deputy;
    sim.kill(0);
    sim.kill(deputy);
    sim.run(4000);
    assert_eq!(sim.node(other).phase(), PhaseKind::Lobby);
    assert!(
        sim.failures(other)
            .iter()
            .any(|f| f.contains("заместитель")),
        "{:?}",
        sim.failures(other)
    );
}

#[test]
fn server_style_creation_by_join_learns_config_via_discover() {
    let mut sim = Sim::new(2, 17);
    sim.server = Some(1);
    sim.nodes[0].node.set_server(Some(addr_of(1)));
    sim.cmd(
        0,
        Command::HostOnServer {
            game: "новая".into(),
            player: "Гость".into(),
        },
    );
    sim.run(2500);
    assert!(
        sim.in_game(0),
        "события: {:?}; {:?}",
        sim.events(0),
        sim.failures(0)
    );
    assert_eq!(sim.view(0).name, "новая");
    assert_eq!(sim.view(0).players.len(), 2);
}

#[test]
fn joining_ignores_other_games_even_if_their_config_is_valid() {
    let now = Instant::now();
    let mut client = Node::new(now, 10, 1);
    client.set_server(Some(addr_of(0)));
    client.command(
        now,
        Command::HostOnServer {
            game: "target".into(),
            player: "client".into(),
        },
    );
    let join = client
        .drain_out()
        .into_iter()
        .find_map(|out| match out {
            Out::Unicast(_, bytes) => Some(decode(&bytes).unwrap()),
            _ => None,
        })
        .unwrap();
    let announcement = |game: &str, cfg| Message {
        seq: 10,
        sender_id: None,
        receiver_id: None,
        game_name: None,
        body: Body::Announcement(vec![Announcement {
            game_name: game.into(),
            config: cfg,
            players: vec![],
            can_join: true,
        }]),
    };
    client.on_datagram(
        now,
        addr_of(0),
        false,
        &encode(&announcement("other", Config::default())).unwrap(),
    );
    assert!(!client.joining_view().unwrap().has_config);
    let invalid = Config {
        width: 9,
        ..Config::default()
    };
    client.on_datagram(
        now,
        addr_of(0),
        false,
        &encode(&announcement("target", invalid)).unwrap(),
    );
    assert!(!client.joining_view().unwrap().has_config);
    let wrong_game = Message {
        seq: join.seq,
        sender_id: Some(1),
        receiver_id: Some(2),
        game_name: Some("other".into()),
        body: Body::Ack,
    };
    client.on_datagram(now, addr_of(0), false, &encode(&wrong_game).unwrap());
    assert!(!client.joining_view().unwrap().acked);
}

#[test]
fn joining_retries_and_discovers_at_the_required_intervals() {
    let now = Instant::now();
    let mut client = Node::new(now, 10, 1);
    client.set_server(Some(addr_of(0)));
    client.command(
        now,
        Command::HostOnServer {
            game: "target".into(),
            player: "client".into(),
        },
    );
    let join_bytes = client
        .drain_out()
        .into_iter()
        .find_map(|out| match out {
            Out::Unicast(_, bytes) => Some(bytes),
            _ => None,
        })
        .unwrap();
    let join = decode(&join_bytes).unwrap();
    client.on_tick(now + Duration::from_millis(99));
    assert!(client.drain_out().is_empty());
    client.on_tick(now + Duration::from_millis(100));
    assert!(client.drain_out().iter().any(|out| match out {
        Out::Unicast(_, bytes) => *bytes == join_bytes,
        _ => false,
    }));
    let ack = Message {
        seq: join.seq,
        sender_id: Some(1),
        receiver_id: Some(2),
        game_name: Some("target".into()),
        body: Body::Ack,
    };
    client.on_datagram(
        now + Duration::from_millis(100),
        addr_of(0),
        false,
        &encode(&ack).unwrap(),
    );
    client.on_tick(now + Duration::from_millis(499));
    assert!(!client.drain_out().iter().any(|out| matches!(out, Out::Unicast(_, bytes) if matches!(decode(bytes).unwrap().body, Body::Discover))));
    client.on_tick(now + Duration::from_millis(500));
    assert!(client.drain_out().iter().any(|out| matches!(out, Out::Unicast(_, bytes) if matches!(decode(bytes).unwrap().body, Body::Discover))));
    let announcement = Message {
        seq: 99,
        sender_id: None,
        receiver_id: None,
        game_name: None,
        body: Body::Announcement(vec![Announcement {
            game_name: "target".into(),
            config: Config::default(),
            players: vec![],
            can_join: true,
        }]),
    };
    client.on_datagram(
        now + Duration::from_millis(500),
        addr_of(0),
        false,
        &encode(&announcement).unwrap(),
    );
    client.on_tick(now + Duration::from_millis(1000));
    assert!(!client.drain_out().iter().any(|out| matches!(out, Out::Unicast(_, bytes) if matches!(decode(bytes).unwrap().body, Body::Discover))));
    let ping = Message {
        seq: 100,
        sender_id: Some(1),
        receiver_id: Some(2),
        game_name: Some("target".into()),
        body: Body::Ping,
    };
    client.on_datagram(
        now + Duration::from_millis(4999),
        addr_of(0),
        false,
        &encode(&ping).unwrap(),
    );
    client.on_tick(now + Duration::from_millis(5000));
    assert_eq!(client.phase(), PhaseKind::Lobby);
    assert!(
        client
            .drain_events()
            .iter()
            .any(|e| matches!(e, Event::Failure(s) if s.contains("состояние")))
    );
}

#[test]
fn joining_accepts_ack_state_and_config_in_any_order() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let now = Instant::now();
        let mut master = Node::new(now, 1, 1);
        let mut client = Node::new(now, 2, 1);
        master.command(
            now,
            Command::Host {
                game: "order".into(),
                player: "master".into(),
                cfg: Config::default(),
            },
        );
        client.set_server(Some(addr_of(0)));
        client.command(
            now,
            Command::HostOnServer {
                game: "order".into(),
                player: "client".into(),
            },
        );
        let join = client
            .drain_out()
            .into_iter()
            .find_map(|out| match out {
                Out::Unicast(_, bytes) => Some(bytes),
                _ => None,
            })
            .unwrap();
        master.on_datagram(now, addr_of(1), false, &join);
        master.on_tick(now);
        let mut packets: [Option<Vec<u8>>; 3] = [None, None, None];
        for out in master.drain_out() {
            let bytes = match out {
                Out::Unicast(_, bytes) | Out::Multicast(bytes) => bytes,
            };
            let index = match decode(&bytes).unwrap().body {
                Body::Ack => 0,
                Body::State(_) => 1,
                Body::Announcement(_) => 2,
                _ => continue,
            };
            packets[index] = Some(bytes);
        }
        client.on_datagram(now, addr_of(2), false, packets[0].as_ref().unwrap());
        client.on_datagram(now, addr_of(2), false, packets[2].as_ref().unwrap());
        assert!(!client.joining_view().unwrap().acked);
        assert!(!client.joining_view().unwrap().has_config);

        let mut newer = decode(packets[1].as_ref().unwrap()).unwrap();
        newer.seq += 100;
        let Body::State(ref mut state) = newer.body else {
            unreachable!()
        };
        state.order += 10;
        let expected_order = state.order;
        let mut same_order = newer.clone();
        same_order.seq += 1;
        let Body::State(ref mut state) = same_order.body else {
            unreachable!();
        };
        state.players[0].score = 123;
        let mut after_ack = newer.clone();
        after_ack.seq += 2;
        let Body::State(ref mut state) = after_ack.body else {
            unreachable!();
        };
        state.order += 1;
        for (i, index) in order.into_iter().enumerate() {
            if index == 1 {
                client.on_datagram(now, addr_of(0), false, &encode(&newer).unwrap());
                client.on_datagram(now, addr_of(0), false, &encode(&same_order).unwrap());
                if order[0] == 0 {
                    client.on_datagram(now, addr_of(0), false, &encode(&after_ack).unwrap());
                }
            }
            client.on_datagram(
                now,
                addr_of(0),
                index == 2,
                packets[index].as_ref().unwrap(),
            );
            if index == 1 {
                let replies = client.drain_out();
                let early_ack = replies
                    .iter()
                    .find_map(|out| match out {
                        Out::Unicast(to, bytes) if *to == addr_of(0) => {
                            let msg = decode(bytes).unwrap();
                            (matches!(msg.body, Body::Ack) && msg.seq == newer.seq).then_some(msg)
                        }
                        _ => None,
                    })
                    .expect("State должен быть подтверждён до завершения подключения");
                assert_eq!(early_ack.sender_id, Some(2));
                assert_eq!(early_ack.receiver_id, Some(1));
            }
            if i < 2 {
                assert_eq!(client.phase(), PhaseKind::Joining, "{order:?}");
            }
        }
        let view = client
            .game_view()
            .unwrap_or_else(|| panic!("joining failed for {order:?}"));
        assert_eq!(view.name, "order");
        assert_eq!(view.my_id, 2);
        assert_eq!(view.players.len(), 2);
        assert_eq!(view.status, PlayerStatus::Playing);
        assert_eq!(view.order, expected_order + i32::from(order[0] == 0));
        assert_ne!(view.players[0].score, 123);
        client.drain_out();
        client.on_tick(now + Duration::from_millis(100));
        assert!(
            client.drain_out().iter().any(|out| match out {
                Out::Unicast(to, bytes) if *to == addr_of(0) => {
                    let msg = decode(bytes).unwrap();
                    matches!(msg.body, Body::Ping)
                        && msg.sender_id == Some(2)
                        && msg.receiver_id == Some(1)
                }
                _ => false,
            }),
            "после подключения Ping должен адресоваться ведущему: {order:?}"
        );
    }
}

#[test]
fn same_game_from_lan_and_server_is_listed_once() {
    let mut sim = Sim::new(2, 18);
    sim.host(0, "g", "A");
    sim.nodes[1].node.set_server(Some(addr_of(0)));
    sim.run(3000);
    let entries = sim.node(1).catalog_entries();
    assert_eq!(entries.len(), 1);
    let now = sim.now;
    assert!(entries[0].from_lan(now), "пришло по multicast");
    assert!(
        entries[0].from_server(now),
        "пришло ответом сервера на Discover"
    );
    assert!(sim.node(1).server_online(now));
}

#[test]
fn old_catalog_entries_expire() {
    let mut sim = Sim::new(2, 19);
    sim.host(0, "g", "A");
    sim.run(1500);
    assert_eq!(sim.node(1).catalog_entries().len(), 1);
    sim.kill(0);
    sim.run(3400);
    assert!(sim.node(1).catalog_entries().is_empty());
}

#[test]
fn states_apply_in_order_and_old_ones_are_ignored() {
    let mut sim = Sim::new(2, 20);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.run(1500);
    let before = sim.view(1).order;
    // Подаём участнику старое состояние (порядок 1) от имени ведущего: оно не должно откатить игру.
    let stale = Message {
        seq: 999_999,
        sender_id: Some(1),
        receiver_id: Some(2),
        game_name: Some("g".into()),
        body: Body::State(StateMsg {
            order: 1,
            snakes: vec![],
            foods: vec![],
            players: sim.view(0).players.to_vec(),
            next_player_id: 3,
        }),
    };
    let bytes = encode(&stale).unwrap();
    let now = sim.now;
    sim.nodes[1]
        .node
        .on_datagram(now, addr_of(0), false, &bytes);
    assert!(sim.view(1).order >= before);
}

#[test]
fn many_unique_ids_across_leave_and_rejoin() {
    let mut sim = Sim::new(3, 21);
    sim.host(0, "g", "A");
    sim.run(600);
    let mut seen = HashSet::new();
    for round in 0..3 {
        sim.join(1, 0, "g", "B", PlayerStatus::Viewing);
        sim.run(900);
        assert!(sim.in_game(1), "раунд {round}: {:?}", sim.events(1));
        assert!(
            seen.insert(sim.view(1).my_id),
            "ID не должны переиспользоваться"
        );
        sim.cmd(1, Command::Leave);
        sim.run(500);
    }
}

/// Фаззинг: потери, дубли и перестановки, затем гибель ведущего. Для каждого
/// начального значения генератора должны выполняться инварианты протокола.
#[test]
fn lossy_network_failover_fuzz() {
    for seed in 100..140u64 {
        let mut sim = Sim::new(4, seed);
        sim.loss = 0.15;
        sim.dup = 0.10;
        sim.jitter_ms = 40;
        sim.host(0, "g", "A");
        sim.run(2200);
        for i in 1..4 {
            sim.join(i, 0, "g", &format!("P{i}"), PlayerStatus::Playing);
        }
        sim.run(7000);
        let joined: Vec<usize> = (1..4).filter(|&i| sim.in_game(i)).collect();
        assert_eq!(
            joined.len(),
            3,
            "seed {seed}: не все присоединились: {:?}",
            (1..4).map(|i| sim.failures(i)).collect::<Vec<_>>()
        );
        assert_eq!(sim.view(0).players.len(), 4, "seed {seed}");

        sim.kill(0);
        sim.run(8000);
        let alive: Vec<usize> = (1..4).filter(|&i| sim.in_game(i)).collect();
        assert!(!alive.is_empty(), "seed {seed}: все вышли из игры");
        let masters: Vec<usize> = alive
            .iter()
            .copied()
            .filter(|&i| sim.view(i).role == NodeRole::Master)
            .collect();
        assert_eq!(
            masters.len(),
            1,
            "seed {seed}: ведущих должно быть ровно один, сейчас {masters:?}"
        );
        let m = masters[0];
        for &i in &alive {
            if i != m {
                assert_eq!(
                    sim.view(i).master,
                    Some(addr_of(m)),
                    "seed {seed}: узел {i} не последовал за новым ведущим"
                );
            }
        }
        // Состав участников согласован и без старого ведущего.
        let reference = ids(&sim.view(m));
        assert!(!reference.contains(&1), "seed {seed}");
        for &i in &alive {
            assert_eq!(ids(&sim.view(i)), reference, "seed {seed}: узел {i}");
        }
    }
}

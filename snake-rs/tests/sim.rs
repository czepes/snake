//! Сценарные тесты протокола в симуляторе сети: потери, дубли, перестановки,
//! «смерть» узлов, смена ведущего. Время виртуальное, сокетов нет.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

use snake::catalog::GameKey;
use snake::node::{Command, Event, GameView, Node, PhaseKind};
use snake::reliable::Out;
use snake::rules::{Rng, Snake};
use snake::wire::*;

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

    fn schedule(&mut self, to: usize, from: SocketAddrV4, mc: bool, bytes: Vec<u8>) {
        if self.chance(self.loss) {
            return;
        }
        let copies = if self.chance(self.dup) { 2 } else { 1 };
        for _ in 0..copies {
            let jitter = if self.jitter_ms > 0 { self.rng.next_u64() % (self.jitter_ms + 1) } else { 0 };
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
                    eprintln!("[{:>6}ms n{i}] {line}", self.now.duration_since(self.t0).as_millis());
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
                        if let Some(j) = index_of(to) {
                            if j < self.nodes.len() {
                                self.schedule(j, from, false, bytes);
                            }
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
        if self.server == Some(p.to) && self.nodes[p.to].node.phase() == PhaseKind::Lobby {
            if let Ok(m) = decode(&p.bytes) {
                if let Body::Join { game_name, .. } = m.body {
                    let cfg = Config { width: 20, height: 15, food_static: 2, state_delay_ms: 200 };
                    self.nodes[p.to].node.command(self.now, Command::Host { game: game_name, player: "Сервер".into(), cfg });
                }
            }
        }
        self.history.push((p.to, p.from, p.bytes.clone()));
        let now = self.now;
        self.nodes[p.to].node.on_datagram(now, p.from, p.mc, &p.bytes);
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
        self.nodes[i].node.game_view().unwrap_or_else(|| panic!("узел {i} не в игре"))
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
            .filter_map(|e| if let Event::Failure(t) = e { Some(t.clone()) } else { None })
            .collect()
    }

    fn host(&mut self, i: usize, name: &str, player: &str) {
        let cfg = Config { width: 20, height: 15, food_static: 2, state_delay_ms: 200 };
        self.cmd(i, Command::Host { game: name.into(), player: player.into(), cfg });
    }

    fn join(&mut self, i: usize, host: usize, name: &str, player: &str, status: PlayerStatus) {
        let key = GameKey { name: name.into(), addr: addr_of(host) };
        self.cmd(i, Command::Join { key, player: player.into(), status });
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
        assert!((v.order - reference.order).abs() <= 2, "порядок состояний расходится: {} и {}", v.order, reference.order);
        assert_eq!(ids(&v), ids(&reference), "составы участников расходятся у узла {i}");
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
    sim.cmd(1, Command::Join { key: key.clone(), player: "Петя".into(), status: PlayerStatus::Playing });
    sim.cmd(2, Command::Join { key, player: "Маша".into(), status: PlayerStatus::Viewing });
    sim.run(1500);

    for i in 0..3 {
        assert!(sim.in_game(i), "узел {i} должен быть в игре; события: {:?}", sim.events(i));
    }
    let host = sim.view(0);
    assert_eq!(host.role, NodeRole::Master);
    assert_eq!(host.players.len(), 3);
    assert_eq!(ids(&host), vec![1, 2, 3]);
    assert_eq!(host.world.snakes.len(), 2, "змейки у Васи и Пети, Маша наблюдает");
    assert_eq!(host.players.iter().filter(|p| p.role == NodeRole::Deputy).count(), 1);
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
            cells: [(10, 7), (10, 8), (11, 8), (11, 7), (11, 6), (10, 6)].into_iter().collect(),
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
    assert_eq!(host.players.len(), 4, "участники: {:?}; события ведущего: {:?}", host.players.iter().map(|p| (&p.name, p.id)).collect::<Vec<_>>(), sim.events(0));
    assert_eq!(ids(&host), vec![1, 2, 3, 4]);
    // Инвариант: змейка есть ровно у тех, кто играет (кто-то мог погибнуть за 10 секунд).
    let playing = host.players.iter().filter(|p| p.status == PlayerStatus::Playing).count();
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
fn join_to_unreachable_game_fails_with_reason() {
    let mut sim = Sim::new(2, 7);
    let key = GameKey { name: "нет такой".into(), addr: addr_of(1) };
    sim.kill(1);
    sim.cmd(0, Command::Join { key, player: "A".into(), status: PlayerStatus::Playing });
    sim.run(4900);
    assert_eq!(sim.node(0).phase(), PhaseKind::Joining);
    sim.run(400);
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
    assert!(sim.failures(6).iter().any(|f| f.contains("нет свободных мест")), "{:?}", sim.failures(6));
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
    assert!(sim.failures(4).iter().any(|f| f.contains("Нет места")), "{:?}", sim.failures(4));

    sim.join(4, 0, "g", "five", PlayerStatus::Viewing);
    sim.run(900);
    assert!(sim.in_game(4));
    // Попытка вернуться к игре без свободной змейки — ошибка, статус не меняется.
    sim.cmd(4, Command::ToggleWatch);
    sim.run(900);
    assert_eq!(sim.view(4).status, PlayerStatus::Viewing);
    assert!(sim.failures(4).iter().any(|f| f.contains("4 змейки")), "{:?}", sim.failures(4));
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
    assert!(host.world.snake(3).is_none(), "змейка удалённого участника исчезает");
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
    let deputy = (1..3).find(|&i| sim.view(i).role == NodeRole::Deputy).expect("заместитель назначен");
    let other = 3 - deputy;
    let order_before = sim.view(deputy).order;

    sim.kill(0);
    sim.run(3000);

    assert_eq!(sim.view(deputy).role, NodeRole::Master, "события: {:?}", sim.events(deputy));
    let v = sim.view(other);
    assert_eq!(v.master, Some(addr_of(deputy)), "второй узел переключился на нового ведущего");
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
fn commands_still_work_after_failover() {
    let mut sim = Sim::new(3, 12);
    sim.host(0, "g", "A");
    sim.run(600);
    sim.join(1, 0, "g", "B", PlayerStatus::Playing);
    sim.join(2, 0, "g", "C", PlayerStatus::Playing);
    sim.run(2000);
    let deputy = (1..3).find(|&i| sim.view(i).role == NodeRole::Deputy).unwrap();
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
        if expected == PlayerStatus::Playing { 1 } else { -1 }
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
    let deputy = (1..3).find(|&i| sim.view(i).role == NodeRole::Deputy).unwrap();

    sim.cmd(0, Command::Leave);
    sim.run(500); // заметно быстрее тайм-аута связи в 1000 мс
    assert_eq!(sim.node(0).phase(), PhaseKind::Lobby);
    assert_eq!(sim.view(deputy).role, NodeRole::Master, "{:?}", sim.events(deputy));
    assert!(sim.view(deputy).players.iter().all(|p| p.name != "A"));
    sim.run(1500);
    assert!(sim.in_game(1) && sim.in_game(2));
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
    let deputy = (1..3).find(|&i| sim.view(i).role == NodeRole::Deputy).unwrap();
    let other = 3 - deputy;
    sim.cmd(deputy, Command::Leave);
    sim.run(1200);
    assert_eq!(sim.view(other).role, NodeRole::Deputy, "наблюдатель тоже может стать заместителем");
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
    let deputy = (1..3).find(|&i| sim.view(i).role == NodeRole::Deputy).unwrap();
    let other = 3 - deputy;
    sim.kill(0);
    sim.kill(deputy);
    sim.run(4000);
    assert_eq!(sim.node(other).phase(), PhaseKind::Lobby);
    assert!(
        sim.failures(other).iter().any(|f| f.contains("заместитель")),
        "{:?}",
        sim.failures(other)
    );
}

#[test]
fn server_style_creation_by_join_learns_config_via_discover() {
    let mut sim = Sim::new(2, 17);
    sim.server = Some(1);
    sim.nodes[0].node.set_server(Some(addr_of(1)));
    sim.cmd(0, Command::HostOnServer { game: "новая".into(), player: "Гость".into() });
    sim.run(2500);
    assert!(sim.in_game(0), "события: {:?}; {:?}", sim.events(0), sim.failures(0));
    assert_eq!(sim.view(0).name, "новая");
    assert_eq!(sim.view(0).players.len(), 2);
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
    assert!(entries[0].from_server(now), "пришло ответом сервера на Discover");
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
    sim.nodes[1].node.on_datagram(now, addr_of(0), false, &bytes);
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
        assert!(seen.insert(sim.view(1).my_id), "ID не должны переиспользоваться");
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
        assert_eq!(joined.len(), 3, "seed {seed}: не все присоединились: {:?}", (1..4).map(|i| sim.failures(i)).collect::<Vec<_>>());
        assert_eq!(sim.view(0).players.len(), 4, "seed {seed}");

        sim.kill(0);
        sim.run(8000);
        let alive: Vec<usize> = (1..4).filter(|&i| sim.in_game(i)).collect();
        assert!(!alive.is_empty(), "seed {seed}: все вышли из игры");
        let masters: Vec<usize> = alive.iter().copied().filter(|&i| sim.view(i).role == NodeRole::Master).collect();
        assert_eq!(masters.len(), 1, "seed {seed}: ведущих должно быть ровно один, сейчас {masters:?}");
        let m = masters[0];
        for &i in &alive {
            if i != m {
                assert_eq!(sim.view(i).master, Some(addr_of(m)), "seed {seed}: узел {i} не последовал за новым ведущим");
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

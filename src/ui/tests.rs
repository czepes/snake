use super::*;
use crate::wire::MAX_PLAYERS;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn host(node: &mut Node, now: Instant) {
    node.command(
        now,
        Command::Host {
            game: "test".into(),
            player: "Игрок".into(),
            cfg: Config::default(),
        },
    );
    assert_eq!(node.phase(), PhaseKind::InGame);
}

fn tab(ui: &mut Ui, node: &mut Node, now: Instant) {
    ui.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), node, now);
}

#[test]
fn create_form_returns_a_command_without_starting_the_game() {
    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    let mut ui = Ui::new("Игрок".into());

    let open = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
    assert!(ui.handle_key(open, &node, now).is_none());
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let command = ui.handle_key(enter, &node, now).unwrap();
    assert!(matches!(
        &command,
        Command::Host { game, player, cfg }
            if game == "Игра" && player == "Игрок"
                && *cfg == (Config { width: 20, height: 15, food_static: 2, state_delay_ms: 300 })
    ));
    assert_eq!(node.phase(), PhaseKind::Lobby);

    node.command(now, command);
    assert_eq!(node.phase(), PhaseKind::InGame);
}

#[test]
fn game_keys_return_commands_without_changing_the_session() {
    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    host(&mut node, now);
    let mut ui = Ui::new("Игрок".into());
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

    for (code, expected) in [
        (KeyCode::Up, Dir::Up),
        (KeyCode::Char('ы'), Dir::Down),
        (KeyCode::Char('h'), Dir::Left),
        (KeyCode::Char('Д'), Dir::Right),
    ] {
        assert!(
            matches!(ui.handle_key(key(code), &node, now), Some(Command::Steer(d)) if d == expected)
        );
    }
    assert!(matches!(
        ui.handle_key(key(KeyCode::Char('v')), &node, now),
        Some(Command::ToggleWatch)
    ));
    assert!(matches!(
        ui.handle_key(key(KeyCode::Char('r')), &node, now),
        Some(Command::InviteRobot)
    ));
    assert!(matches!(
        ui.handle_key(key(KeyCode::Esc), &node, now),
        Some(Command::Leave)
    ));
    assert!(matches!(
        ui.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &node,
            now
        ),
        Some(Command::Quit)
    ));
    assert!(ui.handle_key(key(KeyCode::Tab), &node, now).is_none());
    assert_eq!(node.game_view().unwrap().status, PlayerStatus::Playing);
    assert_eq!(node.phase(), PhaseKind::InGame);
}

#[test]
fn closed_game_can_still_be_joined_as_a_viewer() {
    use crate::wire::{Announcement, Body, Message, decode, encode};
    use std::net::{Ipv4Addr, SocketAddrV4};

    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    let mut ui = Ui::new("Игрок".into());
    let address = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 4000);
    let announcement = Message {
        seq: 1,
        sender_id: None,
        receiver_id: None,
        game_name: None,
        body: Body::Announcement(vec![Announcement {
            game_name: "Занятая игра".into(),
            config: Config::default(),
            players: vec![],
            can_join: false,
        }]),
    };
    node.on_datagram(now, address, true, &encode(&announcement).unwrap());

    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    assert!(ui.handle_key(key(KeyCode::Enter), &node, now).is_none());
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    terminal.draw(|f| ui.draw(f, &node, now, true)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(text.contains("Нет места для змейки"));

    let command = ui.handle_key(key(KeyCode::Char('v')), &node, now).unwrap();
    node.command(now, command);
    assert_eq!(node.phase(), PhaseKind::Joining);
    let packets = node.drain_out();
    let crate::reliable::Out::Unicast(to, bytes) = &packets[0] else {
        panic!("ожидался unicast Join");
    };
    assert_eq!(*to, address);
    assert!(matches!(
        decode(bytes).unwrap().body,
        Body::Join {
            status: PlayerStatus::Viewing,
            ..
        }
    ));

    let cancel = ui.handle_key(key(KeyCode::Esc), &node, now).unwrap();
    node.command(now, cancel);
    assert_eq!(node.phase(), PhaseKind::Lobby);
}

#[test]
fn create_on_server_uses_server_command_and_keeps_parameters_independent() {
    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    let mut ui = Ui::new("Игрок".into());
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    node.set_server(Some("127.0.0.1:9193".parse().unwrap()));

    assert!(ui.handle_key(key(KeyCode::Char('c')), &node, now).is_none());
    for _ in 0..5 {
        assert!(ui.handle_key(key(KeyCode::Tab), &node, now).is_none());
    }
    assert!(ui.handle_key(key(KeyCode::Char(' ')), &node, now).is_none());
    let command = ui.handle_key(key(KeyCode::Enter), &node, now).unwrap();
    assert!(
        matches!(&command, Command::HostOnServer { game, player } if game == "Игра" && player == "Игрок")
    );
    node.command(now, command);
    assert_eq!(node.phase(), PhaseKind::Joining);
}

#[test]
fn create_form_edits_parameters_and_limits_game_name_in_utf8_bytes() {
    let now = Instant::now();
    let node = Node::new(now, 42, 1);
    let mut ui = Ui::new("Игрок".into());
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    ui.handle_key(key(KeyCode::Char('c')), &node, now);
    for _ in 0..4 {
        ui.handle_key(key(KeyCode::Backspace), &node, now);
    }
    for _ in 0..17 {
        ui.handle_key(key(KeyCode::Char('Я')), &node, now);
    }
    for (backspaces, value) in [(2, "10"), (2, "100"), (1, "0"), (3, "3000")] {
        ui.handle_key(key(KeyCode::Tab), &node, now);
        for _ in 0..backspaces {
            ui.handle_key(key(KeyCode::Backspace), &node, now);
        }
        for c in value.chars() {
            ui.handle_key(key(KeyCode::Char(c)), &node, now);
        }
    }
    let command = ui.handle_key(key(KeyCode::Enter), &node, now).unwrap();
    assert!(matches!(command, Command::Host { game, cfg, .. }
        if game == "Я".repeat(16)
            && cfg == (Config { width: 10, height: 100, food_static: 0, state_delay_ms: 3000 })));
}

#[test]
fn create_form_rejects_invalid_configuration_without_a_command() {
    let now = Instant::now();
    let node = Node::new(now, 42, 1);
    let mut ui = Ui::new("Игрок".into());
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    ui.handle_key(key(KeyCode::Char('c')), &node, now);
    ui.handle_key(key(KeyCode::Tab), &node, now);
    ui.handle_key(key(KeyCode::Backspace), &node, now);
    ui.handle_key(key(KeyCode::Backspace), &node, now);
    ui.handle_key(key(KeyCode::Char('9')), &node, now);
    assert!(ui.handle_key(key(KeyCode::Enter), &node, now).is_none());
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|f| ui.draw(f, &node, now, true)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(text.contains("Ширина и высота: 10–100"));
    assert_eq!(node.phase(), PhaseKind::Lobby);
}

#[test]
fn game_controls_follow_player_status_and_role() {
    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    host(&mut node, now);
    let mut view = node.game_view().unwrap();
    for role in [NodeRole::Master, NodeRole::Deputy, NodeRole::Normal] {
        for status in [PlayerStatus::Playing, PlayerStatus::Viewing] {
            view.role = role;
            view.status = status;
            let controls = game_controls(&view);
            let text = controls
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(text.contains("поворот"), status == PlayerStatus::Playing);
            assert_eq!(text.contains("[R]"), role == NodeRole::Master);
            assert!(text.contains(if status == PlayerStatus::Playing {
                "[V] наблюдать"
            } else {
                "[V] играть, если есть место"
            }));
            for key in ["[Tab]", "[Q] / Esc", "Ctrl+C"] {
                assert!(text.contains(key));
            }
            assert!(controls.iter().all(|line| line.width() <= 38));
        }
    }
}

#[test]
fn game_sidebar_fits_all_sections_for_six_players() {
    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    host(&mut node, now);
    let mut view = node.game_view().unwrap();
    let players: Vec<_> = (1..=MAX_PLAYERS)
        .map(|id| {
            let mut player = view.players[0].clone();
            player.id = id as i32;
            player.name = format!("Игрок {id}");
            player
        })
        .collect();
    view.players = &players;
    let ui = Ui::with_animations("Игрок".into(), false);
    let height = (players.len() as u16 + 3) + 7 + game_controls(&view).len() as u16 + 2;
    let mut terminal = Terminal::new(TestBackend::new(40, height)).unwrap();
    terminal.draw(|f| ui.draw_side(f, &view, f.area())).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .chunks(40)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    for label in [
        "Участники",
        "Игрок 6",
        "Параметры игры",
        "Состояние №",
        "Управление",
        "Ctrl+C — закрыть приложение",
    ] {
        assert!(text.contains(label), "Не отображается: {label}");
    }
}

#[test]
fn game_controls_are_visible_at_minimum_window_size() {
    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    host(&mut node, now);
    let mut ui = Ui::with_animations("Игрок".into(), false);
    let (width, height, _) = ui.min_size(&node);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| ui.draw(f, &node, now, false)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .chunks(width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Управление"));
    assert!(text.contains("Ctrl+C — закрыть приложение"));
    assert!(text.contains("Вы играете"));
}

#[test]
fn palette_shift_keeps_all_players_distinct_regardless_of_ids_or_order() {
    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    host(&mut node, now);
    let mut view = node.game_view().unwrap();
    let players: Vec<_> = [100, 3, 42, 7, 200, 12]
        .into_iter()
        .map(|id| {
            let mut player = view.players[0].clone();
            player.id = id;
            player
        })
        .collect();
    assert_eq!(players.len(), MAX_PLAYERS);
    assert!(PALETTE.len() >= MAX_PLAYERS);
    view.players = &players;
    for offset in 0..PALETTE.len() {
        let mut colors = Vec::new();
        for player in &players {
            let color = player_color(&view, player.id, offset);
            assert!(!colors.contains(&color));
            colors.push(color);
            assert_eq!(
                color,
                player_color(&view, player.id, offset + PALETTE.len())
            );
            assert_ne!(color, player_color(&view, player.id, offset + 1));
        }
    }
}

#[test]
fn tab_cycles_locally_and_preserves_selection_between_games() {
    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    let mut ui = Ui::new("Игрок".into());
    tab(&mut ui, &mut node, now);
    assert_eq!(ui.palette_offset, 0);
    host(&mut node, now);
    node.drain_out();
    node.drain_events();
    let before = node.game_view().unwrap();
    let order = before.order;
    let cells = before.world.snakes[0].cells.clone();
    for offset in 1..=PALETTE.len() {
        tab(&mut ui, &mut node, now);
        assert_eq!(ui.palette_offset, offset % PALETTE.len());
    }
    assert!(node.drain_out().is_empty());
    assert!(node.drain_events().is_empty());
    let after = node.game_view().unwrap();
    assert_eq!(after.order, order);
    assert_eq!(after.world.snakes[0].cells, cells);

    node.command(now, Command::ToggleWatch);
    assert_eq!(node.game_view().unwrap().status, PlayerStatus::Viewing);
    tab(&mut ui, &mut node, now);
    assert_eq!(ui.palette_offset, 1);
    node.command(now, Command::Leave);
    assert_eq!(node.phase(), PhaseKind::Lobby);
    host(&mut node, now);
    assert_eq!(ui.palette_offset, 1);
}

#[test]
fn tab_in_create_form_still_selects_the_next_field() {
    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    let mut ui = Ui::new("Игрок".into());
    ui.screen = Screen::Create;
    tab(&mut ui, &mut node, now);
    assert_eq!(ui.form.field, 1);
    assert_eq!(ui.palette_offset, 0);
}

#[test]
fn tab_recolors_board_and_player_swatch_with_or_without_animations() {
    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    host(&mut node, now);
    for enabled in [false, true] {
        let mut ui = Ui::with_animations("Игрок".into(), enabled);
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| ui.draw(f, &node, now, false)).unwrap();
        for &color in &PALETTE {
            let at = now + Duration::from_secs(2);
            terminal.draw(|f| ui.draw(f, &node, at, false)).unwrap();
            let buffer = terminal.backend().buffer();
            assert!(buffer.content.iter().any(|cell| cell.bg == color));
            let swatch = buffer
                .content
                .iter()
                .find(|cell| cell.symbol() == "■")
                .unwrap();
            assert_eq!(swatch.fg, color);
            tab(&mut ui, &mut node, at);
        }
    }
}

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

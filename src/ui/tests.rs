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
        for offset in 0..PALETTE.len() {
            let at = now + Duration::from_secs(2);
            terminal.draw(|f| ui.draw(f, &node, at, false)).unwrap();
            let buffer = terminal.backend().buffer();
            let color = PALETTE[offset];
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

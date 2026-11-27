use super::*;
use crate::rules::{Snake, World};
use crate::wire::{Config, Dir, NodeRole, PlayerStatus};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn snake(owner: i32, cells: &[Cell]) -> Snake {
    Snake {
        owner,
        cells: cells.iter().copied().collect(),
        dir: Dir::Right,
    }
}

fn world() -> World {
    let mut world = World::new(Config::default());
    world.snakes.push(snake(1, &[(2, 2), (1, 2)]));
    world.foods.push((3, 2));
    world
}

fn view(world: &World) -> GameView<'_> {
    GameView {
        name: "test",
        cfg: world.cfg,
        world,
        players: &[],
        my_id: 1,
        role: NodeRole::Master,
        status: PlayerStatus::Playing,
        order: 1,
        leaving: false,
        master: None,
        status_pending: false,
    }
}

#[test]
fn disabled_animations_do_not_track_state_or_change_the_frame() {
    let now = Instant::now();
    let world = world();
    let view = view(&world);
    let mut animations = Animations::new(false);
    animations.set_boom(true);
    let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();
    for phase in [
        PhaseKind::Lobby,
        PhaseKind::Joining,
        PhaseKind::InGame,
        PhaseKind::Lobby,
    ] {
        animations.update(phase, Some(&view), now);
        terminal
            .draw(|f| {
                let area = f.area();
                f.buffer_mut()
                    .set_string(0, 0, "unchanged", Style::new().fg(Color::Cyan));
                let before = f.buffer_mut().clone();
                animations.render_board(
                    f,
                    &view,
                    BoardViewport::new(area, (0, 0), (10, 10), 2),
                    now,
                    0,
                );
                animations.render_transition(f, now);
                assert_eq!(*f.buffer_mut(), before);
            })
            .unwrap();
        assert!(animations.state.is_none());
    }
}

#[test]
fn ui_lifecycle_renders_effects_then_returns_to_the_unanimated_frame() {
    use crate::node::{Command, Node};
    use crate::ui::Ui;

    let now = Instant::now();
    let mut node = Node::new(now, 42, 1);
    let mut animated = Ui::new("Игрок".into());
    let mut plain = Ui::with_animations("Игрок".into(), false);
    let mut animated_terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let mut plain_terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let mut compare = |node: &Node, at, equal| {
        animated_terminal
            .draw(|f| animated.draw(f, node, at, false))
            .unwrap();
        plain_terminal
            .draw(|f| plain.draw(f, node, at, false))
            .unwrap();
        assert_eq!(
            animated_terminal.backend().buffer() == plain_terminal.backend().buffer(),
            equal,
        );
    };
    compare(&node, now, true);
    node.command(
        now,
        Command::Host {
            game: "test".into(),
            player: "Игрок".into(),
            cfg: Config::default(),
        },
    );
    assert_eq!(node.phase(), PhaseKind::InGame);
    compare(&node, now, false);
    compare(&node, now + Duration::from_secs(2), true);
    node.command(now + Duration::from_secs(2), Command::ToggleWatch);
    assert!(node.game_view().unwrap().world.snakes.is_empty());
    compare(&node, now + Duration::from_secs(2), false);
    compare(&node, now + Duration::from_secs(3), true);
    node.command(now + Duration::from_secs(3), Command::ToggleWatch);
    assert_eq!(node.game_view().unwrap().world.snakes.len(), 1);
    compare(&node, now + Duration::from_secs(3), false);
    compare(&node, now + Duration::from_secs(4), true);
    node.command(now + Duration::from_secs(4), Command::Leave);
    assert_eq!(node.phase(), PhaseKind::Lobby);
    compare(&node, now + Duration::from_secs(4), false);
    compare(&node, now + Duration::from_secs(5), true);
}

#[test]
fn screen_transitions_work_in_both_directions_and_clear_board_state() {
    let now = Instant::now();
    let world = world();
    let mut animations = Animations::new(true);
    animations.update(PhaseKind::Lobby, None, now);
    assert!(animations.state.as_ref().unwrap().transition.is_none());
    animations.update(PhaseKind::InGame, Some(&view(&world)), now);
    let state = animations.state.as_ref().unwrap();
    assert!(state.transition.is_some());
    assert!(state.appearances.contains_key(&1));
    let later = now + Duration::from_millis(100);
    animations.update(PhaseKind::InGame, Some(&view(&world)), later);
    assert_eq!(
        animations
            .state
            .as_ref()
            .unwrap()
            .transition
            .as_ref()
            .unwrap()
            .started,
        now
    );
    animations.update(PhaseKind::Lobby, None, later);
    let state = animations.state.as_ref().unwrap();
    assert_eq!(state.transition.as_ref().unwrap().started, later);
    assert!(
        state.snakes.is_empty() && state.food_effects.is_empty() && state.appearances.is_empty()
    );
    animations.update(PhaseKind::Lobby, None, later + Duration::from_millis(300));
    assert!(animations.state.as_ref().unwrap().transition.is_none());
}

#[test]
fn joining_cancellation_and_rejoining_reset_effects() {
    let now = Instant::now();
    let world = world();
    let mut animations = Animations::new(true);
    for phase in [
        PhaseKind::Lobby,
        PhaseKind::Joining,
        PhaseKind::Lobby,
        PhaseKind::Joining,
        PhaseKind::InGame,
    ] {
        animations.update(phase, Some(&view(&world)), now);
    }
    assert_eq!(animations.state.as_ref().unwrap().appearances.len(), 1);
    animations.update(PhaseKind::Lobby, None, now);
    animations.update(PhaseKind::InGame, Some(&view(&world)), now);
    assert_eq!(animations.state.as_ref().unwrap().appearances.len(), 1);
    assert!(animations.state.as_ref().unwrap().departures.is_empty());
}

#[test]
fn snakes_appear_disappear_and_respawn_without_restarting_each_frame() {
    let now = Instant::now();
    let mut world = world();
    let mut state = State::default();
    state.update_board(&view(&world), now);
    state.update_board(&view(&world), now + Duration::from_millis(100));
    assert_eq!(state.appearances[&1].started, now);
    let cells = world.snakes[0].cells.clone();
    world.snakes.clear();
    state.update_board(&view(&world), now + Duration::from_millis(200));
    assert!(state.appearances.is_empty());
    assert_eq!(state.departures.len(), 1);
    assert_eq!(state.departures[0].snake.cells, cells);
    state.update_board(&view(&world), now + Duration::from_millis(220));
    assert_eq!(state.departures.len(), 1);
    world.snakes.push(snake(1, &[(7, 2), (6, 2)]));
    state.update_board(&view(&world), now + Duration::from_millis(250));
    assert_eq!(
        state.appearances[&1].started,
        now + Duration::from_millis(250)
    );
}

#[test]
fn food_effects_are_not_duplicated_and_stop_when_food_is_gone() {
    let now = Instant::now();
    let mut world = world();
    let mut state = State::default();
    state.update_board(&view(&world), now);
    state.update_board(&view(&world), now + Duration::from_millis(100));
    assert_eq!(state.food_effects.len(), 1);
    assert_eq!(state.food_effects[&(3, 2)].started, now);
    world.foods = vec![(8, 8)];
    state.update_board(&view(&world), now + Duration::from_millis(200));
    assert!(!state.food_effects.contains_key(&(3, 2)));
    assert!(state.food_effects.contains_key(&(8, 8)));
}

#[test]
fn eating_triggers_a_wave_even_at_maximum_length() {
    let now = Instant::now();
    let mut world = world();
    let mut state = State::default();
    world.snakes[0].cells = (0..32).map(|x| (x, 2)).collect();
    world.foods = vec![(0, 1)];
    state.update_board(&view(&world), now);
    world.snakes[0].cells.pop_back();
    world.snakes[0].cells.push_front((0, 1));
    world.foods.clear();
    state.update_board(&view(&world), now + Duration::from_millis(100));
    assert_eq!(state.eat_waves[&1].len, 32);
    state.update_board(&view(&world), now + Duration::from_millis(120));
    assert_eq!(
        state.eat_waves[&1].started,
        now + Duration::from_millis(100)
    );
}

#[test]
fn eating_wave_propagates_from_head_to_tail() {
    let now = Instant::now();
    let wave = EatWave {
        started: now,
        len: 2,
    };
    let area = Rect::new(0, 0, 1, 1);
    let mut buffer = Buffer::empty(area);
    buffer[(0, 0)].set_symbol("█").set_fg(Color::Red);
    let base = buffer.clone();
    wave.render(&mut buffer, area, 1, now);
    assert_eq!(buffer, base);
    wave.render(&mut buffer, area, 0, now);
    assert_ne!(buffer, base);
    let mut buffer = base.clone();
    wave.render(&mut buffer, area, 1, now + Duration::from_millis(30));
    assert_ne!(buffer, base);
    assert!(!wave.running(now + Duration::from_millis(270)));
}

#[test]
fn expired_effects_are_cleaned_up_after_a_long_rendering_pause() {
    let now = Instant::now();
    let mut world = world();
    let mut state = State::default();
    state.update_board(&view(&world), now);
    world.snakes.clear();
    state.update_board(&view(&world), now);
    assert_eq!(state.departures.len(), 1);
    state.update_board(&view(&world), now + Duration::from_secs(60));
    assert!(
        state.departures.is_empty()
            && state.appearances.is_empty()
            && state.food_effects.is_empty()
    );
}

#[test]
fn departure_does_not_overwrite_living_snakes_or_food() {
    let now = Instant::now();
    let mut world = world();
    let mut state = State::default();
    state.update_board(&view(&world), now);
    world.snakes.clear();
    state.update_board(&view(&world), now);
    world.foods = vec![(2, 2)];
    world.snakes.push(snake(2, &[(1, 2), (0, 2)]));
    // Проверяем только уход, без эффектов появления новой еды и змейки.
    state.appearances.clear();
    state.food_effects.clear();
    let area = Rect::new(0, 0, 24, 12);
    let mut buffer = Buffer::filled(area, ratatui::buffer::Cell::new("."));
    let before = buffer.clone();
    state.render_board(
        &mut buffer,
        &view(&world),
        BoardViewport::new(area, (0, 0), (10, 10), 2),
        now,
        0,
    );
    assert_eq!(buffer, before);
}

#[test]
fn departure_is_clipped_to_visible_cells_and_preserves_border() {
    let now = Instant::now();
    let mut world = world();
    let mut state = State::default();
    state.update_board(&view(&world), now);
    world.snakes.clear();
    world.foods.clear();
    state.update_board(&view(&world), now);
    let area = Rect::new(4, 5, 12, 8);
    for cw in [1, 2] {
        let viewport = BoardViewport::new(area, (0, 0), (5, 6), cw);
        let mut buffer = Buffer::filled(area, ratatui::buffer::Cell::new("."));
        let before = buffer.clone();
        state.render_board(&mut buffer, &view(&world), viewport, now, 0);
        assert_ne!(buffer, before);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let in_snake = [(1, 2), (2, 2)]
                    .into_iter()
                    .any(|c| viewport.cell_area(c).unwrap().contains((x, y).into()));
                if !in_snake {
                    assert_eq!(buffer[(x, y)], before[(x, y)]);
                }
            }
        }
        let hidden = BoardViewport::new(area, (5, 5), (5, 6), cw);
        let mut buffer = before.clone();
        state.render_board(&mut buffer, &view(&world), hidden, now, 0);
        assert_eq!(buffer, before);
        assert_eq!(state.departures.len(), 1);
    }
}

#[test]
fn palette_shift_recolors_departures_without_restarting_effects() {
    let now = Instant::now();
    let mut world = world();
    let mut state = State::default();
    state.update_board(&view(&world), now);
    world.snakes.clear();
    world.foods.clear();
    state.update_board(&view(&world), now);
    let area = Rect::new(0, 0, 24, 12);
    for cw in [1, 2] {
        let viewport = BoardViewport::new(area, (0, 0), (10, 10), cw);
        for offset in 0..super::super::PALETTE.len() {
            let mut buffer = Buffer::empty(area);
            state.render_board(&mut buffer, &view(&world), viewport, now, offset);
            let cell = viewport.cell_area((2, 2)).unwrap();
            assert_eq!(buffer[(cell.x, cell.y)].fg, super::super::PALETTE[offset]);
            if cw == 2 {
                assert_eq!(buffer[(cell.x, cell.y)].bg, super::super::PALETTE[offset]);
            }
            assert_eq!(state.departures.len(), 1);
            assert_eq!(state.departures[0].effect.started, now);
        }
    }
}

#[test]
fn boom_survives_screen_changes_and_only_replaces_departures() {
    let now = Instant::now();
    let mut world = world();
    let mut animations = Animations::new(true);
    animations.set_boom(true);
    for phase in [
        PhaseKind::Lobby,
        PhaseKind::InGame,
        PhaseKind::Lobby,
        PhaseKind::InGame,
    ] {
        animations.update(phase, Some(&view(&world)), now);
        assert!(animations.state.as_ref().unwrap().boom);
    }
    let state = animations.state.as_ref().unwrap();
    assert_eq!(state.appearances[&1].effect.name(), "fade_from");
    world.snakes.clear();
    animations.update(PhaseKind::InGame, Some(&view(&world)), now);
    let state = animations.state.as_ref().unwrap();
    assert_eq!(state.departures.len(), 1);
    assert_eq!(state.departures[0].effect.effect.name(), "explode");
    animations.update(
        PhaseKind::InGame,
        Some(&view(&world)),
        now + Duration::from_millis(100),
    );
    assert_eq!(
        animations.state.as_ref().unwrap().departures[0]
            .effect
            .started,
        now
    );
    animations.update(
        PhaseKind::InGame,
        Some(&view(&world)),
        now + Duration::from_secs(60),
    );
    assert!(animations.state.as_ref().unwrap().departures.is_empty());
}

#[test]
fn explosion_scatters_colored_particles_without_touching_occupied_cells_or_border() {
    let now = Instant::now();
    let mut world = world();
    world.snakes[0].cells = [(5, 5), (4, 5), (3, 5)].into();
    world.foods.clear();
    let mut state = State {
        boom: true,
        ..State::default()
    };
    state.update_board(&view(&world), now);
    let cells = world.snakes.remove(0).cells;
    state.update_board(&view(&world), now);
    world.foods = vec![(3, 4), (4, 4), (5, 4)];
    world.snakes.push(snake(2, &[(2, 3), (3, 3), (4, 3)]));
    state.appearances.clear();
    state.food_effects.clear();
    let area = Rect::new(4, 5, 24, 12);
    for cw in [1, 2] {
        let viewport = BoardViewport::new(area, (0, 0), (10, 10), cw);
        let mut scattered = false;
        for offset in 0..super::super::PALETTE.len() {
            let mut buffer = Buffer::filled(area, ratatui::buffer::Cell::new("."));
            let before = buffer.clone();
            let at = now + Duration::from_millis(100);
            state.render_board(&mut buffer, &view(&world), viewport, at, offset);
            let mut repeated = before.clone();
            state.render_board(&mut repeated, &view(&world), viewport, at, offset);
            assert_eq!(buffer, repeated);
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    let pos = (x, y).into();
                    let occupied = world
                        .foods
                        .iter()
                        .chain(world.snakes[0].cells.iter())
                        .any(|&cell| viewport.cell_area(cell).unwrap().contains(pos));
                    let visible = (0..10).any(|cy| {
                        (0..10).any(|cx| viewport.cell_area((cx, cy)).unwrap().contains(pos))
                    });
                    if occupied || !visible {
                        assert_eq!(buffer[(x, y)], before[(x, y)]);
                    }
                    if buffer[(x, y)] != before[(x, y)] {
                        assert_eq!(buffer[(x, y)].fg, super::super::PALETTE[offset]);
                        scattered |= !cells
                            .iter()
                            .any(|&cell| viewport.cell_area(cell).unwrap().contains(pos));
                    }
                }
            }
            let mut expired = before.clone();
            state.render_board(
                &mut expired,
                &view(&world),
                viewport,
                now + Duration::from_secs(1),
                offset,
            );
            assert_eq!(expired, before);
        }
        assert!(scattered, "Частицы должны разлетаться за пределы тела");
        let mut buffer = Buffer::empty(area);
        let before = buffer.clone();
        state.render_board(
            &mut buffer,
            &view(&world),
            BoardViewport::new(area, (7, 7), (10, 10), cw),
            now,
            0,
        );
        assert_eq!(buffer, before);
    }
}

#[test]
fn viewport_handles_scrolling_tiny_areas_and_one_or_two_columns() {
    let viewport = BoardViewport::new(Rect::new(10, 5, 12, 6), (7, 8), (5, 4), 2);
    assert_eq!(viewport.cell_area((7, 8)), Some(Rect::new(11, 6, 2, 1)));
    assert_eq!(viewport.cell_area((11, 11)), Some(Rect::new(19, 9, 2, 1)));
    for cell in [(6, 8), (7, 7), (12, 8), (7, 12), (-1, 8)] {
        assert!(viewport.cell_area(cell).is_none());
    }
    let tiny = BoardViewport::new(Rect::new(0, 0, 1, 1), (0, 0), (1, 1), 1);
    assert!(tiny.cell_area((0, 0)).is_none());
    let narrow = BoardViewport::new(Rect::new(0, 0, 4, 4), (0, 0), (2, 2), 1);
    assert_eq!(narrow.cell_area((1, 1)), Some(Rect::new(2, 2, 1, 1)));
}

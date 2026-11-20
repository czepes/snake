//! Terminal interface: discovery browser, in-game field/scoreboard view, and
//! keyboard-driven controls (WASD, no Enter needed) per `snakes_task.md`.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, ExecutableCommand as _};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction as LayoutDirection, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table};
use ratatui::Frame;

use crate::discovery::GameId;
use crate::engine::{Session, UiEvent};
use crate::game::{NodeRole, PlayerStatus};
use crate::model::{Direction, GameConfig};

enum Screen {
    Browse { selected: usize },
    CreateForm(CreateForm),
    JoinNamePrompt { id: GameId, as_player: bool, name: String },
}

struct CreateForm {
    field: usize,
    game_name: String,
    player_name: String,
    width: String,
    height: String,
    food_static: String,
    state_delay_ms: String,
}

impl CreateForm {
    fn new() -> Self {
        Self {
            field: 0,
            game_name: String::new(),
            player_name: String::new(),
            width: "40".into(),
            height: "30".into(),
            food_static: "1".into(),
            state_delay_ms: "1000".into(),
        }
    }

    fn fields_mut(&mut self, idx: usize) -> &mut String {
        match idx {
            0 => &mut self.game_name,
            1 => &mut self.player_name,
            2 => &mut self.width,
            3 => &mut self.height,
            4 => &mut self.food_static,
            _ => &mut self.state_delay_ms,
        }
    }
}

pub fn run() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let result = main_loop(&mut terminal);

    disable_raw_mode()?;
    let stdout = terminal.backend_mut();
    stdout.execute(LeaveAlternateScreen)?;
    result
}

fn main_loop(terminal: &mut ratatui::Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    let mut session = Session::new()?;
    let mut screen = Screen::Browse { selected: 0 };

    loop {
        session.tick();
        if session.should_quit {
            break;
        }

        terminal.draw(|f| draw(f, &session, &screen))?;

        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                handle_key(&mut session, &mut screen, key.code);
                if session.should_quit {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn handle_key(session: &mut Session, screen: &mut Screen, code: KeyCode) {
    // In-game controls always take priority once a game session exists.
    if session.game_name.is_some() {
        match code {
            KeyCode::Char('w') | KeyCode::Char('W') | KeyCode::Up => {
                session.handle_ui_event(UiEvent::Steer(Direction::Up))
            }
            KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Down => {
                session.handle_ui_event(UiEvent::Steer(Direction::Down))
            }
            KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Left => {
                session.handle_ui_event(UiEvent::Steer(Direction::Left))
            }
            KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Right => {
                session.handle_ui_event(UiEvent::Steer(Direction::Right))
            }
            KeyCode::Char('v') | KeyCode::Char('V') => session.handle_ui_event(UiEvent::ToggleStatus),
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                session.handle_ui_event(UiEvent::Leave);
                *screen = Screen::Browse { selected: 0 };
            }
            KeyCode::Esc => session.handle_ui_event(UiEvent::Quit),
            _ => {}
        }
        return;
    }

    match screen {
        Screen::Browse { selected } => match code {
            KeyCode::Char('q') | KeyCode::Esc => session.handle_ui_event(UiEvent::Quit),
            KeyCode::Up => {
                if *selected > 0 {
                    *selected -= 1;
                }
            }
            KeyCode::Down => {
                let len = session.catalog.list().len();
                if *selected + 1 < len {
                    *selected += 1;
                }
            }
            KeyCode::Char('c') | KeyCode::Char('C') => *screen = Screen::CreateForm(CreateForm::new()),
            KeyCode::Enter => {
                if let Some(g) = session.catalog.list().get(*selected) {
                    *screen = Screen::JoinNamePrompt {
                        id: g.id.clone(),
                        as_player: true,
                        name: String::new(),
                    };
                }
            }
            KeyCode::Char('v') | KeyCode::Char('V') => {
                if let Some(g) = session.catalog.list().get(*selected) {
                    *screen = Screen::JoinNamePrompt {
                        id: g.id.clone(),
                        as_player: false,
                        name: String::new(),
                    };
                }
            }
            _ => {}
        },
        Screen::CreateForm(form) => match code {
            KeyCode::Esc => *screen = Screen::Browse { selected: 0 },
            KeyCode::Tab | KeyCode::Down => form.field = (form.field + 1) % 6,
            KeyCode::Up => form.field = (form.field + 5) % 6,
            KeyCode::Backspace => {
                form.fields_mut(form.field).pop();
            }
            KeyCode::Char(c) => form.fields_mut(form.field).push(c),
            KeyCode::Enter => {
                let config = GameConfig {
                    width: form.width.parse().unwrap_or(40),
                    height: form.height.parse().unwrap_or(30),
                    food_static: form.food_static.parse().unwrap_or(1),
                    state_delay_ms: form.state_delay_ms.parse().unwrap_or(1000),
                };
                if !form.game_name.is_empty() && !form.player_name.is_empty() {
                    session.handle_ui_event(UiEvent::Host {
                        game_name: form.game_name.clone(),
                        player_name: form.player_name.clone(),
                        config,
                    });
                    *screen = Screen::Browse { selected: 0 };
                }
            }
            _ => {}
        },
        Screen::JoinNamePrompt { id, as_player, name } => match code {
            KeyCode::Esc => *screen = Screen::Browse { selected: 0 },
            KeyCode::Backspace => {
                name.pop();
            }
            KeyCode::Char(c) => name.push(c),
            KeyCode::Enter => {
                if !name.is_empty() {
                    session.handle_ui_event(UiEvent::Join {
                        id: id.clone(),
                        player_name: name.clone(),
                        as_player: *as_player,
                    });
                    *screen = Screen::Browse { selected: 0 };
                }
            }
            _ => {}
        },
    }
}

fn draw(f: &mut Frame, session: &Session, screen: &Screen) {
    if session.game_name.is_some() {
        draw_game(f, session);
        return;
    }
    match screen {
        Screen::Browse { selected } => draw_browse(f, session, *selected),
        Screen::CreateForm(form) => draw_create_form(f, form),
        Screen::JoinNamePrompt { name, as_player, .. } => draw_join_prompt(f, name, *as_player),
    }
}

fn draw_browse(f: &mut Frame, session: &Session, selected: usize) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(3)])
        .split(area);

    let items: Vec<ListItem> = session
        .catalog
        .list()
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let style = if i == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(format!(
                "{}  {}x{}  участников {}  змеек {}  {}",
                g.id.game_name,
                g.config.width,
                g.config.height,
                g.players,
                g.snakes,
                if g.can_join { "можно войти" } else { "нет мест" },
            ))
            .style(style)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Доступные игры (Enter — играть, V — смотреть, C — создать, Q — выйти)"),
    );
    f.render_widget(list, chunks[0]);

    let error = session.error.clone().unwrap_or_default();
    let footer = Paragraph::new(error).block(Block::default().borders(Borders::ALL).title("Сообщения"));
    f.render_widget(footer, chunks[1]);
}

fn draw_create_form(f: &mut Frame, form: &CreateForm) {
    let area = f.area();
    let labels = [
        "Имя игры",
        "Ваше имя",
        "Ширина (10-100)",
        "Высота (10-100)",
        "Запас еды (0-10)",
        "Интервал хода, мс (100-3000)",
    ];
    let values = [
        &form.game_name,
        &form.player_name,
        &form.width,
        &form.height,
        &form.food_static,
        &form.state_delay_ms,
    ];
    let lines: Vec<Line> = labels
        .iter()
        .zip(values.iter())
        .enumerate()
        .map(|(i, (label, value))| {
            let style = if i == form.field {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(Span::styled(format!("{label}: {value}"), style))
        })
        .collect();
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Создать игру (Tab — след. поле, Enter — создать, Esc — отмена)"),
    );
    f.render_widget(p, area);
}

fn draw_join_prompt(f: &mut Frame, name: &str, as_player: bool) {
    let area = f.area();
    let title = if as_player {
        "Ваше имя (играть)"
    } else {
        "Ваше имя (наблюдать)"
    };
    let p = Paragraph::new(name).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(p, area);
}

fn draw_game(f: &mut Frame, session: &Session) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    draw_field(f, session, chunks[0]);
    draw_scoreboard(f, session, chunks[1]);
}

fn draw_field(f: &mut Frame, session: &Session, area: Rect) {
    let Some(config) = session.config else {
        f.render_widget(Block::default().borders(Borders::ALL).title("Поле"), area);
        return;
    };
    let Some(state) = &session.last_state else {
        f.render_widget(Block::default().borders(Borders::ALL).title("Поле (ожидание)"), area);
        return;
    };

    // Size the field's own box to exactly match the field dimensions (plus
    // the border) so its boundary is visible, and center that box within the
    // available area instead of stretching a borderless grid across it.
    let wanted_w = (config.width as u16).saturating_add(2).min(area.width.max(1));
    let wanted_h = (config.height as u16).saturating_add(2).min(area.height.max(1));
    let field_area = Rect {
        x: area.x + (area.width.saturating_sub(wanted_w)) / 2,
        y: area.y + (area.height.saturating_sub(wanted_h)) / 2,
        width: wanted_w,
        height: wanted_h,
    };

    let inner = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            "{}  Поле {}x{}  Ход {} мс",
            session.game_name.clone().unwrap_or_default(),
            config.width,
            config.height,
            config.state_delay_ms,
        ));
    let inner_area = inner.inner(field_area);
    f.render_widget(inner, field_area);

    let mut grid = vec![vec![' '; config.width as usize]; config.height as usize];
    for food in &state.foods {
        set_cell(&mut grid, food.x(), food.y(), config, '*');
    }
    for snake in &state.snakes {
        if let Some(s) = crate::game::Snake::from_proto(snake) {
            for (i, p) in s.cells(&config).into_iter().enumerate() {
                set_cell(&mut grid, p.x, p.y, config, if i == 0 { '@' } else { '#' });
            }
        }
    }

    let lines: Vec<Line> = grid
        .into_iter()
        .map(|row| Line::from(row.into_iter().collect::<String>()))
        .collect();
    let p = Paragraph::new(lines);
    f.render_widget(p, inner_area);
}

fn set_cell(grid: &mut [Vec<char>], x: i32, y: i32, config: GameConfig, c: char) {
    let p = config.wrap(crate::model::Point::new(x, y));
    if let Some(row) = grid.get_mut(p.y as usize) {
        if let Some(cell) = row.get_mut(p.x as usize) {
            *cell = c;
        }
    }
}

fn draw_scoreboard(f: &mut Frame, session: &Session, area: Rect) {
    let chunks = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(6), Constraint::Length(3)])
        .split(area);

    let rows: Vec<Row> = session
        .last_state
        .as_ref()
        .map(|s| {
            s.players
                .players
                .iter()
                .map(|p| {
                    let role = format!("{:?}", NodeRole::from_proto(p.role()));
                    let status = match PlayerStatus::from_proto(p.status()) {
                        PlayerStatus::Playing => "Играет",
                        PlayerStatus::Viewing => "Смотрит",
                    };
                    let name = if Some(p.id()) == session.my_id {
                        format!("{} (вы)", p.name())
                    } else {
                        p.name().to_string()
                    };
                    Row::new(vec![
                        Cell::from(name),
                        Cell::from(p.score().to_string()),
                        Cell::from(role),
                        Cell::from(status),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(40),
            Constraint::Percentage(15),
            Constraint::Percentage(20),
            Constraint::Percentage(25),
        ],
    )
    .header(Row::new(vec!["Имя", "Очки", "Роль", "Статус"]).style(Style::default().add_modifier(Modifier::BOLD)))
    .block(Block::default().borders(Borders::ALL).title("Участники"));
    f.render_widget(table, chunks[0]);

    let info = if let Some(state) = &session.last_state {
        let target = session
            .config
            .map(|c| c.food_static as usize + state.snakes.len())
            .unwrap_or(0);
        format!(
            "Змейки: {}/4\nЦель еды: {}\nЕды на поле: {}",
            state.snakes.len(),
            target,
            state.foods.len(),
        )
    } else {
        String::new()
    };
    let info_p = Paragraph::new(info).block(Block::default().borders(Borders::ALL).title("Параметры игры"));
    f.render_widget(info_p, chunks[1]);

    let status_text = match session.my_status {
        PlayerStatus::Playing => "Вы играете (V — смотреть, Q — выйти)",
        PlayerStatus::Viewing => "Вы наблюдаете (V — вернуться в игру, Q — выйти)",
    };
    let status_p = Paragraph::new(status_text).block(Block::default().borders(Borders::ALL));
    f.render_widget(status_p, chunks[2]);
}

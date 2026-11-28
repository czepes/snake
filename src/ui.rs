//! Терминальный интерфейс: лобби, создание игры, экран подключения и игровой экран.
//!
//! Интерфейс только читает состояние узла и отдаёт ему команды; сетевой логики
//! здесь нет. Любая причина отказа показывается на экране.
//!
//! Фаза [`Node`] выбирает основной экран (лобби, подключение, игра); локальный
//! `Screen` нужен только для форм внутри лобби. [`Ui::handle_key`] возвращает
//! команды для главного цикла, [`Ui::on_event`] принимает уведомления ядра,
//! кадр собирает [`Ui::draw`].
//! Отрисовка читает [`GameView`], а не изменяет мир; анимации накладываются
//! на готовый буфер и не влияют на сетевые таймеры или правила.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell as TCell, Clear, Paragraph, Row, Table, Wrap};

mod animations;

#[cfg(test)]
mod tests;

use animations::{Animations, BoardViewport};

use crate::node::{Command, Event, GameView, Node, PhaseKind};
use crate::wire::{
    Config, Dir, MAX_SNAKES, NodeRole, PlayerStatus, valid_game_name, valid_player_name,
};

const PALETTE: [Color; 6] = [
    Color::LightRed,
    Color::Cyan,
    Color::Yellow,
    Color::LightGreen,
    Color::Magenta,
    Color::LightBlue,
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Lobby,
    Create,
    EditName,
}

struct Msg {
    text: String,
    error: bool,
    until: Instant,
}

#[derive(Clone)]
struct CreateForm {
    field: usize,
    game: String,
    width: String,
    height: String,
    food: String,
    delay: String,
    on_server: bool,
    error: Option<String>,
}

impl CreateForm {
    fn new(default_name: &str) -> Self {
        CreateForm {
            field: 0,
            game: default_name.to_owned(),
            width: "20".into(),
            height: "15".into(),
            food: "2".into(),
            delay: "300".into(),
            on_server: false,
            error: None,
        }
    }

    fn edit(&mut self, code: KeyCode) {
        match code {
            KeyCode::Backspace => {
                if let Some(value) = self.current_text() {
                    value.pop();
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'H' | 'l' | 'L' | ' ')
                if self.field == 5 =>
            {
                self.on_server = !self.on_server;
            }
            KeyCode::Char(c) => self.insert(c),
            _ => {}
        }
    }

    fn current_text(&mut self) -> Option<&mut String> {
        match self.field {
            0 => Some(&mut self.game),
            1 => Some(&mut self.width),
            2 => Some(&mut self.height),
            3 => Some(&mut self.food),
            4 => Some(&mut self.delay),
            _ => None,
        }
    }

    fn insert(&mut self, c: char) {
        let (limit, allowed) = match self.field {
            0 => (32, !c.is_control()),
            1 | 2 => (4, c.is_ascii_digit()),
            3 => (3, c.is_ascii_digit()),
            4 => (5, c.is_ascii_digit()),
            _ => return,
        };
        if let Some(value) = self.current_text()
            && allowed
            && value.len() + c.len_utf8() <= limit
        {
            value.push(c);
        }
    }
}

/// Локальное состояние TUI: формы, выбор строки, уведомления и визуальные эффекты.
/// Сам мир хранится в `Node`; интерфейс не поддерживает его отдельную игровую копию.
pub struct Ui {
    pub player_name: String,
    /// Выставляется по Event::Exited, когда узел закончил сетевое завершение работы.
    pub quit: bool,
    screen: Screen,
    selected: usize,
    form: CreateForm,
    name_buf: String,
    msg: Option<Msg>,
    /// Общий локальный сдвиг сохраняет различие цветов участников.
    palette_offset: usize,
    show_direction: bool,
    animations: Animations,
}

impl Ui {
    pub fn new(player_name: String) -> Ui {
        Self::with_animations(player_name, true)
    }

    pub fn with_animations(player_name: String, enabled: bool) -> Ui {
        let form = CreateForm::new("Игра");
        Ui {
            player_name,
            quit: false,
            screen: Screen::Lobby,
            selected: 0,
            form,
            name_buf: String::new(),
            msg: None,
            palette_offset: 0,
            show_direction: false,
            animations: Animations::new(enabled),
        }
    }

    #[doc(hidden)]
    pub fn with_boom(mut self, enabled: bool) -> Self {
        self.animations.set_boom(enabled);
        self
    }

    /// Показывать направление движения в символе головы змейки.
    pub fn with_direction(mut self, enabled: bool) -> Self {
        self.show_direction = enabled;
        self
    }

    fn say(&mut self, now: Instant, text: impl Into<String>, error: bool) {
        let secs = if error { 10 } else { 5 };
        self.msg = Some(Msg {
            text: text.into(),
            error,
            until: now + Duration::from_secs(secs),
        });
    }

    /// Превращает событие узла в уведомление или сигнал завершения программы.
    /// Сообщение GameOver не затирает ещё видимую ошибку с причиной завершения.
    pub fn on_event(&mut self, ev: Event, now: Instant) {
        match ev {
            Event::Notice(s) => self.say(now, s, false),
            Event::Failure(s) => self.say(now, s, true),
            Event::Joined => self.say(now, "Вы в игре", false),
            Event::GameOver(s) => {
                if !self.msg.as_ref().is_some_and(|m| m.error && m.until > now) {
                    self.say(now, s, false);
                }
            }
            Event::Exited => self.quit = true,
        }
    }

    // ───────────────────────────── Ввод ──────────────────────────────────────

    /// Обрабатывает клавишу и возвращает команду для узла, не изменяя его.
    /// Ctrl+C запрашивает штатный выход, а не прерывает передачу роли.
    pub fn handle_key(&mut self, key: KeyEvent, node: &Node, now: Instant) -> Option<Command> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return Some(Command::Quit);
        }
        match node.phase() {
            PhaseKind::InGame => self.key_game(key),
            PhaseKind::Joining => matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q' | 'Q' | 'й' | 'Й')
            )
            .then_some(Command::Leave),
            PhaseKind::Lobby => match self.screen {
                Screen::Lobby => self.key_lobby(key, node, now),
                Screen::Create => self.key_create(key),
                Screen::EditName => {
                    self.key_name(key, now);
                    None
                }
            },
        }
    }

    /// Обрабатывает клавишу и сразу передаёт полученную команду узлу.
    pub fn on_key(&mut self, key: KeyEvent, node: &mut Node, now: Instant) {
        if let Some(command) = self.handle_key(key, node, now) {
            node.command(now, command);
        }
    }

    fn key_game(&mut self, key: KeyEvent) -> Option<Command> {
        let steer = match key.code {
            KeyCode::Up | KeyCode::Char('w' | 'W' | 'ц' | 'Ц' | 'k' | 'K' | 'л' | 'Л') => {
                Some(Dir::Up)
            }
            KeyCode::Down | KeyCode::Char('s' | 'S' | 'ы' | 'Ы' | 'j' | 'J' | 'о' | 'О') => {
                Some(Dir::Down)
            }
            KeyCode::Left | KeyCode::Char('a' | 'A' | 'ф' | 'Ф' | 'h' | 'H' | 'р' | 'Р') => {
                Some(Dir::Left)
            }
            KeyCode::Right | KeyCode::Char('d' | 'D' | 'в' | 'В' | 'l' | 'L' | 'д' | 'Д') => {
                Some(Dir::Right)
            }
            _ => None,
        };
        if let Some(d) = steer {
            return Some(Command::Steer(d));
        }
        match key.code {
            KeyCode::Tab => self.palette_offset = (self.palette_offset + 1) % PALETTE.len(),
            KeyCode::Char('e' | 'E' | 'у' | 'У') => self.show_direction = !self.show_direction,
            KeyCode::Char('b' | 'B' | 'и' | 'И') => self.animations.toggle_boom(),
            KeyCode::Char('v' | 'V' | 'м' | 'М') => return Some(Command::ToggleWatch),
            KeyCode::Char('r' | 'R' | 'к' | 'К') => return Some(Command::InviteRobot),
            KeyCode::Esc | KeyCode::Char('q' | 'Q' | 'й' | 'Й') => return Some(Command::Leave),
            _ => {}
        }
        None
    }

    fn key_lobby(&mut self, key: KeyEvent, node: &Node, now: Instant) -> Option<Command> {
        let count = node.catalog_entries().len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k' | 'K' | 'л' | 'Л') => {
                self.selected = self.selected.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j' | 'J' | 'о' | 'О') => {
                if self.selected + 1 < count {
                    self.selected += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char('p' | 'P' | 'з' | 'З' | 'l' | 'L' | 'д' | 'Д') => {
                return self.join(node, now, PlayerStatus::Playing);
            }
            KeyCode::Char('v' | 'V' | 'м' | 'М' | 'h' | 'H' | 'р' | 'Р') => {
                return self.join(node, now, PlayerStatus::Viewing);
            }
            KeyCode::Char('c' | 'C' | 'с' | 'С') => {
                self.form.error = None;
                self.screen = Screen::Create;
            }
            KeyCode::Char('n' | 'N' | 'т' | 'Т') => {
                self.name_buf = self.player_name.clone();
                self.screen = Screen::EditName;
            }
            KeyCode::Esc | KeyCode::Char('q' | 'Q' | 'й' | 'Й') => return Some(Command::Quit),
            _ => {}
        }
        None
    }

    fn join(&mut self, node: &Node, now: Instant, status: PlayerStatus) -> Option<Command> {
        let (key, can_play, can_watch) = {
            let entries = node.catalog_entries();
            let Some(e) = entries.get(self.selected) else {
                self.say(
                    now,
                    "Выберите игру из списка (она появится, когда будет найдена)",
                    true,
                );
                return None;
            };
            (e.key.clone(), e.can_play(), e.can_watch())
        };
        if !valid_player_name(&self.player_name) {
            self.say(now, "Задайте имя игрока: клавиша N", true);
            return None;
        }
        let ok = if status == PlayerStatus::Playing {
            can_play
        } else {
            can_watch
        };
        if !ok {
            let why = if status == PlayerStatus::Playing && can_watch {
                "Нет места для змейки: нажмите V, чтобы наблюдать"
            } else {
                "В игре нет свободных мест"
            };
            self.say(now, why, true);
            return None;
        }
        Some(Command::Join {
            key,
            player: self.player_name.clone(),
            status,
        })
    }

    fn key_name(&mut self, key: KeyEvent, now: Instant) {
        match key.code {
            KeyCode::Esc => self.screen = Screen::Lobby,
            KeyCode::Enter => {
                if valid_player_name(&self.name_buf) {
                    self.player_name = self.name_buf.trim().to_owned();
                    self.screen = Screen::Lobby;
                } else {
                    self.say(now, "Имя: от 1 до 24 байт, без управляющих символов", true);
                }
            }
            KeyCode::Backspace => {
                self.name_buf.pop();
            }
            KeyCode::Char(c) if !c.is_control() && self.name_buf.len() + c.len_utf8() <= 24 => {
                self.name_buf.push(c)
            }
            _ => {}
        }
    }

    fn key_create(&mut self, key: KeyEvent) -> Option<Command> {
        const FIELDS: usize = 6;
        let f = &mut self.form;
        match key.code {
            KeyCode::Esc => self.screen = Screen::Lobby,
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j' | 'J') => {
                f.field = (f.field + 1) % FIELDS
            }
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k' | 'K') => {
                f.field = (f.field + FIELDS - 1) % FIELDS
            }
            KeyCode::Enter => return self.submit_form(),
            code => f.edit(code),
        }
        None
    }

    fn submit_form(&mut self) -> Option<Command> {
        let game = self.form.game.trim().to_owned();
        if !valid_game_name(&game) {
            self.form.error = Some("Имя игры: от 1 до 32 байт".into());
            return None;
        }
        if !valid_player_name(&self.player_name) {
            self.form.error = Some("Сначала задайте имя игрока (в лобби — клавиша N)".into());
            return None;
        }
        if self.form.on_server {
            self.screen = Screen::Lobby;
            return Some(Command::HostOnServer {
                game,
                player: self.player_name.clone(),
            });
        }
        let parse = |s: &str| s.parse::<i32>().unwrap_or(-1);
        let cfg = Config {
            width: parse(&self.form.width),
            height: parse(&self.form.height),
            food_static: parse(&self.form.food),
            state_delay_ms: parse(&self.form.delay),
        };
        if !cfg.is_valid() {
            self.form.error = Some("Ширина и высота: 10–100, еда: 0–10, ход: 100–3000 мс".into());
            return None;
        }
        self.screen = Screen::Lobby;
        Some(Command::Host {
            game,
            player: self.player_name.clone(),
            cfg,
        })
    }

    fn min_size(&self, node: &Node) -> (u16, u16, &'static str) {
        match node.phase() {
            PhaseKind::InGame => {
                let height = node.game_view().map_or(24, |v| {
                    let players_h = (v.players.len() as u16 + 3).max(5);
                    players_h + 7 + game_controls(&v).len() as u16 + 2 + 4
                });
                (100, height, "игрового экрана")
            }
            PhaseKind::Joining => (64, 10, "экрана подключения"),
            PhaseKind::Lobby => match self.screen {
                Screen::Create => (64, 16, "формы создания игры"),
                Screen::EditName => (44, 6, "ввода имени"),
                Screen::Lobby => (80, 18, "лобби"),
            },
        }
    }

    // ───────────────────────────── Отрисовка ─────────────────────────────────

    fn draw_too_small(&self, f: &mut Frame, node: &Node) -> bool {
        let (min_w, min_h, label) = self.min_size(node);
        let area = f.area();

        if area.width >= min_w && area.height >= min_h {
            return false;
        }
        //
        // Если даже под сообщение нет места — молча выходим, чтобы не паниковать.
        if area.width < 20 || area.height < 3 {
            return true;
        }

        let text = vec![
            Line::from(Span::styled(
                "Терминал слишком мал",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(format!("Сейчас:  {} × {}", area.width, area.height)),
            Line::from(format!("Нужно:   {min_w} × {min_h}")),
            Line::from(""),
            Line::from(Span::styled(
                format!("для {label}"),
                Style::new().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "Увеличьте окно терминала",
                Style::new().fg(Color::DarkGray),
            )),
        ];

        // Блок по центру: ширина = max(длина строк, 24), высота = число строк + 2 (рамка).
        let w = (text.iter().map(|l| l.width()).max().unwrap_or(0) as u16 + 4)
            .max(28)
            .min(area.width);
        let h = (text.len() as u16 + 2).min(area.height);
        let rect = centered(area, w, h);

        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(text)
                .alignment(Alignment::Center)
                .block(Block::bordered()),
            rect,
        );

        true
    }

    /// Собирает кадр из текущего состояния, затем накладывает эффекты.
    /// Анимации обновляются даже при слишком маленьком окне, чтобы изменение
    /// размера терминала не создавало заново уже начавшиеся эффекты.
    pub fn draw(&mut self, f: &mut Frame, node: &Node, now: Instant, lan_ok: bool) {
        if self.msg.as_ref().is_some_and(|m| m.until <= now) {
            self.msg = None;
        }

        let view = node.game_view();
        self.animations.update(node.phase(), view.as_ref(), now);

        if self.draw_too_small(f, node) {
            return;
        }

        match node.phase() {
            PhaseKind::InGame => {
                if let Some(v) = view {
                    self.draw_game(f, &v, now);
                }
            }
            PhaseKind::Joining => self.draw_joining(f, node, now),
            PhaseKind::Lobby => {
                match self.screen {
                    Screen::Create => self.draw_create(f),
                    _ => self.draw_lobby(f, node, now, lan_ok),
                }
                if self.screen == Screen::EditName {
                    self.draw_name_modal(f);
                }
            }
        }
        self.animations.render_transition(f, now);
    }

    fn message_line(&self) -> Paragraph<'static> {
        match &self.msg {
            Some(m) => Paragraph::new(m.text.clone()).style(if m.error {
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Green)
            }),
            None => Paragraph::new(""),
        }
    }

    fn draw_lobby(&mut self, f: &mut Frame, node: &Node, now: Instant, lan_ok: bool) {
        let rows = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(f.area());

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Змейка по сети",
                Style::new().add_modifier(Modifier::BOLD).fg(Color::Cyan),
            ))),
            rows[0],
        );

        let dot = |ok: bool| {
            Span::styled(
                "● ",
                Style::new().fg(if ok { Color::Green } else { Color::Red }),
            )
        };
        let server_text = if !node.server_known() {
            "адрес не определён"
        } else if node.server_online(now) {
            "доступен"
        } else {
            "недоступен"
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                dot(lan_ok),
                Span::raw(format!(
                    "Локальная сеть: {}    ",
                    if lan_ok {
                        "доступна"
                    } else {
                        "недоступна"
                    }
                )),
                dot(node.server_online(now)),
                Span::raw(format!("Сервер курса: {server_text}")),
            ])),
            rows[1],
        );

        let entries = node.catalog_entries();
        if self.selected >= entries.len() {
            self.selected = entries.len().saturating_sub(1);
        }
        if entries.is_empty() {
            f.render_widget(
                Paragraph::new("Игр пока не найдено. Они появляются автоматически; свою можно создать клавишей C.")
                    .block(Block::bordered().title(" Доступные игры ")),
                rows[2],
            );
        } else {
            let table_rows: Vec<Row> = entries
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let source = match (e.from_lan(now), e.from_server(now)) {
                        (true, true) => "сеть + сервер",
                        (true, false) => "локальная сеть",
                        (false, true) => "сервер курса",
                        _ => "—",
                    };
                    let style = if i == self.selected {
                        Style::new().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::new()
                    };
                    Row::new(vec![
                        TCell::from(e.key.name.clone()),
                        TCell::from(source),
                        TCell::from(format!("{}/6", e.participants())),
                        TCell::from(format!("{}/4", e.snakes())),
                        TCell::from(format!("{}×{}", e.config.width, e.config.height)),
                        TCell::from(format!("{} мс", e.config.state_delay_ms)),
                        TCell::from(if e.can_play() {
                            "играть/смотреть"
                        } else if e.can_watch() {
                            "только смотреть"
                        } else {
                            "мест нет"
                        }),
                    ])
                    .style(style)
                })
                .collect();
            let table = Table::new(
                table_rows,
                [
                    Constraint::Min(16),
                    Constraint::Length(15),
                    Constraint::Length(13),
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Length(9),
                    Constraint::Length(17),
                ],
            )
            .header(
                Row::new(vec![
                    "Игра",
                    "Источник",
                    "Участники",
                    "Змейки",
                    "Поле",
                    "Ход",
                    "Доступно",
                ])
                .style(Style::new().add_modifier(Modifier::BOLD).fg(Color::Yellow)),
            )
            .block(Block::bordered().title(" Доступные игры "));
            f.render_widget(table, rows[2]);
        }

        f.render_widget(
            Paragraph::new(format!(
                "[↑/↓] выбор   [Enter/P] играть   [V] смотреть   [C] создать игру   [N] имя: {}   [Q] выход",
                if self.player_name.is_empty() { "не задано" } else { &self.player_name }
            ))
            .wrap(Wrap { trim: true }),
            rows[3],
        );
        f.render_widget(self.message_line(), rows[4]);
    }

    fn draw_name_modal(&self, f: &mut Frame) {
        let area = centered(f.area(), 44, 5);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(self.name_buf.clone() + "▏"),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter — сохранить, Esc — отмена",
                    Style::new().fg(Color::DarkGray),
                )),
            ])
            .block(Block::bordered().title(" Ваше имя ")),
            area,
        );
        f.render_widget(
            self.message_line(),
            Rect {
                y: area.y + area.height,
                height: 1,
                ..area
            },
        );
    }

    fn draw_create(&mut self, f: &mut Frame) {
        let area = centered(f.area(), 60, 14);
        f.render_widget(Clear, area);
        let fm = &self.form;
        let field = |i: usize, label: &str, value: &str, dim: bool| -> Line<'static> {
            let marker = if fm.field == i { "▶ " } else { "  " };
            let style = if dim {
                Style::new().fg(Color::DarkGray)
            } else if fm.field == i {
                Style::new().add_modifier(Modifier::BOLD).fg(Color::Yellow)
            } else {
                Style::new()
            };
            Line::from(Span::styled(
                format!(
                    "{marker}{label:<22}{value}{}",
                    if fm.field == i { "▏" } else { "" }
                ),
                style,
            ))
        };
        let dim = fm.on_server;
        let mut lines = vec![
            field(0, "Название игры:", &fm.game, false),
            field(1, "Ширина (10–100):", &fm.width, dim),
            field(2, "Высота (10–100):", &fm.height, dim),
            field(3, "Запас еды (0–10):", &fm.food, dim),
            field(4, "Ход, мс (100–3000):", &fm.delay, dim),
            field(
                5,
                "Создать на сервере:",
                if fm.on_server {
                    "да (параметры задаст сервер)"
                } else {
                    "нет, локально"
                },
                false,
            ),
            Line::from(""),
        ];
        if let Some(e) = &fm.error {
            lines.push(Line::from(Span::styled(
                e.clone(),
                Style::new().fg(Color::Red),
            )));
        }
        lines.push(Line::from(Span::styled(
            "Tab/↑↓ — поле, Пробел — переключить, Enter — создать, Esc — назад",
            Style::new().fg(Color::DarkGray),
        )));
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" Новая игра ")),
            area,
        );
    }

    fn draw_joining(&self, f: &mut Frame, node: &Node, now: Instant) {
        let area = centered(f.area(), 62, 8);
        let Some(j) = node.joining_view() else { return };
        let step = if !j.acked {
            "1/3 ожидание подтверждения от ведущего (ACK)…"
        } else if !j.has_config {
            "2/3 получение параметров игры…"
        } else {
            "3/3 ожидание состояния игры…"
        };
        let secs = now.saturating_duration_since(j.started).as_secs_f32();
        f.render_widget(
            Paragraph::new(vec![
                Line::from(format!("Подключение к «{}»", j.game)),
                Line::from(""),
                Line::from(step),
                Line::from(format!("Прошло {secs:.1} с из 5")),
                Line::from(""),
                Line::from(Span::styled(
                    "Esc — отмена",
                    Style::new().fg(Color::DarkGray),
                )),
            ])
            .alignment(Alignment::Center)
            .block(Block::bordered().title(" Присоединение ")),
            area,
        );
    }

    fn draw_game(&mut self, f: &mut Frame, v: &GameView, now: Instant) {
        let rows = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(f.area());

        let role = match (v.role, v.status) {
            (NodeRole::Master, _) => "вы ведущий",
            (NodeRole::Deputy, _) => "вы заместитель ведущего",
            _ => "вы участник",
        };
        let mut head = vec![
            Span::styled(
                format!("Змейка · {}", v.name),
                Style::new().add_modifier(Modifier::BOLD).fg(Color::Cyan),
            ),
            Span::raw(format!("   {role}")),
        ];
        if v.leaving {
            head.push(Span::styled(
                "   передача игры другому ведущему…",
                Style::new().fg(Color::Yellow),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(head)), rows[0]);

        // Сайдбар фиксированной ширины прижат к правому краю; поле центрируется
        // в оставшейся левой колонке.
        const SIDE_W: u16 = 40;
        let board_col = rows[1].width.saturating_sub(SIDE_W);
        let (cw, vw, vh) = board_dims(&v.cfg, board_col as usize, rows[1].height as usize);
        let board_w = ((vw * cw + 2) as u16).max(24);

        let body = Layout::new(
            Direction::Horizontal,
            [Constraint::Min(0), Constraint::Length(SIDE_W)],
        )
        .split(rows[1]);

        let board_area = centered(body[0], board_w, (vh + 2) as u16);
        self.draw_board(f, v, board_area, now);
        self.draw_side(f, v, body[1]);

        let state_line = match v.status {
            PlayerStatus::Playing => "Вы играете",
            PlayerStatus::Viewing if v.status_pending => "Запрос отправлен ведущему…",
            PlayerStatus::Viewing => "Вы наблюдаете",
        };
        let foot = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(rows[2]);
        f.render_widget(Paragraph::new(state_line), foot[0]);
        f.render_widget(self.message_line(), foot[1]);
    }

    fn draw_board(&mut self, f: &mut Frame, v: &GameView, area: Rect, now: Instant) {
        let (w, h) = (v.cfg.width as usize, v.cfg.height as usize);
        let (cw, vw, vh) = board_dims(&v.cfg, area.width as usize, area.height as usize);

        // Если поле не помещается, окно следит за своей головой. Наблюдателю
        // показываем первую живую змейку, а на пустом поле — центр.
        let focus = v
            .world
            .snake(v.my_id)
            .or(v.world.snakes.first())
            .map_or((w as i32 / 2, h as i32 / 2), |s| s.head());
        let ox = ((focus.0 as usize).saturating_sub(vw / 2)).min(w - vw);
        let oy = ((focus.1 as usize).saturating_sub(vh / 2)).min(h - vh);

        let mut occ: HashMap<(i32, i32), (Color, Option<Dir>)> = HashMap::new();
        for s in &v.world.snakes {
            let color = player_color(v, s.owner, self.palette_offset);
            for (i, &c) in s.cells.iter().enumerate() {
                occ.insert(c, (color, (i == 0).then_some(s.dir)));
            }
        }

        let foods: std::collections::HashSet<_> = v.world.foods.iter().copied().collect();

        let mut lines: Vec<Line> = Vec::with_capacity(vh);
        for y in oy..oy + vh {
            let mut spans: Vec<Span> = Vec::with_capacity(vw);
            for x in ox..ox + vw {
                let cell = (x as i32, y as i32);
                let span = if let Some(&(color, head)) = occ.get(&cell) {
                    match (head, cw) {
                        (Some(dir), _) if self.show_direction => Span::styled(
                            head_symbol(dir, cw),
                            Style::new().bg(color).fg(Color::Black),
                        ),
                        (Some(_), 2) => Span::styled(
                            "••",
                            Style::new()
                                .bg(color)
                                .fg(Color::Black)
                                .add_modifier(Modifier::BOLD),
                        ),
                        (Some(_), _) => Span::styled("●", Style::new().bg(color).fg(Color::Black)),
                        (None, 2) => Span::styled("  ", Style::new().bg(color)),
                        (None, _) => Span::styled("█", Style::new().fg(color)),
                    }
                } else if foods.contains(&cell) {
                    Span::styled(
                        if cw == 2 { "● " } else { "●" },
                        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled(
                        if cw == 2 { "· " } else { "·" },
                        Style::new().fg(Color::DarkGray),
                    )
                };
                spans.push(span);
            }
            lines.push(Line::from(spans));
        }
        let mut title = format!(" Поле {w}×{h} · ход {} мс ", v.cfg.state_delay_ms);
        if vw < w || vh < h {
            title.push_str(&format!("· окно {vw}×{vh} "));
        }
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(title)),
            area,
        );

        self.animations.render_board(
            f,
            v,
            BoardViewport::new(area, (ox, oy), (vw, vh), cw),
            now,
            self.palette_offset,
        );
    }

    fn draw_side(&self, f: &mut Frame, v: &GameView, area: Rect) {
        let players_h = (v.players.len() as u16 + 3).max(5);
        let controls = game_controls(v);
        let parts = Layout::vertical([
            Constraint::Length(players_h),
            Constraint::Length(7),
            Constraint::Length(controls.len() as u16 + 2),
            Constraint::Min(0),
        ])
        .split(area);

        let mut players: Vec<_> = v.players.iter().collect();
        players.sort_by(|a, b| b.score.cmp(&a.score).then(a.id.cmp(&b.id)));
        let rows: Vec<Row> = players
            .iter()
            .map(|p| {
                let has_snake = v.world.snake(p.id).is_some();
                let color = player_color(v, p.id, self.palette_offset);
                let swatch = Span::styled(
                    "■ ",
                    Style::new().fg(if has_snake { color } else { Color::DarkGray }),
                );
                let me = if p.id == v.my_id { " (вы)" } else { "" };
                let role = match p.role {
                    NodeRole::Normal => "NORMAL",
                    NodeRole::Master => "MASTER",
                    NodeRole::Deputy => "DEPUTY",
                };
                let (status, status_style) = match p.status {
                    PlayerStatus::Playing => ("Играет", Style::new().fg(Color::Green)),
                    PlayerStatus::Viewing => ("Смотрит", Style::new().fg(Color::DarkGray)),
                };
                let name_style = if p.id == v.my_id {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                Row::new(vec![
                    TCell::from(Line::from(vec![
                        swatch,
                        Span::styled(format!("{}{me}", p.name), name_style),
                    ])),
                    TCell::from(p.score.to_string()),
                    TCell::from(role),
                    TCell::from(Span::styled(status, status_style)),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Min(10),
                Constraint::Length(5),
                Constraint::Length(7),
                Constraint::Length(8),
            ],
        )
        .header(
            Row::new(vec!["Имя", "Очки", "Роль", "Статус"])
                .style(Style::new().add_modifier(Modifier::BOLD).fg(Color::Yellow)),
        )
        .block(Block::bordered().title(format!(" Участники · {}/6 ", v.players.len())));
        f.render_widget(table, parts[0]);

        let snakes = v.world.snakes.len();
        let fs = v.cfg.food_static.max(0) as usize;
        f.render_widget(
            Paragraph::new(vec![
                Line::from(format!("Змейки:        {snakes}/{MAX_SNAKES}")),
                Line::from(format!("Запас еды:     {fs}")),
                Line::from(format!("Цель еды:      {fs} + {snakes} = {}", fs + snakes)),
                Line::from(format!("Еды на поле:   {}", v.world.foods.len())),
                Line::from(format!("Состояние №:   {}", v.order)),
            ])
            .block(Block::bordered().title(" Параметры игры ")),
            parts[1],
        );
        f.render_widget(
            Paragraph::new(controls).block(Block::bordered().title(" Управление ")),
            parts[2],
        );
    }
}

fn game_controls(v: &GameView) -> Vec<Line<'static>> {
    let mut controls = Vec::new();
    if v.status == PlayerStatus::Playing {
        controls.push(Line::from("↑↓←→ / WASD / HJKL — поворот"));
    }
    controls.push(Line::from(match v.status {
        PlayerStatus::Playing => "[V] наблюдать",
        PlayerStatus::Viewing => "[V] играть, если есть место",
    }));
    controls.push(Line::from("[E] отобразить направление змеек"));
    controls.push(Line::from("[Tab] сменить цвета"));
    if v.role == NodeRole::Master {
        controls.push(Line::from("[R] пригласить робота с сервера"));
    }
    controls.push(Line::from("[Q] / Esc — выйти в лобби"));
    controls.push(Line::from("Ctrl+C — закрыть приложение"));
    controls
}

fn head_symbol(dir: Dir, cell_width: usize) -> &'static str {
    match (dir, cell_width) {
        (Dir::Up, 2) => "˙˙",
        (Dir::Down, 2) => "..",
        (Dir::Left, 2) => ": ",
        (Dir::Right, 2) => " :",
        (Dir::Up, _) => "˙",
        (Dir::Down, _) => ".",
        (Dir::Left, _) | (Dir::Right, _) => ":",
    }
}

fn player_color_index(v: &GameView, id: i32) -> usize {
    v.players.iter().filter(|p| p.id < id).count()
}

fn player_color(v: &GameView, id: i32, palette_offset: usize) -> Color {
    PALETTE[(player_color_index(v, id) + palette_offset) % PALETTE.len()]
}

/// Размеры видимой части поля: ширина клетки (1 или 2 столбца), клеток по ширине и высоте.
fn board_dims(cfg: &Config, avail_w: usize, avail_h: usize) -> (usize, usize, usize) {
    let (w, h) = (cfg.width as usize, cfg.height as usize);
    let inner_w = avail_w.saturating_sub(2);
    let inner_h = avail_h.saturating_sub(2);
    // Клетка занимает два столбца, чтобы выглядеть квадратной; на узком терминале — один.
    let cw = if w * 2 <= inner_w { 2 } else { 1 };
    let vw = w.min((inner_w / cw).max(1));
    let vh = h.min(inner_h.max(1));
    (cw, vw, vh)
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

#[allow(dead_code)]
fn _text_is_used(_: Text) {}

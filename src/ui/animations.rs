//! Необязательные эффекты поверх готового кадра; игровое состояние не изменяется.
//!
//! События для анимаций выводятся из разницы двух наблюдаемых снимков: новая еда,
//! появившаяся/исчезнувшая змейка, рост или вход головы в прежнюю еду. Это только
//! визуальная интерпретация, а не журнал всех ходов: между кадрами могли прийти
//! несколько State. Эффекты привязаны к абсолютному времени старта, поэтому
//! частота кадров и обрезка поля не меняют их длительность.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Style};
use tachyonfx::{Effect, Interpolation, Motion, fx};

use crate::node::{GameView, PhaseKind};
use crate::rules::Cell;

#[cfg(test)]
mod tests;

const FOOD_MS: u32 = 500;
const SPAWN_MS: u32 = 360;
const DEPARTURE_MS: u32 = 650;
const TRANSITION_MS: u32 = 280;
const SEGMENT_DELAY_MS: u64 = 30;
const PULSE_MS: u32 = 240;

/// Шаблон эффекта и его абсолютная временная шкала: рендер не накапливает dt.
struct TimedEffect {
    effect: Effect,
    started: Instant,
    duration: Duration,
}

impl TimedEffect {
    fn new(effect: Effect, started: Instant, millis: u32) -> Self {
        Self {
            effect,
            started,
            duration: Duration::from_millis(u64::from(millis)),
        }
    }

    fn running(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) < self.duration
    }

    fn render(&self, buf: &mut Buffer, area: Rect, now: Instant) {
        let area = area.intersection(buf.area);
        if area.is_empty() || !self.running(now) || now < self.started {
            return;
        }
        // Отсчёт от начала эффекта, а не от предыдущего кадра: скрытые клетки
        // и уменьшенное окно не останавливают и не перезапускают анимацию.
        let elapsed = now.saturating_duration_since(self.started);
        self.effect.clone().process(elapsed.into(), buf, area);
    }
}

struct SnakeSnapshot {
    cells: VecDeque<Cell>,
    /// Индекс без сдвига: даже исчезнувшие змейки следуют текущей палитре.
    color_index: usize,
}

struct Departure {
    snake: SnakeSnapshot,
    effect: TimedEffect,
}

struct EatWave {
    started: Instant,
    len: usize,
}

impl EatWave {
    fn running(&self, now: Instant) -> bool {
        let millis = self.len.saturating_sub(1) as u64 * SEGMENT_DELAY_MS + u64::from(PULSE_MS);
        now.saturating_duration_since(self.started) < Duration::from_millis(millis)
    }

    fn render(&self, buf: &mut Buffer, area: Rect, segment: usize, now: Instant) {
        let started = self.started + Duration::from_millis(segment as u64 * SEGMENT_DELAY_MS);
        TimedEffect::new(
            fx::fade_from(
                Color::White,
                Color::White,
                (PULSE_MS, Interpolation::QuadOut),
            ),
            started,
            PULSE_MS,
        )
        .render(buf, area, now);
    }
}

/// Проекция координат мира в буфер терминала, общая с обычной отрисовкой поля.
/// Учитывает рамку, начало видимого окна и ширину клетки в столбцах терминала.
#[derive(Clone, Copy)]
pub(super) struct BoardViewport {
    area: Rect,
    origin: (usize, usize),
    size: (usize, usize),
    cell_width: usize,
}

impl BoardViewport {
    pub(super) fn new(
        area: Rect,
        origin: (usize, usize),
        size: (usize, usize),
        cell_width: usize,
    ) -> Self {
        Self {
            area,
            origin,
            size,
            cell_width,
        }
    }

    fn cell_area(self, (x, y): Cell) -> Option<Rect> {
        let x = usize::try_from(x).ok()?.checked_sub(self.origin.0)?;
        let y = usize::try_from(y).ok()?.checked_sub(self.origin.1)?;
        if x >= self.size.0 || y >= self.size.1 {
            return None;
        }
        let inner = self.area.inner(Margin::new(1, 1));
        if (x + 1) * self.cell_width > usize::from(inner.width) || y >= usize::from(inner.height) {
            return None;
        }
        Some(Rect::new(
            inner.x + (x * self.cell_width) as u16,
            inner.y + y as u16,
            self.cell_width as u16,
            1,
        ))
    }
}

#[derive(Default)]
struct State {
    phase: Option<PhaseKind>,
    transition: Option<TimedEffect>,
    foods: HashSet<Cell>,
    snakes: HashMap<i32, SnakeSnapshot>,
    food_effects: HashMap<Cell, TimedEffect>,
    appearances: HashMap<i32, TimedEffect>,
    departures: Vec<Departure>,
    eat_waves: HashMap<i32, EatWave>,
}

/// При отключённых анимациях состояние вообще не создаётся и кадр не меняется.
pub(super) struct Animations {
    state: Option<State>,
}

impl Animations {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            state: enabled.then(State::default),
        }
    }

    pub(super) fn update(&mut self, phase: PhaseKind, view: Option<&GameView>, now: Instant) {
        let Some(state) = &mut self.state else { return };
        if state.phase != Some(phase) {
            let previous = state.phase;
            *state = State {
                phase: Some(phase),
                ..State::default()
            };
            if let Some(previous) = previous {
                let direction = if previous == PhaseKind::InGame {
                    Motion::RightToLeft
                } else {
                    Motion::LeftToRight
                };
                state.transition = Some(TimedEffect::new(
                    fx::sweep_in(
                        direction,
                        12,
                        0,
                        Color::Black,
                        (TRANSITION_MS, Interpolation::QuadOut),
                    ),
                    now,
                    TRANSITION_MS,
                ));
            }
        }
        if state.transition.as_ref().is_some_and(|fx| !fx.running(now)) {
            state.transition = None;
        }
        if let Some(view) = view.filter(|_| phase == PhaseKind::InGame) {
            state.update_board(view, now);
        }
    }

    pub(super) fn render_transition(&self, f: &mut Frame, now: Instant) {
        if let Some(effect) = self.state.as_ref().and_then(|s| s.transition.as_ref()) {
            let area = f.area();
            effect.render(f.buffer_mut(), area, now);
        }
    }

    pub(super) fn render_board(
        &self,
        f: &mut Frame,
        v: &GameView,
        viewport: BoardViewport,
        now: Instant,
        palette_offset: usize,
    ) {
        let Some(state) = &self.state else { return };
        state.render_board(f.buffer_mut(), v, viewport, now, palette_offset);
    }
}

impl State {
    /// Сравнивает наблюдаемые снимки и удаляет завершённые эффекты. Исчезнувшие
    /// змейки ненадолго сохраняются отдельно для затухания уже удалённого тела.
    fn update_board(&mut self, v: &GameView, now: Instant) {
        let foods: HashSet<_> = v.world.foods.iter().copied().collect();
        self.food_effects
            .retain(|cell, fx| foods.contains(cell) && fx.running(now));
        for &cell in foods.difference(&self.foods) {
            self.food_effects.insert(
                cell,
                TimedEffect::new(
                    fx::fade_from_fg(Color::DarkGray, (FOOD_MS, Interpolation::QuadOut)),
                    now,
                    FOOD_MS,
                ),
            );
        }

        let snakes: HashMap<_, _> = v
            .world
            .snakes
            .iter()
            .map(|s| {
                (
                    s.owner,
                    SnakeSnapshot {
                        cells: s.cells.clone(),
                        color_index: super::player_color_index(v, s.owner),
                    },
                )
            })
            .collect();
        self.appearances
            .retain(|id, fx| snakes.contains_key(id) && fx.running(now));
        self.eat_waves
            .retain(|id, wave| snakes.contains_key(id) && wave.running(now));
        self.departures.retain(|d| d.effect.running(now));

        for (&owner, snake) in &snakes {
            if let Some(previous) = self.snakes.get(&owner) {
                let grew = snake.cells.len() > previous.cells.len();
                // При длине 32 еда не увеличивает тело, поэтому одного grew
                // недостаточно: проверяем также голову на месте прежней еды.
                let ate = snake.cells.front() != previous.cells.front()
                    && snake
                        .cells
                        .front()
                        .is_some_and(|head| self.foods.contains(head));
                if grew || ate {
                    self.eat_waves.insert(
                        owner,
                        EatWave {
                            started: now,
                            len: snake.cells.len(),
                        },
                    );
                }
            } else {
                self.appearances.insert(
                    owner,
                    TimedEffect::new(
                        fx::fade_from(
                            Color::White,
                            Color::White,
                            (SPAWN_MS, Interpolation::QuadOut),
                        ),
                        now,
                        SPAWN_MS,
                    ),
                );
            }
        }
        for (owner, snake) in std::mem::replace(&mut self.snakes, snakes) {
            if !self.snakes.contains_key(&owner) {
                self.departures.push(Departure {
                    snake,
                    effect: TimedEffect::new(
                        fx::fade_to(
                            Color::DarkGray,
                            Color::Black,
                            (DEPARTURE_MS, Interpolation::QuadIn),
                        ),
                        now,
                        DEPARTURE_MS,
                    ),
                });
            }
        }
        self.foods = foods;
    }

    fn render_board(
        &self,
        buf: &mut Buffer,
        v: &GameView,
        viewport: BoardViewport,
        now: Instant,
        palette_offset: usize,
    ) {
        let occupied: HashSet<_> = v
            .world
            .snakes
            .iter()
            .flat_map(|s| s.cells.iter().copied())
            .chain(v.world.foods.iter().copied())
            .collect();
        for departure in &self.departures {
            if !departure.effect.running(now) {
                continue;
            }
            for &cell in &departure.snake.cells {
                // След исчезнувшей змейки не перекрывает новую еду и живые тела.
                if occupied.contains(&cell) {
                    continue;
                }
                let Some(area) = viewport.cell_area(cell) else {
                    continue;
                };
                let area = area.intersection(buf.area);
                if area.is_empty() {
                    continue;
                }
                let color = super::PALETTE
                    [(departure.snake.color_index + palette_offset) % super::PALETTE.len()];
                for x in area.left()..area.right() {
                    buf[(x, area.y)]
                        .set_symbol(if viewport.cell_width == 2 { " " } else { "█" })
                        .set_style(Style::new().fg(color).bg(if viewport.cell_width == 2 {
                            color
                        } else {
                            Color::Reset
                        }));
                }
                departure.effect.render(buf, area, now);
            }
        }
        for (&cell, effect) in &self.food_effects {
            if let Some(area) = viewport.cell_area(cell) {
                effect.render(buf, area, now);
            }
        }
        for snake in &v.world.snakes {
            for (segment, &cell) in snake.cells.iter().enumerate() {
                let Some(area) = viewport.cell_area(cell) else {
                    continue;
                };
                if let Some(effect) = self.appearances.get(&snake.owner) {
                    effect.render(buf, area, now);
                }
                if let Some(wave) = self.eat_waves.get(&snake.owner) {
                    wave.render(buf, area, segment, now);
                }
            }
        }
    }
}

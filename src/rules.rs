//! Правила игры: поле-тор, змейки, еда, расчёт хода и размещение новых змеек.
//!
//! Модуль чистый: никакой сети и времени, только состояние и генератор случайных
//! чисел, поэтому каждое правило проверяется обычными юнит-тестами.
//!
//! В памяти тело змейки — все клетки от головы к хвосту, в протоколе — голова и
//! осевые смещения до поворотов. Конверсии выполняют [`Snake::to_wire`] и
//! [`Snake::from_wire`]. [`World::step`] двигает сразу всех змеек и возвращает
//! начисления и смерти, но не хранит таблицу игроков: статусы, суммарные очки
//! и последующее пополнение еды обновляет сетевое ядро.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::wire::{Config, Dir, WireSnake};

/// Координата `(x, y)`: начало слева сверху, `x` вправо, `y` вниз.
pub type Cell = (i32, i32);

/// Предельная длина змейки.
pub const MAX_SNAKE_LEN: usize = 32;

/// Простой детерминированный генератор (SplitMix64): без внешних зависимостей
/// и с повторяемыми результатами в тестах.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Число в `0..n` (n > 0).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

/// Последовательность клеток одной живой змейки во внутреннем представлении.
#[derive(Clone, Debug, PartialEq)]
pub struct Snake {
    pub owner: i32,
    /// Клетки от головы к хвосту.
    pub cells: VecDeque<Cell>,
    pub dir: Dir,
}

impl Snake {
    pub fn head(&self) -> Cell {
        self.cells[0]
    }

    /// Переводит клетки в ключевые точки протокола. Путь описывает тело, а не
    /// кратчайший путь по тору, поэтому шаги считаются между соседними клетками.
    pub fn to_wire(&self, w: i32, h: i32) -> WireSnake {
        let cells: Vec<Cell> = self.cells.iter().copied().collect();
        let unit = |a: Cell, b: Cell| -> Cell {
            let (mut dx, mut dy) = (b.0 - a.0, b.1 - a.1);
            if dx > 1 {
                dx -= w;
            } else if dx < -1 {
                dx += w;
            }
            if dy > 1 {
                dy -= h;
            } else if dy < -1 {
                dy += h;
            }
            (dx, dy)
        };

        let mut points = vec![cells[0]];
        let mut run: Option<(Cell, i32)> = None;
        for pair in cells.windows(2) {
            let d = unit(pair[0], pair[1]);
            match &mut run {
                Some((rd, len)) if *rd == d => *len += 1,
                _ => {
                    if let Some((rd, len)) = run {
                        points.push((rd.0 * len, rd.1 * len));
                    }
                    run = Some((d, 1));
                }
            }
        }
        if let Some((rd, len)) = run {
            points.push((rd.0 * len, rd.1 * len));
        }
        WireSnake {
            player_id: self.owner,
            points,
            dir: self.dir,
        }
    }

    /// Разбирает ключевые точки. `None`, если змейка некорректна.
    /// Восстанавливает каждый единичный шаг с замыканием координат, не дублируя
    /// стык отрезков. Проверяет форму смещений и длину, но не владельца/статус.
    /// `w` и `h` должны быть положительными.
    pub fn from_wire(ws: &WireSnake, w: i32, h: i32) -> Option<Snake> {
        let (&head, offsets) = ws.points.split_first()?;
        if offsets.is_empty() {
            return None;
        }
        let wrap = |c: Cell| (c.0.rem_euclid(w), c.1.rem_euclid(h));
        let mut cur = wrap(head);
        let mut cells = VecDeque::from([cur]);
        for &(dx, dy) in offsets {
            // Смещение ненулевое и осевое.
            if (dx == 0) == (dy == 0) {
                return None;
            }
            let steps = dx.unsigned_abs() as usize + dy.unsigned_abs() as usize;
            if cells.len() + steps > MAX_SNAKE_LEN {
                return None;
            }
            let unit = (dx.signum(), dy.signum());
            for _ in 0..steps {
                cur = wrap((cur.0 + unit.0, cur.1 + unit.1));
                cells.push_back(cur);
            }
        }
        Some(Snake {
            owner: ws.player_id,
            cells,
            dir: ws.dir,
        })
    }
}

/// Поле, живые змейки и еда; не содержит сетевых ролей и таблицы очков.
///
/// Методы геометрии предполагают допустимый конфиг с положительными размерами;
/// его проверка выполняется до создания игровой сессии.
#[derive(Clone, Debug)]
pub struct World {
    pub cfg: Config,
    pub snakes: Vec<Snake>,
    pub foods: Vec<Cell>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct StepResult {
    /// Владельцы погибших змеек.
    pub dead: Vec<i32>,
    /// Начисленные очки: (владелец, прибавка).
    pub points: Vec<(i32, i32)>,
}

impl World {
    pub fn new(cfg: Config) -> World {
        World {
            cfg,
            snakes: Vec::new(),
            foods: Vec::new(),
        }
    }

    pub fn wrap(&self, c: Cell) -> Cell {
        (
            c.0.rem_euclid(self.cfg.width),
            c.1.rem_euclid(self.cfg.height),
        )
    }

    pub fn advance(&self, c: Cell, d: Dir) -> Cell {
        let (dx, dy) = d.delta();
        self.wrap((c.0 + dx, c.1 + dy))
    }

    pub fn snake(&self, owner: i32) -> Option<&Snake> {
        self.snakes.iter().find(|s| s.owner == owner)
    }

    /// Команда допустима, если голова не развернётся в клетку шеи.
    /// Проверка идёт по текущему телу, а не по предыдущей команде.
    pub fn is_steer_valid(&self, owner: i32, d: Dir) -> bool {
        match self.snake(owner) {
            Some(s) if s.cells.len() >= 2 => self.advance(s.head(), d) != s.cells[1],
            Some(_) => true,
            None => false,
        }
    }

    pub fn occupied(&self) -> HashSet<Cell> {
        self.snakes
            .iter()
            .flat_map(|s| s.cells.iter().copied())
            .collect()
    }

    pub fn remove_snake(&mut self, owner: i32) -> bool {
        let before = self.snakes.len();
        self.snakes.retain(|s| s.owner != owner);
        self.snakes.len() != before
    }

    /// Ищет квадрат 5×5 без чужих клеток (с учётом замыкания поля), ставит голову
    /// в центр, хвост — в случайную соседнюю клетку. Обе клетки без еды.
    pub fn place_snake(&mut self, owner: i32, rng: &mut Rng) -> bool {
        let occupied = self.occupied();
        let foods: HashSet<Cell> = self.foods.iter().copied().collect();
        let (w, h) = (self.cfg.width, self.cfg.height);

        let mut centers: Vec<Cell> = (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).collect();
        rng.shuffle(&mut centers);

        for center in centers {
            if foods.contains(&center) {
                continue;
            }
            let square_free = (-2..=2).all(|dy| {
                (-2..=2).all(|dx| !occupied.contains(&self.wrap((center.0 + dx, center.1 + dy))))
            });
            if !square_free {
                continue;
            }
            let mut dirs = [Dir::Up, Dir::Down, Dir::Left, Dir::Right];
            rng.shuffle(&mut dirs);
            for d in dirs {
                let tail = self.advance(center, d);
                if !foods.contains(&tail) {
                    self.snakes.push(Snake {
                        owner,
                        cells: VecDeque::from([center, tail]),
                        dir: d.opposite(),
                    });
                    return true;
                }
            }
        }
        false
    }

    /// Целевое минимальное число клеток с едой: запас + число змеек.
    pub fn food_target(&self) -> usize {
        self.cfg.food_static.max(0) as usize + self.snakes.len()
    }

    /// Пополняет еду до цели на случайных свободных клетках.
    /// Лишнюю еду после удаления змейки не убирает; при нехватке места заполняет
    /// только доступные клетки. Вызывается отдельно от `step` и `place_snake`.
    pub fn refill_food(&mut self, rng: &mut Rng) {
        if self.foods.len() >= self.food_target() {
            return;
        }
        let occupied = self.occupied();
        let taken: HashSet<Cell> = self.foods.iter().copied().collect();
        let mut free: Vec<Cell> = (0..self.cfg.height)
            .flat_map(|y| (0..self.cfg.width).map(move |x| (x, y)))
            .filter(|c| !occupied.contains(c) && !taken.contains(c))
            .collect();
        while self.foods.len() < self.food_target() && !free.is_empty() {
            let i = rng.below(free.len());
            self.foods.push(free.swap_remove(i));
        }
    }

    /// Один одновременный ход игры. `steers` — последние допустимые команды участников.
    ///
    /// Сначала меняются тела всех змеек, затем по итоговой занятости определяются
    /// смерти и очки. Поэтому освобождённый хвост уже не препятствие, а змейка,
    /// погибшая в этот ход, всё ещё может получить очки за чужое столкновение.
    /// Погибшие удаляются, но еда не пополняется: вызывающий обновляет статусы
    /// игроков, начисляет `StepResult::points` и вызывает `refill_food`.
    pub fn step(&mut self, steers: &HashMap<i32, Dir>) -> StepResult {
        let food_set: HashSet<Cell> = self.foods.iter().copied().collect();
        let mut points: HashMap<i32, i32> = HashMap::new();
        let mut eaten: HashSet<Cell> = HashSet::new();

        // 1. Выбор направления и новой клетки головы.
        let moves: Vec<(usize, Dir, Cell)> = self
            .snakes
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let dir = steers
                    .get(&s.owner)
                    .copied()
                    .filter(|&d| self.is_steer_valid(s.owner, d))
                    .unwrap_or(s.dir);
                (i, dir, self.advance(s.head(), dir))
            })
            .collect();

        // 2. Движение: голова вперёд, хвост освобождается, если еды не было
        //    (или змейка уже достигла предельной длины).
        for (i, dir, new_head) in moves {
            let s = &mut self.snakes[i];
            let len_before = s.cells.len();
            s.cells.push_front(new_head);
            s.dir = dir;
            if food_set.contains(&new_head) {
                *points.entry(s.owner).or_insert(0) += 1;
                eaten.insert(new_head);
                if len_before >= MAX_SNAKE_LEN {
                    s.cells.pop_back();
                }
            } else {
                s.cells.pop_back();
            }
        }
        self.foods.retain(|f| !eaten.contains(f));

        // 3. Столкновения по итоговой занятости клеток. Своя голова сама себе
        //    не мешает, а своё тело — препятствие.
        let mut occupancy: HashMap<Cell, Vec<usize>> = HashMap::new();
        for (i, s) in self.snakes.iter().enumerate() {
            for &c in &s.cells {
                occupancy.entry(c).or_default().push(i);
            }
        }
        let mut dead_idx = Vec::new();
        for (i, s) in self.snakes.iter().enumerate() {
            let here = &occupancy[&s.head()];
            if here.len() > 1 {
                dead_idx.push(i);
                // Каждый другой владелец клетки получает очко за врезавшуюся голову.
                let mut others: Vec<usize> = here.iter().copied().filter(|&t| t != i).collect();
                others.sort_unstable();
                others.dedup();
                for t in others {
                    *points.entry(self.snakes[t].owner).or_insert(0) += 1;
                }
            }
        }

        let dead: Vec<i32> = dead_idx.iter().map(|&i| self.snakes[i].owner).collect();
        self.snakes.retain(|s| !dead.contains(&s.owner));

        let mut points: Vec<(i32, i32)> = points.into_iter().collect();
        points.sort_unstable();
        StepResult { dead, points }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            width: 10,
            height: 10,
            food_static: 1,
            state_delay_ms: 1000,
        }
    }

    fn snake(owner: i32, cells: &[Cell], dir: Dir) -> Snake {
        Snake {
            owner,
            cells: cells.iter().copied().collect(),
            dir,
        }
    }

    fn world(snakes: Vec<Snake>, foods: Vec<Cell>) -> World {
        World {
            cfg: cfg(),
            snakes,
            foods,
        }
    }

    fn no_steer() -> HashMap<i32, Dir> {
        HashMap::new()
    }

    #[test]
    fn moves_forward_and_wraps_around_the_torus() {
        let mut w = world(vec![snake(1, &[(9, 5), (8, 5)], Dir::Right)], vec![]);
        w.step(&no_steer());
        assert_eq!(w.snakes[0].cells, VecDeque::from([(0, 5), (9, 5)]));
    }

    #[test]
    fn eating_grows_snake_and_scores() {
        let mut w = world(vec![snake(1, &[(3, 3), (2, 3)], Dir::Right)], vec![(4, 3)]);
        let r = w.step(&no_steer());
        assert_eq!(w.snakes[0].cells.len(), 3);
        assert_eq!(r.points, vec![(1, 1)]);
        assert!(w.foods.is_empty());
    }

    #[test]
    fn full_length_snake_eats_without_growing() {
        // Путь «змейкой» по четырём строкам поля: ровно 32 клетки, голова (0,0).
        let mut cells = VecDeque::new();
        for row in 0..4 {
            for col in 0..10 {
                let x = if row % 2 == 0 { col } else { 9 - col };
                cells.push_back((x, row));
            }
        }
        cells.truncate(MAX_SNAKE_LEN);
        let mut w = world(
            vec![Snake {
                owner: 1,
                cells,
                dir: Dir::Left,
            }],
            vec![(0, 9)],
        );
        let mut steer = HashMap::new();
        steer.insert(1, Dir::Up);
        let r = w.step(&steer);
        assert_eq!(w.snakes[0].cells.len(), MAX_SNAKE_LEN);
        assert_eq!(w.snakes[0].head(), (0, 9));
        assert_eq!(r.points, vec![(1, 1)]);
        assert!(r.dead.is_empty());
    }

    #[test]
    fn chasing_a_freed_tail_is_allowed() {
        // A: голова (5,5) идёт вправо, хвост (3,5) освобождается; B догоняет в клетку (4,5)? нет —
        // проверяем простой случай: голова входит в клетку, которую только что покинул хвост.
        let mut w = world(
            vec![snake(1, &[(5, 5), (5, 6), (4, 6), (4, 5)], Dir::Up)],
            vec![],
        );
        // Поворот направо: новая голова (6,5)... вместо этого идём влево в клетку хвоста (4,5).
        let mut steer = HashMap::new();
        steer.insert(1, Dir::Left);
        let r = w.step(&steer);
        assert!(
            r.dead.is_empty(),
            "змейка не должна погибнуть: {:?}",
            w.snakes
        );
    }

    #[test]
    fn reverse_into_neck_is_ignored() {
        let mut w = world(vec![snake(1, &[(5, 5), (4, 5)], Dir::Right)], vec![]);
        let mut steer = HashMap::new();
        steer.insert(1, Dir::Left);
        w.step(&steer);
        assert_eq!(w.snakes[0].head(), (6, 5));
    }

    #[test]
    fn head_on_collision_kills_both_and_scores_each() {
        let mut w = world(
            vec![
                snake(1, &[(4, 5), (3, 5)], Dir::Right),
                snake(2, &[(6, 5), (7, 5)], Dir::Left),
            ],
            vec![],
        );
        let r = w.step(&no_steer());
        assert_eq!(r.dead.len(), 2);
        assert_eq!(r.points, vec![(1, 1), (2, 1)]);
        assert!(w.snakes.is_empty());
    }

    #[test]
    fn hitting_a_body_gives_the_victim_a_point_and_kills_only_the_attacker() {
        let mut w = world(
            vec![
                snake(1, &[(5, 3), (5, 4), (5, 5), (5, 6)], Dir::Up),
                snake(2, &[(3, 4), (2, 4)], Dir::Right),
            ],
            vec![],
        );
        // B идёт вправо: (4,4) свободна; нужен удар в тело A: ставим B прямо перед (5,4).
        w.snakes[1] = snake(2, &[(4, 4), (3, 4)], Dir::Right);
        let r = w.step(&no_steer());
        assert_eq!(r.dead, vec![2]);
        assert_eq!(r.points, vec![(1, 1)]);
        assert_eq!(w.snakes.len(), 1);
    }

    #[test]
    fn self_collision_scores_nothing() {
        let mut w = world(
            vec![snake(
                1,
                &[(5, 5), (5, 6), (4, 6), (4, 5), (4, 4), (5, 4)],
                Dir::Up,
            )],
            vec![],
        );
        let mut steer = HashMap::new();
        steer.insert(1, Dir::Left);
        // Голова (5,5) идёт влево в (4,5) — своё тело.
        let r = w.step(&steer);
        assert_eq!(r.dead, vec![1]);
        assert!(r.points.is_empty());
    }

    #[test]
    fn head_swap_kills_both() {
        let mut w = world(
            vec![
                snake(1, &[(4, 5), (3, 5)], Dir::Right),
                snake(2, &[(5, 5), (6, 5)], Dir::Left),
            ],
            vec![],
        );
        let r = w.step(&no_steer());
        assert_eq!(r.dead.len(), 2);
    }

    #[test]
    fn three_heads_on_one_cell_score_two_each() {
        let mut w = world(
            vec![
                snake(1, &[(4, 5), (3, 5)], Dir::Right),
                snake(2, &[(6, 5), (7, 5)], Dir::Left),
                snake(3, &[(5, 4), (5, 3)], Dir::Down),
            ],
            vec![],
        );
        let r = w.step(&no_steer());
        assert_eq!(r.dead.len(), 3);
        assert_eq!(r.points, vec![(1, 2), (2, 2), (3, 2)]);
    }

    #[test]
    fn placement_uses_free_square_and_avoids_food() {
        let mut rng = Rng::new(7);
        let mut w = world(vec![], vec![]);
        for owner in 1..=4 {
            assert!(w.place_snake(owner, &mut rng), "змейка {owner}");
        }
        let all: Vec<Cell> = w
            .snakes
            .iter()
            .flat_map(|s| s.cells.iter().copied())
            .collect();
        let unique: HashSet<Cell> = all.iter().copied().collect();
        assert_eq!(all.len(), unique.len());
        // Хвост действительно позади головы.
        for s in &w.snakes {
            assert_eq!(w.advance(s.cells[1], s.dir), s.cells[0]);
        }
    }

    #[test]
    fn placement_fails_when_there_is_no_free_square() {
        let mut rng = Rng::new(1);
        let mut w = world(vec![], vec![]);
        // Занимаем каждую третью клетку по обеим осям, чтобы свободного квадрата 5x5 не осталось.
        let mut owner = 100;
        for x in (0..10).step_by(3) {
            for y in (0..10).step_by(3) {
                w.snakes.push(snake(owner, &[(x, y), (x, y)], Dir::Up));
                owner += 1;
            }
        }
        assert!(!w.place_snake(1, &mut rng));
    }

    #[test]
    fn refill_reaches_target_on_free_cells() {
        let mut rng = Rng::new(3);
        let mut w = world(vec![snake(1, &[(0, 0), (1, 0)], Dir::Left)], vec![]);
        w.cfg.food_static = 3;
        w.refill_food(&mut rng);
        assert_eq!(w.foods.len(), 4);
        assert!(w.foods.iter().all(|f| *f != (0, 0) && *f != (1, 0)));
    }

    #[test]
    fn wire_roundtrip_keeps_cells_including_wraparound() {
        let s = snake(
            2,
            &[(1, 0), (0, 0), (9, 0), (9, 1), (9, 2), (8, 2)],
            Dir::Right,
        );
        let wire = s.to_wire(10, 10);
        assert_eq!(wire.points, vec![(1, 0), (-2, 0), (0, 2), (-1, 0)]);
        assert_eq!(Snake::from_wire(&wire, 10, 10).unwrap(), s);
    }

    #[test]
    fn spec_example_decodes_to_ten_cells() {
        let ws = WireSnake {
            player_id: 1,
            points: vec![(5, 1), (3, 0), (0, 2), (-4, 0)],
            dir: Dir::Left,
        };
        let s = Snake::from_wire(&ws, 40, 30).unwrap();
        assert_eq!(s.cells.len(), 10);
        assert_eq!(s.cells[3], (8, 1));
        assert_eq!(s.cells[9], (4, 3));
    }

    #[test]
    fn rejects_malformed_snakes() {
        let bad_diag = WireSnake {
            player_id: 1,
            points: vec![(5, 5), (1, 1)],
            dir: Dir::Up,
        };
        assert!(Snake::from_wire(&bad_diag, 10, 10).is_none());
        let too_long = WireSnake {
            player_id: 1,
            points: vec![(0, 0), (40, 0)],
            dir: Dir::Up,
        };
        assert!(Snake::from_wire(&too_long, 10, 10).is_none());
        let head_only = WireSnake {
            player_id: 1,
            points: vec![(0, 0)],
            dir: Dir::Up,
        };
        assert!(Snake::from_wire(&head_only, 10, 10).is_none());
    }
}

//! Authoritative game simulation: snakes, food, collisions and scoring.
//! Owned and advanced by the MASTER node; DEPUTY keeps it warm so it can take
//! over seamlessly; everyone else only renders snapshots received from MASTER.

use std::collections::{BTreeMap, HashSet};

use rand::seq::SliceRandom;

use crate::model::{Direction, GameConfig, Point, MAX_SNAKES, MAX_SNAKE_LEN, MIN_SNAKE_LEN};
use crate::proto;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRole {
    Normal,
    Master,
    Deputy,
}

impl NodeRole {
    pub fn to_proto(self) -> proto::NodeRole {
        match self {
            NodeRole::Normal => proto::NodeRole::NORMAL,
            NodeRole::Master => proto::NodeRole::MASTER,
            NodeRole::Deputy => proto::NodeRole::DEPUTY,
        }
    }

    pub fn from_proto(r: proto::NodeRole) -> NodeRole {
        match r {
            proto::NodeRole::NORMAL => NodeRole::Normal,
            proto::NodeRole::MASTER => NodeRole::Master,
            proto::NodeRole::DEPUTY => NodeRole::Deputy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerStatus {
    Playing,
    Viewing,
}

impl PlayerStatus {
    pub fn to_proto(self) -> proto::PlayerStatus {
        match self {
            PlayerStatus::Playing => proto::PlayerStatus::PLAYING,
            PlayerStatus::Viewing => proto::PlayerStatus::VIEWING,
        }
    }

    pub fn from_proto(s: proto::PlayerStatus) -> PlayerStatus {
        match s {
            proto::PlayerStatus::PLAYING => PlayerStatus::Playing,
            proto::PlayerStatus::VIEWING => PlayerStatus::Viewing,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Player {
    pub id: i32,
    pub name: String,
    pub addr: Option<std::net::SocketAddrV4>,
    pub role: NodeRole,
    pub score: i32,
    pub status: PlayerStatus,
}

impl Player {
    pub fn add_score(&mut self, delta: i32) {
        self.score = self.score.saturating_add(delta);
    }
}

/// A snake stored as head position plus run-length-encoded straight segments
/// towards the tail; this mirrors the wire encoding closely.
#[derive(Debug, Clone)]
pub struct Snake {
    pub player_id: i32,
    pub head: Point,
    pub head_dir: Direction,
    /// (direction, length) segments walking from head to tail; directions
    /// alternate (no two consecutive segments share a direction).
    pub segments: Vec<(Direction, i32)>,
}

impl Snake {
    pub fn len(&self) -> i32 {
        1 + self.segments.iter().map(|(_, l)| l).sum::<i32>()
    }

    pub fn cells(&self, config: &GameConfig) -> Vec<Point> {
        let mut out = Vec::with_capacity(self.len() as usize);
        out.push(self.head);
        let mut cur = self.head;
        for (dir, len) in &self.segments {
            let (dx, dy) = dir.delta();
            for _ in 0..*len {
                cur = config.wrap(Point::new(cur.x + dx, cur.y + dy));
                out.push(cur);
            }
        }
        out
    }

    pub fn to_proto(&self) -> proto::game_state::Snake {
        let mut s = proto::game_state::Snake::new();
        s.set_player_id(self.player_id);
        s.set_head_direction(self.head_dir.to_proto());
        let mut points = Vec::with_capacity(1 + self.segments.len());
        points.push(coord_signed(self.head.x, self.head.y));
        for (dir, len) in &self.segments {
            let (dx, dy) = dir.delta();
            points.push(coord_signed(dx * len, dy * len));
        }
        s.points = points;
        s
    }

    pub fn from_proto(s: &proto::game_state::Snake) -> Option<Snake> {
        let mut pts = s.points.iter();
        let first = pts.next()?;
        let head = Point::new(first.x(), first.y());
        let mut segments = Vec::new();
        for p in pts {
            let (x, y) = (p.x(), p.y());
            let (dir, len) = if x != 0 && y == 0 {
                (if x > 0 { Direction::Right } else { Direction::Left }, x.abs())
            } else if y != 0 && x == 0 {
                (if y > 0 { Direction::Down } else { Direction::Up }, y.abs())
            } else {
                return None;
            };
            segments.push((dir, len));
        }
        Some(Snake {
            player_id: s.player_id(),
            head,
            head_dir: Direction::from_proto(s.head_direction()),
            segments,
        })
    }

    /// Advances the head in `dir`, releasing or keeping the tail depending on
    /// `grow`. `dir` must already be validated against reversing into the neck.
    fn advance(&mut self, dir: Direction, grow: bool, config: &GameConfig) {
        let new_head = config.wrap(Point::new(
            self.head.x + dir.delta().0,
            self.head.y + dir.delta().1,
        ));
        let neck_dir = dir.opposite();
        self.head = new_head;
        self.head_dir = dir;
        if let Some((first_dir, first_len)) = self.segments.first_mut() {
            if *first_dir == neck_dir {
                *first_len += 1;
            } else {
                self.segments.insert(0, (neck_dir, 1));
            }
        } else {
            self.segments.insert(0, (neck_dir, 1));
        }
        if !grow {
            if let Some((_, last_len)) = self.segments.last_mut() {
                *last_len -= 1;
                if *last_len == 0 {
                    self.segments.pop();
                }
            }
        }
    }
}

fn coord_signed(x: i32, y: i32) -> proto::game_state::Coord {
    let mut c = proto::game_state::Coord::new();
    if x != 0 {
        c.set_x(x);
    }
    if y != 0 {
        c.set_y(y);
    }
    c
}

pub struct Game {
    pub config: GameConfig,
    pub state_order: i32,
    pub next_player_id: i32,
    pub players: BTreeMap<i32, Player>,
    pub snakes: BTreeMap<i32, Snake>,
    pub foods: HashSet<Point>,
}

#[derive(Debug)]
pub enum JoinError {
    NoSpace,
    TooManyPlayers,
}

impl Game {
    pub fn new(config: GameConfig) -> Self {
        Game {
            config,
            state_order: 0,
            next_player_id: 1,
            players: BTreeMap::new(),
            snakes: BTreeMap::new(),
            foods: HashSet::new(),
        }
    }

    pub fn alloc_player_id(&mut self) -> Option<i32> {
        if self.next_player_id == i32::MAX {
            return None;
        }
        let id = self.next_player_id;
        self.next_player_id += 1;
        Some(id)
    }

    fn occupied_by_snakes(&self) -> HashSet<Point> {
        let mut set = HashSet::new();
        for s in self.snakes.values() {
            set.extend(s.cells(&self.config));
        }
        set
    }

    /// Finds a free 5x5 square and spawns a new 2-cell snake per the spawn
    /// rules, returning the snake without inserting it.
    pub fn try_spawn(&self) -> Option<Snake> {
        if self.snakes.len() >= MAX_SNAKES {
            return None;
        }
        let occupied = self.occupied_by_snakes();
        let w = self.config.width;
        let h = self.config.height;
        let mut rng = rand::rng();
        let mut tops: Vec<(i32, i32)> = (0..w).flat_map(|x| (0..h).map(move |y| (x, y))).collect();
        tops.shuffle(&mut rng);
        let mut dirs = [Direction::Up, Direction::Down, Direction::Left, Direction::Right];
        for (tx, ty) in tops {
            let mut free = true;
            'scan: for dx in 0..5 {
                for dy in 0..5 {
                    let p = self.config.wrap(Point::new(tx + dx, ty + dy));
                    if occupied.contains(&p) {
                        free = false;
                        break 'scan;
                    }
                }
            }
            if !free {
                continue;
            }
            let center = self.config.wrap(Point::new(tx + 2, ty + 2));
            if self.foods.contains(&center) {
                continue;
            }
            dirs.shuffle(&mut rng);
            for d in dirs {
                let tail = self.config.wrap(Point::new(
                    center.x + d.delta().0,
                    center.y + d.delta().1,
                ));
                if self.foods.contains(&tail) {
                    continue;
                }
                return Some(Snake {
                    player_id: 0,
                    head: center,
                    head_dir: d.opposite(),
                    segments: vec![(d, 1)],
                });
            }
        }
        None
    }

    pub fn join_player(&mut self, player_id: i32) -> Result<(), JoinError> {
        if self.snakes.len() >= MAX_SNAKES {
            return Err(JoinError::TooManyPlayers);
        }
        let mut snake = self.try_spawn().ok_or(JoinError::NoSpace)?;
        snake.player_id = player_id;
        self.snakes.insert(player_id, snake);
        self.refill_food();
        Ok(())
    }

    pub fn target_food(&self) -> usize {
        (self.config.food_static as usize + self.snakes.len()).min(14)
    }

    pub fn refill_food(&mut self) {
        let target = self.target_food();
        if self.foods.len() >= target {
            return;
        }
        let occupied = self.occupied_by_snakes();
        let mut free: Vec<Point> = Vec::new();
        for x in 0..self.config.width {
            for y in 0..self.config.height {
                let p = Point::new(x, y);
                if !occupied.contains(&p) && !self.foods.contains(&p) {
                    free.push(p);
                }
            }
        }
        let mut rng = rand::rng();
        free.shuffle(&mut rng);
        for p in free.into_iter().take(target - self.foods.len()) {
            self.foods.insert(p);
        }
    }

    /// Advances the whole field by one tick given each playing snake's
    /// requested steer direction (if any). Returns the set of player ids
    /// whose snakes died this tick (already removed, status not yet changed).
    pub fn step(&mut self, requested: &BTreeMap<i32, Direction>) -> Vec<i32> {
        let ids: Vec<i32> = self.snakes.keys().copied().collect();
        let mut moved_snakes: BTreeMap<i32, Snake> = BTreeMap::new();
        let mut new_cells: BTreeMap<i32, Vec<Point>> = BTreeMap::new();
        let mut ate: BTreeMap<i32, bool> = BTreeMap::new();

        for id in &ids {
            let snake = self.snakes.get(id).unwrap();
            let cur_dir = snake.head_dir;
            let mut dir = requested.get(id).copied().unwrap_or(cur_dir);
            if dir == cur_dir.opposite() {
                dir = cur_dir;
            }
            let new_head = self.config.wrap(Point::new(
                snake.head.x + dir.delta().0,
                snake.head.y + dir.delta().1,
            ));
            let will_eat = self.foods.contains(&new_head);
            let grow = will_eat && snake.len() < MAX_SNAKE_LEN as i32;
            ate.insert(*id, will_eat);
            let mut moved = snake.clone();
            moved.advance(dir, grow, &self.config);
            new_cells.insert(*id, moved.cells(&self.config));
            moved_snakes.insert(*id, moved);
        }

        // A snake dies if its new head lands on its own body (cells[1..]) or
        // on any cell of any snake's new position, including heads (so two
        // heads meeting, or swapping into occupied bodies, both kill).
        let mut dead: Vec<i32> = Vec::new();
        for id in &ids {
            let my_cells = &new_cells[id];
            let my_head = my_cells[0];
            let mut collided = my_cells[1..].contains(&my_head);
            if !collided {
                for other in &ids {
                    if other == id {
                        continue;
                    }
                    if new_cells[other].contains(&my_head) {
                        collided = true;
                        break;
                    }
                }
            }
            if collided {
                dead.push(*id);
            }
        }

        // Scoring for collisions: for every ordered pair (p, q), p != q, if
        // q's new head landed anywhere inside p's new body (including p's own
        // new head), p scores +1 regardless of p's own survival.
        let mut bonus: BTreeMap<i32, i32> = BTreeMap::new();
        for p in &ids {
            let mut gained = 0;
            for q in &ids {
                if p == q {
                    continue;
                }
                let q_head = new_cells[q][0];
                if new_cells[p].contains(&q_head) {
                    gained += 1;
                }
            }
            if gained > 0 {
                bonus.insert(*p, gained);
            }
        }

        // Replace snakes with their moved versions and apply scoring.
        for id in &ids {
            let moved = moved_snakes.remove(id).unwrap();
            let will_eat = ate[id];
            if will_eat {
                self.foods.remove(&moved.head);
                if let Some(p) = self.players.get_mut(id) {
                    p.add_score(1);
                }
            }
            if let Some(b) = bonus.get(id) {
                if let Some(p) = self.players.get_mut(id) {
                    p.add_score(*b);
                }
            }
            self.snakes.insert(*id, moved);
        }
        for id in &dead {
            self.snakes.remove(id);
            if let Some(p) = self.players.get_mut(id) {
                p.status = PlayerStatus::Viewing;
            }
        }
        self.refill_food();
        dead
    }

    pub fn to_proto(&self) -> proto::GameState {
        let mut gs = proto::GameState::new();
        gs.set_state_order(self.state_order);
        gs.set_next_player_id(self.next_player_id);
        gs.snakes = self.snakes.values().map(Snake::to_proto).collect();
        gs.foods = self
            .foods
            .iter()
            .map(|p| coord_signed(p.x, p.y))
            .collect();
        let mut players = proto::GamePlayers::new();
        players.players = self
            .players
            .values()
            .map(|p| {
                let mut gp = proto::GamePlayer::new();
                gp.set_name(p.name.clone());
                gp.set_id(p.id);
                if let Some(addr) = p.addr {
                    gp.set_ip_address(addr.ip().to_string());
                    gp.set_port(addr.port() as i32);
                }
                gp.set_role(p.role.to_proto());
                gp.set_score(p.score);
                gp.set_status(p.status.to_proto());
                gp
            })
            .collect();
        gs.players = protobuf::MessageField::some(players);
        gs
    }
}

#[allow(dead_code)]
pub fn min_snake_len_ok(len: i32) -> bool {
    (MIN_SNAKE_LEN as i32..=MAX_SNAKE_LEN as i32).contains(&len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn small_config() -> GameConfig {
        GameConfig {
            width: 10,
            height: 10,
            food_static: 1,
            state_delay_ms: 1000,
        }
    }

    #[test]
    fn snake_len_counts_head_plus_segments() {
        let s = Snake {
            player_id: 1,
            head: Point::new(5, 5),
            head_dir: Direction::Right,
            segments: vec![(Direction::Left, 1)],
        };
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn snake_cells_follow_segments_from_head() {
        let config = small_config();
        let s = Snake {
            player_id: 1,
            head: Point::new(5, 5),
            head_dir: Direction::Right,
            segments: vec![(Direction::Left, 2)],
        };
        let cells = s.cells(&config);
        assert_eq!(cells, vec![Point::new(5, 5), Point::new(4, 5), Point::new(3, 5)]);
    }

    #[test]
    fn advance_moves_head_and_grows_tail_when_eating() {
        let config = small_config();
        let mut s = Snake {
            player_id: 1,
            head: Point::new(5, 5),
            head_dir: Direction::Right,
            segments: vec![(Direction::Left, 1)],
        };
        s.advance(Direction::Right, true, &config);
        assert_eq!(s.head, Point::new(6, 5));
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn advance_keeps_length_when_not_eating() {
        let config = small_config();
        let mut s = Snake {
            player_id: 1,
            head: Point::new(5, 5),
            head_dir: Direction::Right,
            segments: vec![(Direction::Left, 1)],
        };
        s.advance(Direction::Right, false, &config);
        assert_eq!(s.head, Point::new(6, 5));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn head_wraps_around_torus_edges() {
        let config = small_config();
        let mut s = Snake {
            player_id: 1,
            head: Point::new(9, 5),
            head_dir: Direction::Right,
            segments: vec![(Direction::Left, 1)],
        };
        s.advance(Direction::Right, false, &config);
        assert_eq!(s.head, Point::new(0, 5));
    }

    #[test]
    fn snake_round_trips_through_proto() {
        let s = Snake {
            player_id: 7,
            head: Point::new(3, 4),
            head_dir: Direction::Up,
            segments: vec![(Direction::Down, 2), (Direction::Right, 1)],
        };
        let proto = s.to_proto();
        let back = Snake::from_proto(&proto).expect("valid snake");
        assert_eq!(back.player_id, s.player_id);
        assert_eq!(back.head, s.head);
        assert_eq!(back.head_dir, s.head_dir);
        assert_eq!(back.segments, s.segments);
    }

    #[test]
    fn try_spawn_places_a_two_cell_snake_on_empty_field() {
        let game = Game::new(small_config());
        let snake = game.try_spawn().expect("space available");
        assert_eq!(snake.len(), 2);
    }

    #[test]
    fn join_player_inserts_snake_and_refills_food() {
        let mut game = Game::new(small_config());
        let id = game.alloc_player_id().unwrap();
        assert!(game.join_player(id).is_ok());
        assert!(game.snakes.contains_key(&id));
        assert!(!game.foods.is_empty());
    }

    #[test]
    fn step_moves_snake_forward_without_input() {
        let mut game = Game::new(small_config());
        let id = game.alloc_player_id().unwrap();
        game.join_player(id).unwrap();
        let head_before = game.snakes[&id].head;
        let requested = BTreeMap::new();
        let dead = game.step(&requested);
        assert!(dead.is_empty());
        let head_after = game.snakes[&id].head;
        assert_ne!(head_before, head_after);
    }

    #[test]
    fn step_kills_snake_on_self_collision() {
        let config = small_config();
        let mut game = Game::new(config);
        // A snake curled so that turning left immediately bites its own neck.
        let s = Snake {
            player_id: 1,
            head: Point::new(5, 5),
            head_dir: Direction::Right,
            segments: vec![(Direction::Down, 1), (Direction::Left, 1), (Direction::Up, 1)],
        };
        game.snakes.insert(1, s);
        let mut requested = BTreeMap::new();
        requested.insert(1, Direction::Down);
        let dead = game.step(&requested);
        assert_eq!(dead, vec![1]);
    }

    #[test]
    fn snake_survives_many_random_direction_changes_round_trip() {
        use rand::RngExt;
        let config = GameConfig {
            width: 30,
            height: 30,
            food_static: 0,
            state_delay_ms: 1000,
        };
        let mut rng = rand::rng();
        let mut s = Snake {
            player_id: 1,
            head: Point::new(15, 15),
            head_dir: Direction::Right,
            segments: vec![(Direction::Left, 1)],
        };
        let dirs = [Direction::Up, Direction::Down, Direction::Left, Direction::Right];
        for _ in 0..200 {
            let mut d = dirs[rng.random_range(0..4)];
            if d == s.head_dir.opposite() {
                d = s.head_dir;
            }
            let grow = s.len() < 10;
            s.advance(d, grow, &config);
            let proto = s.to_proto();
            let back = Snake::from_proto(&proto)
                .unwrap_or_else(|| panic!("round trip failed for snake {:?} -> {:?}", s, proto));
            assert_eq!(back.head, s.head);
            assert_eq!(back.segments, s.segments);
            assert_eq!(back.cells(&config), s.cells(&config));
        }
    }

    #[test]
    fn step_ignores_reversal_into_neck() {
        let config = small_config();
        let mut game = Game::new(config);
        let s = Snake {
            player_id: 1,
            head: Point::new(5, 5),
            head_dir: Direction::Right,
            segments: vec![(Direction::Left, 1)],
        };
        game.snakes.insert(1, s);
        let mut requested = BTreeMap::new();
        requested.insert(1, Direction::Left); // would reverse into neck
        let dead = game.step(&requested);
        assert!(dead.is_empty());
        assert_eq!(game.snakes[&1].head, Point::new(6, 5)); // continued right
    }
}

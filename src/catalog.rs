//! Каталог найденных игр. Игра определяется парой (имя, endpoint отправителя
//! анонса); одна и та же игра, пришедшая и из локальной сети, и от сервера
//! курса, показывается одной строкой.
//!
//! Анонс обновляет только перечисленные в нём игры: сервер может прислать каталог
//! несколькими пакетами. У каждой записи свой TTL, а отметки источников хранятся
//! отдельно, чтобы интерфейс мог показать, откуда игра была недавно видна.

use std::collections::HashMap;
use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

use crate::wire::{
    Announcement, Config, MAX_PLAYERS, MAX_SNAKES, PlayerInfo, PlayerStatus, valid_game_name,
};

/// Сколько запись живёт без новых анонсов.
pub const ENTRY_TTL: Duration = Duration::from_millis(3000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Lan,
    Server,
}

/// Ключ игры: одноимённые игры разных MASTER не объединяются.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GameKey {
    pub name: String,
    pub addr: SocketAddrV4,
}

/// Последние параметры игры и независимые отметки получения из двух источников.
#[derive(Clone, Debug)]
pub struct Entry {
    pub key: GameKey,
    pub config: Config,
    pub players: Vec<PlayerInfo>,
    pub can_join: bool,
    pub lan_seen: Option<Instant>,
    pub server_seen: Option<Instant>,
    pub updated: Instant,
}

impl Entry {
    pub fn participants(&self) -> usize {
        self.players.len()
    }

    pub fn snakes(&self) -> usize {
        self.players
            .iter()
            .filter(|p| p.status == PlayerStatus::Playing)
            .count()
    }

    pub fn from_lan(&self, now: Instant) -> bool {
        self.lan_seen
            .is_some_and(|t| now.duration_since(t) <= ENTRY_TTL)
    }

    pub fn from_server(&self, now: Instant) -> bool {
        self.server_seen
            .is_some_and(|t| now.duration_since(t) <= ENTRY_TTL)
    }

    /// Можно ли присоединиться наблюдателем.
    pub fn can_watch(&self) -> bool {
        self.participants() < MAX_PLAYERS
    }

    /// Можно ли присоединиться игроком: нужны место для участника и для змейки.
    pub fn can_play(&self) -> bool {
        self.can_join && self.can_watch() && self.snakes() < MAX_SNAKES
    }
}

#[derive(Default)]
pub struct Catalog {
    entries: HashMap<GameKey, Entry>,
}

impl Catalog {
    pub fn new() -> Self {
        Catalog::default()
    }

    /// Учитывает анонсы от `from`. Некорректные игры пропускаются по одной,
    /// не отвергая сообщение целиком.
    pub fn ingest(
        &mut self,
        now: Instant,
        from: SocketAddrV4,
        games: &[Announcement],
        source: Source,
    ) {
        for g in games {
            if !valid_game_name(&g.game_name)
                || !g.config.is_valid()
                || g.players.len() > MAX_PLAYERS
            {
                continue;
            }
            let key = GameKey {
                name: g.game_name.clone(),
                addr: from,
            };
            let entry = self.entries.entry(key.clone()).or_insert_with(|| Entry {
                key,
                config: g.config,
                players: Vec::new(),
                can_join: true,
                lan_seen: None,
                server_seen: None,
                updated: now,
            });
            entry.config = g.config;
            entry.players = g.players.clone();
            entry.can_join = g.can_join;
            entry.updated = now;
            match source {
                Source::Lan => entry.lan_seen = Some(now),
                Source::Server => entry.server_seen = Some(now),
            }
        }
    }

    /// Удаляет только записи, не обновлявшиеся ни из одного источника в течение TTL.
    pub fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|_, e| now.duration_since(e.updated) <= ENTRY_TTL);
    }

    pub fn get(&self, key: &GameKey) -> Option<&Entry> {
        self.entries.get(key)
    }

    /// Записи в стабильном порядке: по имени, затем по адресу.
    pub fn list(&self) -> Vec<&Entry> {
        let mut v: Vec<&Entry> = self.entries.values().collect();
        v.sort_by(|a, b| a.key.cmp(&b.key));
        v
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{NodeRole, PlayerType};
    use std::net::Ipv4Addr;

    fn ann(name: &str, players: usize) -> Announcement {
        Announcement {
            players: (0..players)
                .map(|i| PlayerInfo {
                    name: format!("p{i}"),
                    id: i as i32 + 1,
                    addr: None,
                    role: if i == 0 {
                        NodeRole::Master
                    } else {
                        NodeRole::Normal
                    },
                    ptype: PlayerType::Human,
                    score: 0,
                    status: PlayerStatus::Playing,
                })
                .collect(),
            config: Config::default(),
            can_join: true,
            game_name: name.into(),
        }
    }

    fn a(n: u8) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, n), 9000)
    }

    #[test]
    fn same_game_from_lan_and_server_is_one_entry() {
        let mut c = Catalog::new();
        let t = Instant::now();
        c.ingest(t, a(1), &[ann("g", 2)], Source::Lan);
        c.ingest(t, a(1), &[ann("g", 2)], Source::Server);
        let list = c.list();
        assert_eq!(list.len(), 1);
        assert!(list[0].from_lan(t) && list[0].from_server(t));
    }

    #[test]
    fn entries_expire_and_invalid_games_are_skipped() {
        let mut c = Catalog::new();
        let t = Instant::now();
        let mut bad = ann("", 1);
        bad.game_name.clear();
        c.ingest(t, a(1), &[ann("ok", 1), bad, ann("big", 7)], Source::Lan);
        assert_eq!(c.list().len(), 1);
        c.expire(t + Duration::from_millis(3100));
        assert!(c.list().is_empty());
    }

    #[test]
    fn same_name_from_different_hosts_are_distinct() {
        let mut c = Catalog::new();
        let t = Instant::now();
        c.ingest(t, a(1), &[ann("g", 1)], Source::Lan);
        c.ingest(t, a(2), &[ann("g", 1)], Source::Lan);
        assert_eq!(c.list().len(), 2);
    }

    #[test]
    fn playing_availability_does_not_prevent_spectating() {
        let mut c = Catalog::new();
        let t = Instant::now();
        let mut game = ann("crowded", 1);
        game.can_join = false;
        c.ingest(t, a(1), &[game], Source::Lan);

        let entry = c.list()[0];
        assert!(!entry.can_play());
        assert!(entry.can_watch());
    }

    #[test]
    fn join_availability_follows_limits() {
        let mut c = Catalog::new();
        let t = Instant::now();
        c.ingest(t, a(1), &[ann("full", 6)], Source::Lan);
        c.ingest(t, a(2), &[ann("four", 4)], Source::Lan);
        let list = c.list();
        let by_name = |n: &str| {
            list.iter()
                .find(|e| e.key.name == n)
                .copied()
                .expect("игра есть в каталоге")
        };
        assert!(!by_name("full").can_watch(), "6 участников: мест нет");
        assert!(
            by_name("four").can_watch() && !by_name("four").can_play(),
            "4 змейки: только наблюдать"
        );
    }
}

//! Discovery catalog: merges local multicast announcements with the course
//! server's unicast catalog into one list, expiring stale entries.

use std::collections::HashMap;
use std::net::SocketAddrV4;
use std::time::Instant;

use crate::model::GameConfig;
use crate::proto::GameAnnouncement;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GameId {
    pub game_name: String,
    pub master_addr: SocketAddrV4,
}

#[derive(Debug, Clone)]
pub struct DiscoveredGame {
    pub id: GameId,
    pub config: GameConfig,
    pub can_join: bool,
    pub players: usize,
    pub snakes: usize,
    pub last_seen: Instant,
    #[allow(dead_code)]
    pub from_server: bool,
}

pub struct Catalog {
    entries: HashMap<GameId, DiscoveredGame>,
}

impl Catalog {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn ingest(&mut self, from: SocketAddrV4, announcement: &GameAnnouncement, from_server: bool) {
        let id = GameId {
            game_name: announcement.game_name().to_string(),
            master_addr: from,
        };
        let config = GameConfig::from_proto(&announcement.config);
        let players = announcement.players.players.len();
        let snakes = announcement
            .players
            .players
            .iter()
            .filter(|p| p.status() == crate::proto::PlayerStatus::PLAYING)
            .count();
        self.entries.insert(
            id.clone(),
            DiscoveredGame {
                id,
                config,
                can_join: announcement.can_join(),
                players,
                snakes,
                last_seen: Instant::now(),
                from_server,
            },
        );
    }

    /// Drops entries that stopped being refreshed for `ttl`.
    pub fn expire(&mut self, ttl: std::time::Duration) {
        let now = Instant::now();
        self.entries
            .retain(|_, g| now.duration_since(g.last_seen) <= ttl);
    }

    pub fn list(&self) -> Vec<&DiscoveredGame> {
        let mut v: Vec<&DiscoveredGame> = self.entries.values().collect();
        v.sort_by(|a, b| a.id.game_name.cmp(&b.id.game_name));
        v
    }

    #[allow(dead_code)]
    pub fn get(&self, id: &GameId) -> Option<&DiscoveredGame> {
        self.entries.get(id)
    }

    /// Updates the endpoint of a known game after a confirmed failover,
    /// preserving its other fields.
    #[allow(dead_code)]
    pub fn update_master(&mut self, old: &GameId, new_addr: SocketAddrV4) {
        if let Some(mut g) = self.entries.remove(old) {
            g.id.master_addr = new_addr;
            self.entries.insert(g.id.clone(), g);
        }
    }
}

//! Вспомогательная утилита для сверки кодека с эталонной реализацией protobuf.
//!   codec_io enc          — печатает «имя hex» для набора сообщений
//!   codec_io dec < файл   — читает строки «имя hex» и печатает результат разбора
use snake::wire::*;
use std::io::BufRead;
use std::net::{Ipv4Addr, SocketAddrV4};

fn p(id: i32, role: NodeRole, addr: bool, status: PlayerStatus) -> PlayerInfo {
    PlayerInfo {
        name: format!("игрок{id}"),
        id,
        addr: addr.then(|| SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, id as u8), 30000 + id as u16)),
        role,
        ptype: if id == 9 { PlayerType::Robot } else { PlayerType::Human },
        score: id * 11,
        status,
    }
}

fn msg(seq: i64, body: Body) -> Message {
    Message {
        seq,
        sender_id: Some(1),
        receiver_id: Some(2),
        game_name: Some("партия".into()),
        body,
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "enc" {
        let players = vec![
            p(1, NodeRole::Master, false, PlayerStatus::Playing),
            p(2, NodeRole::Deputy, true, PlayerStatus::Playing),
            p(9, NodeRole::Normal, true, PlayerStatus::Viewing),
        ];
        let state = StateMsg {
            order: 42,
            snakes: vec![
                WireSnake { player_id: 1, points: vec![(5, 1), (3, 0), (0, 2), (-4, 0)], dir: Dir::Left },
                WireSnake { player_id: 2, points: vec![(0, 0), (0, -3)], dir: Dir::Down },
            ],
            foods: vec![(0, 0), (7, -2), (-1, 30)],
            players: players.clone(),
            next_player_id: 10,
        };
        let ann = Announcement {
            players: players.clone(),
            config: Config { width: 20, height: 15, food_static: 2, state_delay_ms: 300 },
            can_join: true,
            game_name: "Практика".into(),
        };
        let all = vec![
            ("state", msg(100, Body::State(state))),
            ("announcement", msg(101, Body::Announcement(vec![ann.clone(), ann]))),
            ("join", msg(102, Body::Join { ptype: PlayerType::Human, player_name: "Маша".into(), game_name: "партия".into(), status: PlayerStatus::Viewing })),
            ("error", msg(103, Body::Error { text: "нет мест".into(), request_seq: 55 })),
            ("rolechange", msg(104, Body::RoleChange { sender: Some(NodeRole::Normal), receiver: Some(NodeRole::Master), order: 7 })),
            ("steer", msg(105, Body::Steer(Dir::Right))),
            ("ping", msg(106, Body::Ping)),
            ("ack", msg(107, Body::Ack)),
            ("status", msg(108, Body::StatusChange(PlayerStatus::Playing))),
            ("leave", msg(109, Body::Leave)),
            ("discover", Message { seq: 110, sender_id: None, receiver_id: None, game_name: None, body: Body::Discover }),
        ];
        for (name, m) in all {
            let bytes = encode(&m).unwrap();
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            println!("{name} {hex}");
        }
    } else {
        for line in std::io::stdin().lock().lines() {
            let line = line.unwrap();
            let Some((name, hex)) = line.split_once(' ') else { continue };
            let bytes: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
            match decode(&bytes) {
                Ok(m) => println!("{name}: OK {m:?}"),
                Err(e) => println!("{name}: ERR {e:?}"),
            }
        }
    }
}

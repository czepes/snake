use std::net::{Ipv4Addr, SocketAddrV4};

use snakes::wire::{
    Body, Dir, MAX_DATAGRAM, Message, NodeRole, PlayerInfo, PlayerStatus, PlayerType, StateMsg,
    WireSnake, decode, encode,
};

#[test]
fn maximum_legal_state_fits_in_one_datagram_and_roundtrips() {
    let first_id = i32::MAX - 6;
    let players = (0..6)
        .map(|i| PlayerInfo {
            name: "И".repeat(12),
            id: first_id + i,
            addr: (i != 0).then_some(SocketAddrV4::new(
                Ipv4Addr::new(223, 223, 223, (220 + i) as u8),
                65535,
            )),
            role: match i {
                0 => NodeRole::Master,
                1 => NodeRole::Deputy,
                _ => NodeRole::Normal,
            },
            ptype: PlayerType::Human,
            score: i32::MAX,
            status: if i < 4 {
                PlayerStatus::Playing
            } else {
                PlayerStatus::Viewing
            },
        })
        .collect();
    let snakes = (0..4)
        .map(|i| {
            let mut points = vec![(65 + i * 9, 70)];
            points.extend((0..31).map(|step| if step % 2 == 0 { (1, 0) } else { (0, 1) }));
            WireSnake {
                player_id: first_id + i,
                points,
                dir: Dir::Left,
            }
        })
        .collect();
    let message = Message {
        seq: i64::MAX,
        sender_id: Some(first_id),
        receiver_id: Some(first_id + 1),
        game_name: Some("Я".repeat(16)),
        body: Body::State(StateMsg {
            order: i32::MAX,
            snakes,
            foods: (64..78).map(|x| (x, 90)).collect(),
            players,
            next_player_id: i32::MAX,
        }),
    };

    let bytes = encode(&message).unwrap();
    assert!(
        bytes.len() <= 1145,
        "{} bytes exceeds the specified bound",
        bytes.len()
    );
    assert!(bytes.len() <= MAX_DATAGRAM);
    assert_eq!(decode(&bytes).unwrap(), message);
}

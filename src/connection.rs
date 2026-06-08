use bytes::Bytes;
use std::{sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::sleep};
use tracing::{debug, error, info};

use crate::game::Game;
use crate::payload;

pub const RELIABLE_CHANNEL_LABEL: &str = "reliable-main";
pub const UNRELIABLE_CHANNEL_LABEL: &str = "unreliable-main";

macro_rules! jsend {
    ( $dc:expr, $msg_variant:ident { $($field:tt)* } ) => {
        // マクロが裏側で「重複する表現」をガッチャンコして組み立ててくれる！
        let msg = payload::JsonMessage::$msg_variant(payload::$msg_variant {
            $($field)*
        });

        let body = msg
            .to_response_body()
            .expect("Failed to build JSON response");
        let binary_resp = payload::wrap_with_opcode(payload::Opcode::JSONResponsePayload, body);

        if let Err(e) = $dc.send(&Bytes::from(binary_resp)).await {
            error!("Failed to send response: {e}");
        }
    };
}

pub async fn handle_reliable_connection(
    dc: Arc<webrtc::data_channel::RTCDataChannel>,
    game: Arc<Mutex<Game>>,
    id: uuid::Uuid,
) -> () {
    let dc_clone = Arc::clone(&dc);

    game.lock()
        .await
        .add_reliable_connection(id, Arc::clone(&dc));

    let game_on_message = Arc::clone(&game);
    dc.on_message(Box::new(move |msg| {
        let dc_clone = Arc::clone(&dc_clone);
        let game = Arc::clone(&game_on_message);

        Box::pin(async move {
            let data = &msg.data;

            if data.is_empty() {
                return;
            }

            if data.len() < 2 {
                return;
            }

            // 最初の1バイトをOpcodeとして取得を試みる
            let op: payload::Opcode = match payload::Opcode::try_from(data[0]) {
                Ok(v) => v,
                Err(_) => {
                    error!("Unknown opcode: {}", data[0]);
                    return;
                }
            };
            let body = &data[1..];
            match op {
                payload::Opcode::PingPayload => {
                    let payload: payload::PingPayload =
                        match wincode::config::deserialize(body, payload::wincode_config()) {
                            Ok(v) => v,
                            Err(e) => {
                                error!("Failed to deserialize PingPayload: {e}");
                                return;
                            }
                        };

                    let resp = payload::PongPayload { id: payload.id };
                    let body = resp
                        .to_binary()
                        .expect("Failed to convert PongPayload to binary");
                    let binary_resp = payload::wrap_with_opcode(payload::Opcode::PongPayload, body);
                    if let Err(e) = dc_clone.send(&Bytes::from(binary_resp)).await {
                        error!("Failed to send PongPayload: {e}");
                    }
                }
                payload::Opcode::PongPayload => {
                    let payload: payload::PongPayload =
                        match wincode::config::deserialize(body, payload::wincode_config()) {
                            Ok(v) => v,
                            Err(e) => {
                                error!("Failed to deserialize PongPayload: {e}");
                                return;
                            }
                        };
                    info!("Received PongPayload: {payload:?}");
                }
                payload::Opcode::ClosePayload => {
                    info!(
                        "Received Close opcode from player [{}]. Closing connection...",
                        id
                    );
                    let mut game = game.lock().await;
                    if let Some(state) = game.get_connection_state(&id) {
                        let mut state = state.lock().unwrap();
                        *state = crate::game::ConnectionState::Disconnected;
                    }
                    game.remove_connection(&id).await;
                }
                payload::Opcode::JSONRequestPayload => {
                    // JSONRequestPayload (bincode/wincode serialized)
                    let payload: payload::JSONRequestPayload =
                        match wincode::config::deserialize(body, payload::wincode_config()) {
                            Ok(v) => v,
                            Err(e) => {
                                error!("Failed to deserialize JSONRequestPayload: {e}");
                                return;
                            }
                        };

                    let s = payload.data;
                    debug!("Received JSONRequestPayload with data: {s}");

                    match serde_json::from_str::<payload::JsonMessage>(&s) {
                        Ok(msg) => match msg {
                            payload::JsonMessage::JSONPing(req) => {
                                jsend!(dc_clone, JSONPong { id: req.id });
                            }
                            payload::JsonMessage::JSONGetRoomsRequest(req) => {
                                let req_id = req.id;
                                let game = game.lock().await;
                                let rooms: Vec<payload::ListRoomInfo> = game
                                    .rooms
                                    .iter()
                                    .filter(|(_, raw_room)| {
                                        raw_room.status == crate::room::RoomStatus::Waiting
                                    })
                                    .map(|(id, raw_room)| payload::ListRoomInfo {
                                        id: *id,
                                        room_name: raw_room.room_name.clone(),
                                        players: raw_room.players.len() as u8,
                                        max_players: raw_room.max_players,
                                        locked: raw_room.password.is_some(),
                                        tags: raw_room.tags.iter().map(|tag| *tag as u32).collect(),
                                    })
                                    .collect();

                                jsend!(dc_clone, JSONGetRoomsResponse { id: req_id, rooms });
                            }
                            payload::JsonMessage::JSONCreateRoomRequest(req) => {
                                let req_id = req.id;
                                let room_name = req.room_name;
                                let max_players = req.max_players;
                                let password = req.password;
                                let tags = req
                                    .tags
                                    .into_iter()
                                    .map(|t| match t {
                                        0 => crate::room::RoomTag::PuyoTet,
                                        1 => crate::room::RoomTag::PuyoOnly,
                                        2 => crate::room::RoomTag::TetOnly,
                                        3 => crate::room::RoomTag::Casual,
                                        4 => crate::room::RoomTag::Competitive,
                                        _ => {
                                            error!("Unknown room tag: {t}");
                                            crate::room::RoomTag::Casual
                                        }
                                    })
                                    .collect();

                                let room_id = game.lock().await.new_room(
                                    id,
                                    room_name,
                                    max_players,
                                    password,
                                    tags,
                                );

                                jsend!(
                                    dc_clone,
                                    JSONCreateRoomResponse {
                                        id: req_id,
                                        room_id
                                    }
                                );
                            }
                            payload::JsonMessage::JSONJoinRoomRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;
                                let password = req.password;
                                let username = req.username;

                                let mut game = game.lock().await;
                                let room = match game.rooms.get_mut(&room_id) {
                                    Some(r) => r,
                                    None => {
                                        jsend!(
                                            dc_clone,
                                            JSONJoinRoomResponse {
                                                id: req_id,
                                                success: false,
                                                message: Some("Room not found".to_string()),
                                            }
                                        );
                                        return;
                                    }
                                };

                                if room.password.is_some() && room.password != password {
                                    jsend!(
                                        dc_clone,
                                        JSONJoinRoomResponse {
                                            id: req_id,
                                            success: false,
                                            message: Some("Incorrect password".to_string()),
                                        }
                                    );
                                    return;
                                }

                                if room.players.len() as u8 >= room.max_players {
                                    jsend!(
                                        dc_clone,
                                        JSONJoinRoomResponse {
                                            id: req_id,
                                            success: false,
                                            message: Some("Room is full".to_string()),
                                        }
                                    );
                                    return;
                                }

                                if room.status != crate::room::RoomStatus::Waiting {
                                    jsend!(
                                        dc_clone,
                                        JSONJoinRoomResponse {
                                            id: req_id,
                                            success: false,
                                            message: Some("Game has already started".to_string()),
                                        }
                                    );
                                    return;
                                }

                                room.players.push((id, username));

                                jsend!(
                                    dc_clone,
                                    JSONJoinRoomResponse {
                                        id: req_id,
                                        success: true,
                                        message: None,
                                    }
                                );
                            }
                            payload::JsonMessage::JSONLeaveRoomRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;

                                let mut game = game.lock().await;
                                let leave_result = game
                                    .rooms
                                    .get_mut(&room_id)
                                    .map(|room| {
                                        room.players.retain(|(pid, _)| *pid != id);
                                        Ok(())
                                    })
                                    .unwrap_or_else(|| Err("Room not found".to_string()));

                                jsend!(
                                    dc_clone,
                                    JSONLeaveRoomResponse {
                                        id: req_id,
                                        success: leave_result.is_ok(),
                                        message: leave_result.err()
                                    }
                                );
                            }
                            payload::JsonMessage::JSONRoomUpdateRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;

                                let mut game = game.lock().await;
                                let room = match game.rooms.get_mut(&room_id) {
                                    Some(r) => r,
                                    None => {
                                        jsend!(
                                            dc_clone,
                                            JSONRoomUpdateResponse {
                                                id: req_id,
                                                success: false,
                                                message: Some("Room not found".to_string()),
                                            }
                                        );
                                        return;
                                    }
                                };

                                if room.owner != id {
                                    jsend!(
                                        dc_clone,
                                        JSONRoomUpdateResponse {
                                            id: req_id,
                                            success: false,
                                            message: Some(
                                                "Only the room owner can update the room"
                                                    .to_string()
                                            ),
                                        }
                                    );
                                    return;
                                }

                                let update_result = (|| {
                                    room.room_name = req.room_name;
                                    if req.max_players < room.players.len() as u8 {
                                        return Err(
                                            "Max players cannot be less than current player count"
                                                .to_string(),
                                        );
                                    }
                                    room.max_players = req.max_players;
                                    if req.password.is_some() {
                                        room.password = req.password;
                                    }
                                    room.tags = req
                                        .tags
                                        .into_iter()
                                        .map(|t| match t {
                                            0 => crate::room::RoomTag::PuyoTet,
                                            1 => crate::room::RoomTag::PuyoOnly,
                                            2 => crate::room::RoomTag::TetOnly,
                                            3 => crate::room::RoomTag::Casual,
                                            4 => crate::room::RoomTag::Competitive,
                                            _ => {
                                                error!("Unknown room tag: {t}");
                                                crate::room::RoomTag::Casual
                                            }
                                        })
                                        .collect();
                                    Ok(())
                                })();

                                let resp = payload::JSONRoomUpdateResponse {
                                    id: req_id,
                                    success: update_result.is_ok(),
                                    message: update_result.err(),
                                };
                                let body = payload::JsonMessage::JSONRoomUpdateResponse(resp)
                                    .to_response_body()
                                    .expect("Failed to build JSON response");
                                let binary_resp = payload::wrap_with_opcode(
                                    payload::Opcode::JSONResponsePayload,
                                    body,
                                );

                                if let Err(e) = dc_clone.send(&Bytes::from(binary_resp)).await {
                                    error!("Failed to send response: {e}");
                                }
                            }
                            other => {
                                info!("Unhandled JSON request: {other:?}");
                            }
                        },
                        Err(e) => {
                            error!("Failed to parse JSON message: {e}");
                        }
                    }
                }
                other => {
                    info!("Received other opcode: {other:?}");
                }
            }
        })
    }));

    dc.on_close(Box::new(move || {
        let game = Arc::clone(&game);
        Box::pin(async move {
            {
                let game = game.lock().await;
                if let Some(state) = game.get_connection_state(&id) {
                    let mut state = state.lock().unwrap();
                    if matches!(*state, crate::game::ConnectionState::Disconnected) {
                        // 正常切断
                        return;
                    }
                    *state = crate::game::ConnectionState::Disconnected;
                    println!(
                        "Disconnected player [{}]. Waiting for 30 seconds before removing their data...",
                        id
                    );
                }
            }

            sleep(Duration::from_secs(30)).await;

            let mut game = game.lock().await;
            if let Some(state) = game.get_connection_state(&id) {
                if matches!(*state.lock().unwrap(), crate::game::ConnectionState::Disconnected) {
                    game.remove_connection(&id).await;
                    println!(
                        "Removed player [{}] data after 30 seconds of disconnection.",
                        id
                    );
                }
            }
        })
    }));
}

pub async fn handle_unreliable_connection(
    dc: Arc<webrtc::data_channel::RTCDataChannel>,
    game: Arc<Mutex<Game>>,
    id: uuid::Uuid,
) -> () {
    let dc_clone = Arc::clone(&dc);

    game.lock()
        .await
        .add_unreliable_connection(id, Arc::clone(&dc));

    let game_on_message = Arc::clone(&game);
    dc.on_message(Box::new(move |msg| {
        let dc_clone = Arc::clone(&dc_clone);
        let game = Arc::clone(&game_on_message);

        Box::pin(async move {
            let data = &msg.data;

            if data.is_empty() {
                return;
            }

            if data.len() < 2 {
                return;
            }

            // 最初の1バイトをOpcodeとして取得を試みる
            let op: payload::Opcode = match payload::Opcode::try_from(data[0]) {
                Ok(v) => v,
                Err(_) => {
                    error!("Unknown opcode: {}", data[0]);
                    return;
                }
            };
            let body = &data[1..];
            match op {
                payload::Opcode::PingPayload => {
                    let payload: payload::PingPayload =
                        match wincode::config::deserialize(body, payload::wincode_config()) {
                            Ok(v) => v,
                            Err(e) => {
                                error!("Failed to deserialize PingPayload: {e}");
                                return;
                            }
                        };

                    let resp = payload::PongPayload { id: payload.id };
                    let body = resp
                        .to_binary()
                        .expect("Failed to convert PongPayload to binary");
                    let binary_resp = payload::wrap_with_opcode(payload::Opcode::PongPayload, body);
                    if let Err(e) = dc_clone.send(&Bytes::from(binary_resp)).await {
                        error!("Failed to send PongPayload: {e}");
                    }
                }
                payload::Opcode::PongPayload => {
                    let payload: payload::PongPayload =
                        match wincode::config::deserialize(body, payload::wincode_config()) {
                            Ok(v) => v,
                            Err(e) => {
                                error!("Failed to deserialize PongPayload: {e}");
                                return;
                            }
                        };
                    info!("Received PongPayload: {payload:?}");
                }
                other => {
                    info!("Received other opcode: {other:?}");
                }
            }
        })
    }));

    dc.on_close(Box::new(move || {
        let game = Arc::clone(&game);
        Box::pin(async move {
            {
                let game = game.lock().await;
                if let Some(state) = game.get_connection_state(&id) {
                    let mut state = state.lock().unwrap();
                    if matches!(*state, crate::game::ConnectionState::Disconnected) {
                        // 正常切断
                        return;
                    }
                    *state = crate::game::ConnectionState::Disconnected;
                    println!(
                        "Disconnected player [{}]. Waiting for 30 seconds before removing their data...",
                        id
                    );
                }
            }

            sleep(Duration::from_secs(30)).await;

            let mut game = game.lock().await;
            if let Some(state) = game.get_connection_state(&id) {
                if matches!(*state.lock().unwrap(), crate::game::ConnectionState::Disconnected) {
                    game.remove_connection(&id).await;
                    println!(
                        "Removed player [{}] data after 30 seconds of disconnection.",
                        id
                    );
                }
            }
        })
    }));
}

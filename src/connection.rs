use bytes::Bytes;
use std::{sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::sleep};
use tracing::{debug, error, info};

use crate::game::Game;
use crate::payload;

pub const RELIABLE_CHANNEL_LABEL: &str = "reliable-main";
pub const UNRELIABLE_CHANNEL_LABEL: &str = "unreliable-main";

pub async fn handle_reliable_connection(
    dc: Arc<webrtc::data_channel::RTCDataChannel>,
    game: Arc<Mutex<Game>>,
) -> () {
    let dc_clone = Arc::clone(&dc);

    let id = uuid::Uuid::new_v4();

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
                    info!("Received Close opcode from player [{}]. Closing connection...", id);
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
                                let resp = payload::JsonMessage::JSONPong(payload::JSONPong {
                                    id: req.id,
                                });
                                let body = resp
                                    .to_response_body()
                                    .expect("Failed to build JSONPong response");
                                let binary_resp = payload::wrap_with_opcode(
                                    payload::Opcode::JSONResponsePayload,
                                    body,
                                );
                                if let Err(e) = dc_clone.send(&Bytes::from(binary_resp)).await {
                                    error!("Failed to send JSONPong response: {e}");
                                }
                            }
                            payload::JsonMessage::JSONGetRoomsRequest(req) => {
                                let req_id = req.id;
                                let rooms: Vec<crate::room::Room> = game.lock().await.rooms.clone();
                                let response_rooms: Vec<payload::ListRoomInfo> = rooms
                                    .into_iter()
                                    .filter(|room| room.status == crate::room::RoomStatus::Waiting)
                                    .map(|room| payload::ListRoomInfo {
                                        id: room.id,
                                        room_name: room.room_name,
                                        players: room.players.len() as u8,
                                        max_players: room.max_players,
                                        locked: room.password.is_some(),
                                        tags: room.tags.into_iter().map(|tag| tag as u32).collect(),
                                    })
                                    .collect();

                                let resp = payload::JSONGetRoomsResponse {
                                    id: req_id,
                                    rooms: response_rooms,
                                };
                                let body = payload::JsonMessage::JSONGetRoomsResponse(resp)
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
) -> () {
    let dc_clone = Arc::clone(&dc);

    let id = uuid::Uuid::new_v4();

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

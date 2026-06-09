use bytes::Bytes;
use std::{sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::sleep};
use tracing::{debug, error, info};
use uuid::Uuid;

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

/// 指定ルームに居る全プレイヤーの reliable チャンネルへ RoomInfoNotification をプッシュする。
/// 参加・退出・マッチ成立でルーム構成が変わったときに各クライアントへ伝えるための関数。
/// 注意: 内部で game ロックを取得するため、呼び出し側はロックを保持していないこと。
async fn notify_room(game: &Arc<Mutex<Game>>, room_id: Uuid) {
    let (body, dcs): (Vec<u8>, Vec<Arc<webrtc::data_channel::RTCDataChannel>>) = {
        let game = game.lock().await;
        let Some(room) = game.rooms.get(&room_id) else {
            return;
        };
        let notif = payload::JSONRoomInfoNotification {
            room_id,
            room_name: room.room_name.clone(),
            players: room.players.clone(),
            max_players: room.max_players,
            tags: room.tags.iter().map(|t| *t as u32).collect(),
        };
        let body = payload::JsonMessage::JSONRoomInfoNotification(notif)
            .to_response_body()
            .expect("Failed to build RoomInfoNotification");
        let dcs = room
            .players
            .iter()
            .filter_map(|(pid, _, _)| game.get_reliable_connection(pid))
            .collect();
        (body, dcs)
    };

    for dc in dcs {
        let binary = payload::wrap_with_opcode(payload::Opcode::JSONResponsePayload, body.clone());
        if let Err(e) = dc.send(&Bytes::from(binary)).await {
            error!("Failed to send RoomInfoNotification: {e}");
        }
    }
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
                                let username = req.username;
                                let rule = req.rule;
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

                                // 作成者は new_room の中で自動的に最初のプレイヤーとして参加する
                                let (room_id, code) = game.lock().await.new_room(
                                    id,
                                    room_name,
                                    max_players,
                                    password,
                                    tags,
                                    username,
                                    rule,
                                );

                                jsend!(
                                    dc_clone,
                                    JSONCreateRoomResponse {
                                        id: req_id,
                                        room_id,
                                        code
                                    }
                                );
                            }

                            payload::JsonMessage::JSONJoinRoomRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;
                                let password = req.password;
                                let username = req.username;
                                let rule = req.rule;

                                // ロックを取得して参加処理。パスワード検証はここで行い、
                                // 満員/開始済み/存在チェックは add_player_to_room に委譲する。
                                let result = {
                                    let mut g = game.lock().await;
                                    let pw_ok = g
                                        .rooms
                                        .get(&room_id)
                                        .map(|room| {
                                            room.password.is_none() || room.password == password
                                        });
                                    match pw_ok {
                                        None => Err("Room not found".to_string()),
                                        Some(false) => Err("Incorrect password".to_string()),
                                        Some(true) => g.add_player_to_room(room_id, id, username, rule),
                                    }
                                };

                                match &result {
                                    Ok(()) => {
                                        jsend!(
                                            dc_clone,
                                            JSONJoinRoomResponse {
                                                id: req_id,
                                                success: true,
                                                message: None,
                                            }
                                        );
                                    }
                                    Err(msg) => {
                                        jsend!(
                                            dc_clone,
                                            JSONJoinRoomResponse {
                                                id: req_id,
                                                success: false,
                                                message: Some(msg.clone()),
                                            }
                                        );
                                    }
                                }

                                if result.is_ok() {
                                    notify_room(&game, room_id).await;
                                }
                            }
                            payload::JsonMessage::JSONJoinByCodeRequest(req) => {
                                let req_id = req.id;
                                let code = req.code;
                                let password = req.password;
                                let username = req.username;
                                let rule = req.rule;

                                let (result, joined_room) = {
                                    let mut g = game.lock().await;
                                    match g.find_room_by_code(&code) {
                                        None => (Err("Room not found".to_string()), None),
                                        Some(room_id) => {
                                            let pw_ok = g
                                                .rooms
                                                .get(&room_id)
                                                .map(|room| {
                                                    room.password.is_none()
                                                        || room.password == password
                                                })
                                                .unwrap_or(false);
                                            if !pw_ok {
                                                (
                                                    Err("Incorrect password".to_string()),
                                                    Some(room_id),
                                                )
                                            } else {
                                                (
                                                    g.add_player_to_room(room_id, id, username, rule),
                                                    Some(room_id),
                                                )
                                            }
                                        }
                                    }
                                };

                                match &result {
                                    Ok(()) => {
                                        jsend!(
                                            dc_clone,
                                            JSONJoinByCodeResponse {
                                                id: req_id,
                                                success: true,
                                                message: None,
                                                room_id: joined_room,
                                            }
                                        );
                                    }
                                    Err(msg) => {
                                        jsend!(
                                            dc_clone,
                                            JSONJoinByCodeResponse {
                                                id: req_id,
                                                success: false,
                                                message: Some(msg.clone()),
                                                room_id: None,
                                            }
                                        );
                                    }
                                }

                                if result.is_ok() {
                                    if let Some(room_id) = joined_room {
                                        notify_room(&game, room_id).await;
                                    }
                                }
                            }
                            payload::JsonMessage::JSONJoinRandomMatchRequest(req) => {
                                let req_id = req.id;
                                let username = req.username;
                                let rule = req.rule;

                                let outcome = game.lock().await.random_match(id, username, rule);
                                match outcome {
                                    crate::game::MatchResult::Matched { room_id, .. } => {
                                        jsend!(
                                            dc_clone,
                                            JSONJoinRandomMatchResponse {
                                                id: req_id,
                                                matched: true,
                                                room_id: Some(room_id),
                                            }
                                        );
                                        // マッチした両者へ RoomInfoNotification を配る
                                        // （待機していた側はこれで相手の参加を知る）
                                        notify_room(&game, room_id).await;
                                    }
                                    crate::game::MatchResult::Waiting => {
                                        jsend!(
                                            dc_clone,
                                            JSONJoinRandomMatchResponse {
                                                id: req_id,
                                                matched: false,
                                                room_id: None,
                                            }
                                        );
                                    }
                                }
                            }
                            payload::JsonMessage::JSONCancelRandomMatchRequest(req) => {
                                let req_id = req.id;
                                let success = game.lock().await.cancel_random_match(&id);
                                jsend!(
                                    dc_clone,
                                    JSONCancelRandomMatchResponse {
                                        id: req_id,
                                        success
                                    }
                                );

                                let game = game.lock().await;

                                for (pid, _) in &room.players {
                                    if *pid != id {
                                        if let Some(dc) = game.get_reliable_connection(pid) {
                                            jsend!(
                                                dc,
                                                JSONRoomInfoNotification {
                                                    room_id,
                                                    room_name: room.room_name.clone(),
                                                    players: room.players.clone(),
                                                    max_players: room.max_players,
                                                    tags: room
                                                        .tags
                                                        .iter()
                                                        .map(|tag| *tag as u32)
                                                        .collect(),
                                                }
                                            );
                                        }
                                    }
                                }
                            }

                            payload::JsonMessage::JSONLeaveRoomRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;

                                let leave_result: Result<(), String> = {
                                    let mut g = game.lock().await;
                                    if g.rooms.contains_key(&room_id) {
                                        // players からの除去と経路表の削除をまとめて行う
                                        g.leave_room(&id);
                                        Ok(())
                                    } else {
                                        Err("Room not found".to_string())
                                    }
                                };

                                jsend!(
                                    dc_clone,
                                    JSONLeaveRoomResponse {
                                        id: req_id,
                                        success: leave_result.is_ok(),
                                        message: leave_result.clone().err()
                                    }
                                );

                                if leave_result.is_ok() {
                                    // 残ったプレイヤーへ更新を通知（無人で部屋が消えていれば何もしない）
                                    notify_room(&game, room_id).await;
                                }
                            }

                            payload::JsonMessage::JSONRoomUpdateRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;

                                let mut game_mut = game.lock().await;
                                let room = match game_mut.rooms.get_mut(&room_id) {
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

                                jsend!(
                                    dc_clone,
                                    JSONRoomUpdateResponse {
                                        id: req_id,
                                        success: update_result.is_ok(),
                                        message: update_result.err()
                                    }
                                );

                                let game = game.lock().await;

                                for (pid, _) in &room.players {
                                    if *pid != id {
                                        if let Some(dc) = game.get_reliable_connection(pid) {
                                            jsend!(
                                                dc,
                                                JSONRoomInfoNotification {
                                                    room_id,
                                                    room_name: room.room_name.clone(),
                                                    players: room.players.clone(),
                                                    max_players: room.max_players,
                                                    tags: room
                                                        .tags
                                                        .iter()
                                                        .map(|tag| *tag as u32)
                                                        .collect(),
                                                }
                                            );
                                        }
                                    }
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
                payload::Opcode::GameEventPayload => {
                    // 中身は解釈せず、相手の reliable チャンネルへフレームを素通し中継する。
                    let opponent_dc = game.lock().await.get_opponent_channel(&id, true);
                    match opponent_dc {
                        Some(dc) => {
                            if let Err(e) = dc.send(&msg.data).await {
                                error!("Failed to relay GameEvent to opponent: {e}");
                            }
                        }
                        None => debug!("GameEvent received but no opponent to relay to (player [{id}])"),
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
                payload::Opcode::PieceStatePayload => {
                    // 中身は解釈せず、相手の unreliable チャンネルへフレームを素通し中継する。
                    // 高頻度(30〜60Hz)・最新優先・欠落OK のホットパス。
                    let opponent_dc = game.lock().await.get_opponent_channel(&id, false);
                    if let Some(dc) = opponent_dc {
                        if let Err(e) = dc.send(&msg.data).await {
                            error!("Failed to relay PieceState to opponent: {e}");
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

use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

use crate::connection::common::{
    broadcast_json, disconnect_player, notify_room, relay_match_frame,
};
use crate::game::{Game, MatchResult, WinnerStatus};
use crate::payload;

/// JSONのPayloadをレスポンスとして楽に返すためのマクロ．
macro_rules! jsend {
    ( $dc:expr, $msg_variant:ident { $($field:tt)* } ) => {
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
    game: Arc<RwLock<Game>>,
    id: uuid::Uuid,
) -> () {
    // ★ Weak で保持する。on_message が Arc<DC> をキャプチャすると DC が自分自身を参照する
    //   サイクルになり、close 後も Drop されず FD がリークするため（毎回 upgrade して使う）。
    let dc_weak = Arc::downgrade(&dc);

    game.write()
        .await
        .add_reliable_connection(id, Arc::clone(&dc));

    let game_on_message = Arc::clone(&game);
    dc.on_message(Box::new(move |msg| {
        let dc_weak = dc_weak.clone();
        let game = Arc::clone(&game_on_message);

        Box::pin(async move {
            let Some(dc_clone) = dc_weak.upgrade() else { return };
            let data = &msg.data;

            if data.is_empty() {
                return;
            }

            if data.len() < 2 {
                return;
            }

            // 0x2N/0x3N: 対戦中ゲームデータ。送信元UUID付与して同室全員へ中継（サーバーは内容を解釈しない）。
            if data[0] >= 0x20 && data[0] <= 0x3F {
                let relayed = relay_match_frame(&msg.data, &id);
                let peer_dcs = game.read().await.get_room_peer_channels(&id, true);
                for dc in peer_dcs {
                    if let Err(e) = dc.send(&relayed).await {
                        error!("Failed to relay match frame 0x{:02X}: {e}", data[0]);
                    }
                }
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

                    let mut game = game.write().await;
                    if let Some(state) = game.get_connection_state(&id) {
                        let mut state = match state.lock() {
                            Ok(s) => s,
                            Err(e) => {
                                error!("Failed to lock connection state for player {}: {:?}", id, e);
                                return;
                            }
                        };
                        *state = crate::game::ConnectionState::Disconnected;
                    }
                    game.remove_connection(&id);
                }
                payload::Opcode::JSONRequestPayload => {
                    // MARK: JSONRequestPayload
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
                                let game = game.read().await;
                                let rooms: Vec<payload::ListRoomInfo> = game
                                    .rooms
                                    .iter()
                                    .filter(|(_, raw_room)| {
                                        raw_room.status == crate::room::RoomStatus::Waiting
                                            && raw_room.is_public
                                    })
                                    .map(|(id, raw_room)| payload::ListRoomInfo {
                                        id: *id,
                                        room_name: raw_room.room_name.clone(),
                                        players: raw_room.players.len() as u8,
                                        max_players: raw_room.max_players,
                                        locked: false,
                                        tags: raw_room.tags.iter().map(|tag| *tag as u32).collect(),
                                    })
                                    .collect();

                                jsend!(dc_clone, JSONGetRoomsResponse { id: req_id, rooms });
                            }

                            payload::JsonMessage::JSONCreateRoomRequest(req) => {
                                let req_id = req.id;
                                let room_name = req.room_name;
                                let max_players = req.max_players;
                                let is_public = req.is_public;
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
                                let (room_id, code) = game.write().await.new_room(
                                    id,
                                    room_name,
                                    max_players,
                                    is_public,
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
                                let username = req.username;
                                let rule = req.rule;

                                // 公開ルームのみ一覧から参加可能。非公開ルームはコード参加のみ。
                                let result = {
                                    let mut g = game.write().await;
                                    let is_public = g.rooms.get(&room_id).map(|room| room.is_public);
                                    match is_public {
                                        None => Err("Room not found".to_string()),
                                        Some(false) => Err("This room is private. Use room code to join.".to_string()),
                                        Some(true) => {
                                            g.add_player_to_room(room_id, id, username, rule)
                                        }
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
                                let username = req.username;
                                let rule = req.rule;

                                let (result, joined_room) = {
                                    let mut g = game.write().await;
                                    match g.find_room_by_code(&code) {
                                        None => (Err("Room not found".to_string()), None),
                                        Some(room_id) => {
                                            (
                                                g.add_player_to_room(
                                                    room_id, id, username, rule,
                                                ),
                                                Some(room_id),
                                            )
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

                                let outcome = game.write().await.random_match(id, username, rule);
                                match outcome {
                                    MatchResult::Matched { room_id, .. } => {
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
                                    MatchResult::Waiting => {
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
                                let success = game.write().await.cancel_random_match(&id);
                                jsend!(
                                    dc_clone,
                                    JSONCancelRandomMatchResponse {
                                        id: req_id,
                                        success
                                    }
                                );
                            }

                            payload::JsonMessage::JSONLeaveRoomRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;

                                let leave_result: Result<(), String> = {
                                    let mut g = game.write().await;
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

                            payload::JsonMessage::JSONUpdateRoomRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;

                                let update_result = {
                                    let mut game_mut = game.write().await;
                                    let room = match game_mut.rooms.get_mut(&room_id) {
                                        Some(r) => r,
                                        None => {
                                            jsend!(
                                                dc_clone,
                                                JSONUpdateRoomResponse {
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
                                            JSONUpdateRoomResponse {
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
                                        room.is_public = req.is_public;
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
                                        JSONUpdateRoomResponse {
                                            id: req_id,
                                            success: update_result.is_ok(),
                                            message: update_result.clone().err()
                                        }
                                    );

                                    update_result.is_ok()
                                };

                                if update_result {
                                    notify_room(&game, room_id).await;
                                }
                            }

                            payload::JsonMessage::JSONRoomInfoNotificationRequest(_) => {
                                let game = game.read().await;
                                let room_id_opt = game.room_of(&id);
                                match room_id_opt {
                                    Some(room_id) => {
                                        let room = match game.rooms.get(&room_id) {
                                            Some(r) => r,
                                            None => {
                                                error!("Room not found for RoomInfoNotificationRequest: {room_id}");
                                                return;
                                            }
                                        };
                                        let owner_id = room.owner;
                                        let notif = payload::JSONRoomInfoNotification {
                                            room_id,
                                            owner_id,
                                            room_name: room.room_name.clone(),
                                            players: room.players.clone(),
                                            max_players: room.max_players,
                                            tags: room.tags.iter().map(|t| *t as u32).collect(),
                                            match_setting: room.match_setting.clone(),
                                            ready_players: room.ready_players.clone(),
                                            pings: room.pings.iter().map(|(pid, ms)| (*pid, *ms)).collect(),
                                            code: room.code.clone(),
                                            is_public: room.is_public,
                                        };
                                        jsend!(dc_clone, JSONRoomInfoNotification { ..notif });
                                    }
                                    None => {
                                        error!("Player [{}] requested RoomInfoNotification but is not in any room", id);
                                    }
                                }
                            }

                            // ── マッチ制御 ─────────────────────────────────────────────────

                            payload::JsonMessage::JSONStartMatchRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;

                                let is_owner = {
                                    let g = game.read().await;
                                    g.rooms.get(&room_id).map(|r| r.owner == id).unwrap_or(false)
                                };
                                if !is_owner {
                                    jsend!(dc_clone, JSONStartMatchResponse {
                                        id: req_id,
                                        success: false,
                                        message: Some("Only the room owner can start the match".to_string())
                                    });
                                    return;
                                }

                                let result = game.write().await.start_match(room_id);
                                match result {
                                    Err(msg) => {
                                        jsend!(dc_clone, JSONStartMatchResponse {
                                            id: req_id,
                                            success: false,
                                            message: Some(msg)
                                        });
                                    }
                                    Ok((seed, match_setting)) => {
                                        jsend!(dc_clone, JSONStartMatchResponse {
                                            id: req_id,
                                            success: true,
                                            message: None
                                        });
                                        let start_time_ms = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_millis() as u64
                                            + 3000;
                                        broadcast_json(
                                            &game,
                                            room_id,
                                            &payload::JsonMessage::JSONStartMatchNotification(
                                                payload::JSONStartMatchNotification {
                                                    room_id,
                                                    seed: seed.to_vec(),
                                                    start_time_ms,
                                                    match_setting,
                                                },
                                            ),
                                        )
                                        .await;
                                    }
                                }
                            }

                            payload::JsonMessage::JSONUpdateMatchSettingRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;
                                let setting = req.setting;

                                let result = {
                                    let mut g = game.write().await;
                                    match g.rooms.get_mut(&room_id) {
                                        None => Err("Room not found".to_string()),
                                        Some(r) => {
                                            if r.owner != id {
                                                Err("Only the room owner can change match settings".to_string())
                                            } else {
                                                r.match_setting = setting.clone();
                                                Ok(())
                                            }
                                        }
                                    }
                                };

                                jsend!(dc_clone, JSONUpdateMatchSettingResponse {
                                    id: req_id,
                                    success: result.is_ok(),
                                    message: result.clone().err()
                                });

                                if result.is_ok() {
                                    broadcast_json(
                                        &game,
                                        room_id,
                                        &payload::JsonMessage::JSONUpdateMatchSettingNotification(
                                            payload::JSONUpdateMatchSettingNotification {
                                                room_id,
                                                setting,
                                            },
                                        ),
                                    )
                                    .await;
                                    // ルームページは RoomInfoNotification でのみ再描画されるため、
                                    // 設定変更後の状態を全員に配って画面を更新させる。
                                    notify_room(&game, room_id).await;
                                }
                            }

                            payload::JsonMessage::JSONUpdatePlayerRuleRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;
                                let rule = req.rule;

                                let result = game.write().await.update_player_rule(&id, rule);

                                jsend!(dc_clone, JSONUpdatePlayerRuleResponse {
                                    id: req_id,
                                    success: result.is_ok(),
                                    message: result.clone().err()
                                });

                                if result.is_ok() {
                                    notify_room(&game, room_id).await;
                                }
                            }

                            payload::JsonMessage::JSONSetReadyRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;
                                let ready = req.ready;

                                let result = game.write().await.set_ready(&id, ready);

                                jsend!(dc_clone, JSONSetReadyResponse {
                                    id: req_id,
                                    success: result.is_ok(),
                                    message: result.clone().err()
                                });

                                if result.is_ok() {
                                    notify_room(&game, room_id).await;
                                }
                            }

                            payload::JsonMessage::JSONTimeSyncRequest(req) => {
                                let server_time_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis() as u64;
                                jsend!(dc_clone, JSONTimeSyncResponse {
                                    id: req.id,
                                    server_time_ms
                                });
                            }

                            payload::JsonMessage::JSONUpdatePlayerPingRequest(req) => {
                                let room_id = req.room_id;
                                game.write().await.set_ping(&id, req.ping_ms);
                                // 応答不要（fire-and-forget）。全員の表示を更新する。
                                notify_room(&game, room_id).await;
                            }

                            payload::JsonMessage::JSONPauseRequest(req) => {
                                let room_id = req.room_id;
                                broadcast_json(
                                    &game,
                                    room_id,
                                    &payload::JsonMessage::JSONPauseNotification(
                                        payload::JSONPauseNotification {
                                            room_id,
                                            paused_by: id,
                                        },
                                    ),
                                )
                                .await;
                            }

                            payload::JsonMessage::JSONResumeRequest(req) => {
                                let room_id = req.room_id;
                                broadcast_json(
                                    &game,
                                    room_id,
                                    &payload::JsonMessage::JSONResumeNotification(
                                        payload::JSONResumeNotification { room_id },
                                    ),
                                )
                                .await;
                            }

                            payload::JsonMessage::JSONNotifyGameOverRequest(req) => {
                                let req_id = req.id;
                                let room_id = req.room_id;

                                let winner_status = game.write().await.record_game_over(&id);

                                jsend!(dc_clone, JSONNotifyGameOverResponse {
                                    id: req_id,
                                    success: true
                                });

                                let winner_opt = match winner_status {
                                    WinnerStatus::Winner(w) => Some(Some(w)),
                                    WinnerStatus::Draw => Some(None),
                                    WinnerStatus::MatchContinues => None,
                                };
                                if let Some(winner) = winner_opt {
                                    broadcast_json(
                                        &game,
                                        room_id,
                                        &payload::JsonMessage::JSONWinnerNotification(
                                            payload::JSONWinnerNotification { room_id, winner },
                                        ),
                                    )
                                    .await;
                                }
                            }

                            payload::JsonMessage::JSONPostMatchActionRequest(req) => {
                                let room_id = req.room_id;
                                let action = req.action;
                                broadcast_json(
                                    &game,
                                    room_id,
                                    &payload::JsonMessage::JSONPostMatchActionNotification(
                                        payload::JSONPostMatchActionNotification {
                                            player_id: id,
                                            room_id,
                                            action,
                                        },
                                    ),
                                )
                                .await;
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
            disconnect_player(game, id, None).await;
        })
    }));
}

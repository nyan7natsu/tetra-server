use bytes::Bytes;
use std::{sync::Arc, time::Duration};
use tokio::{sync::RwLock, time::sleep};
use tracing::{error, warn};
use uuid::Uuid;

use crate::game::{Game, WinnerStatus};
use crate::payload;

pub const RELIABLE_CHANNEL_LABEL: &str = "reliable-main";
pub const UNRELIABLE_CHANNEL_LABEL: &str = "unreliable-main";

/// 実切断確定後、プレイヤーを除去するまでの再接続猶予(秒)。クライアントは ICE failed 検知後
/// この秒数以内に「同一 player_id を載せた再接続offer」を送れば同一マッチに復帰できる。
/// 長くすると復帰しやすくなるが「相手が落ちた→勝ち」確定が遅くなるトレードオフ。
pub const RECONNECT_GRACE_SECS: u64 = 8;

/// 0x2N 中継フレームを構築する。送信元UUID(16バイト)をオペコードの直後に挿入する。
pub fn relay_match_frame(data: &Bytes, sender_id: &uuid::Uuid) -> Bytes {
    let mut out = Vec::with_capacity(1 + 16 + data.len().saturating_sub(1));
    out.push(data[0]); // opcode
    out.extend_from_slice(sender_id.as_bytes()); // 16-byte UUID
    if data.len() > 1 {
        out.extend_from_slice(&data[1..]); // original payload
    }
    Bytes::from(out)
}

/// 指定ルームの全メンバーへ JSON メッセージをブロードキャストする。
pub async fn broadcast_json(
    game: &Arc<RwLock<Game>>,
    room_id: uuid::Uuid,
    msg: &payload::JsonMessage,
) {
    let body = match msg.to_response_body() {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to build broadcast JSON: {e:?}");
            return;
        }
    };
    let binary = payload::wrap_with_opcode(payload::Opcode::JSONResponsePayload, body);
    let dcs = game.read().await.get_room_reliable_channels(&room_id);
    for dc in dcs {
        if let Err(e) = dc.send(&Bytes::from(binary.clone())).await {
            error!("Failed to broadcast JSON to room {room_id}: {e}");
        }
    }
}

/// 指定ルームに居る全プレイヤーの reliable チャンネルへ RoomInfoNotification をプッシュする。
/// 参加・退出・マッチ成立でルーム構成が変わったときに各クライアントへ伝えるための関数。
/// 注意: 内部で game ロックを取得するため、呼び出し側はロックを保持していないこと。
pub async fn notify_room(game: &Arc<RwLock<Game>>, room_id: Uuid) {
    let (body, dcs): (Vec<u8>, Vec<Arc<webrtc::data_channel::RTCDataChannel>>) = {
        let game = game.read().await;
        let Some(room) = game.rooms.get(&room_id) else {
            return;
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
        let body = payload::JsonMessage::JSONRoomInfoNotification(notif)
            .to_response_body()
            .expect("Failed to build RoomInfoNotification");
        let dcs: Vec<Arc<webrtc::data_channel::RTCDataChannel>> = room
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

/// プレイヤー切断時の共通処理。PeerConnection が Failed/Closed になったとき、または
/// DataChannel の on_close から呼ぶ。シグナリングWSは確立後に閉じる設計なので、WS切断では呼ばない。
///
/// 既に切断猶予中なら即 return（二重起動防止）。3秒の猶予後にまだ切断状態（=再接続して
/// いない）なら実切断としてプレイヤーを除去＋通知する。`pc_weak` があれば PeerConnection を
/// close して ICE ソケット等のFDを解放する。
/// ※ ICE は failed_timeout(8s) を過ぎて初めて Failed になる＝既に実切断が確定しているので、
///   ここでの猶予は「再接続待ち」のための短いもの。
pub async fn disconnect_player(
    game: Arc<RwLock<Game>>,
    id: uuid::Uuid,
    pc_weak: Option<std::sync::Weak<webrtc::peer_connection::RTCPeerConnection>>,
) {
    {
        let game = game.write().await;
        if let Some(state) = game.get_connection_state(&id) {
            let mut state = match state.lock() {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to lock connection state for player {}: {:?}", id, e);
                    return;
                }
            };
            if matches!(*state, crate::game::ConnectionState::Disconnected) {
                // 既に切断猶予中
                return;
            }
            *state = crate::game::ConnectionState::Disconnected;
            warn!(
                "Connection lost for player [{}]. Removing in {RECONNECT_GRACE_SECS}s if not recovered...",
                id
            );
        } else {
            return;
        }
    }

    // 実切断確定 → PeerConnection を閉じて FD を解放（ICE ソケット等）
    if let Some(weak) = pc_weak {
        if let Some(pc) = weak.upgrade() {
            let _ = pc.close().await;
        }
    }

    // 再接続猶予: この間にクライアントが「同一 player_id を載せた再接続offer」を送れば、
    // main.rs の Offer ハンドラが state を Establishing に戻し、ここでの除去を回避する。
    sleep(Duration::from_secs(RECONNECT_GRACE_SECS)).await;

    // 猶予後もまだ切断状態（=再接続で Connected に戻っていない）なら本当に除去する
    let (should_remove, room_id) = {
        let game = game.read().await;
        let still_disconnected = game
            .get_connection_state(&id)
            .map(|s| {
                let s = match s.lock() {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Failed to lock connection state for player {}: {:?}", id, e);
                        return false;
                    }
                };
                matches!(*s, crate::game::ConnectionState::Disconnected)
            })
            .unwrap_or(false);
        (still_disconnected, game.room_of(&id))
    };
    if should_remove {
        let winner_info: Option<(uuid::Uuid, WinnerStatus)> = if let Some(rid) = room_id {
            let was_playing = game
                .read()
                .await
                .rooms
                .get(&rid)
                .map(|r| r.status == crate::room::RoomStatus::Playing)
                .unwrap_or(false);
            if was_playing {
                let ws = game.write().await.record_game_over(&id);
                Some((rid, ws))
            } else {
                None
            }
        } else {
            None
        };

        game.write().await.remove_connection(&id);
        warn!("Removed player [{id}] data after 5 seconds of disconnection.");

        if let Some(room_id) = room_id {
            notify_room(&game, room_id).await;
        }

        if let Some((rid, ws)) = winner_info {
            broadcast_json(
                &game,
                rid,
                &payload::JsonMessage::JSONPlayerDisconnectedNotification(
                    payload::JSONPlayerDisconnectedNotification {
                        room_id: rid,
                        player_id: id,
                    },
                ),
            )
            .await;

            let winner_opt = match ws {
                WinnerStatus::Winner(w) => Some(Some(w)),
                WinnerStatus::Draw => Some(None),
                WinnerStatus::MatchContinues => None,
            };
            if let Some(winner) = winner_opt {
                broadcast_json(
                    &game,
                    rid,
                    &payload::JsonMessage::JSONWinnerNotification(
                        payload::JSONWinnerNotification {
                            room_id: rid,
                            winner,
                        },
                    ),
                )
                .await;
            }
        }
    }
}

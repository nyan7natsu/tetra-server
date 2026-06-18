use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::connection::common::{disconnect_player, relay_match_frame};
use crate::game::Game;
use crate::payload;

pub async fn handle_unreliable_connection(
    dc: Arc<webrtc::data_channel::RTCDataChannel>,
    game: Arc<RwLock<Game>>,
    id: uuid::Uuid,
) -> () {
    // ★ Weak で保持（reliable 側と同じくサイクル＝FDリーク防止）
    let dc_weak = Arc::downgrade(&dc);

    game.write()
        .await
        .add_unreliable_connection(id, Arc::clone(&dc));

    let game_on_message = Arc::clone(&game);
    dc.on_message(Box::new(move |msg| {
        let dc_weak = dc_weak.clone();
        let game = Arc::clone(&game_on_message);

        Box::pin(async move {
            let Some(dc_clone) = dc_weak.upgrade() else {
                return;
            };
            let data = &msg.data;

            if data.is_empty() {
                return;
            }

            if data.len() < 2 {
                return;
            }

            // 0x2N/0x3N: 対戦中ゲームデータ（PieceState 高頻度）。送信元UUID付与して同室全員へ中継。
            if data[0] >= 0x20 && data[0] <= 0x3F {
                let relayed = relay_match_frame(&msg.data, &id);
                let peer_dcs = game.read().await.get_room_peer_channels(&id, false);
                for dc in peer_dcs {
                    if let Err(e) = dc.send(&relayed).await {
                        error!(
                            "Failed to relay unreliable match frame 0x{:02X}: {e}",
                            data[0]
                        );
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
                other => {
                    info!("Received other opcode (unreliable): {other:?}");
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

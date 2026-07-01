use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use reqwest::{
    Url,
    header::{self, HeaderMap, HeaderValue},
};
use std::env;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::Instrument;
use tracing::{debug, error, info, info_span, warn};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use crate::connection::{self, handle_reliable_connection, handle_unreliable_connection};
use crate::signaling;
use crate::state::AppState;

macro_rules! nest {
    ($($n:ident),+ $(,)?) => {
        $(let $n = std::sync::Arc::clone(&$n);)+
    };
}

/// WebSocketをアップグレードするエンドポイント
pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

/// WebSocketのハンドラの実態
pub async fn handle_socket(socket: WebSocket, state: AppState) {
    let (ws_sender, mut ws_receiver) = socket.split();
    let ws_sender = Arc::new(Mutex::new(ws_sender));

    let peer_connection = Arc::new(
        state
            .api
            .new_peer_connection(state.config)
            .await
            .expect("Failed to create peer connection"),
    );

    // この接続（=1本の PeerConnection）に属する player_id。
    // 初期は新規発番だが、再接続offer(player_id付き)を受けると既存プレイヤーのIDへ
    // 差し替える（再バインド）。各ハンドラは発火時に共有値を読むため、Mutex で包む。
    let player_id_shared = Arc::new(Mutex::new(uuid::Uuid::new_v4()));

    let ws_sender_for_ice = Arc::clone(&ws_sender);
    let player_id_for_ice = Arc::clone(&player_id_shared);
    peer_connection.on_ice_candidate(Box::new(move |candidate| {
        let ws_sender = Arc::clone(&ws_sender_for_ice);
        let player_id_for_ice = Arc::clone(&player_id_for_ice);
        Box::pin(async move {
            if let Some(c) = candidate {
                let init = match c.to_json() {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("Failed to serialize ICE candidate: {e}");
                        return;
                    }
                };
                let pid = *player_id_for_ice.lock().await;
                let msg = signaling::SignalMessage::Candidate {
                    candidate: init.candidate,
                    sdp_mid: init.sdp_mid,
                    sdp_m_line_index: init.sdp_mline_index,
                    user_id: pid,
                };
                let json = serde_json::to_string(&msg);
                match json {
                    Ok(j) => {
                        let _ = ws_sender.lock().await.send(Message::Text(j.into())).await;
                    }
                    Err(e) => {
                        debug!("Failed to serialize signaling message: {e}");
                        return;
                    }
                };
            }
        })
    }));

    {
        let game = Arc::clone(&state.game);
        // PC への弱参照（cycle を作らず、実切断時に disconnect_player が close() してFDを解放する）
        let pc_weak = Arc::downgrade(&peer_connection);
        let player_id_for_state = Arc::clone(&player_id_shared);
        peer_connection.on_peer_connection_state_change(Box::new(
            move |s: RTCPeerConnectionState| {
                debug!("Peer connection state changed: {s:?}");
                // 終端状態 (Failed/Closed) のみで切断処理を起動する。
                // Disconnected は一過性(ICE瞬断で復帰しうる)なので起動しない
                //   → ICE が failed_timeout(8s) を過ぎて初めて Failed になり、実切断と確定する。
                // シグナリングWSの切断はトリガにしない（WSは確立後に閉じる設計）。
                let trigger = matches!(
                    s,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                );
                let game = Arc::clone(&game);
                let pc_weak = pc_weak.clone();
                let player_id_for_state = Arc::clone(&player_id_for_state);
                Box::pin(async move {
                    if trigger {
                        let pid = *player_id_for_state.lock().await;
                        connection::disconnect_player(game, pid, Some(pc_weak)).await;
                    }
                })
            },
        ));
    }

    // Offer ハンドラで再接続(player_id再利用)判定に Game を読むため、ループ用クローンを
    // on_data_channel クロージャが game をムーブするより前に確保しておく。
    let game_for_loop = Arc::clone(&state.game);
    let player_id_for_loop = Arc::clone(&player_id_shared);

    let game_for_dc = Arc::clone(&state.game);
    let player_id_for_dc = Arc::clone(&player_id_shared);
    peer_connection.on_data_channel(Box::new(
        move |dc: Arc<webrtc::data_channel::RTCDataChannel>| {
            let game = Arc::clone(&game_for_dc);
            let player_id_for_dc = Arc::clone(&player_id_for_dc);
            let dc_label = dc.label().to_string();

            info!("Data channel created: {dc_label}");

            match dc_label.as_str() {
                connection::RELIABLE_CHANNEL_LABEL => Box::pin(async move {
                    nest!(game);
                    // チャネルが開くのは offer 処理後なので、ここでは確定済みの player_id を読む。
                    let pid = *player_id_for_dc.lock().await;
                    let _ = tokio::spawn(handle_reliable_connection(dc, game, pid)).instrument(
                        info_span!("handle_connection", dc_label = %dc_label, player_id = %pid),
                    );
                }),
                connection::UNRELIABLE_CHANNEL_LABEL => Box::pin(async move {
                    nest!(game);
                    let pid = *player_id_for_dc.lock().await;
                    let _ = tokio::spawn(handle_unreliable_connection(dc, game, pid)).instrument(
                        info_span!("handle_connection", dc_label = %dc_label, player_id = %pid),
                    );
                }),
                _ => {
                    debug!("Unknown data channel label: {dc_label}");
                    return Box::pin(async {});
                }
            }
        },
    ));

    let mut version_authenticated = false;

    while let Some(Ok(msg)) = ws_receiver.next().await {
        if let Message::Text(text) = msg {
            // 不正なシグナリングメッセージでタスクを panic させない
            let signal: signaling::SignalMessage = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    debug!("Failed to parse signaling message: {e}");
                    continue;
                }
            };

            match signal {
                signaling::SignalMessage::Offer {
                    sdp,
                    player_id: reconnect_id,
                } => {
                    if !version_authenticated {
                        warn!("Received offer before client authentication; ignoring.");
                        continue;
                    }
                    // ★ 再接続: クライアントが既存の player_id を載せてきて、その人がまだ
                    //    サーバーに居る（猶予中など）なら、この新PCをそのIDへ再バインドする。
                    //    state を Establishing に戻して disconnect_player の猶予removalを止める
                    //    （新しい両チャネルが揃えば check_if_ready が Connected に進める）。
                    if let Some(rid) = reconnect_id {
                        let exists = {
                            let g = game_for_loop.read().await;
                            g.get_connection_state(&rid).is_some()
                        };
                        if exists {
                            *player_id_for_loop.lock().await = rid;
                            let g = game_for_loop.read().await;
                            if let Some(conn_state) = g.get_connection_state(&rid) {
                                let s = conn_state.lock();
                                if let Ok(mut s) = s {
                                    if matches!(*s, crate::game::ConnectionState::Disconnected) {
                                        *s = crate::game::ConnectionState::Establishing;
                                    }
                                }
                            }
                            info!("Reconnect offer: rebinding new PC to existing player [{rid}]");
                        } else {
                            warn!(
                                "Reconnect offer for unknown player [{rid}]; treating as new connection."
                            );
                        }
                    }

                    let offer = match RTCSessionDescription::offer(sdp) {
                        Ok(o) => o,
                        Err(e) => {
                            debug!("Failed to parse SDP offer: {e}");
                            continue;
                        }
                    };

                    if let Err(e) = peer_connection.set_remote_description(offer).await {
                        debug!("Failed to set remote description: {e}");
                        continue;
                    }

                    let answer = match peer_connection.create_answer(None).await {
                        Ok(a) => a,
                        Err(e) => {
                            debug!("Failed to create SDP answer: {e}");
                            continue;
                        }
                    };

                    if let Err(e) = peer_connection.set_local_description(answer.clone()).await {
                        debug!("Failed to set local description: {e}");
                        continue;
                    }

                    let response = match serde_json::to_string(&signaling::SignalMessage::Answer {
                        sdp: answer.sdp,
                    }) {
                        Ok(r) => r,
                        Err(e) => {
                            debug!("Failed to serialize signaling message: {e}");
                            continue;
                        }
                    };

                    let _ = ws_sender
                        .lock()
                        .await
                        .send(Message::Text(response.into()))
                        .await;
                }
                signaling::SignalMessage::Candidate {
                    candidate,
                    sdp_mid,
                    sdp_m_line_index,
                    user_id: _,
                } => {
                    if !version_authenticated {
                        warn!("Received offer before client authentication; ignoring.");
                        continue;
                    }
                    let init = RTCIceCandidateInit {
                        candidate,
                        sdp_mid,
                        sdp_mline_index: sdp_m_line_index,
                        username_fragment: None,
                    };
                    if let Err(e) = peer_connection.add_ice_candidate(init).await {
                        warn!("Failed to add ICE candidate: {e}");
                    }
                }
                signaling::SignalMessage::Auth { version } => {
                    if !state.allow_versions.contains(&version) {
                        error!("Rejected connection with unsupported client version: {version}");

                        let _ = ws_sender.lock().await.send(Message::Text(
                            serde_json::to_string(&signaling::SignalMessage::AuthResult {
                                success: false,
                                message: Some(format!(
                                    "Unsupported client version: {version}. Allowed versions: {:?}\nPlease reload or reopen the client to update to a supported version.",
                                    state.allow_versions
                                )),
                                rtc_peer_ice_config: None,
                            })
                            .unwrap()
                            .into(),
                        )).await;

                        break;
                    } else {
                        info!("Client authenticated with version: {version}");

                        let ice_config: Option<String> = {
                            let token_id = env::var("CF_TURN_TOKEN_ID").ok();
                            let token_secret = env::var("CF_TURN_API_TOKEN").ok();

                            if let (Some(id), Some(secret)) = (token_id, token_secret) {
                                let url = Url::parse(format!("https://rtc.live.cloudflare.com/v1/turn/keys/{}/credentials/generate-ice-servers", id).as_str()).unwrap();
                                let mut map: std::collections::HashMap<&str, i32> =
                                    std::collections::HashMap::new();
                                map.insert("ttl", 86400);

                                let header = format!("Bearer {}", secret);
                                let mut headers = HeaderMap::new();
                                headers.insert(
                                    header::AUTHORIZATION,
                                    HeaderValue::from_str(&header).unwrap(),
                                );
                                headers.insert(
                                    header::CONTENT_TYPE,
                                    HeaderValue::from_static("application/json"),
                                );

                                let client = reqwest::Client::new();
                                let res = client.post(url).headers(headers).json(&map).send().await;
                                if let Ok(r) = res {
                                    if r.status().is_success() {
                                        let text = r.text().await;
                                        if let Ok(t) = text {
                                            Some(t)
                                        } else {
                                            error!("Failed to read ICE server response text");
                                            None
                                        }
                                    } else {
                                        error!("Failed to get ICE servers: HTTP {}", r.status());
                                        None
                                    }
                                } else {
                                    error!(
                                        "Failed to send request for ICE servers: {}",
                                        res.err().unwrap()
                                    );
                                    None
                                }
                            } else {
                                None
                            }
                        };

                        let _ = ws_sender
                            .lock()
                            .await
                            .send(Message::Text(
                                serde_json::to_string(&signaling::SignalMessage::AuthResult {
                                    success: true,
                                    message: None,
                                    rtc_peer_ice_config: ice_config,
                                })
                                .unwrap()
                                .into(),
                            ))
                            .await;

                        version_authenticated = true;
                    }
                }
                _ => {
                    debug!("Unexpected signaling message type: {signal:?}");
                    continue;
                }
            }
        }
    }

    // ★ シグナリングWSが閉じても PeerConnection は閉じない。
    //   WSは「確立後に閉じてよい」設計（client connection.ts 参照）で、DataChannel は
    //   生き続けるため。ここで close すると対戦中に WS がアイドル切断された瞬間に
    //   全員の接続が切れる。リソース解放は PeerConnection が Failed になった時に行う。
}

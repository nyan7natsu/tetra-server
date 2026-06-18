use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::env;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};
use tracing::Instrument;
use tracing::debug;
use tracing::info;
use tracing::info_span;
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

mod connection;
mod game;
mod payload;
mod room;
mod signaling;

use connection::{handle_reliable_connection, handle_unreliable_connection};

macro_rules! nest {
    ($($n:ident),+ $(,)?) => {
        $(let $n = std::sync::Arc::clone(&$n);)+
    };
}

/// ファイルディスクリプタのソフト上限を引き上げる。
/// webrtc-rs は接続ごとに ICE 用 UDP ソケットを多数開き、close 後も一部が即時解放されない
/// （ライブラリ側の挙動）。既定の soft=1024 だと多数接続でFDが枯渇するため、ハード上限の
/// 範囲で十分大きな値へ引き上げて、現実的なセッション長で枯渇しないようにする。
fn raise_fd_limit() {
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let target = 65536u64.min(lim.rlim_max as u64);
        if (lim.rlim_cur as u64) < target {
            lim.rlim_cur = target as libc::rlim_t;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) == 0 {
                println!("Raised RLIMIT_NOFILE soft limit to {target}");
            } else {
                eprintln!("Failed to raise RLIMIT_NOFILE (continuing with current limit)");
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    raise_fd_limit();

    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let file_appender = rolling::never("logs", "server.log");
    let (file_writer, _guard) = tracing_appender::non_blocking(file_appender);

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_writer(std::io::stdout);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_ansi(false)
        .with_writer(file_writer);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    info!("Starting server...");

    let ws_addr = env::var("WS_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    info!("Listening on {ws_addr}...");

    let listener = TcpListener::bind(&ws_addr).await?;

    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    // ICE タイムアウトを短縮して、クライアント切断(タブ閉じ/回線断)を素早く検知する。
    // 既定だと failed まで ~25s かかり切断検知が遅れるため。
    // disconnected: 一過性とみなす猶予 / failed: これを過ぎたら終端(Failed) / keepalive: 疎通確認間隔
    let mut s = webrtc::api::setting_engine::SettingEngine::default();
    s.set_ice_timeouts(
        Some(std::time::Duration::from_secs(5)),
        Some(std::time::Duration::from_secs(8)),
        Some(std::time::Duration::from_secs(2)),
    );
    let api = Arc::new(
        APIBuilder::new()
            .with_media_engine(m)
            .with_setting_engine(s)
            .build(),
    );
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec![
                "stun:stun.l.google.com:19302".to_string(),
                "stun:stun1.l.google.com:19302".to_string(),
                "stun:stun2.l.google.com:19302".to_string(),
                "stun:stun3.l.google.com:19302".to_string(),
                "stun:stun4.l.google.com:19302".to_string(),
            ],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Game は読み取り(中継時の RTCDataChannel 取得)が高頻度・書き込み(ルーム操作)が低頻度
    // なので RwLock を使い、中継の読み取りロックを全ルーム並行に取れるようにする(Issue #8)。
    let game = Arc::new(RwLock::new(game::Game::default()));

    // accept() が一過性のエラー（FD枯渇など）を返しても、ループを抜けて
    // サーバーを終了させない。ログを出して少し待ち、受付を継続する。
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("accept() failed (continuing): {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        nest!(game, api);

        let config = config.clone();

        tokio::spawn(async move {
            let ws_stream = tokio_tungstenite::accept_async(stream)
                .await
                .expect("Failed to accept WebSocket connection");
            debug!("Accepted WebSocket connection");

            let (ws_sender, mut ws_receiver) = ws_stream.split();
            let ws_sender = Arc::new(Mutex::new(ws_sender));

            let peer_connection = Arc::new(
                api.new_peer_connection(config)
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
                        let json = serde_json::to_string(&msg).unwrap();
                        let _ = ws_sender
                            .lock()
                            .await
                            .send(tokio_tungstenite::tungstenite::Message::Text(json.into()))
                            .await;
                    }
                })
            }));

            {
                nest!(game);
                // PC への弱参照（cycle を作らず、実切断時に disconnect_player が close() してFDを解放する）
                let pc_weak = Arc::downgrade(&peer_connection);
                let player_id_for_state = Arc::clone(&player_id_shared);
                peer_connection.on_peer_connection_state_change(Box::new(
                    move |state: RTCPeerConnectionState| {
                        debug!("Peer connection state changed: {state:?}");
                        // 終端状態 (Failed/Closed) のみで切断処理を起動する。
                        // Disconnected は一過性(ICE瞬断で復帰しうる)なので起動しない
                        //   → ICE が failed_timeout(8s) を過ぎて初めて Failed になり、実切断と確定する。
                        // シグナリングWSの切断はトリガにしない（WSは確立後に閉じる設計）。
                        let trigger = matches!(
                            state,
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
            let game_for_loop = Arc::clone(&game);
            let player_id_for_loop = Arc::clone(&player_id_shared);

            let player_id_for_dc = Arc::clone(&player_id_shared);
            peer_connection.on_data_channel(Box::new(
                move |dc: Arc<webrtc::data_channel::RTCDataChannel>| {
                    nest!(game);
                    let player_id_for_dc = Arc::clone(&player_id_for_dc);
                    let dc_label = dc.label().to_string();

                    println!("Data channel created: {dc_label}");

                    match dc_label.as_str() {
                        connection::RELIABLE_CHANNEL_LABEL => Box::pin(async move {
                            nest!(game);
                            // チャネルが開くのは offer 処理後なので、ここでは確定済みの player_id を読む。
                            let pid = *player_id_for_dc.lock().await;
                            let _ = tokio::spawn(handle_reliable_connection(dc, game, pid))
                                .instrument(info_span!("handle_connection", dc_label = %dc_label, player_id = %pid));
                        }),
                        connection::UNRELIABLE_CHANNEL_LABEL => Box::pin(async move {
                            nest!(game);
                            let pid = *player_id_for_dc.lock().await;
                            let _ = tokio::spawn(handle_unreliable_connection(dc, game, pid))
                                .instrument(info_span!("handle_connection", dc_label = %dc_label, player_id = %pid));
                        }),
                        _ => {
                            debug!("Unknown data channel label: {dc_label}");
                            return Box::pin(async {});
                        }
                    }
                },
            ));

            while let Some(Ok(msg)) = ws_receiver.next().await {
                if msg.is_text() {
                    let Ok(text) = msg.to_text() else { continue };
                    // 不正なシグナリングメッセージでタスクを panic させない
                    let signal: signaling::SignalMessage = match serde_json::from_str(text) {
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
                                    if let Some(state) = g.get_connection_state(&rid) {
                                        let mut s = state.lock().unwrap();
                                        if matches!(*s, crate::game::ConnectionState::Disconnected)
                                        {
                                            *s = crate::game::ConnectionState::Establishing;
                                        }
                                    }
                                    println!(
                                        "Reconnect offer: rebinding new PC to existing player [{rid}]"
                                    );
                                } else {
                                    println!(
                                        "Reconnect offer for unknown player [{rid}]; treating as new connection."
                                    );
                                }
                            }

                            peer_connection
                                .set_remote_description(RTCSessionDescription::offer(sdp).unwrap())
                                .await
                                .unwrap();

                            let answer = peer_connection.create_answer(None).await.unwrap();
                            peer_connection
                                .set_local_description(answer.clone())
                                .await
                                .unwrap();

                            let response =
                                serde_json::to_string(&signaling::SignalMessage::Answer {
                                    sdp: answer.sdp,
                                })
                                .unwrap();

                            let _ = ws_sender
                                .lock()
                                .await
                                .send(tokio_tungstenite::tungstenite::Message::Text(
                                    response.into(),
                                ))
                                .await;
                        }
                        signaling::SignalMessage::Candidate {
                            candidate,
                            sdp_mid,
                            sdp_m_line_index,
                            user_id: _,
                        } => {
                            let init = RTCIceCandidateInit {
                                candidate,
                                sdp_mid,
                                sdp_mline_index: sdp_m_line_index,
                                username_fragment: None,
                            };
                            let _ = peer_connection.add_ice_candidate(init).await;
                        }
                        _ => {}
                    }
                }
            }

            // ★ シグナリングWSが閉じても PeerConnection は閉じない。
            //   WSは「確立後に閉じてよい」設計（client connection.ts 参照）で、DataChannel は
            //   生き続けるため。ここで close すると対戦中に WS がアイドル切断された瞬間に
            //   全員の接続が切れる。リソース解放は PeerConnection が Failed になった時に行う。
        });
    }
}

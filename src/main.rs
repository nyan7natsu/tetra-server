use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
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

use connection::handle_reliable_connection;

macro_rules! nest {
    ($($n:ident),+ $(,)?) => {
        $(let $n = std::sync::Arc::clone(&$n);)+
    };
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    let listener = TcpListener::bind("127.0.0.1:8080").await?;

    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    let api = Arc::new(APIBuilder::new().with_media_engine(m).build());
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

    let game = Arc::new(Mutex::new(game::Game::default()));

    game.lock()
        .await
        .new_room("TEST".to_string(), 2, None, vec![]);

    while let Ok((stream, _)) = listener.accept().await {
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

            let ws_sender_for_ice = Arc::clone(&ws_sender);
            peer_connection.on_ice_candidate(Box::new(move |candidate| {
                let ws_sender = Arc::clone(&ws_sender_for_ice);
                Box::pin(async move {
                    if let Some(c) = candidate {
                        let init = match c.to_json() {
                            Ok(v) => v,
                            Err(e) => {
                                debug!("Failed to serialize ICE candidate: {e}");
                                return;
                            }
                        };
                        let msg = signaling::SignalMessage::Candidate {
                            candidate: init.candidate,
                            sdp_mid: init.sdp_mid,
                            sdp_m_line_index: init.sdp_mline_index,
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

            peer_connection.on_peer_connection_state_change(Box::new(
                |state: RTCPeerConnectionState| {
                    debug!("Peer connection state changed: {state:?}");
                    Box::pin(async {})
                },
            ));

            peer_connection.on_data_channel(Box::new(
                move |dc: Arc<webrtc::data_channel::RTCDataChannel>| {
                    nest!(game);
                    let dc_label = dc.label().to_string();

                    println!("Data channel created: {dc_label}");

                    match dc_label.as_str() {
                        connection::RELIABLE_CHANNEL_LABEL=> Box::pin(async move {
                            nest!(game);

                            let _ = tokio::spawn(handle_reliable_connection(dc, game))
                                .instrument(info_span!("handle_connection", dc_label = %dc_label));
                        }),
                        connection::UNRELIABLE_CHANNEL_LABEL => Box::pin(async move {
                            nest!(game);

                            let _ = tokio::spawn(connection::handle_unreliable_connection(dc, game))
                                .instrument(info_span!("handle_connection", dc_label = %dc_label));
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
                    let text = msg.to_text().unwrap();
                    let signal: signaling::SignalMessage = serde_json::from_str(text).unwrap();

                    match signal {
                        signaling::SignalMessage::Offer { sdp } => {
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
        });
    }

    Ok(())
}

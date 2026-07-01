use anyhow::Result;
use axum::{Router, routing::get};
use std::env;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;

mod connection;
mod endpoint;
mod game;
mod payload;
mod room;
mod signaling;
mod state;

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
                info!("Raised RLIMIT_NOFILE soft limit to {target}");
            } else {
                error!("Failed to raise RLIMIT_NOFILE (continuing with current limit)");
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    raise_fd_limit();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

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

    let http_addr = env::var("HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    // ICE タイムアウト。切断検知を早めたいが、短すぎると低品質な回線
    // (高レイテンシ・パケロス)でチェックが完了する前にFailedになってしまう
    // (大学WIFI等での接続断の主因だった。旧設定は 5s/8s = 13s で機械的に打ち切っていた)。
    // disconnected: 一過性とみなす猶予 / failed: これを過ぎたら終端(Failed) / keepalive: 疎通確認間隔
    let mut s = webrtc::api::setting_engine::SettingEngine::default();
    s.set_ice_timeouts(
        Some(std::time::Duration::from_secs(10)),
        Some(std::time::Duration::from_secs(20)),
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
                "stun:stun.cloudflare.com:3478".to_string(),
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

    let state = state::AppState {
        game,
        api,
        config,
        allow_versions: env::var("ALLOW_VERSIONS")
            .unwrap_or_else(|_| "0.0.0".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .collect(),
    };

    info!("Allowed versions: {:?}", state.allow_versions);

    let app = Router::new()
        .route("/", get(endpoint::helloworld::hello))
        .route("/ws", get(endpoint::ws::ws_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&http_addr).await?;
    info!("Listening on {http_addr}...");
    axum::serve(listener, app).await?;

    Ok(())
}

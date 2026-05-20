use anyhow::Result;
use std::time::Duration;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use wtransport::Endpoint;
use wtransport::Identity;
use wtransport::ServerConfig;
use wtransport::endpoint::IncomingSession;
use wtransport::tls::Sha256DigestFmt;

mod connection;
mod payload;

#[tokio::main]
async fn main() -> Result<()> {
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    tracing_subscriber::fmt()
        .with_target(true)
        .with_level(true)
        .with_env_filter(env_filter)
        .init();

    info!("Starting server...");

    let identity = Identity::self_signed(["localhost"])?;

    info!(
        "Identity certificate hash (algorithm: sha-256)\n\n{}\n\n(This will be used for the WebTransport development client)",
        identity.certificate_chain().as_slice()[0]
            .hash()
            .fmt(Sha256DigestFmt::BytesArray)
    );

    let config = ServerConfig::builder()
        .with_bind_default(4433)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(5)))
        .build();

    let server = Endpoint::server(config)?;

    info!("Server is ready");

    for id in 0.. {
        let session = server.accept().await;

        debug!("Accepted session {id}");

        tokio::spawn(
            connection::handle_connection(session).instrument(info_span!("Connection", id)),
        );
    }

    Ok(())
}

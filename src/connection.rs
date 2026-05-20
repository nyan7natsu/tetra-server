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

use crate::payload;

pub async fn handle_connection(session: IncomingSession) -> Result<()> {
    let mut buffer = vec![0; 65536].into_boxed_slice();

    let session_request = session.await?;

    info!(
        "New session: Authority: '{}', Path: '{}'",
        session_request.authority(),
        session_request.path()
    );

    let connection = session_request.accept().await?;

    loop {
        tokio::select! {
            stream = connection.accept_bi() => {
                let mut stream = stream?;
                debug!("Accepted BI stream");

                let Some(bytes_read) = stream.1.read(&mut buffer).await? else {
                    continue;
                };
                let data = &buffer[..bytes_read];

                let payload: payload::GameMessage = match wincode::deserialize(data) {
                    Ok(payload) => payload,
                    Err(e) => {
                        error!("Failed to deserialize payload: {e}");
                        continue;
                    }
                };

                match payload {
                    payload::GameMessage::JoinRoom(p) => {
                        info!("Received join room request");
                    }
                    payload::GameMessage::Attack { power } => {
                        info!("Received attack with power {power}");
                    }
                }

                stream.0.write_all(b"ACK").await?;
            }
            dgram = connection.receive_datagram() => {
                let dgram = dgram?;
                let str_data = std::str::from_utf8(&dgram)?;

                debug!("Received (dgram) '{str_data}' from client");

                connection.send_datagram(b"ACK")?;
            }
        }
    }
}

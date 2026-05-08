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

                let str_data = std::str::from_utf8(&buffer[..bytes_read])?;

                debug!("Received (bi) '{str_data}' from client");

                stream.0.write_all(b"ACK").await?;
            }
            stream = connection.accept_uni() => {
                let mut stream = stream?;
                debug!("Accepted UNI stream");

                let Some(bytes_read) = stream.read(&mut buffer).await? else {
                    continue;
                };

                let str_data = std::str::from_utf8(&buffer[..bytes_read])?;

                debug!("Received (uni) '{str_data}' from client");

                let mut stream = connection.open_uni().await?.await?;
                stream.write_all(b"ACK").await?;
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

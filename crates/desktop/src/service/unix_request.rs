//! One Unix request owns its socket until success, timeout or cancellation.
//! Watch uses its existing separate lifecycle in the parent module.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::{ServiceError, EXPECTED_CLIENT_LOCATION, MAX_FRAME_BYTES};

pub(super) async fn transact(
    endpoint: &str,
    body: &[u8],
    timeout: Duration,
    handshake: bool,
) -> Result<Vec<u8>, ServiceError> {
    // Preserve the caller's one-second scheduling margin, but apply one
    // deadline to connect, write and the complete response. Receiving bytes
    // must not restart it. Dropping this future also drops its owned socket.
    tokio::time::timeout(timeout + Duration::from_secs(1), async {
        let mut stream = UnixStream::connect(endpoint)
            .await
            .map_err(|error| ServiceError::Unavailable(format!("connect {endpoint}: {error}")))?;
        let write =
            |error: std::io::Error| ServiceError::Unavailable(format!("write request: {error}"));
        stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .await
            .map_err(write)?;
        stream.write_all(body).await.map_err(write)?;
        stream.flush().await.map_err(write)?;

        let mut length = [0u8; 4];
        let mut filled = 0;
        while filled < length.len() {
            match stream.read(&mut length[filled..]).await {
                Ok(0) if filled == 0 && handshake => {
                    return Err(ServiceError::Rejected(format!(
                        "SERVICE_CLIENT_REJECTED: the service closed the connection without answering; \
                         it only accepts the app installed at its expected location ({EXPECTED_CLIENT_LOCATION})"
                    )))
                }
                Ok(0) => {
                    return Err(ServiceError::Protocol(format!(
                        "SERVICE_PROTOCOL: connection closed after {filled} of 4 length bytes"
                    )))
                }
                Ok(read) => filled += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(ServiceError::Unavailable(format!(
                        "read response length: {error}"
                    )))
                }
            }
        }
        let length = u32::from_be_bytes(length) as usize;
        if length > MAX_FRAME_BYTES {
            return Err(ServiceError::Protocol(format!(
                "SERVICE_PROTOCOL: response too large: {length}"
            )));
        }
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                ServiceError::Protocol(format!(
                    "SERVICE_PROTOCOL: response body truncated ({length} bytes expected): {error}"
                ))
            } else {
                ServiceError::Unavailable(format!("read response body: {error}"))
            }
        })?;
        Ok(body)
    })
    .await
    .map_err(|_| ServiceError::Unavailable("service call timed out".into()))?
}

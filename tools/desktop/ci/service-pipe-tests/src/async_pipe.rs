//! The Windows request pipe is owned by one timed async operation. Dropping
//! it cancels pending I/O and closes the handle, including a blocked write.

use std::future::Future;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{ServiceError, EXPECTED_CLIENT_LOCATION, MAX_FRAME_BYTES};

#[cfg(windows)]
pub(super) async fn transact(
    endpoint: &str,
    body: &[u8],
    timeout: Duration,
    handshake: bool,
) -> Result<Vec<u8>, ServiceError> {
    // Retain the caller's existing one-second scheduling margin. Opening,
    // sending and receiving share this deadline; progress never resets it.
    within_deadline(timeout + Duration::from_secs(1), async {
        let pipe = open_pipe(endpoint).await?;
        exchange(pipe, body, handshake).await
    })
    .await
}

async fn within_deadline(
    timeout: Duration,
    operation: impl Future<Output = Result<Vec<u8>, ServiceError>>,
) -> Result<Vec<u8>, ServiceError> {
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| ServiceError::Unavailable("service call timed out".into()))?
}

#[cfg(windows)]
async fn open_pipe(endpoint: &str) -> Result<OwnedPipe, ServiceError> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        // Like std::fs::OpenOptions, Tokio defaults to byte mode and
        // SECURITY_IDENTIFICATION | SECURITY_SQOS_PRESENT.
        match ClientOptions::new().open(endpoint) {
            Ok(pipe) => return Ok(OwnedPipe(pipe)),
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(ServiceError::Unavailable(format!(
                    "open pipe {endpoint}: {error}"
                )))
            }
        }
    }
}

// Mio deliberately leaves queued writes running when NamedPipeClient drops.
// Explicit cancellation is required even after write_all returned: it may
// only have queued the request, retaining the handle until the peer reads it.
#[cfg(windows)]
struct OwnedPipe(tokio::net::windows::named_pipe::NamedPipeClient);

#[cfg(windows)]
impl Drop for OwnedPipe {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: the client still owns this valid handle. A null OVERLAPPED
        // cancels all its pending I/O; Mio retains its buffers until the
        // completion arrives. Normal field drop then releases the client.
        // ERROR_NOT_FOUND is expected when no operation remains outstanding.
        unsafe {
            windows_sys::Win32::System::IO::CancelIoEx(
                self.0.as_raw_handle() as _,
                std::ptr::null(),
            );
        }
    }
}

#[cfg(windows)]
impl AsyncRead for OwnedPipe {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

#[cfg(windows)]
impl AsyncWrite for OwnedPipe {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Same framing and EOF classification as the synchronous Unix exchange.
/// Own the stream so a deadline drops it, rather than a detached I/O worker.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    body: &[u8],
    handshake: bool,
) -> Result<Vec<u8>, ServiceError> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, DuplexStream};

    const DEADLINE: Duration = Duration::from_millis(100);

    fn assert_timeout(result: Result<Vec<u8>, ServiceError>) {
        assert!(
            matches!(result, Err(ServiceError::Unavailable(ref detail)) if detail == "service call timed out"),
            "{result:?}"
        );
    }

    async fn request(server: &mut (impl AsyncRead + Unpin)) {
        let mut bytes = [0u8; 6];
        server.read_exact(&mut bytes).await.unwrap();
        assert_eq!(bytes, [0, 0, 0, 2, b'{', b'}']);
    }

    async fn assert_closed(server: &mut DuplexStream) {
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), server.read_to_end(&mut rest))
            .await
            .expect("timed-out transaction retained its stream")
            .unwrap();
    }

    #[tokio::test]
    async fn a_missing_or_partial_header_times_out_and_closes_the_stream() {
        for prefix in [vec![], vec![0, 0]] {
            let (client, mut server) = duplex(64);
            let exchange = within_deadline(DEADLINE, exchange(client, b"{}", true));
            let service = async {
                request(&mut server).await;
                server.write_all(&prefix).await.unwrap();
                assert_closed(&mut server).await;
            };
            let (result, ()) = tokio::join!(exchange, service);
            assert_timeout(result);
        }
    }

    #[tokio::test]
    async fn a_partial_body_times_out_and_closes_the_stream() {
        let (client, mut server) = duplex(64);
        let exchange = within_deadline(DEADLINE, exchange(client, b"{}", false));
        let service = async {
            request(&mut server).await;
            server.write_all(&[0, 0, 0, 4, 1, 2]).await.unwrap();
            assert_closed(&mut server).await;
        };
        let (result, ()) = tokio::join!(exchange, service);
        assert_timeout(result);
    }

    #[tokio::test]
    async fn a_blocked_request_write_times_out_and_closes_the_stream() {
        // The peer never drains even the complete request header.
        let (client, mut server) = duplex(2);
        assert_timeout(within_deadline(DEADLINE, exchange(client, b"{}", false)).await);
        assert_closed(&mut server).await;
    }

    #[tokio::test]
    async fn cancelling_the_caller_closes_the_stream_without_waiting_for_its_deadline() {
        let (client, mut server) = duplex(64);
        let task = tokio::spawn(within_deadline(
            Duration::from_secs(30),
            exchange(client, b"{}", false),
        ));
        request(&mut server).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_closed(&mut server).await;
    }

    #[tokio::test]
    async fn response_progress_does_not_extend_the_total_deadline() {
        let (client, mut server) = duplex(64);
        let exchange = within_deadline(DEADLINE, exchange(client, b"{}", false));
        let service = async {
            request(&mut server).await;
            server.write_all(&[0, 0, 16, 0]).await.unwrap();
            let mut sent = 0;
            while server.write_all(&[1]).await.is_ok() {
                sent += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
                assert!(sent < 100, "progress reset the total deadline");
            }
            assert_closed(&mut server).await;
        };
        let (result, ()) = tokio::join!(exchange, service);
        assert_timeout(result);
    }

    async fn response(bytes: &[u8], handshake: bool) -> Result<Vec<u8>, ServiceError> {
        let (client, mut server) = duplex(64);
        let service = async {
            request(&mut server).await;
            for byte in bytes {
                server.write_all(&[*byte]).await.unwrap();
            }
            drop(server);
        };
        let (result, ()) = tokio::join!(
            within_deadline(Duration::from_secs(1), exchange(client, b"{}", handshake)),
            service
        );
        result
    }

    #[tokio::test]
    async fn a_complete_response_keeps_the_existing_wire_format() {
        assert_eq!(
            response(&[0, 0, 0, 2, b'{', b'}'], false).await.unwrap(),
            b"{}"
        );
    }

    #[tokio::test]
    async fn closed_and_oversized_responses_keep_their_existing_classification() {
        assert!(matches!(
            response(&[], true).await,
            Err(ServiceError::Rejected(_))
        ));
        for (bytes, handshake) in [
            (vec![], false),
            (vec![0, 0], true),
            (vec![0, 0, 0, 9, b'{'], true),
        ] {
            assert!(matches!(
                response(&bytes, handshake).await,
                Err(ServiceError::Protocol(_))
            ));
        }
        let too_big = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
        assert!(matches!(
            response(&too_big, false).await,
            Err(ServiceError::Protocol(_))
        ));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_pipe_success_and_handshake_rejection_keep_the_protocol() {
        use tokio::net::windows::named_pipe::ServerOptions;
        for (reply, handshake) in [(true, false), (false, true), (false, false)] {
            let endpoint = format!(r"\\.\pipe\ppvpn-frame-test-{}", uuid::Uuid::new_v4());
            let mut server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&endpoint)
                .unwrap();
            let transaction = transact(&endpoint, b"{}", Duration::from_secs(1), handshake);
            let service = async {
                server.connect().await.unwrap();
                request(&mut server).await;
                if reply {
                    server.write_all(&[0, 0, 0, 2, b'{', b'}']).await.unwrap();
                    // Keep the response available until the client consumes
                    // it and closes; disconnecting can discard unread bytes.
                    let mut rest = [0];
                    let result =
                        tokio::time::timeout(Duration::from_secs(3), server.read(&mut rest))
                            .await
                            .unwrap();
                    assert!(matches!(result, Ok(0)) || result.is_err());
                }
                drop(server);
            };
            let (result, ()) = tokio::join!(transaction, service);
            if reply {
                assert_eq!(result.unwrap(), b"{}");
            } else if handshake {
                assert!(
                    matches!(result, Err(ServiceError::Rejected(_))),
                    "{result:?}"
                );
            } else {
                assert!(
                    matches!(result, Err(ServiceError::Protocol(_))),
                    "{result:?}"
                );
            }
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_pipe_deadlines_close_stalled_reads_and_writes() {
        use tokio::net::windows::named_pipe::ServerOptions;
        for case in 0..3 {
            let endpoint = format!(r"\\.\pipe\ppvpn-deadline-test-{}", uuid::Uuid::new_v4());
            let mut server = ServerOptions::new()
                .first_pipe_instance(true)
                .in_buffer_size(64)
                .create(&endpoint)
                .unwrap();
            let body = if case == 2 {
                vec![0; 1024 * 1024]
            } else {
                b"{}".to_vec()
            };
            let transaction = transact(&endpoint, &body, Duration::ZERO, false);
            let service = async {
                server.connect().await.unwrap();
                if case != 2 {
                    request(&mut server).await;
                    if case == 1 {
                        server.write_all(&[0, 0, 0, 4, 1, 2]).await.unwrap();
                    }
                    let mut rest = [0];
                    let result =
                        tokio::time::timeout(Duration::from_secs(3), server.read(&mut rest))
                            .await
                            .expect("native pipe did not close");
                    assert!(matches!(result, Ok(0)) || result.is_err());
                } else {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    let mut rest = Vec::new();
                    let _ =
                        tokio::time::timeout(Duration::from_secs(2), server.read_to_end(&mut rest))
                            .await
                            .expect("blocked write kept the native pipe open");
                    assert!(rest.len() < body.len() + 4);
                }
            };
            let (result, ()) = tokio::join!(transaction, service);
            assert_timeout(result);
        }
    }
}

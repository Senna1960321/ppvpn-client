//! Real Unix sockets exercise the production request path; no service or core
//! is started. Peers are always closed before asserting a regression failure.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

const CALL: Duration = Duration::from_millis(100);
const CLOSED: Duration = Duration::from_secs(1);

struct Socket {
    dir: std::path::PathBuf,
    client: Arc<ServiceClient>,
    listener: UnixListener,
}

impl Socket {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "ps-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        std::fs::create_dir(&dir).unwrap();
        let endpoint = dir.join("s").to_string_lossy().into_owned();
        let listener = UnixListener::bind(&endpoint).unwrap();
        Self {
            dir,
            client: Arc::new(ServiceClient::new(endpoint)),
            listener,
        }
    }

    fn call(
        &self,
        request: Request,
        handshake: bool,
    ) -> tokio::task::JoinHandle<Result<Response, ServiceError>> {
        let client = self.client.clone();
        tokio::spawn(async move { client.transact(request, CALL, handshake).await })
    }

    async fn accept(&self) -> UnixStream {
        tokio::time::timeout(CLOSED, self.listener.accept())
            .await
            .unwrap()
            .unwrap()
            .0
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn request(peer: &mut UnixStream) -> Request {
    let len = peer.read_u32().await.unwrap() as usize;
    let mut body = vec![0; len];
    peer.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn closed(peer: &mut (impl tokio::io::AsyncRead + Unpin)) -> bool {
    let mut byte = [0];
    matches!(
        tokio::time::timeout(CLOSED, peer.read(&mut byte)).await,
        Ok(Ok(0))
    )
}

fn assert_timeout(result: Result<Response, ServiceError>) {
    assert!(
        matches!(result, Err(ServiceError::Unavailable(ref detail)) if detail == "service call timed out"),
        "{result:?}"
    );
}

#[tokio::test]
async fn slow_response_progress_does_not_keep_a_timed_out_request_alive() {
    let socket = Socket::new();
    let call = socket.call(Request::handshake(), true);
    let mut peer = socket.accept().await;
    request(&mut peer).await;
    peer.write_all(&4096u32.to_be_bytes()).await.unwrap();
    let (mut read, mut write) = peer.into_split();
    let sending = tokio::spawn(async move {
        while write.write_all(b"x").await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let result = call.await.unwrap();
    let was_closed = closed(&mut read).await;
    sending.abort();
    let _ = sending.await;
    drop(read);
    assert_timeout(result);
    assert!(was_closed, "caller timed out but its socket remained open");
}

#[tokio::test]
async fn cancelling_a_response_read_closes_the_socket() {
    let socket = Socket::new();
    let call = socket.call(Request::handshake(), true);
    let mut peer = socket.accept().await;
    request(&mut peer).await;
    // Continuous progress keeps the old per-read timeout from expiring.
    peer.write_all(&4096u32.to_be_bytes()).await.unwrap();
    let (mut read, mut write) = peer.into_split();
    let sending = tokio::spawn(async move {
        while write.write_all(b"x").await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    let was_closed = closed(&mut read).await;
    sending.abort();
    let _ = sending.await;
    drop(read);
    assert!(was_closed, "cancelled caller retained its socket");
}

async fn blocked_write(cancel: bool) {
    let socket = Socket::new();
    let mut outgoing = Request::handshake();
    outgoing.payload = Value::String("x".repeat(3 * 1024 * 1024));
    let body_len = serde_json::to_vec(&outgoing).unwrap().len();
    let call = socket.call(outgoing, false);
    let mut peer = socket.accept().await;
    assert_eq!(peer.read_u32().await.unwrap() as usize, body_len);
    // Reading only the header leaves the large request blocked in write_all.
    if cancel {
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
    } else {
        assert_timeout(call.await.unwrap());
    }
    let mut received = Vec::new();
    let drained = tokio::time::timeout(CLOSED, peer.read_to_end(&mut received)).await;
    drop(peer);
    assert!(
        matches!(drained, Ok(Ok(_))),
        "request retained its socket: {drained:?}"
    );
    assert!(
        received.len() < body_len,
        "the detached worker kept writing the entire request"
    );
}

#[tokio::test]
async fn timing_out_a_blocked_write_closes_the_socket() {
    blocked_write(false).await;
}

#[tokio::test]
async fn cancelling_a_blocked_write_closes_the_socket() {
    blocked_write(true).await;
}

#[tokio::test]
async fn a_complete_fragmented_response_keeps_the_protocol() {
    let socket = Socket::new();
    let outgoing = Request::handshake();
    let id = outgoing.id.clone();
    let call = socket.call(outgoing, true);
    let mut peer = socket.accept().await;
    let sent = request(&mut peer).await;
    assert_eq!(sent.id, id);
    assert!(matches!(sent.command, Command::Handshake));
    let reply = serde_json::to_vec(&Response {
        id: id.clone(),
        success: true,
        data: Some(serde_json::json!({"test": 7})),
        error: None,
        signature: String::new(),
    })
    .unwrap();
    peer.write_all(&(reply.len() as u32).to_be_bytes())
        .await
        .unwrap();
    for byte in reply {
        peer.write_all(&[byte]).await.unwrap();
    }
    let response = call.await.unwrap().unwrap();
    assert_eq!(response.id, id);
    assert_eq!(response.data, Some(serde_json::json!({"test": 7})));
    assert!(closed(&mut peer).await);
}

async fn scripted_response(bytes: &[u8], handshake: bool) -> Result<Response, ServiceError> {
    let socket = Socket::new();
    let call = socket.call(Request::handshake(), handshake);
    let mut peer = socket.accept().await;
    request(&mut peer).await;
    peer.write_all(bytes).await.unwrap();
    drop(peer);
    call.await.unwrap()
}

#[tokio::test]
async fn only_an_empty_handshake_close_is_a_client_rejection() {
    assert!(matches!(
        scripted_response(&[], true).await,
        Err(ServiceError::Rejected(_))
    ));
    for (bytes, handshake) in [
        (vec![], false),
        (vec![0, 0], true),
        (vec![0, 0], false),
        (vec![0, 0, 0, 9, b'{'], true),
        (vec![0, 0, 0, 9, b'{'], false),
    ] {
        let error = scripted_response(&bytes, handshake).await.unwrap_err();
        assert!(matches!(error, ServiceError::Protocol(_)), "{error:?}");
        assert_eq!(error.info().code, ErrorCode::ServiceIncompatible);
    }
}

#[tokio::test]
async fn oversized_frames_and_invalid_json_remain_protocol_errors() {
    let too_large = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes();
    for bytes in [&too_large[..], &[0, 0, 0, 2, b'{', b'{']] {
        let error = scripted_response(bytes, false).await.unwrap_err();
        assert!(matches!(error, ServiceError::Protocol(_)), "{error:?}");
    }
}

#[tokio::test]
async fn stalled_headers_and_bodies_release_the_socket_and_allow_retry() {
    let socket = Socket::new();
    for prefix in [vec![], vec![0, 0], vec![0, 0, 0, 4, 1, 2]] {
        let call = socket.call(Request::handshake(), true);
        let mut peer = socket.accept().await;
        request(&mut peer).await;
        peer.write_all(&prefix).await.unwrap();
        let result = call.await.unwrap();
        let was_closed = closed(&mut peer).await;
        drop(peer);
        assert_timeout(result);
        assert!(was_closed, "a stalled response retained its socket");
    }
    // The same ServiceClient can issue another request after failed ones.
    let call = socket.call(Request::handshake(), true);
    let mut peer = socket.accept().await;
    let sent = request(&mut peer).await;
    let bytes = serde_json::to_vec(&Response {
        id: sent.id,
        success: true,
        data: Some(serde_json::json!({"retried": true})),
        error: None,
        signature: String::new(),
    })
    .unwrap();
    peer.write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .unwrap();
    peer.write_all(&bytes).await.unwrap();
    let response = call.await.unwrap().unwrap();
    assert_eq!(response.data, Some(serde_json::json!({"retried": true})));
    assert!(closed(&mut peer).await);
}

#[tokio::test]
async fn a_missing_socket_remains_unavailable() {
    let socket = Socket::new();
    let client = socket.client.clone();
    drop(socket);
    let error = client
        .transact(Request::handshake(), CALL, true)
        .await
        .unwrap_err();
    assert!(matches!(error, ServiceError::Unavailable(_)), "{error:?}");
}

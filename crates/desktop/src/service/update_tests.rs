use super::*;

#[test]
fn update_status_survives_the_client_protocol_mirror() {
    let json = serde_json::json!({
        "service": "PPVPN Service", "version": "0.7.5", "build_id": "build-id", "build": 43,
        "update": {"version": "0.7.6", "build": 44, "phase": "installing"},
        "last_update": {"version": "0.7.5", "build": 43, "outcome": "installed",
            "error": null, "finished_at_ms": 1234}
    });
    let parsed: VersionInfo = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), json);
    let old: VersionInfo = serde_json::from_value(serde_json::json!({
        "service": "PPVPN Service", "version": "0.7.4"
    }))
    .unwrap();
    assert_eq!(old.build, None, "old service must not look update-capable");
    assert!(old.update.is_none());
    assert!(old.last_update.is_none());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    fn read(stream: &mut UnixStream) -> Request {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut length = [0; 4];
        stream.read_exact(&mut length).unwrap();
        let mut bytes = vec![0; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut bytes).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn write(stream: &mut UnixStream, response: &Response) {
        let bytes = serde_json::to_vec(response).unwrap();
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&bytes).unwrap();
    }

    async fn exchange(
        data: Option<Value>,
        error: Option<&str>,
        bad_signature: bool,
        old: bool,
    ) -> (Result<(), ServiceError>, Request) {
        let directory = std::env::temp_dir().join(format!(
            "pu-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("s.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let error = error.map(str::to_string);
        let worker = std::thread::spawn(move || {
            let key = [7; 32];
            let (mut stream, _) = listener.accept().unwrap();
            let request = read(&mut stream);
            assert_eq!(request.command, Command::Handshake);
            write(
                &mut stream,
                &Response {
                    id: request.id,
                    success: true,
                    data: Some(
                        serde_json::json!({"auth_token": "token", "auth_key": hex::encode(key), "expires_at": now_epoch() + 120}),
                    ),
                    error: None,
                    signature: String::new(),
                },
            );
            drop(stream);
            let (mut stream, _) = listener.accept().unwrap();
            let request = read(&mut stream);
            assert_eq!(request.command, Command::InstallUpdate);
            assert_eq!(request.auth_token.as_deref(), Some("token"));
            // Match the service's canonical form: declaration-order JSON with
            // the signature present but empty, not an alphabetically sorted map.
            let bare = Request {
                signature: String::new(),
                ..request.clone()
            };
            assert_eq!(
                request.signature,
                sign_with(&key, &serde_json::to_string(&bare).unwrap()).unwrap()
            );
            if !old {
                let mut response = Response {
                    id: request.id.clone(),
                    success: error.is_none(),
                    data,
                    error,
                    signature: String::new(),
                };
                response.signature =
                    sign_with(&key, &serde_json::to_string(&response).unwrap()).unwrap();
                if bad_signature {
                    response.signature = "00".repeat(32);
                }
                write(&mut stream, &response);
            }
            request
        });
        let result = ServiceClient::new(socket.to_string_lossy())
            .install_update(InstallUpdatePayload {
                path: "/cache/update.pkg".into(),
                version: "0.7.5".into(),
                build: 43,
                length: 123,
                ed_signature: "signature".into(),
                relaunch: false,
            })
            .await;
        let request = worker.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        (result, request)
    }

    #[tokio::test]
    async fn install_update_uses_the_upstream_payload_and_signed_acknowledgement() {
        let (result, request) = exchange(
            Some(serde_json::json!({"accepted": true})),
            None,
            false,
            false,
        )
        .await;
        result.unwrap();
        assert_eq!(
            request.payload,
            serde_json::json!({
                "path": "/cache/update.pkg", "version": "0.7.5", "build": 43,
                "length": 123, "ed_signature": "signature", "relaunch": false
            })
        );
    }

    #[tokio::test]
    async fn install_update_rejects_missing_false_unsigned_and_old_service_answers() {
        for data in [
            serde_json::json!({}),
            serde_json::json!({"accepted": false}),
        ] {
            let (result, _) = exchange(Some(data), None, false, false).await;
            assert!(matches!(result, Err(ServiceError::Protocol(_))));
        }
        let (unsigned, _) = exchange(
            Some(serde_json::json!({"accepted": true})),
            None,
            true,
            false,
        )
        .await;
        assert!(matches!(unsigned, Err(ServiceError::Protocol(_))));
        let (old, _) = exchange(None, None, false, true).await;
        assert!(old.is_err());
    }

    #[tokio::test]
    async fn install_update_preserves_busy_and_unsupported_reason_codes() {
        for code in [
            "UPDATE_BUSY",
            "UPDATE_UNSUPPORTED",
            "UPDATE_SIGNATURE_INVALID: detail",
        ] {
            let (result, _) = exchange(None, Some(code), false, false).await;
            assert_eq!(result.unwrap_err().detail(), code);
        }
    }
}

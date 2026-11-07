//! Isolated native validation of the unchanged production pipe module.
//! These shims supply its existing parent error variants and constants only.

#[allow(dead_code)]
#[derive(Debug)]
enum ServiceError {
    Unavailable(String),
    Protocol(String),
    Rejected(String),
}

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const EXPECTED_CLIENT_LOCATION: &str = "ppvpn.exe in the service's install directory";

mod async_pipe;

//! Silent desktop updates, the app's side (#548, docs/host-integration.md
//! section 16): the channel pointer's package for this platform, downloaded
//! and checked against the pointer, handed to the privileged service with
//! `InstallUpdate`; and the service's `GetVersion` for an update in
//! progress and the last result. The app decides when to install (section
//! 16.1) and counts the failures (16.5, [`failure_kind`]).
//!
//! The package is the pointer's `install_*` entry on macOS (the .pkg; a
//! release without one has no silent update there) and the setup.exe on
//! Windows. Linux has none.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::service::{InstallUpdatePayload, ServiceApi, ServiceClient, ServiceError};
pub use crate::service::{UpdateProgress, UpdateResult};

/// The service refuses anything larger (section 16.3).
const MAX_PACKAGE: u64 = 1 << 30;
const POINTER_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A download that stalls this long is abandoned.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);

/// The pointer's platform key of this build, `None` where there are no
/// silent updates.
pub fn platform() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("macos-arm64")
    } else if cfg!(target_os = "macos") {
        Some("macos-x64")
    } else if cfg!(windows) {
        Some("windows-x64")
    } else {
        None
    }
}

/// A newer release's package for this platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePackage {
    pub version: String,
    pub build: u64,
    pub file: String,
    pub url: String,
    pub length: u64,
    /// Hex, lower case.
    pub sha256: String,
    pub ed_signature: String,
}

/// The channel's release, for this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Latest {
    /// Not newer than this build.
    UpToDate,
    Newer(UpdatePackage),
    /// Newer, but with no silent package for this platform (a macOS release
    /// that signed no pkg): Sparkle's prompt.
    NoPackage,
}

/// What the service says about silent updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceUpdates {
    /// Not installed, not running, or older than `InstallUpdate`: the app
    /// keeps Sparkle's or WinSparkle's prompt (section 16.6).
    Unsupported,
    Supported {
        /// The service's own build number.
        build: u64,
        update: Option<UpdateProgress>,
        last_update: Option<UpdateResult>,
    },
}

/// How `InstallUpdate` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallAnswer {
    /// The service installs it once the app has exited.
    Accepted,
    /// `success=false` with this `UPDATE_*` code (see [`failure_kind`]).
    Refused(String),
    /// No answer (the connection failed, closed or timed out): nothing is
    /// installed (section 16.3).
    NoAnswer(String),
}

/// What a failure's code means for the app (section 16.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Counts against this build: three, and the app goes back to the prompt.
    Counted,
    /// Not counted: tried again when the app is next idle.
    Retry,
    /// Back to the prompt at once.
    Fallback,
}

/// The kind of an `UPDATE_*` code, matched on the part before a colon
/// (`UPDATE_INSTALL_FAILED:<detail>`). Codes this app does not know are
/// counted.
pub fn failure_kind(code: &str) -> FailureKind {
    match code.split(':').next().unwrap_or_default().trim() {
        "UPDATE_BUSY" | "UPDATE_IN_PROGRESS" | "UPDATE_CLIENT_STILL_RUNNING" => FailureKind::Retry,
        "UPDATE_UNSUPPORTED" | "UPDATE_NOT_NEWER" => FailureKind::Fallback,
        _ => FailureKind::Counted,
    }
}

/// The release in a channel pointer (tools/desktop/ci/update_site.py,
/// schema 2) for `platform` and this `build`.
pub fn package_in_pointer(pointer: &Value, platform: &str, build: u64) -> Result<Latest, String> {
    let version = pointer
        .get("version")
        .and_then(Value::as_str)
        .filter(|version| !version.trim().is_empty())
        .ok_or("the pointer has no version")?;
    let newer = pointer
        .get("build")
        .and_then(Value::as_u64)
        .ok_or("the pointer has no build")?;
    if newer <= build {
        return Ok(Latest::UpToDate);
    }
    let entry = pointer
        .get("platforms")
        .and_then(|platforms| platforms.get(platform))
        .ok_or_else(|| format!("the pointer has no {platform}"))?;
    // The .pkg on macOS; a release without one keeps Sparkle's prompt.
    let prefix = if platform.starts_with("macos") {
        "install_"
    } else {
        ""
    };
    let text = |key: &str| {
        entry
            .get(format!("{prefix}{key}"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let signature = if prefix.is_empty() {
        "ed_signature"
    } else {
        "install_ed_signature"
    };
    let (Some(file), Some(url), Some(sha256), Some(ed_signature)) = (
        text("file"),
        text("url"),
        text("sha256"),
        entry
            .get(signature)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty()),
    ) else {
        return Ok(Latest::NoPackage);
    };
    let length = entry
        .get(format!("{prefix}length"))
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{platform}: no {prefix}length"))?;
    if !url.starts_with("https://") || !url.ends_with(&format!("/{file}")) {
        return Err(format!("{platform}: {url} is not an https URL of {file}"));
    }
    if file.contains(['/', '\\']) || file.starts_with('.') {
        return Err(format!("{platform}: {file} is not a file name"));
    }
    if length == 0 || length > MAX_PACKAGE {
        return Err(format!("{platform}: length {length}"));
    }
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{platform}: sha256 {sha256}"));
    }
    Ok(Latest::Newer(UpdatePackage {
        version: version.trim().to_string(),
        build: newer,
        file: file.to_string(),
        url: url.to_string(),
        length,
        sha256: sha256.to_ascii_lowercase(),
        ed_signature: ed_signature.to_string(),
    }))
}

/// The app's side of silent updates for one build.
pub struct SilentUpdater {
    service: Box<dyn ServiceApi>,
    http: reqwest::Client,
    pointer_url: String,
    platform: &'static str,
    build: u64,
    cache: PathBuf,
}

impl SilentUpdater {
    /// `pointer_url` is the channel's `desktop/channels/<channel>.json`,
    /// `build` this build's number, `cache` a directory of the user's own
    /// for the download (emptied of anything else). `None` on a system
    /// without silent updates.
    pub fn new(pointer_url: String, build: u64, cache: PathBuf, user_agent: &str) -> Option<Self> {
        Self::with_service(
            Box::new(ServiceClient::default()),
            pointer_url,
            build,
            cache,
            user_agent,
        )
    }

    fn with_service(
        service: Box<dyn ServiceApi>,
        pointer_url: String,
        build: u64,
        cache: PathBuf,
        user_agent: &str,
    ) -> Option<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(user_agent)
            .build()
            .ok()?;
        Some(Self {
            service,
            http,
            pointer_url,
            platform: platform()?,
            build,
            cache,
        })
    }

    /// The channel's release for this build.
    pub async fn latest(&self) -> Result<Latest, String> {
        let response = self
            .http
            .get(&self.pointer_url)
            .timeout(POINTER_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| format!("pointer: {error}"))?;
        let pointer: Value = response
            .json()
            .await
            .map_err(|error| format!("pointer: {error}"))?;
        package_in_pointer(&pointer, self.platform, self.build)
    }

    /// The package in the cache, downloaded unless a copy of the pointer's
    /// length and SHA-256 is there already; other files there are removed.
    pub async fn download(&self, package: &UpdatePackage) -> Result<PathBuf, String> {
        let path = self.cache.join(&package.file);
        tokio::fs::create_dir_all(&self.cache)
            .await
            .map_err(|error| format!("{}: {error}", self.cache.display()))?;
        remove_others(&self.cache, &package.file).await;
        if matches(&path, package).await {
            return Ok(path);
        }
        let partial = self.cache.join(format!("{}.part", package.file));
        let result = self.fetch(package, &partial).await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(error);
        }
        tokio::fs::rename(&partial, &path)
            .await
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(path)
    }

    async fn fetch(&self, package: &UpdatePackage, partial: &Path) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let mut response = self
            .http
            .get(&package.url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| format!("download: {error}"))?;
        let mut file = tokio::fs::File::create(partial)
            .await
            .map_err(|error| format!("{}: {error}", partial.display()))?;
        let mut hash = Sha256::new();
        let mut written = 0u64;
        loop {
            let chunk = tokio::time::timeout(CHUNK_TIMEOUT, response.chunk())
                .await
                .map_err(|_| "download: stalled".to_string())?
                .map_err(|error| format!("download: {error}"))?;
            let Some(chunk) = chunk else { break };
            written += chunk.len() as u64;
            if written > package.length {
                return Err(format!("download: more than {} bytes", package.length));
            }
            hash.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|error| format!("{}: {error}", partial.display()))?;
        }
        file.flush().await.map_err(|error| error.to_string())?;
        if written != package.length {
            return Err(format!("download: {written} of {} bytes", package.length));
        }
        if hex::encode(hash.finalize()) != package.sha256 {
            return Err("download: SHA-256 differs from the pointer's".to_string());
        }
        Ok(())
    }

    /// The service's `GetVersion`, for silent updates.
    pub async fn service_updates(&self) -> ServiceUpdates {
        match self.service.get_version().await {
            Ok(version) => match version.build {
                Some(build) => ServiceUpdates::Supported {
                    build,
                    update: version.update,
                    last_update: version.last_update,
                },
                None => ServiceUpdates::Unsupported,
            },
            Err(error) => {
                tracing::info!(error = %error.detail(), "silent update: no service");
                ServiceUpdates::Unsupported
            }
        }
    }

    /// Hands the downloaded `package` at `path` to the service; with
    /// `relaunch` the service opens the app again afterwards (section 16.1).
    pub async fn install(
        &self,
        path: &Path,
        package: &UpdatePackage,
        relaunch: bool,
    ) -> InstallAnswer {
        let payload = InstallUpdatePayload {
            path: path.display().to_string(),
            version: package.version.clone(),
            build: package.build,
            length: package.length,
            ed_signature: package.ed_signature.clone(),
            relaunch,
        };
        match self.service.install_update(&payload).await {
            Ok(()) => InstallAnswer::Accepted,
            Err(ServiceError::Failed(code)) => InstallAnswer::Refused(code),
            Err(error) => InstallAnswer::NoAnswer(error.detail()),
        }
    }
}

/// A file of the package's length and SHA-256 at `path`.
async fn matches(path: &Path, package: &UpdatePackage) -> bool {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return false;
    };
    if !metadata.is_file() || metadata.len() != package.length {
        return false;
    }
    let path = path.to_path_buf();
    let expected = package.sha256.clone();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path).ok()?;
        let mut hash = Sha256::new();
        std::io::copy(&mut file, &mut hash).ok()?;
        Some(hex::encode(hash.finalize()) == expected)
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// Earlier downloads, and partial ones.
async fn remove_others(cache: &Path, keep: &str) {
    let Ok(mut entries) = tokio::fs::read_dir(cache).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_str() != Some(keep) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ipc::{ApplyChoices, BoxFuture};
    use crate::service::{ServiceStatus, SessionRef, VersionInfo};
    use crate::RoutingMode;
    use std::sync::{Arc, Mutex};

    fn pointer() -> Value {
        serde_json::json!({
            "schema": 2, "channel": "stable", "version": "0.7.6", "build": 420,
            "platforms": {
                "macos-arm64": {
                    "file": "PPVPN-0.7.6.dmg", "url": "https://example.com/v0.7.6/PPVPN-0.7.6.dmg",
                    "length": 10, "sha256": "aa".repeat(32), "ed_signature": "dmg",
                    "install_file": "PPVPN-0.7.6.pkg",
                    "install_url": "https://example.com/v0.7.6/PPVPN-0.7.6.pkg",
                    "install_length": 20, "install_sha256": "BB".repeat(32),
                    "install_ed_signature": "pkg"
                },
                "macos-x64": {
                    "file": "PPVPN-0.7.6.dmg", "url": "https://example.com/v0.7.6/PPVPN-0.7.6.dmg",
                    "length": 10, "sha256": "aa".repeat(32), "ed_signature": "dmg"
                },
                "windows-x64": {
                    "file": "PPVPN-Setup.exe", "url": "https://example.com/v0.7.6/PPVPN-Setup.exe",
                    "length": 30, "sha256": "cc".repeat(32), "ed_signature": "exe"
                }
            }
        })
    }

    #[test]
    fn macos_takes_the_pkg_and_windows_the_setup() {
        let Latest::Newer(pkg) = package_in_pointer(&pointer(), "macos-arm64", 412).unwrap() else {
            panic!("no pkg")
        };
        assert_eq!(pkg.file, "PPVPN-0.7.6.pkg");
        assert_eq!(pkg.length, 20);
        assert_eq!(pkg.sha256, "bb".repeat(32));
        assert_eq!(pkg.ed_signature, "pkg");
        assert_eq!((pkg.version.as_str(), pkg.build), ("0.7.6", 420));
        let Latest::Newer(exe) = package_in_pointer(&pointer(), "windows-x64", 412).unwrap() else {
            panic!("no setup")
        };
        assert_eq!(
            (exe.file.as_str(), exe.length, exe.ed_signature.as_str()),
            ("PPVPN-Setup.exe", 30, "exe")
        );
    }

    #[test]
    fn no_package_when_not_newer_or_without_a_pkg() {
        assert_eq!(
            package_in_pointer(&pointer(), "macos-arm64", 420).unwrap(),
            Latest::UpToDate
        );
        assert_eq!(
            package_in_pointer(&pointer(), "macos-arm64", 500).unwrap(),
            Latest::UpToDate
        );
        // A release that signed no pkg: Sparkle's prompt.
        assert_eq!(
            package_in_pointer(&pointer(), "macos-x64", 412).unwrap(),
            Latest::NoPackage
        );
    }

    #[test]
    fn a_pointer_with_a_bad_entry_is_an_error() {
        let mut bad = pointer();
        bad["platforms"]["windows-x64"]["url"] = "http://example.com/v0.7.6/PPVPN-Setup.exe".into();
        assert!(package_in_pointer(&bad, "windows-x64", 1).is_err());
        let mut bad = pointer();
        bad["platforms"]["windows-x64"]["file"] = "../PPVPN-Setup.exe".into();
        bad["platforms"]["windows-x64"]["url"] = "https://example.com/../PPVPN-Setup.exe".into();
        assert!(package_in_pointer(&bad, "windows-x64", 1).is_err());
        let mut bad = pointer();
        bad["platforms"]["windows-x64"]["length"] = (MAX_PACKAGE + 1).into();
        assert!(package_in_pointer(&bad, "windows-x64", 1).is_err());
        let mut bad = pointer();
        bad["build"] = "420".into();
        assert!(package_in_pointer(&bad, "windows-x64", 1).is_err());
    }

    #[test]
    fn failure_kinds_follow_section_16_5() {
        for code in [
            "UPDATE_BUSY",
            "UPDATE_IN_PROGRESS",
            "UPDATE_CLIENT_STILL_RUNNING",
        ] {
            assert_eq!(failure_kind(code), FailureKind::Retry, "{code}");
        }
        for code in ["UPDATE_UNSUPPORTED", "UPDATE_NOT_NEWER"] {
            assert_eq!(failure_kind(code), FailureKind::Fallback, "{code}");
        }
        for code in [
            "UPDATE_FILE_INVALID",
            "UPDATE_SIGNATURE_INVALID",
            "UPDATE_PUBLISHER_MISMATCH",
            "UPDATE_VERSION_MISMATCH",
            "UPDATE_INSTALL_FAILED:exit 1",
            "UPDATE_INSTALL_FAILED:interrupted",
            "SOMETHING_NEW",
        ] {
            assert_eq!(failure_kind(code), FailureKind::Counted, "{code}");
        }
    }

    /// The service's answers, and the payload it was sent.
    struct Fake {
        version: Result<VersionInfo, ServiceError>,
        install: Result<(), ServiceError>,
        sent: Arc<Mutex<Option<InstallUpdatePayload>>>,
    }

    impl ServiceApi for Fake {
        fn get_version(&self) -> BoxFuture<'_, Result<VersionInfo, ServiceError>> {
            Box::pin(async { self.version.clone() })
        }
        fn get_status(&self) -> BoxFuture<'_, Result<ServiceStatus, ServiceError>> {
            unreachable!()
        }
        fn connect<'a>(
            &'a self,
            _: &'a SessionRef,
            _: &'a Value,
            _: bool,
            _: RoutingMode,
            _: &'a ApplyChoices,
        ) -> BoxFuture<'a, Result<u32, ServiceError>> {
            unreachable!()
        }
        fn update_profile<'a>(
            &'a self,
            _: &'a SessionRef,
            _: &'a Value,
            _: RoutingMode,
            _: &'a ApplyChoices,
        ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
            unreachable!()
        }
        fn renew_lease<'a>(
            &'a self,
            _: &'a SessionRef,
        ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
            unreachable!()
        }
        fn disconnect<'a>(&'a self, _: &'a SessionRef) -> BoxFuture<'a, Result<(), ServiceError>> {
            unreachable!()
        }
        fn core_api<'a>(
            &'a self,
            _: &'a SessionRef,
            _: &'a str,
            _: Value,
            _: Duration,
        ) -> BoxFuture<'a, Result<Value, ServiceError>> {
            unreachable!()
        }
        fn install_update<'a>(
            &'a self,
            payload: &'a InstallUpdatePayload,
        ) -> BoxFuture<'a, Result<(), ServiceError>> {
            *self.sent.lock().unwrap() = Some(payload.clone());
            Box::pin(async { self.install.clone() })
        }
    }

    type Sent = Arc<Mutex<Option<InstallUpdatePayload>>>;

    fn updater(
        version: Result<VersionInfo, ServiceError>,
        install: Result<(), ServiceError>,
    ) -> (SilentUpdater, Sent) {
        let sent = Sent::default();
        let fake = Fake {
            version,
            install,
            sent: sent.clone(),
        };
        let updater = SilentUpdater::with_service(
            Box::new(fake),
            "https://example.com/desktop/channels/stable.json".into(),
            412,
            std::env::temp_dir().join("ppvpn-silent-update-test"),
            "PPVPN/test",
        )
        .expect("a system with silent updates");
        (updater, sent)
    }

    fn package() -> UpdatePackage {
        match package_in_pointer(&pointer(), "windows-x64", 412).unwrap() {
            Latest::Newer(package) => package,
            other => panic!("{other:?}"),
        }
    }

    #[cfg(any(target_os = "macos", windows))]
    #[tokio::test]
    async fn an_older_service_has_no_silent_updates() {
        let old = VersionInfo {
            service: "PPVPN Service".into(),
            version: "0.7.4".into(),
            ..VersionInfo::default()
        };
        assert_eq!(
            updater(Ok(old), Ok(())).0.service_updates().await,
            ServiceUpdates::Unsupported
        );
        let missing = Err(ServiceError::Unavailable("connect: no such file".into()));
        assert_eq!(
            updater(missing, Ok(())).0.service_updates().await,
            ServiceUpdates::Unsupported
        );
        let current = VersionInfo {
            build: Some(412),
            ..VersionInfo::default()
        };
        assert_eq!(
            updater(Ok(current), Ok(())).0.service_updates().await,
            ServiceUpdates::Supported {
                build: 412,
                update: None,
                last_update: None
            }
        );
    }

    #[cfg(any(target_os = "macos", windows))]
    #[tokio::test]
    async fn install_sends_the_package_and_maps_the_answer() {
        let (accepted, sent) = updater(Ok(VersionInfo::default()), Ok(()));
        let path = Path::new("/tmp/PPVPN-Setup.exe");
        assert_eq!(
            accepted.install(path, &package(), true).await,
            InstallAnswer::Accepted
        );
        assert_eq!(
            sent.lock().unwrap().clone().unwrap(),
            InstallUpdatePayload {
                path: path.display().to_string(),
                version: "0.7.6".into(),
                build: 420,
                length: 30,
                ed_signature: "exe".into(),
                relaunch: true,
            }
        );
        let (refused, _) = updater(
            Ok(VersionInfo::default()),
            Err(ServiceError::Failed("UPDATE_BUSY".into())),
        );
        assert_eq!(
            refused.install(path, &package(), false).await,
            InstallAnswer::Refused("UPDATE_BUSY".into())
        );
        let (closed, _) = updater(
            Ok(VersionInfo::default()),
            Err(ServiceError::Protocol("read: connection closed".into())),
        );
        assert!(matches!(
            closed.install(path, &package(), false).await,
            InstallAnswer::NoAnswer(_)
        ));
    }
}

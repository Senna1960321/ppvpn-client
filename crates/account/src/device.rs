//! Device registration for push: the install's stable id, the
//! `POST /devices/register` request and the registration state of a
//! signed-in session.
//!
//! Desktop installs register with a stable `installation_id`
//! (`<data_dir>/installation.json`, kept across sign-outs) and get the
//! self-hosted channel's push token back; what the host does with it (the
//! desktop hands it to its push agent) stays with the host. Phones register
//! the APNs or FCM token their OS gave them instead (#539).

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Holds the installation id in the host's data directory.
pub const INSTALLATION_FILE: &str = "installation.json";

/// Registration progress of the signed-in session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeviceState {
    /// Not registered yet (or the last attempt failed transiently).
    #[default]
    Pending,
    Registered,
    /// Refused because the access token lacks `device:register`; retried
    /// after the next access-token refresh.
    AwaitingRefresh,
}

/// The `POST /devices/register` body.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct DeviceRegistrationRequest {
    /// [`push_platform`] of the host platform.
    pub platform: String,
    /// Desktop (self-hosted channel) only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<String>,
    /// The APNs device token or FCM registration token (official channels).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The app's bundle id / package name, when the host gives one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    pub app_version: String,
    /// Language tag, e.g. `zh-CN`.
    pub locale: String,
    /// IANA time zone, e.g. `Asia/Shanghai`.
    pub timezone: String,
}

/// `ios` / `macos` / `android` for APNs / FCM; Windows and Linux use
/// the `desktop-*` self-hosted channel.
pub fn push_platform(platform: &str) -> String {
    if official_push(platform) {
        platform.to_string()
    } else {
        format!("desktop-{platform}")
    }
}

/// The platform registers an OS push token (APNs, FCM) rather than an
/// installation id.
pub fn official_push(platform: &str) -> bool {
    matches!(platform, "ios" | "macos" | "android")
}

#[derive(Serialize, Deserialize)]
struct InstallationFile {
    installation_id: String,
}

/// Writes `bytes` to `path` atomically (temp file + rename), private to the
/// user on Unix (0600); Windows inherits the profile directory's ACL.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    })();
    result.map_err(|error: std::io::Error| {
        let _ = std::fs::remove_file(&tmp);
        format!("write {}: {error}", path.display())
    })
}

/// The stable id of this install, created on first use.
pub fn installation_id(data_dir: &str) -> Result<String, String> {
    let path = Path::new(data_dir).join(INSTALLATION_FILE);
    if let Some(id) = std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<InstallationFile>(&raw).ok())
        .map(|file| file.installation_id)
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
    {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let raw = serde_json::to_vec(&InstallationFile {
        installation_id: id.clone(),
    })
    .map_err(|error| error.to_string())?;
    write_private(&path, &raw)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(prefix: &str) -> String {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn installation_id_is_stable_and_private() {
        let dir = temp_dir("ppvpn-install-test");
        let first = installation_id(&dir).unwrap();
        assert_eq!(installation_id(&dir).unwrap(), first);
        assert!(uuid::Uuid::parse_str(&first).is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(Path::new(&dir).join(INSTALLATION_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // An unusable file is replaced by a new id.
        std::fs::write(
            Path::new(&dir).join(INSTALLATION_FILE),
            r#"{"installation_id":"not-a-uuid"}"#,
        )
        .unwrap();
        let second = installation_id(&dir).unwrap();
        assert_ne!(second, first);
        assert!(uuid::Uuid::parse_str(&second).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn private_writes_replace_the_file_and_leave_no_temp_files() {
        let dir = temp_dir("ppvpn-write-test");
        let path = Path::new(&dir).join("nested").join("file.json");
        write_private(&path, b"one").unwrap();
        write_private(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let entries = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(entries, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn registration_body_and_platform() {
        assert_eq!(push_platform("windows"), "desktop-windows");
        assert_eq!(push_platform("ios"), "ios");
        assert_eq!(push_platform("android"), "android");
        assert_eq!(push_platform("macos"), "macos");
        assert_eq!(push_platform("linux"), "desktop-linux");
        let body = serde_json::to_value(DeviceRegistrationRequest {
            platform: push_platform("windows"),
            installation_id: Some("id".into()),
            token: None,
            bundle_id: None,
            app_version: "1.2.3".into(),
            locale: "zh-CN".into(),
            timezone: "UTC".into(),
        })
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "platform": "desktop-windows",
                "installation_id": "id",
                "app_version": "1.2.3",
                "locale": "zh-CN",
                "timezone": "UTC",
            })
        );
        let body = serde_json::to_value(DeviceRegistrationRequest {
            platform: push_platform("ios"),
            installation_id: None,
            token: Some("apns".into()),
            bundle_id: Some("com.example.app".into()),
            app_version: "1.2.3".into(),
            locale: "zh-CN".into(),
            timezone: "UTC".into(),
        })
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "platform": "ios",
                "token": "apns",
                "bundle_id": "com.example.app",
                "app_version": "1.2.3",
                "locale": "zh-CN",
                "timezone": "UTC",
            })
        );
    }
}

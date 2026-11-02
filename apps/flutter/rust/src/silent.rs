//! Silent updates on macOS and Windows (#548), around
//! `ppvpn_client::silent_update`: the 「自动更新」 switch, the downloaded
//! package, and what the app keeps between launches in
//! `silent-update.json` in the data directory:
//! - `auto_update`: the switch (absent before the first launch of a build
//!   with it: updates.dart takes the old automatic-check switch's value);
//! - `seen_finished_at_ms`: the service's last result already handled
//!   (docs/host-integration.md section 16.5);
//! - `failures`: counted failures per build, three and that build goes back
//!   to Sparkle's or WinSparkle's prompt (`given_up`); `retries`: the others,
//!   [`MAX_RETRIES`] and the same;
//! - `start_hidden_at_ms`: set before an idle install with `relaunch`, so
//!   that the app the service opens again starts in the tray (section
//!   16.1); a mark older than [`HIDDEN_FOR`] is stale (the service did not
//!   open the app).
//!
//! A file that is there but unreadable reads as the switch on and its last
//! result handled, never as the switch's first launch.
//!
//! The pointer is `<PPVPN_UPDATE_SITE>/desktop/channels/<channel>.json`; a
//! build without a site, a channel or a build number has none of it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ppvpn_client::silent_update::{
    failure_kind, FailureKind, InstallAnswer, Latest, ServiceUpdates, SilentUpdater, UpdatePackage,
};
use serde::{Deserialize, Serialize};

use crate::api::{UiSilentPrepared, UiSilentUpdate};
use crate::update::baked;

/// Counted failures of one build before it goes back to the prompt.
const MAX_FAILURES: u32 = 3;
/// Failures that are no verdict on the package (the service busy or
/// stopping, the app not exiting) before the same: at most about a few days
/// of tries.
const MAX_RETRIES: u32 = 20;
/// The service waits a minute for the app and installs for at most ten.
const HIDDEN_FOR: Duration = Duration::from_secs(15 * 60);
/// The app's own code for a download that failed its checks or stalled.
const DOWNLOAD_FAILED: &str = "UPDATE_DOWNLOAD_FAILED";

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Saved {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auto_update: Option<bool>,
    #[serde(default)]
    seen_finished_at_ms: u64,
    #[serde(default)]
    failures: BTreeMap<u64, u32>,
    #[serde(default)]
    retries: BTreeMap<u64, u32>,
    #[serde(default)]
    given_up: Vec<u64>,
    #[serde(default)]
    start_hidden_at_ms: u64,
}

impl Saved {
    /// Records a failure's `code` for `build`; true when that build now
    /// goes back to the prompt.
    fn fail(&mut self, build: u64, code: &str) -> bool {
        let (counts, limit) = match failure_kind(code) {
            FailureKind::Fallback => return self.give_up(build),
            FailureKind::Counted => (&mut self.failures, MAX_FAILURES),
            FailureKind::Retry => (&mut self.retries, MAX_RETRIES),
        };
        let count = counts.entry(build).or_default();
        *count += 1;
        if *count >= limit {
            self.give_up(build)
        } else {
            false
        }
    }

    fn give_up(&mut self, build: u64) -> bool {
        if !self.given_up.contains(&build) {
            self.given_up.push(build);
        }
        true
    }
}

/// The file as read.
enum Loaded {
    Missing,
    /// There, but not readable as the state.
    Unreadable,
    Read(Saved),
}

fn path() -> Result<PathBuf, String> {
    let (data, _) = crate::dirs::dirs()?;
    Ok(PathBuf::from(data).join("silent-update.json"))
}

fn read() -> Loaded {
    let Ok(path) = path() else {
        return Loaded::Unreadable;
    };
    match std::fs::read(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Loaded::Missing,
        Err(_) => Loaded::Unreadable,
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(saved) => Loaded::Read(saved),
            Err(_) => Loaded::Unreadable,
        },
    }
}

fn load() -> Saved {
    match read() {
        Loaded::Read(saved) => saved,
        Loaded::Missing => Saved::default(),
        Loaded::Unreadable => {
            tracing::warn!("silent update: state unreadable");
            Saved {
                auto_update: Some(true),
                ..Saved::default()
            }
        }
    }
}

/// A file whole: a temporary file, then a rename. False when not saved.
fn store(saved: &Saved) -> bool {
    let result = (|| {
        let path = path()?;
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
        }
        let staging = path.with_extension("json.new");
        let bytes = serde_json::to_vec_pretty(saved).map_err(|error| error.to_string())?;
        std::fs::write(&staging, bytes).map_err(|error| error.to_string())?;
        std::fs::rename(&staging, &path).map_err(|error| error.to_string())
    })();
    if let Err(error) = &result {
        tracing::warn!(%error, "silent update: state not saved");
    }
    result.is_ok()
}

/// Serialises the reads and writes of the file.
static LOCK: Mutex<()> = Mutex::new(());

/// Changes the file; false when the change was not saved.
fn update(change: impl FnOnce(&mut Saved)) -> bool {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut saved = load();
    change(&mut saved);
    store(&saved)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// This build's number, 0 without one.
fn build() -> u64 {
    baked(option_env!("PPVPN_BUILD_NUMBER"))
        .and_then(|build| build.parse().ok())
        .unwrap_or(0)
}

/// The updater, on macOS and Windows in a release build.
fn updater() -> Option<&'static SilentUpdater> {
    static UPDATER: OnceLock<Option<SilentUpdater>> = OnceLock::new();
    UPDATER
        .get_or_init(|| {
            let site = baked(option_env!("PPVPN_UPDATE_SITE"))?;
            let channel = baked(option_env!("PPVPN_RELEASE_CHANNEL"))?;
            let build = Some(build()).filter(|build| *build > 0)?;
            let pointer = format!(
                "{}/desktop/channels/{channel}.json",
                site.trim_end_matches('/')
            );
            let cache = cache_dir()?;
            let agent = format!("PPVPN/{}", crate::update::build_version());
            SilentUpdater::new(pointer, build, cache, &agent)
        })
        .as_ref()
}

/// The user's own cache: `~/Library/Caches/PPVPN/updates`,
/// `%LOCALAPPDATA%\PPVPN\updates`.
fn cache_dir() -> Option<PathBuf> {
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    if cfg!(target_os = "macos") {
        Some(PathBuf::from(env("HOME")?).join("Library/Caches/PPVPN/updates"))
    } else if cfg!(windows) {
        Some(
            PathBuf::from(env("LOCALAPPDATA")?)
                .join("PPVPN")
                .join("updates"),
        )
    } else {
        None
    }
}

/// The downloaded package, ready to hand to the service.
static READY: Mutex<Option<(PathBuf, UpdatePackage)>> = Mutex::new(None);

/// No package any more: none ready, and none in the cache.
fn drop_package() {
    *READY.lock().unwrap() = None;
    if let Some(updater) = updater() {
        updater.clear_cache();
    }
}

/// See [`crate::api::auto_update`].
pub(crate) fn auto_update() -> Option<bool> {
    match read() {
        Loaded::Missing => None,
        _ => load().auto_update.or(Some(true)),
    }
}

/// See [`crate::api::set_auto_update`].
pub(crate) fn set_auto_update(enabled: bool) -> bool {
    if !enabled {
        drop_package();
    }
    update(|saved| saved.auto_update = Some(enabled))
}

/// See [`crate::api::start_hidden`].
pub(crate) fn start_hidden() -> bool {
    let at = load().start_hidden_at_ms;
    at > 0 && now_ms().saturating_sub(at) < HIDDEN_FOR.as_millis() as u64
}

/// See [`crate::api::clear_start_hidden`].
pub(crate) fn clear_start_hidden() {
    if load().start_hidden_at_ms != 0 {
        update(|saved| saved.start_hidden_at_ms = 0);
    }
}

/// See [`crate::api::silent_update_state`].
pub(crate) async fn state() -> UiSilentUpdate {
    let none = UiSilentUpdate {
        available: false,
        installing: false,
        updated_to: None,
    };
    let Some(updater) = updater() else {
        return none;
    };
    let ServiceUpdates::Supported {
        update: progress,
        last_update,
        ..
    } = updater.service_updates().await
    else {
        return none;
    };
    let mut updated_to = None;
    // An unreadable file has lost which result was handled: the last one
    // is taken as handled, not counted or shown again.
    let unreadable = matches!(read(), Loaded::Unreadable);
    if let Some(last) = last_update.filter(|last| last.finished_at_ms > load().seen_finished_at_ms)
    {
        update(|saved| {
            saved.seen_finished_at_ms = last.finished_at_ms;
            if unreadable {
                return;
            }
            if last.outcome == "installed" {
                if last.build == build() {
                    updated_to = Some(last.version.clone());
                }
            } else {
                let code = last.error.as_deref().unwrap_or("UPDATE_INSTALL_FAILED");
                tracing::warn!(build = last.build, code, "silent update failed");
                saved.fail(last.build, code);
            }
        });
    }
    UiSilentUpdate {
        available: true,
        installing: progress.is_some(),
        updated_to,
    }
}

/// See [`crate::api::silent_update_prepare`].
pub(crate) async fn prepare() -> UiSilentPrepared {
    let Some(updater) = updater() else {
        return UiSilentPrepared::Prompt;
    };
    let package = match updater.latest().await {
        Ok(Latest::Newer(package)) => package,
        Ok(Latest::UpToDate) => {
            drop_package();
            return UiSilentPrepared::UpToDate;
        }
        Ok(Latest::NoPackage) => {
            drop_package();
            return UiSilentPrepared::Prompt;
        }
        Err(error) => {
            tracing::info!(%error, "silent update: no pointer");
            return UiSilentPrepared::Later;
        }
    };
    if load().given_up.contains(&package.build) {
        drop_package();
        return UiSilentPrepared::Prompt;
    }
    let ready = READY
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|(_, ready)| *ready == package);
    if ready {
        return UiSilentPrepared::Ready;
    }
    match updater.download(&package).await {
        Ok(path) => {
            tracing::info!(version = %package.version, build = package.build, "silent update ready");
            *READY.lock().unwrap() = Some((path, package));
            UiSilentPrepared::Ready
        }
        // Counted: three, and this build goes back to the prompt.
        Err(error) => {
            tracing::info!(%error, "silent update: download failed");
            let mut given_up = false;
            update(|saved| given_up = saved.fail(package.build, DOWNLOAD_FAILED));
            if given_up {
                drop_package();
                UiSilentPrepared::Prompt
            } else {
                UiSilentPrepared::Later
            }
        }
    }
}

/// See [`crate::api::silent_update_install`].
pub(crate) async fn install(relaunch: bool) -> bool {
    let Some(updater) = updater() else {
        return false;
    };
    let Some((path, package)) = READY.lock().unwrap().clone() else {
        return false;
    };
    if relaunch {
        update(|saved| saved.start_hidden_at_ms = now_ms());
    }
    let accepted = match updater.install(&path, &package, relaunch).await {
        InstallAnswer::Accepted => true,
        InstallAnswer::Refused(code) => {
            tracing::warn!(build = package.build, code = %code, "silent update refused");
            let mut given_up = false;
            update(|saved| given_up = saved.fail(package.build, &code));
            if given_up {
                drop_package();
            }
            false
        }
        InstallAnswer::NoAnswer(error) => {
            tracing::info!(%error, "silent update: no answer");
            false
        }
    };
    if !accepted {
        *READY.lock().unwrap() = None;
        if relaunch {
            clear_start_hidden();
        }
    }
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_counted_failures_give_a_build_up() {
        let mut saved = Saved::default();
        assert!(!saved.fail(420, "UPDATE_SIGNATURE_INVALID"));
        assert!(!saved.fail(420, "UPDATE_INSTALL_FAILED:exit 1"));
        assert!(!saved.fail(421, DOWNLOAD_FAILED));
        assert!(saved.fail(420, DOWNLOAD_FAILED));
        assert_eq!(saved.given_up, vec![420]);
        assert_eq!(saved.failures.get(&421), Some(&1));
    }

    #[test]
    fn retries_count_apart_and_fallbacks_give_up_at_once() {
        let mut saved = Saved::default();
        for _ in 0..(MAX_RETRIES - 1) / 2 {
            assert!(!saved.fail(420, "UPDATE_BUSY"));
            assert!(!saved.fail(420, "SERVICE_STOPPING"));
        }
        assert!(saved.failures.is_empty());
        assert!(saved.given_up.is_empty());
        assert!(saved.fail(421, "UPDATE_NOT_NEWER"));
        assert!(saved.fail(422, "UPDATE_UNSUPPORTED"));
        assert_eq!(saved.given_up, vec![421, 422]);
        let mut saved = Saved::default();
        for _ in 0..MAX_RETRIES - 1 {
            assert!(!saved.fail(420, "UPDATE_CLIENT_STILL_RUNNING"));
        }
        assert!(saved.fail(420, "UPDATE_CLIENT_STILL_RUNNING"));
    }

    #[test]
    fn the_file_reads_back_and_tolerates_missing_keys() {
        let saved: Saved = serde_json::from_str("{}").unwrap();
        assert_eq!(saved, Saved::default());
        // The first version's mark is ignored.
        let saved: Saved = serde_json::from_str(r#"{"start_hidden": true}"#).unwrap();
        assert_eq!(saved.start_hidden_at_ms, 0);
        let mut saved = Saved {
            auto_update: Some(false),
            seen_finished_at_ms: 7,
            start_hidden_at_ms: 9,
            ..Saved::default()
        };
        saved.fail(420, "UPDATE_FILE_INVALID");
        saved.fail(420, "UPDATE_BUSY");
        let text = serde_json::to_string(&saved).unwrap();
        assert_eq!(serde_json::from_str::<Saved>(&text).unwrap(), saved);
    }
}

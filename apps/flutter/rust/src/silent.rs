//! Silent updates on macOS and Windows (#548), around
//! `ppvpn_desktop::silent_update`: the 「自动更新」 switch, the downloaded
//! package, and what the app keeps between launches in
//! `silent-update.json` in the data directory:
//! - `auto_update`: the switch (absent before the first launch of a build
//!   with it: updates.dart takes the old automatic-check switch's value);
//! - `seen_finished_at_ms`: the service's last result already handled
//!   (docs/host-integration.md section 16.5);
//! - `failures`: counted failures per build; three, and that build goes
//!   back to Sparkle's or WinSparkle's prompt (`given_up`);
//! - `start_hidden`: set before an idle install with `relaunch`, so that the
//!   app the service opens again starts in the tray (section 16.1).
//!
//! The pointer is `<PPVPN_UPDATE_SITE>/desktop/channels/<channel>.json`; a
//! build without a site, a channel or a build number has none of it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use ppvpn_desktop::silent_update::{
    failure_kind, FailureKind, InstallAnswer, Latest, ServiceUpdates, SilentUpdater, UpdatePackage,
};
use serde::{Deserialize, Serialize};

use crate::api::{UiSilentPrepared, UiSilentUpdate};
use crate::update::baked;

/// Counted failures of one build before it goes back to the prompt.
const MAX_FAILURES: u32 = 3;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Saved {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auto_update: Option<bool>,
    #[serde(default)]
    seen_finished_at_ms: u64,
    #[serde(default)]
    failures: BTreeMap<u64, u32>,
    #[serde(default)]
    given_up: Vec<u64>,
    #[serde(default)]
    start_hidden: bool,
}

impl Saved {
    /// Records a failure's `code` for `build`; true when that build now
    /// goes back to the prompt.
    fn fail(&mut self, build: u64, code: &str) -> bool {
        match failure_kind(code) {
            FailureKind::Retry => false,
            FailureKind::Fallback => self.give_up(build),
            FailureKind::Counted => {
                let count = self.failures.entry(build).or_default();
                *count += 1;
                if *count >= MAX_FAILURES {
                    self.give_up(build)
                } else {
                    false
                }
            }
        }
    }

    fn give_up(&mut self, build: u64) -> bool {
        if !self.given_up.contains(&build) {
            self.given_up.push(build);
        }
        true
    }
}

fn path() -> Result<PathBuf, String> {
    let (data, _) = crate::dirs::dirs()?;
    Ok(PathBuf::from(data).join("silent-update.json"))
}

fn load() -> Saved {
    let Ok(path) = path() else {
        return Saved::default();
    };
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// A file whole: a temporary file, then a rename.
fn store(saved: &Saved) {
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
    if let Err(error) = result {
        tracing::warn!(%error, "silent update: state not saved");
    }
}

/// Serialises the reads and writes of the file.
static LOCK: Mutex<()> = Mutex::new(());

fn update(change: impl FnOnce(&mut Saved)) -> Saved {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut saved = load();
    change(&mut saved);
    store(&saved);
    saved
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

/// See [`crate::api::auto_update`].
pub(crate) fn auto_update() -> Option<bool> {
    load().auto_update
}

/// See [`crate::api::set_auto_update`].
pub(crate) fn set_auto_update(enabled: bool) {
    update(|saved| saved.auto_update = Some(enabled));
}

/// See [`crate::api::take_start_hidden`].
pub(crate) fn take_start_hidden() -> bool {
    if !load().start_hidden {
        return false;
    }
    update(|saved| saved.start_hidden = false);
    true
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
    if let Some(last) = last_update.filter(|last| last.finished_at_ms > load().seen_finished_at_ms)
    {
        update(|saved| {
            saved.seen_finished_at_ms = last.finished_at_ms;
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
        Ok(Latest::UpToDate) => return UiSilentPrepared::UpToDate,
        Ok(Latest::NoPackage) => return UiSilentPrepared::Prompt,
        Err(error) => {
            tracing::info!(%error, "silent update: no pointer");
            return UiSilentPrepared::Later;
        }
    };
    if load().given_up.contains(&package.build) {
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
        Err(error) => {
            tracing::info!(%error, "silent update: download failed");
            UiSilentPrepared::Later
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
        update(|saved| saved.start_hidden = true);
    }
    let accepted = match updater.install(&path, &package, relaunch).await {
        InstallAnswer::Accepted => true,
        InstallAnswer::Refused(code) => {
            tracing::warn!(build = package.build, code = %code, "silent update refused");
            update(|saved| {
                saved.fail(package.build, &code);
            });
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
            update(|saved| saved.start_hidden = false);
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
        assert!(!saved.fail(421, "UPDATE_FILE_INVALID"));
        assert!(saved.fail(420, "UPDATE_VERSION_MISMATCH"));
        assert_eq!(saved.given_up, vec![420]);
        assert_eq!(saved.failures.get(&421), Some(&1));
    }

    #[test]
    fn retries_are_not_counted_and_fallbacks_give_up_at_once() {
        let mut saved = Saved::default();
        for _ in 0..5 {
            assert!(!saved.fail(420, "UPDATE_BUSY"));
            assert!(!saved.fail(420, "UPDATE_CLIENT_STILL_RUNNING"));
        }
        assert!(saved.failures.is_empty());
        assert!(saved.fail(420, "UPDATE_NOT_NEWER"));
        assert!(saved.fail(420, "UPDATE_UNSUPPORTED"));
        assert_eq!(saved.given_up, vec![420]);
    }

    #[test]
    fn the_file_reads_back_and_tolerates_missing_keys() {
        let saved: Saved = serde_json::from_str("{}").unwrap();
        assert_eq!(saved, Saved::default());
        let mut saved = Saved {
            auto_update: Some(false),
            seen_finished_at_ms: 7,
            ..Saved::default()
        };
        saved.fail(420, "UPDATE_FILE_INVALID");
        let text = serde_json::to_string(&saved).unwrap();
        assert_eq!(serde_json::from_str::<Saved>(&text).unwrap(), saved);
    }
}

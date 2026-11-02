//! The app's own settings (#401 step 3): appearance, language and the API
//! override. They belong to the UI, not to the client, which keeps its own
//! state (routing mode, node) in the data directory. They are kept where
//! and as the native apps keep theirs, so that either app reads the
//! other's:
//! - macOS: the user defaults of `com.peakpassvpn.ppvpn.desktop`, keys
//!   `appearance` (`system`, `light`, `dark`) and `apiBaseOverride`
//!   (apps/macos `@AppStorage`);
//! - Windows: `app-settings.json` in the data directory (apps/windows
//!   JsonSettingsStore);
//! - Linux: `$XDG_CONFIG_HOME/ppvpn/settings.json` (apps/linux
//!   SettingsStore).
//!
//! The two files are the .NET apps' JSON: `Appearance` (`System`, `Light`,
//! `Dark`) and `ApiBaseOverride` (a string or null) among keys of theirs,
//! which are kept as they are; on Windows also `AutoCheckUpdates` (a bool,
//! update.rs). The native apps have no language setting:
//! this app keeps its own as `language` (macOS) or `Language`, with the
//! values `system`, `zh` and `en`; the native apps ignore it. With
//! `PPVPN_FLUTTER_HOME` (the end-to-end tests) the settings are the JSON
//! file `app-settings.json` in that home's data directory, on every
//! system. Anything missing or unreadable reads as the defaults.
//!
//! The API override is for development only (#519): a release build
//! neither shows it nor honours it, and keeps a saved one as it is (the
//! native apps share the file).

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::api::{UiAppearance, UiLanguage, UiSettings};
use crate::dirs::dirs;

/// The values as stored; plain strings, so that an unknown one (from a
/// later version) falls back to its default.
struct Saved {
    appearance: String,
    language: String,
    api_override: String,
}

/// Where the settings are.
enum Store {
    Json(PathBuf),
    #[cfg(target_os = "macos")]
    Defaults,
}

#[cfg(target_os = "macos")]
const DOMAIN: &str = "com.peakpassvpn.ppvpn.desktop";

fn store(data_dir: &str) -> Result<Store, String> {
    if crate::dirs::home_override().is_some() || cfg!(windows) {
        return Ok(Store::Json(
            std::path::Path::new(data_dir).join("app-settings.json"),
        ));
    }
    native_store()
}

#[cfg(target_os = "macos")]
fn native_store() -> Result<Store, String> {
    Ok(Store::Defaults)
}

/// Linux (Windows has returned above).
#[cfg(not(target_os = "macos"))]
fn native_store() -> Result<Store, String> {
    let config = crate::dirs::xdg("XDG_CONFIG_HOME", ".config")?;
    Ok(Store::Json(
        PathBuf::from(config).join("ppvpn/settings.json"),
    ))
}

fn read_json(path: &std::path::Path) -> Map<String, Value> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(Value::Object(map)) => map,
            Ok(_) | Err(_) => {
                tracing::warn!(path = %path.display(), "settings unreadable; defaults");
                Map::new()
            }
        },
        Err(_) => Map::new(),
    }
}

/// A file whole: a temporary file, then a rename.
fn write_json(path: &std::path::Path, json: &Map<String, Value>) -> Result<(), String> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?;
    }
    let staging = path.with_extension("json.new");
    let bytes = serde_json::to_vec_pretty(json).map_err(|error| error.to_string())?;
    std::fs::write(&staging, bytes).map_err(|error| format!("{}: {error}", staging.display()))?;
    std::fs::rename(&staging, path).map_err(|error| format!("{}: {error}", path.display()))
}

fn read(store: &Store) -> Saved {
    match store {
        Store::Json(path) => {
            let json = read_json(path);
            let text = |key: &str| {
                json.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            Saved {
                // System.Text.Json reads enum names in any case.
                appearance: text("Appearance").to_ascii_lowercase(),
                language: text("Language"),
                api_override: text("ApiBaseOverride").trim().to_string(),
            }
        }
        #[cfg(target_os = "macos")]
        Store::Defaults => {
            let text = |key: &str| crate::cf::preference(DOMAIN, key).unwrap_or_default();
            Saved {
                appearance: text("appearance"),
                language: text("language"),
                api_override: text("apiBaseOverride").trim().to_string(),
            }
        }
    }
}

fn write(store: &Store, saved: &Saved) -> Result<(), String> {
    match store {
        Store::Json(path) => {
            // The native app's other keys are kept.
            let mut json = read_json(path);
            let appearance = match saved.appearance.as_str() {
                "light" => "Light",
                "dark" => "Dark",
                _ => "System",
            };
            json.insert("Appearance".into(), appearance.into());
            json.insert(
                "ApiBaseOverride".into(),
                match saved.api_override.as_str() {
                    "" => Value::Null,
                    base => base.into(),
                },
            );
            json.insert("Language".into(), saved.language.as_str().into());
            write_json(path, &json)
        }
        #[cfg(target_os = "macos")]
        Store::Defaults => crate::cf::set_preferences(
            DOMAIN,
            &[
                ("appearance", saved.appearance.as_str()),
                ("apiBaseOverride", saved.api_override.as_str()),
                ("language", saved.language.as_str()),
            ],
        ),
    }
}

pub(crate) fn load() -> Result<UiSettings, String> {
    let (data_dir, _) = dirs()?;
    let saved = read(&store(&data_dir)?);
    Ok(UiSettings {
        appearance: match saved.appearance.as_str() {
            "light" => UiAppearance::Light,
            "dark" => UiAppearance::Dark,
            _ => UiAppearance::System,
        },
        language: match saved.language.as_str() {
            "zh" => UiLanguage::Chinese,
            "en" => UiLanguage::English,
            _ => UiLanguage::System,
        },
        api_override: saved.api_override,
    })
}

/// Writes the settings (a file whole: a temporary file, then a rename). The
/// override must be empty or an http(s) URL.
pub(crate) fn save(settings: UiSettings) -> Result<(), String> {
    let api_override = settings.api_override.trim().trim_end_matches('/');
    if !api_override.is_empty()
        && !api_override.starts_with("https://")
        && !api_override.starts_with("http://")
    {
        return Err(format!("not an http(s) URL: {api_override}"));
    }
    let saved = Saved {
        appearance: match settings.appearance {
            UiAppearance::System => "system",
            UiAppearance::Light => "light",
            UiAppearance::Dark => "dark",
        }
        .into(),
        language: match settings.language {
            UiLanguage::System => "system",
            UiLanguage::Chinese => "zh",
            UiLanguage::English => "en",
        }
        .into(),
        api_override: api_override.into(),
    };
    let (data_dir, _) = dirs()?;
    write(&store(&data_dir)?, &saved)
}

/// Whether this build offers and honours the API override (#519): debug
/// builds only. In a release build anyone could otherwise point the app
/// at a backend it then trusts with the sign-in and the traffic.
pub(crate) const API_OVERRIDE_ALLOWED: bool = cfg!(debug_assertions);

/// The saved API override, if any and if this build honours one; read
/// once when the client starts.
pub(crate) fn api_override(data_dir: &str) -> Option<String> {
    if !API_OVERRIDE_ALLOWED {
        return None;
    }
    let store = store(data_dir).ok()?;
    Some(read(&store).api_override).filter(|base| !base.is_empty())
}

/// Windows: the native app's "Automatically check for updates",
/// `AutoCheckUpdates` in app-settings.json; on unless saved off.
#[cfg(windows)]
pub(crate) fn auto_check_updates() -> bool {
    let Ok((data_dir, _)) = dirs() else {
        return true;
    };
    match store(&data_dir) {
        Ok(Store::Json(path)) => read_json(&path)
            .get("AutoCheckUpdates")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        _ => true,
    }
}

/// Saves [`auto_check_updates`], keeping the file's other keys.
#[cfg(windows)]
pub(crate) fn set_auto_check_updates(enabled: bool) -> Result<(), String> {
    let (data_dir, _) = dirs()?;
    let Store::Json(path) = store(&data_dir)?;
    let mut json = read_json(&path);
    json.insert("AutoCheckUpdates".into(), enabled.into());
    write_json(&path, &json)
}

/// Separate from Sparkle/WinSparkle's scheduled-prompt flag. Temporarily
/// disabling that flag must not overwrite the user's automatic-update choice.
pub(crate) fn auto_update(enabled: Option<bool>, legacy: bool) -> Result<bool, String> {
    let (data, _) = dirs()?;
    let path = PathBuf::from(data).join("auto-update-preference.json");
    let mut json = read_json(&path);
    let saved = json.get("enabled").and_then(Value::as_bool);
    let value = enabled.or(saved).unwrap_or(legacy);
    if enabled.is_some() || saved.is_none() {
        json.insert("enabled".into(), value.into());
        write_json(&path, &json)?;
    }
    Ok(value)
}

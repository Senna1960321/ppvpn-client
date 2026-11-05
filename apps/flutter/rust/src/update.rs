//! In-app updates (#401 step 5), from the native apps' feeds, as they do
//! them:
//! - Windows: WinSparkle, as apps/windows/PPVPN.Windows/Platform/AppUpdater.cs
//!   (`WinSparkle.dll` beside ppvpn.exe, from the WinSparkle package the
//!   native app references: scripts/package-windows.ps1). Once the installer
//!   runs, the app hides and waits for the installer's quit event
//!   (instance.rs), so that the installer sees it running and starts it
//!   again after the upgrade; after 30 s it quits on its own.
//! - Linux: a notice only, as apps/linux/Update/UpdateNotifier.cs: Dart
//!   compares the repository's latest.json with this build, and the
//!   system's package manager does the upgrade.
//! - macOS: Sparkle, in the runner (runner/macos/Runner/MainFlutterWindow.swift).
//!
//! The feeds and the key are baked in when the library is built (the
//! environment of ci-build.sh's cargo build), with the native builds'
//! variables:
//! - `PPVPN_VERSION`, `PPVPN_BUILD_NUMBER`: this build, `<version>.<build>`
//!   (sparkle:version, latest.json's `build`);
//! - `PPVPN_UPDATE_SITE` and `PPVPN_RELEASE_CHANNEL`: the channel's feed,
//!   `<site>/desktop/<channel>/appcast-windows-x64.xml` (Windows; as
//!   apps/windows/scripts/ci-build.ps1) or `<site>/linux/<channel>/latest.json`
//!   (Linux, stable's without a channel, as apps/linux/scripts/build-package.sh);
//!   `PPVPN_UPDATE_FEED_URL` (Windows) or `PPVPN_UPDATE_FEED` (Linux) instead;
//! - `PPVPN_SPARKLE_PUBLIC_KEY` (or `PPVPN_UPDATE_PUBLIC_KEY`): the feed's
//!   EdDSA key (Windows).
//!
//! Without a feed or a key, a Windows build has updates off; a Linux build
//! without a build number or a package format (a development build) never
//! shows the notice.

use crate::api::UiUpdater;

/// A baked-in value, None when unset or blank.
pub(crate) fn baked(value: Option<&'static str>) -> Option<&'static str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// This build's version: a release's `PPVPN_VERSION` (a dev run's comes
/// before Cargo.toml's bump), else the workspace's.
pub(crate) fn version() -> &'static str {
    baked(option_env!("PPVPN_VERSION")).unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// `<version>.<build>`, as the native builds number theirs.
pub(crate) fn build_version() -> String {
    let build = baked(option_env!("PPVPN_BUILD_NUMBER")).unwrap_or("0");
    format!("{}.{build}", version())
}

/// The channel's feed on the update site, `<site>/<path>` with `{channel}`
/// filled in.
#[cfg(any(windows, target_os = "linux"))]
fn site_feed(site: Option<&str>, channel: Option<&str>, path: &str) -> Option<String> {
    Some(format!(
        "{}/{}",
        site?.trim_end_matches('/'),
        path.replace("{channel}", channel?)
    ))
}

/// See [`crate::api::start_updater`].
pub(crate) fn start() -> UiUpdater {
    platform::start()
}

/// See [`crate::api::check_for_updates`].
pub(crate) fn check() {
    #[cfg(windows)]
    platform::check();
}

/// See [`crate::api::set_auto_check_updates`].
pub(crate) fn set_auto_check(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        crate::settings::set_auto_check_updates(enabled)?;
        platform::set_auto_check(enabled);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("no automatic checks on this system".into())
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::{c_char, c_int, CString};
    use std::sync::OnceLock;
    use std::time::Duration;

    use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
        LOAD_LIBRARY_SEARCH_SYSTEM32,
    };

    use crate::api::{UiActivation, UiLanguage, UiUpdater};

    /// winnt.h's primary language id of English.
    const LANG_ENGLISH: u16 = 0x09;
    /// HKCU key of WinSparkle's own state (last check, skipped version), the native app's.
    const REGISTRY_PATH: &str = r"Software\PPVPN\WinSparkle";
    /// How long the app waits, hidden, for the installer's quit event.
    const QUIT_FALLBACK: Duration = Duration::from_secs(30);

    /// WinSparkle's functions (winsparkle.h), as AppUpdater.cs declares them.
    struct WinSparkle {
        init: unsafe extern "C" fn(),
        set_lang: unsafe extern "C" fn(*const c_char),
        set_appcast_url: unsafe extern "C" fn(*const c_char),
        set_eddsa_public_key: unsafe extern "C" fn(*const c_char) -> c_int,
        set_app_details: unsafe extern "C" fn(*const u16, *const u16, *const u16),
        set_app_build_version: unsafe extern "C" fn(*const u16),
        set_registry_path: unsafe extern "C" fn(*const c_char),
        set_automatic_check_for_updates: unsafe extern "C" fn(c_int),
        set_can_shutdown_callback: unsafe extern "C" fn(extern "C" fn() -> c_int),
        set_shutdown_request_callback: unsafe extern "C" fn(extern "C" fn()),
        set_error_callback: unsafe extern "C" fn(extern "C" fn()),
        check_update_with_ui: unsafe extern "C" fn(),
    }

    /// Set once WinSparkle runs.
    static STARTED: OnceLock<WinSparkle> = OnceLock::new();

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn feed() -> Option<String> {
        super::baked(option_env!("PPVPN_UPDATE_FEED_URL"))
            .map(String::from)
            .or_else(|| {
                super::site_feed(
                    super::baked(option_env!("PPVPN_UPDATE_SITE")),
                    super::baked(option_env!("PPVPN_RELEASE_CHANNEL")),
                    "desktop/{channel}/appcast-windows-x64.xml",
                )
            })
            .filter(|feed| feed.starts_with("https://"))
    }

    fn public_key() -> Option<&'static str> {
        super::baked(option_env!("PPVPN_UPDATE_PUBLIC_KEY"))
            .or(super::baked(option_env!("PPVPN_SPARKLE_PUBLIC_KEY")))
    }

    fn load() -> Result<WinSparkle, String> {
        let name = wide("WinSparkle.dll");
        // SAFETY: a NUL-terminated name; the app's own directory and
        // System32 are searched. The flags apply to WinSparkle's own imports
        // too, so without System32 its system DLLs are not found (error 126)
        // and the load fails. The library stays loaded for the process's life.
        let library = unsafe {
            LoadLibraryExW(
                name.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if library.is_null() {
            return Err(format!(
                "WinSparkle.dll beside the app did not load: {}",
                std::io::Error::last_os_error()
            ));
        }
        macro_rules! symbol {
            ($name:literal) => {{
                // SAFETY: a loaded library and a NUL-terminated name.
                let proc = unsafe { GetProcAddress(library, concat!($name, "\0").as_ptr()) }
                    .ok_or(concat!($name, " not in WinSparkle.dll"))?;
                // SAFETY: the field's type is the function's in winsparkle.h
                // (cdecl is the only convention on x64).
                unsafe { function(proc) }
            }};
        }
        Ok(WinSparkle {
            init: symbol!("win_sparkle_init"),
            set_lang: symbol!("win_sparkle_set_lang"),
            set_appcast_url: symbol!("win_sparkle_set_appcast_url"),
            set_eddsa_public_key: symbol!("win_sparkle_set_eddsa_public_key"),
            set_app_details: symbol!("win_sparkle_set_app_details"),
            set_app_build_version: symbol!("win_sparkle_set_app_build_version"),
            set_registry_path: symbol!("win_sparkle_set_registry_path"),
            set_automatic_check_for_updates: symbol!("win_sparkle_set_automatic_check_for_updates"),
            set_can_shutdown_callback: symbol!("win_sparkle_set_can_shutdown_callback"),
            set_shutdown_request_callback: symbol!("win_sparkle_set_shutdown_request_callback"),
            set_error_callback: symbol!("win_sparkle_set_error_callback"),
            check_update_with_ui: symbol!("win_sparkle_check_update_with_ui"),
        })
    }

    /// An exported function as the function pointer type `F`.
    ///
    /// # Safety
    /// `F` is a function pointer of the export's signature.
    unsafe fn function<F: Copy>(proc: unsafe extern "system" fn() -> isize) -> F {
        // SAFETY: function pointers are all one size; the caller vouches for the type.
        unsafe { std::mem::transmute_copy(&proc) }
    }

    extern "C" fn can_shutdown() -> c_int {
        1
    }

    /// On a WinSparkle thread, once the installer runs: the app hides
    /// (Dart), and quits after [`QUIT_FALLBACK`] if the installer has not
    /// asked it to by then.
    extern "C" fn shutdown_requested() {
        tracing::info!("update: installer launched; hiding until it asks the app to quit");
        crate::instance::deliver(UiActivation {
            push_id: None,
            quit: false,
            update: true,
        });
        let spawned = std::thread::Builder::new()
            .name("ppvpn-update-quit".into())
            .spawn(|| {
                std::thread::sleep(QUIT_FALLBACK);
                tracing::warn!("update: the installer did not ask the app to quit; quitting");
                crate::instance::deliver(UiActivation {
                    push_id: None,
                    quit: true,
                    update: false,
                });
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "update: no quit fallback");
        }
    }

    extern "C" fn failed() {
        tracing::warn!("update: WinSparkle reported an error (feed, download or signature)");
    }

    pub(super) fn start() -> UiUpdater {
        let auto_check = crate::settings::auto_check_updates();
        let off = UiUpdater {
            installs: false,
            auto_check,
            notice_feed: None,
            build: super::build_version(),
            upgrade_command: String::new(),
        };
        if STARTED.get().is_some() {
            return UiUpdater {
                installs: true,
                ..off
            };
        }
        let (Some(feed), Some(key)) = (feed(), public_key()) else {
            tracing::info!("updates off: no feed or no public key in this build");
            return off;
        };
        let winsparkle = match load() {
            Ok(winsparkle) => winsparkle,
            Err(error) => {
                tracing::warn!("updates off: {error}");
                return off;
            }
        };
        let (Ok(feed_c), Ok(key_c), Ok(registry)) = (
            CString::new(feed.as_str()),
            CString::new(key),
            CString::new(REGISTRY_PATH),
        ) else {
            return off;
        };
        // WinSparkle's own dialogs: English for an English UI, else Chinese,
        // as the native app chooses (the app's language, else the system's).
        let english = match crate::settings::load().map(|settings| settings.language) {
            Ok(UiLanguage::English) => true,
            Ok(UiLanguage::Chinese) => false,
            // SAFETY: no arguments. The primary language is the low 10 bits.
            _ => (unsafe { GetUserDefaultUILanguage() } & 0x3ff) == LANG_ENGLISH,
        };
        let lang = CString::new(if english { "en" } else { "zh_CN" }).unwrap();
        let build = super::build_version();
        let version = build
            .rsplit_once('.')
            .map_or(build.as_str(), |(version, _)| version);
        // SAFETY: WinSparkle's documented order (set up, then init) with
        // NUL-terminated strings it copies; the callbacks are plain functions.
        unsafe {
            (winsparkle.set_lang)(lang.as_ptr());
            (winsparkle.set_appcast_url)(feed_c.as_ptr());
            if (winsparkle.set_eddsa_public_key)(key_c.as_ptr()) != 1 {
                tracing::error!("updates off: invalid EdDSA public key");
                return off;
            }
            (winsparkle.set_app_details)(
                wide("PeakPass Labs LLC").as_ptr(),
                wide("PPVPN").as_ptr(),
                wide(version).as_ptr(),
            );
            (winsparkle.set_app_build_version)(wide(&build).as_ptr());
            (winsparkle.set_registry_path)(registry.as_ptr());
            (winsparkle.set_automatic_check_for_updates)(auto_check.into());
            (winsparkle.set_can_shutdown_callback)(can_shutdown);
            (winsparkle.set_shutdown_request_callback)(shutdown_requested);
            (winsparkle.set_error_callback)(failed);
            (winsparkle.init)();
        }
        tracing::info!(%feed, %build, english, "updates on");
        let _ = STARTED.set(winsparkle);
        UiUpdater {
            installs: true,
            ..off
        }
    }

    pub(super) fn check() {
        if let Some(winsparkle) = STARTED.get() {
            tracing::info!("update: manual check");
            // SAFETY: WinSparkle runs (init above).
            unsafe { (winsparkle.check_update_with_ui)() };
        }
    }

    pub(super) fn set_auto_check(enabled: bool) {
        if let Some(winsparkle) = STARTED.get() {
            // SAFETY: as above.
            unsafe { (winsparkle.set_automatic_check_for_updates)(enabled.into()) };
            tracing::info!(enabled, "update: automatic checks");
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use crate::api::UiUpdater;

    /// "deb" or "rpm", beside the app in a package (scripts/package-linux.sh).
    fn package_format() -> Option<String> {
        let exe = std::env::current_exe().ok()?;
        let text = std::fs::read_to_string(exe.parent()?.join("package-format")).ok()?;
        Some(text.trim().to_string()).filter(|format| format == "deb" || format == "rpm")
    }

    pub(super) fn start() -> UiUpdater {
        let format = package_format();
        let feed = super::baked(option_env!("PPVPN_UPDATE_FEED"))
            .map(String::from)
            .or_else(|| {
                super::site_feed(
                    Some(
                        super::baked(option_env!("PPVPN_UPDATE_SITE"))
                            .unwrap_or("https://pkg.peakpassvpn.com"),
                    ),
                    Some(super::baked(option_env!("PPVPN_RELEASE_CHANNEL")).unwrap_or("stable")),
                    "linux/{channel}/latest.json",
                )
            })
            .filter(|feed| feed.starts_with("https://"));
        let released =
            super::baked(option_env!("PPVPN_BUILD_NUMBER")).is_some_and(|build| build != "0");
        UiUpdater {
            installs: false,
            auto_check: false,
            notice_feed: feed.filter(|_| released && format.is_some()),
            build: super::build_version(),
            upgrade_command: match format.as_deref() {
                Some("rpm") => "sudo dnf upgrade ppvpn",
                _ => "sudo apt update && sudo apt install --only-upgrade ppvpn",
            }
            .into(),
        }
    }
}

/// macOS: the runner's Sparkle.
#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use crate::api::UiUpdater;

    pub(super) fn start() -> UiUpdater {
        UiUpdater {
            installs: false,
            auto_check: false,
            notice_feed: None,
            build: super::build_version(),
            upgrade_command: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn baked_values_are_trimmed_and_blank_is_unset() {
        assert_eq!(super::baked(Some(" x ")), Some("x"));
        assert_eq!(super::baked(Some("  ")), None);
        assert_eq!(super::baked(None), None);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn site_feed_needs_a_site_and_a_channel() {
        assert_eq!(
            super::site_feed(
                Some("https://pkg.example/"),
                Some("stable"),
                "linux/{channel}/latest.json"
            )
            .as_deref(),
            Some("https://pkg.example/linux/stable/latest.json")
        );
        assert_eq!(super::site_feed(None, Some("stable"), "{channel}"), None);
        assert_eq!(
            super::site_feed(Some("https://pkg.example"), None, "{channel}"),
            None
        );
    }
}

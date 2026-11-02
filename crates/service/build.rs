include!("build_id.rs");

fn main() {
    println!("cargo:rerun-if-env-changed=PPVPN_WINDOWS_ALLOW_UNSIGNED_CLIENT");
    println!("cargo:rerun-if-env-changed=PPVPN_WINDOWS_PUBLISHER_EKU");
    println!("cargo:rerun-if-env-changed=PPVPN_WINDOWS_NEXT_PUBLISHER_EKU");
    // Silent updates (src/update.rs): the feed's key and this build's number.
    println!("cargo:rerun-if-env-changed=PPVPN_SPARKLE_PUBLIC_KEY");
    println!("cargo:rerun-if-env-changed=PPVPN_BUILD_NUMBER");

    // Clients reinstall a service whose build id differs from the one they
    // were built with (crates/desktop/build.rs computes the same value).
    let dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    for input in [
        "Cargo.toml",
        "../../Cargo.lock",
        "src",
        "build_id.rs",
        "../engine-host",
        "../core",
    ] {
        println!("cargo:rerun-if-changed={}", dir.join(input).display());
    }
    let id = service_build_id(&dir).expect("hash the service sources");
    println!("cargo:rustc-env=PPVPN_SERVICE_BUILD_ID={id}");

    version_resource();
}

/// Four-part file version of the app (package.ps1 passes the msbuild FileVersion, e.g. 0.2.90.3),
/// else Cargo's x.y.z with a 0 build. Not part of the build id: a new app version alone does not
/// make clients reinstall the service.
#[cfg(windows)]
fn file_version() -> String {
    println!("cargo:rerun-if-env-changed=PPVPN_BUILD_VERSION");
    let four_part =
        |v: &str| v.split('.').count() == 4 && v.split('.').all(|p| p.parse::<u16>().is_ok());
    match std::env::var("PPVPN_BUILD_VERSION") {
        Ok(v) if four_part(v.trim()) => v.trim().to_string(),
        Ok(v) if !v.trim().is_empty() => {
            panic!("PPVPN_BUILD_VERSION must be a four-part numeric version, not '{v}'")
        }
        _ => format!("{}.0", std::env::var("CARGO_PKG_VERSION").unwrap()),
    }
}

/// Icon and FileVersion / ProductVersion / CompanyName / ProductName on ppvpn-service*.exe, as on ppvpn.exe.
#[cfg(windows)]
fn version_resource() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = file_version();
    let packed = version
        .split('.')
        .map(|p| p.parse::<u64>().unwrap())
        .fold(0u64, |acc, p| (acc << 16) | p);
    // The app icon (assets/icons/icon.ico), so Explorer and Task Manager show PPVPN, not a blank exe.
    let icon = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../../apps/assets/icons/icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon.to_str().expect("UTF-8 icon path"));
    res.set("FileVersion", &version)
        .set("ProductVersion", &version)
        .set("CompanyName", "PeakPass Labs LLC")
        .set("ProductName", "PPVPN")
        .set("FileDescription", "PPVPN privileged service")
        .set("LegalCopyright", "© PeakPass Labs LLC")
        .set_version_info(winresource::VersionInfo::FILEVERSION, packed)
        .set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);
    res.compile().expect("compile the Windows version resource");
}

#[cfg(not(windows))]
fn version_resource() {}

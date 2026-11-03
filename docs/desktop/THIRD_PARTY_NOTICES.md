# Third-party notices (desktop)

Third-party components the desktop apps ship or link, beyond what the engine
links. For the engine see the root
[THIRD_PARTY_NOTICES.md](../../THIRD_PARTY_NOTICES.md): the Rust core, which the
client and the privileged service link, links sail and, through btls,
BoringSSL, at the commits pinned in the root `Cargo.toml`.

The desktop code itself is GPL-3.0-or-later, like the rest of the repository
([LICENSE](../../LICENSE)). Versions below are the ones the tree pins; where the
tree pins none, none is given.

## Images

| Component | Used by | Licence | Upstream |
|---|---|---|---|
| flag-icons (4:3 country flags) | the Flutter app (`apps/flutter/assets/flags`) | MIT; text in `apps/flutter/assets/flags/LICENSE` | https://github.com/lipis/flag-icons |

## macOS

| Component | Version | Licence | Upstream |
|---|---|---|---|
| Sparkle (in-app updates) | 2.10.1 (`apps/flutter/packaging/macos/SparkleArtifacts/Package.resolved`) | MIT; its bundled components carry their own notices, see upstream | https://github.com/sparkle-project/Sparkle |

## Windows

| Component | Version | Licence | Upstream |
|---|---|---|---|
| WinSparkle (in-app updates) | 0.9.2 | MIT | https://github.com/vslavik/winsparkle |
| Wintun (`wintun.dll`, the TUN driver of enhanced mode; shipped unmodified as published) | 0.14.1 | Wintun Prebuilt Binaries License; text in the release zip's `LICENSE.txt` | https://www.wintun.net |

## Rust

| Component | Version | Licence | Upstream |
|---|---|---|---|
| uniffi (bindings runtime in `ppvpn-client`, and the Swift generator) | 0.31.0 | MPL-2.0 | https://github.com/mozilla/uniffi-rs |

`ppvpn-client` and `ppvpn-service` link further crates from crates.io (tokio,
reqwest with rustls, serde, tracing, log4rs, sysinfo and others). Their exact
versions are in the root `Cargo.lock`; each crate's licence is stated in its own manifest — see upstream.

PPVPN is not affiliated with or endorsed by any of the projects above.

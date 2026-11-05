//! The Flutter desktop prototype's bridge (#387): flutter_rust_bridge
//! generates the Dart side from [`api`]. ci.yml runs the codegen, which adds
//! `mod frb_generated;` here; nothing generated is committed.

#[cfg(target_os = "android")]
mod android;
pub mod api;
mod aumid;
mod autostart;
mod bridge;
#[cfg(target_os = "macos")]
mod cf;
mod credentials;
mod dirs;
mod hooks;
mod instance;
mod messages;
mod push_agent;
mod settings;
mod silent;
mod update;

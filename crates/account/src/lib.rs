//! PPVPN account client: the backend HTTP API a signed-in app uses and the
//! browser device login that issues its credentials.
//!
//! The crate's only platform code reads the OS proxy for its API calls
//! ([`sysproxy`]). A host provides:
//! - an [`api::ApiConfig`] (backend base URL, product audiences, language,
//!   local-backend test relaxations);
//! - an [`auth::AuthConfig`] (where the browser authorization page lives);
//! - a [`auth::CredentialStore`] over its secret store (Keychain, Credential
//!   Manager, Secret Service).
//!
//! [`profile`] is the UI view of a proxy profile, [`device`] registers an
//! install for push, and [`session`] holds the rules a signed-in host
//! follows between calls. [`reachability`] checks what the resolvers
//! answer for the API and node hosts and whether each address completes TLS.
//!
//! Errors stay structured ([`api::ApiError`], [`auth::AuthError`]); hosts map
//! them to their own user-facing codes.

pub mod api;
pub mod auth;
pub mod device;
pub mod profile;
pub mod reachability;
mod service_dns;
pub mod session;
mod sysproxy;

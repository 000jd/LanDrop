//! LanDrop — LAN file sharing with no cloud and no accounts.
//!
//! The crate is split into modules so tests (unit and integration) can link
//! against the library:
//!
//! - [`config`]: persistent device configuration (TOML, mode 0600).
//! - [`crypto`]: ephemeral X25519 + seekable ChaCha20 wire encryption.
//! - [`protocol`]: wire types and helpers (sanitize, unique paths, hashing).
//! - [`peers`]: discovered + manual peer store with expiry.
//! - [`transfer`]: transfer state machine, sender and receiver logic.
//! - [`discovery`]: mDNS/DNS-SD plus UDP LAN beacon for nearby devices.
//! - [`server`]: axum HTTP server, local API, SSE, embedded UI.
//! - [`events`]: SSE event bus.
//! - [`notify`]: desktop notifications (best-effort).
//! - [`autostart`]: session login autostart (background receiver).
//! - [`instance`]: single-instance lock file (pid + port).
//! - [`util`]: small shared helpers (detached processes, disk space).
//! - [`zip`]: minimal ZIP writer for browser share-link bundles.

/// Compile-time build marker (unix seconds of the last source/UI change).
///
/// Reported by `/api/info` and shown in Settings, and used to refuse attaching
/// to a running node built from different sources.
pub const BUILD_ID: &str = match option_env!("LANDROP_BUILD_ID") {
    Some(v) => v,
    None => "0",
};

pub mod app;
pub mod autostart;
pub mod config;
pub mod crypto;
pub mod discovery;
pub mod events;
pub mod instance;
pub mod metrics;
pub mod notify;
pub mod peers;
pub mod protocol;
pub mod server;
pub mod transfer;
pub mod util;
pub mod zip;

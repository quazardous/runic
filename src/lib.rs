//! runic core library.
//!
//! The SOCKS5 ↔ HTTP-CONNECT data plane, the three-layer config store, and the
//! loopback admin API, exposed as a reusable library. The `runic` binary
//! (`src/main.rs`) is a thin CLI shell over this; the planned Windows tray app
//! (`runic-tray`) is intended to be a second shell over the same core — hence
//! the split into a `[lib]` target.
//!
//! The data plane is platform-agnostic; the few platform-specific bits (default
//! file locations, owner-only permissions) are isolated — see [`paths`] for the
//! `%APPDATA%`-vs-XDG default resolver shared with the tray.

pub mod admin;
pub mod config;
pub mod filter;
pub mod paths;
pub mod routing;
pub mod server;
pub mod silo;
pub mod stats;
pub mod store;
pub mod upstream;
pub mod watcher;

#[cfg(test)]
mod test_helpers;

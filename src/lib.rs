//! runic core library.
//!
//! The SOCKS5 ↔ HTTP-CONNECT data plane, the three-layer config store, and the
//! loopback admin API, exposed as a reusable library. The `runic` binary
//! (`src/main.rs`) is a thin CLI shell over this; the planned Windows tray app
//! (`runic-tray`) is intended to be a second shell over the same core — hence
//! the split into a `[lib]` target.
//!
//! Nothing here is platform-specific: it builds and is tested on Linux, and the
//! same library is what a Windows tray front-end links against.

pub mod admin;
pub mod config;
pub mod routing;
pub mod server;
pub mod silo;
pub mod store;
pub mod upstream;
pub mod watcher;

#[cfg(test)]
mod test_helpers;

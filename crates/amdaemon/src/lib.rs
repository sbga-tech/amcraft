//! Faithful Rust recreation of `amdaemonExe Ver.91f83d14` (May 20 2024): the daemon ↔ game IPC protocol and
//! the daemon's internal behaviour, derived from the binary (`spec/`).
//!
//! - [`backend`]: external interfaces shared with libam: processes, services, DLLs and devices not managed
//!   by the app, including the game peer's named pipes, sections and mutexes.
//! - [`schema`]: byte images of `ServerData` / `ClientData` / the ServerIO body and the checked `zero-schema`
//!   views of every block placed in them.
//! - [`ipc`]: relation table, Messenger session crypto, framing, publication streams, transports.
//! - [`controllers`]: the 27 `amdaemon::server` controllers, built against the [`libam`] facade traits.
//! - [`daemon`]: process entry, `App` lifecycle, `arcade::Manager` event hub and the daemon's own internal
//!   services: clock, window, console, single-instance guard and log. These are not external backends.
//! - [`executor`], [`manager`]: shared controller framework.
//!
//! Diagnostics are `tracing` events; the binary installs the subscriber ([`daemon::log::subscriber`]).
//! Unsafe code in this crate is confined to the Win32 implementations of [`daemon::window`] and
//! [`daemon::console`].

#![deny(unsafe_code)]

pub mod controllers;
pub mod daemon;
pub mod executor;
pub mod ipc;
pub mod manager;
pub mod schema;

#[cfg(test)]
mod mock;

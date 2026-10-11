//! Recreation of libam, the device, network and main-flow library statically linked into
//! `amdaemonExe Ver.91f83d14`, implementing the [`libam_api`] interface. Evidence: `spec/libam/`.
//!
//! # Architecture
//!
//! * [`Libam`] is the one shared library state. It owns a single-threaded [`core::Core`] behind an
//!   `Rc<RefCell<..>>` and hands out boxed handles that implement the `libam-api` port traits
//!   ([`Libam::interface`], [`Libam::process`], ...). Every handle shares the state.
//! * [`backend::Backend`] comes from the backend crate and carries the external interfaces: OS services,
//!   network transports, attached devices and vendor DLLs. That crate owns their platform implementations.
//! * [`clock`] is libam's internal time seam, implemented cross-platform by [`clock::SystemClock`].
//! * With the `mock` feature, `libam::mock` holds device and server emulators that reuse libam's protocol code,
//!   the clock double and the healthy-cabinet assembly. Generic external-interface doubles live in
//!   `backend::mock`.
//! * One directory per subsystem (`error`, `sequence`, `platform`, `usbio`, `jvs`, `can`, `display`, `credit`,
//!   `nvram`, `appimage`, `apm`, `abaas`, `allnet`, `network`, `aime`, `emoney`, `aimepay`) holds the module
//!   struct, its hooks and its port handles. [`flow`] drives them (`spec/libam/core.md` M).
//! * `execute` runs exactly one frame: the main-flow step, then the error aggregator. The events it produced come
//!   back in emission order. Blocking work runs on worker threads whose results modules apply inside their
//!   `update`.
//!
//! The module contract (hooks, readiness, error cells, cross-module methods) is documented in [`module`].
//!
//! This crate contains no `unsafe` code.

#![deny(unsafe_code)]

pub mod abaas;
pub mod aime;
pub mod aimepay;
pub mod allnet;
pub mod apm;
pub mod appimage;
pub mod can;
pub mod clock;
pub mod config;
pub mod core;
pub mod credit;
pub mod display;
pub mod emoney;
pub mod error;
pub mod flow;
pub mod interface;
pub mod jvs;
pub mod master;
#[cfg(any(test, feature = "mock"))]
pub mod mock;
pub mod module;
pub mod network;
pub mod nvram;
pub mod platform;
pub mod registry_keys;
pub mod sequence;
pub mod shared;
pub mod usbio;
pub mod util;

pub use shared::Libam;

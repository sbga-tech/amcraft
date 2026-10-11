//! Recreation of libam, the device, network and main-flow library statically linked into
//! `amdaemonExe Ver.91f83d14`: the facades amdaemon is built against and their default implementation. Evidence:
//! `spec/libam/`.
//!
//! # Facades
//!
//! The daemon's controllers, its App and its Manager reach libam only through traits, one per facade of the original.
//! A module's `port.rs` holds its facades: the port traits, the values they exchange and the handles that implement
//! them. The module re-exports them, so a facade is `libam::<module>::<Trait>`.
//!
//! | module | facade traits | used by (amdaemon) | libam's handles |
//! |---|---|---|---|
//! | [`interface`] | `AmInterface` | App, arcade Manager | `InterfaceHandle` |
//! | [`config`] | `ConfigLoader` | the cabinet-type callback | `FileConfigLoader` |
//! | [`platform`] | `ProcessPort`, `CommonPort`, `SystemInfoPort` | Process, Common, SystemInfo | `ProcessHandle`, `CommonHandle`, `SystemInfoHandle` |
//! | [`error`] | `ErrorPort` | Error | `ErrorHandle` |
//! | [`sequence`] | `SequencePort` | Sequence | `SequenceHandle` |
//! | [`jvs`] | `JvsPort`, `InputPort` and `OutputPort` over `JvsNodePort` | Jvs, Input, Output | `JvsHandle`, `JvsNodeHandle` |
//! | [`can`] | `CanPort` | Can | `CanHandle` |
//! | [`usbio`] | `UsbIoPort`, `BoardIoPort`, `UsbDevicePort` | UsbIo, BoardIo, UsbDevice | `UsbIoHandle`, `BoardIoHandle`, `UsbDeviceHandle` |
//! | [`nvram`] | `BackupPort` | Backup | `BackupHandle` |
//! | [`appimage`] | `AppImagePort` | AppImage | `AppImageHandle` |
//! | [`credit`] | `CreditPort` | Credit | `CreditHandle` |
//! | [`network`] | `NetworkPort`, `LanInstallPort`, `NetDeliveryPort` | Network, LanInstall, NetDelivery | `NetworkHandle`, `LanInstallHandle`, `NetDeliveryHandle` |
//! | [`allnet`] | `AuthPort`, `AirPort` | ALL.Net Auth, Air | `AuthHandle`, `AirHandle` |
//! | [`allnet::billing`] | `AccountingPort` | ALL.Net Accounting | `AccountingHandle` |
//! | [`allnet::wifi`] | `WiFiPort` | ALL.Net WiFi | `WifiHandle` |
//! | [`aime`] | `AimePort` | Aime | `AimeHandle` |
//! | [`emoney`] | `EMoneyPort` | EMoney | `EMoneyHandle` |
//! | [`aimepay`] | `AimePayPort` | AimePay | `AimePayHandle` |
//! | [`abaas`] | `LogPort` | ABaaS Log | `LogHandle` |
//! | [`apm`] | `ApmPort` | Apm | `ApmHandle` |
//!
//! Every facade shares [`result`] (`am::Result`), [`time`] (`DateTime` and `struct tm`) and
//! [`AmConfig`](config::AmConfig) (the typed `am::AppConfig` and its validator).
//!
//! # Another implementation
//!
//! A custom libam layer is ordinary code in any crate: implement the facade traits and hand the boxed handles to the
//! daemon (`amdaemon::daemon::registry::ControllerPorts` and `AppServices::with_ports`), keeping or wrapping any
//! handle [`Libam`] gives out.
//!
//! # Architecture of the default implementation
//!
//! * [`Libam`] is the one shared library state. It owns a single-threaded [`core::Core`] behind an
//!   `Rc<RefCell<..>>` and hands out boxed handles that implement the facade traits ([`Libam::interface`],
//!   [`Libam::process`], ...). Every handle shares the state.
//! * [`backend::Backend`] comes from the backend crate and carries the external interfaces: OS services,
//!   network transports, attached devices and vendor DLLs. That crate owns their platform implementations.
//! * [`clock`] is libam's internal time seam, implemented cross-platform by [`clock::SystemClock`].
//! * With the `mock` feature, `libam::mock` holds device and server emulators that reuse libam's protocol code,
//!   the clock double and the healthy-cabinet assembly. Generic external-interface doubles live in
//!   `backend::mock`.
//! * One directory per subsystem (`error`, `sequence`, `platform`, `usbio`, `jvs`, `can`, `display`, `credit`,
//!   `nvram`, `appimage`, `apm`, `abaas`, `allnet`, `network`, `aime`, `emoney`, `aimepay`) holds the module
//!   struct, its hooks and its facades. [`flow`] drives them (`spec/libam/core.md` M).
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
pub mod result;
pub mod sequence;
pub mod shared;
pub mod time;
pub mod usbio;
pub mod util;

pub use shared::Libam;

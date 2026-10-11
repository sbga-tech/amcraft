//! Deterministic doubles of the external interfaces, for tests (feature `mock`).
//!
//! * [`ipc`]: the in-process hub behind [`LoopbackPlatform`](ipc::LoopbackPlatform). Every clone shares one registry
//!   of named mutexes, sections and pipes, so a daemon `Server` and a peer `Messenger` in the same test find each
//!   other's objects as two processes would. [`ScriptedPipe`](ipc::ScriptedPipe) is a single pipe endpoint whose
//!   input the test supplies and whose writes it reads back.
//! * One double per libam interface, named `Sim<Interface>`: a scripted or recording implementation whose clones
//!   share state, so a test keeps one clone as the handle it scripts and inspects and hands the other to the
//!   [`Backend`](crate::Backend).
//!
//! Emulators of the specific devices and servers libam talks to (Aime readers, the coin selector, the VFD, the
//! keychip, the Aime DB server) reuse libam's protocol code and are libam's own `mock` module, which also assembles
//! these doubles into a healthy cabinet.

pub mod adapters;
pub mod dns;
pub mod fs;
pub mod hid;
pub mod http;
pub mod hwm;
pub mod icmp;
pub mod ipc;
pub mod lan;
pub mod preload;
pub mod registry;
pub mod services;
pub mod system;
pub mod thinca;
pub mod tls;
pub mod video;
pub mod wlan;

pub use adapters::SimAdapters;
pub use dns::SimDns;
pub use fs::SimFs;
pub use hid::SimHid;
pub use http::SimHttp;
pub use hwm::SimHwMonitor;
pub use icmp::SimIcmp;
pub use lan::SimLan;
pub use preload::SimPreloader;
pub use registry::SimRegistry;
pub use services::{SimServiceState, SimServices};
pub use system::{SimSystem, SimSystemState};
pub use thinca::SimThinca;
pub use tls::{Responder, Script, SimConnection, SimTls};
pub use video::SimVideoDriver;
pub use wlan::SimWlan;

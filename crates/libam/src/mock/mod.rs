//! Device and server emulators, the internal clock double and the healthy-cabinet assembly.
//!
//! The emulators live with libam because they reuse its protocol codecs: Aime DB packets, keychip properties,
//! Aime reader commands, AS-6DB frames and VFD text. Generic doubles of external interfaces belong to
//! [`backend::mock`], not here. This module is available for tests and through the optional `mock` feature.
//!
//! [`SimBackend::new`] returns the external [`Backend`] and script handles for a healthy cabinet. Pass
//! `Box::new(sim.clock.clone())` separately to [`crate::Libam::new`]; the clock is internal libam state.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use backend::Backend;
use backend::mock::{
    SimAdapters, SimDns, SimFs, SimHid, SimHttp, SimHwMonitor, SimIcmp, SimLan, SimPreloader, SimRegistry, SimServices,
    SimSystem, SimThinca, SimTls, SimVideoDriver, SimWlan,
};

pub mod aime_db;
pub mod aimepay;
pub mod cabinet;
pub mod clock;
pub mod devices;
pub mod keychip;
pub mod serial;

pub use aime_db::SimDb;
pub use aimepay::SimAimePayServer;
pub use cabinet::SIMULATED_RESOURCE_PATH;
pub use clock::SimClock;
pub use keychip::SimKeychip;
pub use serial::{SerialDevice, SimSerial};

/// Handles for scripting and inspecting the external interfaces returned by [`SimBackend::new`] and its clock.
#[derive(Debug)]
pub struct SimBackend {
    /// Directory every cabinet drive maps below.
    pub root: PathBuf,
    /// The internal clock to pass separately to [`crate::Libam::new`].
    pub clock: SimClock,
    /// The registry the backend uses.
    pub registry: SimRegistry,
    /// The HTTP client the backend uses.
    pub http: SimHttp,
    /// The system actions the backend uses.
    pub system: SimSystem,
    /// The service control the backend uses.
    pub services: SimServices,
    /// The simulated keychip.
    pub keychip: SimKeychip,
    /// The simulated hardware monitor.
    pub hwm: SimHwMonitor,
    /// The simulated video DLL.
    pub video: SimVideoDriver,
    /// The simulated preload worker.
    pub preload: SimPreloader,
    /// The simulated USB I/O bus.
    pub hid: SimHid,
    /// The simulated Thinca payment SDK: script return codes and callbacks, inspect the calls.
    pub thinca: SimThinca,
    /// The COM ports. A test scripts a device with [`SimSerial::aime_reader`], [`SimSerial::as6db`] or
    /// [`SimSerial::vfd`].
    pub serial: SimSerial,
    /// The simulated Aime DB: add accounts and campaigns, inspect the decrypted requests.
    pub aime_db: SimDb,
    /// The scripted TLS server.
    pub tls: SimTls,
    /// System DNS lookups.
    pub dns: SimDns,
    /// ICMP echo for traceroute probes.
    pub icmp: SimIcmp,
    /// Adapter enumeration and address configuration.
    pub adapters: SimAdapters,
    /// Adapters, beacon socket and sync channel of LAN install.
    pub lan: SimLan,
    /// The Wi-Fi unit announcement socket.
    pub wlan: SimWlan,
}

impl SimBackend {
    /// Deterministic backend for a healthy cabinet: simulated clock at a fixed time, every cabinet drive below
    /// `root`, a scripted HTTP client, recording system actions and service control. The registry, the computer
    /// name and the drive directories hold what a working cabinet has (platform id, OS version, the AMFS and
    /// APPDATA mounts, the CPU temperature limit), so the main flow reaches Operating; a test that needs a broken
    /// cabinet overrides the value it is about.
    pub fn new(root: impl Into<PathBuf>) -> (Backend, Self) {
        let root = root.into();
        let sim = Self {
            clock: SimClock::new(),
            registry: SimRegistry::new(),
            http: SimHttp::new(),
            system: SimSystem::new(),
            services: SimServices::new(),
            keychip: SimKeychip::new(),
            hwm: SimHwMonitor::new(),
            video: SimVideoDriver::new(),
            preload: SimPreloader::new(),
            hid: SimHid::default(),
            thinca: SimThinca::new(),
            serial: SimSerial::new(),
            aime_db: SimDb::default(),
            tls: SimTls::default(),
            dns: SimDns::new(),
            icmp: SimIcmp::new(),
            adapters: SimAdapters::new(),
            lan: SimLan::new(),
            wlan: SimWlan::default(),
            root,
        };
        cabinet::seed_healthy_cabinet(&sim.root, &sim.registry, &sim.system);
        let serial = Rc::new(sim.serial.clone());
        let backend = Backend {
            registry: Box::new(sim.registry.clone()),
            services: Box::new(sim.services.clone()),
            system: Box::new(sim.system.clone()),
            fs: Box::new(SimFs::new(sim.root.clone())),

            http: Box::new(sim.http.clone()),
            tls: Box::new(sim.tls.clone()),
            lan: Box::new(sim.lan.clone()),
            wlan: Box::new(sim.wlan.clone()),
            aime_db: Arc::new(sim.aime_db.clone()),
            dns: Arc::new(sim.dns.clone()),
            icmp: Arc::new(sim.icmp.clone()),
            adapters: Arc::new(sim.adapters.clone()),

            keychip: Box::new(sim.keychip.clone()),
            hwm: Box::new(sim.hwm.clone()),
            video: Box::new(sim.video.clone()),
            preload: Box::new(sim.preload.clone()),
            hid: Box::new(sim.hid.bus()),
            serial,

            thinca: Box::new(sim.thinca.clone()),
        };
        (backend, sim)
    }
}

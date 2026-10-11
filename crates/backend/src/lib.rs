//! The external interfaces of amdaemon and libam: how the daemon talks to the processes, services, DLLs and devices
//! it does not manage itself, as traits with one implementation per platform.
//!
//! # What belongs here
//!
//! An interface belongs to this crate when the other side is outside the daemon:
//!
//! * **another process**: the game, which shares the IPC pipes, sections and mutexes, and whose modules the
//!   preloader pages in;
//! * **an operating-system service**: the registry the launcher writes, service control, reboot and the clock and
//!   time-zone setters, the cabinet's storage volumes;
//! * **the network**: HTTP, TLS, DNS, ICMP, the adapters, the LAN-install and Wi-Fi sockets, the Aime DB server;
//! * **a device or its driver**: the keychip, the hardware monitor, the USB I/O boards, the COM-port devices;
//! * **a vendor DLL**: the display DLL and `ThincaPayment.dll`.
//!
//! The daemon's own machinery is not an external interface, even when it calls the operating system: the main-loop
//! clock, the log and its subscriber, the configuration, the hidden window and its message pump, the console and the
//! single-instance guard. Those live in the crate that owns them, with one cross-platform implementation (`std` or a
//! crate); where a test must control one, that crate keeps the seam and its double.
//!
//! # Interfaces
//!
//! One file per interface: the trait, the value types of its signatures and its default implementation. The default
//! is portable: `std` or a cross-platform crate where one serves the interface on any operating system, otherwise
//! [`Unsupported`], the answers of a cabinet where that part is absent. The game-process IPC interfaces have no
//! portable implementation.
//!
//! | file | interface | other side | default | `windows` |
//! |---|---|---|---|---|
//! | [`ipc`] | [`IpcPlatform`], [`Services`] | the game process: factory of the IPC objects | - | `WindowsIpcPlatform` |
//! | [`pipe`] | [`PipeEndpoint`] | the game process: one overlapped named-pipe endpoint | - | yes |
//! | [`section`] | [`Section`] | the game process: a mapped named section | - | yes |
//! | [`mutex`] | [`NamedMutex`] | the game process: a named mutex of the IPC layer | - | yes |
//! | [`protector`] | [`MemoryProtector`] | the game process: `CryptProtectMemory` (same logon) | - | yes |
//! | [`registry`] | [`Registry`] | the launcher and the platform: `HKLM` values and their named mutexes | `Unsupported` | `WindowsRegistry` |
//! | [`services`] | [`ServiceControl`] | Windows services: user control codes | `Unsupported` | `WindowsServices` |
//! | [`system`] | [`SystemActions`] | the OS: reboot, clock/zone setters, computer name, environment | `Unsupported` (environment from `std`) | `WindowsSystem` |
//! | [`fs`] | [`Fs`] | the cabinet's volumes: path mapping, share modes, system attribute, directory links | `NativeFs` | `WindowsFs` |
//! | [`preload`] | [`Preloader`] | the game process: page-in of its modules | `Unsupported` | `WindowsPreloader` |
//! | [`http`] | [`HttpClient`] | network services: blocking HTTP(S) | `UreqHttp` | - |
//! | [`tls`] | [`TlsProvider`] | ALL.Net: TLS sessions | `NativeTls` | - |
//! | [`dns`] | [`Dns`] | the system resolver: A lookups | `HickoryDns` | - |
//! | [`icmp`] | [`Icmp`] | the network: one echo with TTL and timeout | `Unsupported` | `WindowsIcmp` |
//! | [`adapters`] | [`Adapters`] | the network adapters: enumeration, read-back, address configuration | `Unsupported` | `WindowsAdapters` |
//! | [`lan`] | [`LanNet`] | other cabinets: LAN-install beacon socket and sync channel | `StdLanNet` | - |
//! | [`wlan`] | [`WlanSocket`] | the Wi-Fi unit: announcement socket | `UdpAnnounce` | - |
//! | [`aime_db`] | [`AimeDbTransport`] | the Aime DB server: resolution and connections | `TcpTransport` | - |
//! | [`keychip`] | [`KeychipDriver`] | the keychip driver and its registry store | `Unsupported` | `WindowsKeychip` |
//! | [`hwm`] | [`HwMonitor`] | the hardware monitor driver | `Unsupported` | `WindowsHwMonitor` |
//! | [`hid`] | [`HidBus`] | the USB I/O boards on HID | `Unsupported` (no boards) | `WindowsHid` |
//! | [`serial`] | [`SerialPorts`] | the COM-port devices: Aime readers, coin selector, VFD | `SerialportPorts` | - |
//! | [`video`] | [`VideoDriver`] | the vendor display DLL | `Unsupported` | `WindowsVideoDriver` |
//! | [`thinca`] | [`ThincaSdk`] | `ThincaPayment.dll` | `Unsupported` | `WindowsThinca` |
//!
//! The daemon's IPC layer takes an [`IpcPlatform`]; libam takes a [`Backend`], one public field per libam interface.
//! [`Backend::default`] assembles the defaults.
//!
//! # Other implementations
//!
//! An implementation module has a file, with the same name as the interface's file, only for the interfaces it
//! implements.
//!
//! * `windows` (`cfg(windows)`): the cabinet. `Backend::windows` is [`Backend::default`] with the Windows parts in
//!   place; the only module of the crate with `unsafe`.
//! * `mock` (feature `mock`): deterministic doubles of every interface, for tests: the in-process IPC hub
//!   (`mock::ipc`) and scripted or recording doubles of libam's interfaces. Emulators of the specific devices and
//!   servers libam talks to (they reuse libam's protocol code) are libam's own `mock` module.
//!
//! This crate holds the only `cfg(windows)` of the external interfaces. Code above it takes only the traits and
//! never names an implementation type, except where a binary composes the process.
//!
//! # Another platform or environment
//!
//! An implementation for another platform is ordinary code in any crate: implement the interfaces it can serve and
//! put them into a [`Backend`], starting from [`Backend::default`] (or from `Backend::windows` for a variant of the
//! cabinet):
//!
//! ```no_run
//! # use backend::Backend;
//! # use backend::serial::SerialportPorts;
//! let mut backend = Backend::default();
//! backend.serial = std::rc::Rc::new(SerialportPorts::named(|com| format!("/dev/ttyS{}", com - 1)));
//! // backend.keychip = Box::new(MyKeychip::connect(..)); and so on for every part the machine has.
//! ```

#![deny(unsafe_code)]

pub mod adapters;
pub mod aime_db;
pub mod dns;
pub mod fs;
pub mod hid;
pub mod http;
pub mod hwm;
pub mod icmp;
pub mod ipc;
pub mod keychip;
pub mod lan;
#[cfg(any(test, feature = "mock"))]
pub mod mock;
pub mod mutex;
pub mod pipe;
pub mod preload;
pub mod protector;
pub mod registry;
pub mod section;
pub mod serial;
pub mod services;
pub mod system;
pub mod thinca;
pub mod tls;
pub mod video;
#[cfg(windows)]
#[allow(unsafe_code)]
pub mod windows;
pub mod wlan;

use std::rc::Rc;
use std::sync::Arc;

pub use adapters::{AdapterConfig, AdapterError, AdapterId, Adapters, DhcpApply, InterfaceKind, Nic};
pub use aime_db::{AimeDbStream, AimeDbTransport, ServerAddr};
pub use dns::Dns;
pub use fs::Fs;
pub use hid::{HidBoard, HidBus, HidDevice, HidError};
pub use http::{HttpClient, HttpError, HttpRequest, HttpResponse, Method};
pub use hwm::{HwMonitor, HwmError};
pub use icmp::{EchoReply, EchoStatus, Icmp, IcmpError};
pub use ipc::{IpcPlatform, Services};
pub use keychip::KeychipDriver;
pub use lan::{Datagram, LanNet, SyncListener};
pub use mutex::NamedMutex;
pub use pipe::PipeEndpoint;
pub use preload::{PreloadError, Preloader};
pub use protector::MemoryProtector;
pub use registry::{KeyMode, RegValue, Registry, RegistryError};
pub use section::Section;
pub use serial::{DataBits, FlowControl, Parity, SerialPort, SerialPorts, SerialSettings, StopBits};
pub use services::{ServiceControl, ServiceError};
pub use system::{SystemActions, SystemError, TimeZoneInformation, TransitionRule, WallTime};
pub use thinca::ThincaSdk;
pub use tls::{AllnetTls, TlsError, TlsProvider};
pub use video::{DisplayMode, DisplayRequest, VideoDriver, VideoError};
pub use wlan::WlanSocket;

/// The external interfaces libam uses, one public field per interface.
pub struct Backend {
    // Operating system.
    /// `HKLM` registry.
    pub registry: Box<dyn Registry>,
    /// Windows service control.
    pub services: Box<dyn ServiceControl>,
    /// Reboot, clock and time-zone setters, computer name.
    pub system: Box<dyn SystemActions>,
    /// Cabinet path to host path mapping, sharing modes, system attribute and directory links.
    pub fs: Box<dyn Fs>,
    /// The page-in worker behind `CommonPort::preload`.
    pub preload: Box<dyn Preloader>,

    // Network.
    /// Blocking HTTP client.
    pub http: Box<dyn HttpClient>,
    /// The TLS client of alAuth, alAbEx and the accounting reporter.
    pub tls: Box<dyn TlsProvider>,
    /// A lookup of the system DNS configuration.
    pub dns: Arc<dyn Dns>,
    /// ICMP echo for the traceroute probes.
    pub icmp: Arc<dyn Icmp>,
    /// Adapter enumeration, read-back and address configuration.
    pub adapters: Arc<dyn Adapters>,
    /// Adapters, beacon socket and sync channel of LAN install.
    pub lan: Box<dyn LanNet>,
    /// The Wi-Fi unit announcement socket.
    pub wlan: Box<dyn WlanSocket>,
    /// The Aime DB transport.
    pub aime_db: Arc<dyn AimeDbTransport>,

    // Devices.
    /// The keychip driver `\??\FddDriver` and the registry store behind the `amy` request pipeline.
    pub keychip: Box<dyn KeychipDriver>,
    /// The hardware monitor driver (`sghwmonitor`).
    pub hwm: Box<dyn HwMonitor>,
    /// The HID bus the USB I/O boards sit on.
    pub hid: Box<dyn HidBus>,
    /// The COM ports: Aime readers, the AS-6DB coin selector and the VFD.
    pub serial: Rc<dyn SerialPorts>,

    // Vendor DLLs.
    /// The vendor video DLL (`amVideo`).
    pub video: Box<dyn VideoDriver>,
    /// `ThincaPayment.dll` as the e-money module sees it.
    pub thinca: Box<dyn ThincaSdk>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend").finish_non_exhaustive()
    }
}

/// What `std` and the cross-platform crates provide on any operating system, the default implementation of every
/// interface: the file system as it is, HTTP, TLS, DNS, the LAN, Wi-Fi and Aime DB sockets and the COM ports (named
/// `\\.\COM<n>`); every OS-only interface is [`Unsupported`]. The base of another platform.
impl Default for Backend {
    fn default() -> Self {
        Self {
            registry: Box::new(Unsupported),
            services: Box::new(Unsupported),
            system: Box::new(Unsupported),
            fs: Box::new(fs::NativeFs),
            preload: Box::new(Unsupported),

            http: Box::new(http::UreqHttp::new()),
            tls: Box::new(tls::NativeTls),
            dns: Arc::new(dns::HickoryDns),
            icmp: Arc::new(Unsupported),
            adapters: Arc::new(Unsupported),
            lan: Box::new(lan::StdLanNet::real()),
            wlan: Box::new(wlan::UdpAnnounce::default()),
            aime_db: Arc::new(aime_db::TcpTransport::new()),

            keychip: Box::new(Unsupported),
            hwm: Box::new(Unsupported),
            hid: Box::new(Unsupported),
            serial: Rc::new(serial::SerialportPorts::new()),

            video: Box::new(Unsupported),
            thinca: Box::new(Unsupported),
        }
    }
}

impl Backend {
    /// The backend of a cabinet: [`Backend::default`] with the Windows registry, service control, system actions,
    /// file system, ICMP, adapters, keychip and hardware-monitor drivers, video DLL, preloader, HID bus and Thinca
    /// SDK.
    #[cfg(windows)]
    pub fn windows() -> Self {
        Self {
            registry: Box::new(windows::registry::WindowsRegistry),
            services: Box::new(windows::services::WindowsServices),
            system: Box::new(windows::system::WindowsSystem),
            fs: Box::new(windows::fs::WindowsFs::default()),
            preload: Box::new(windows::preload::WindowsPreloader),
            icmp: Arc::new(windows::icmp::WindowsIcmp),
            adapters: Arc::new(windows::adapters::WindowsAdapters),
            keychip: Box::new(windows::keychip::WindowsKeychip::new()),
            hwm: Box::new(windows::hwm::WindowsHwMonitor::new()),
            hid: Box::new(windows::hid::WindowsHid::new()),
            video: Box::new(windows::video::WindowsVideoDriver),
            thinca: Box::new(windows::thinca::WindowsThinca::load()),
            ..Self::default()
        }
    }
}

/// The default implementation of every OS-only interface: the answers of a cabinet where that part is absent.
///
/// A machine without the SEGA registry keys, the keychip and hardware-monitor drivers, the vendor DLLs, the USB I/O
/// boards, an ICMP stack or an adapter enumeration still runs libam: each request fails like it fails on a cabinet
/// where that part is missing, and libam raises the error it raises there. Each interface's file holds its
/// implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unsupported;

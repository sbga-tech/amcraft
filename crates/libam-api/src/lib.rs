//! The libam interface amdaemon is built against.
//!
//! libam is the library statically linked into `amdaemonExe Ver.91f83d14`: it owns the devices (JVS, USBIO,
//! system IO, CAN, USB devices, card readers), the network services (ALL.Net, network tests, delivery, LAN
//! install, e-money, Aime Pay, Aime), NVRAM and the main-flow state machine. amdaemon reaches it only through the
//! traits of this crate; the `libam` crate implements them.
//!
//! - [`interface`]: the lifecycle (`am::Interface`), the listener events and the configuration loader.
//! - [`config`]: the process and `am::AppConfig` parts the controllers consume.
//! - [`am_config`]: the full typed `am::AppConfig` and its validator.
//! - One module per libam facade, each holding the port trait its controller uses and the values it exchanges.
//! - [`result`], [`time`]: `am::Result` codes, the `DateTime` value and libam's `struct tm`.

pub mod abaas;
pub mod aime;
pub mod aime_pay;
pub mod allnet;
pub mod am_config;
pub mod apm;
pub mod app_image;
pub mod backup;
pub mod board_io;
pub mod can;
pub mod common;
pub mod config;
pub mod credit;
pub mod emoney;
pub mod error;
pub mod input;
pub mod interface;
pub mod jvs;
pub mod lan_install;
pub mod net_delivery;
pub mod network;
pub mod output;
pub mod process;
pub mod result;
pub mod sequence;
pub mod system_info;
pub mod time;
pub mod usb_device;
pub mod usb_io;

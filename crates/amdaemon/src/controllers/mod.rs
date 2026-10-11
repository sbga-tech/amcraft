//! The 27 amdaemon 2024 controllers (`amdaemon::server::*Controller`).
//!
//! Each module owns one controller. Every call the original makes into the statically linked `am::` library goes
//! through a [`libam`] facade trait (a port) the controller is given.

pub mod controller;

pub mod abaas_log;
pub mod aime;
pub mod aime_pay;
pub mod allnet_accounting;
pub mod allnet_air;
pub mod allnet_auth;
pub mod allnet_wifi;
pub mod apm;
pub mod app_image;
pub mod backup;
pub mod board_io;
pub mod can;
pub mod common;
pub mod credit;
pub mod emoney;
pub mod error;
pub mod input;
pub mod jvs;
pub mod lan_install;
pub mod net_delivery;
pub mod network;
pub mod output;
pub mod process;
pub mod sequence;
pub mod system_info;
pub mod usb_device;
pub mod usb_io;

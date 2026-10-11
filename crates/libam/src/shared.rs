//! The shared state behind every handle, and the [`Libam`] handle factory.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use libam_api::interface::{AmInterface, ConfigLoader};
use libam_api::result::{AmError, AmResult};
use libam_api::{
    abaas::LogPort, aime::AimePort, aime_pay::AimePayPort, allnet::AccountingPort, allnet::AirPort, allnet::AuthPort,
    allnet::WiFiPort, apm::ApmPort, app_image::AppImagePort, backup::BackupPort, board_io::BoardIoPort, can::CanPort,
    common::CommonPort, credit::CreditPort, emoney::EMoneyPort, error::ErrorPort, input::InputPort, input::JvsNodePort,
    jvs::JvsPort, lan_install::LanInstallPort, net_delivery::NetDeliveryPort, network::NetworkPort, output::OutputPort,
    process::ProcessPort, sequence::SequencePort, system_info::SystemInfoPort, usb_device::UsbDevicePort,
    usb_io::UsbIoPort,
};

use crate::clock::Clock;
use crate::core::Core;
use backend::Backend;

/// The libam state every handle points at.
#[derive(Clone)]
pub struct Shared(Rc<RefCell<Core>>);

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Shared(libam)")
    }
}

impl Shared {
    /// Runs `f` with exclusive access to the state.
    pub fn with<R>(&self, f: impl FnOnce(&mut Core) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }

    /// Runs `f` with shared access to the state.
    pub fn read<R>(&self, f: impl FnOnce(&Core) -> R) -> R {
        f(&self.0.borrow())
    }

    /// The facade accessor check every handle method starts with (`0x140218C10` and siblings): `Err` with
    /// result code 2 ("amw is uninitialized") while libam is not initialised.
    pub fn guard(&self) -> AmResult<()> {
        if self.read(|core| core.exists) {
            Ok(())
        } else {
            Err(AmError::with_message(AmError::NOT_INITIALIZED, "amw is uninitialized"))
        }
    }
}

/// The one shared libam. Every handle it returns shares the same state.
#[derive(Debug, Clone)]
pub struct Libam {
    shared: Shared,
}

impl Libam {
    /// A libam that has not been initialised yet, on the given external interfaces and internal clock.
    pub fn new(backend: Backend, clock: Box<dyn Clock>) -> Self {
        Self { shared: Shared(Rc::new(RefCell::new(Core::new(backend, clock)))) }
    }

    /// `am::Interface`.
    pub fn interface(&self) -> Box<dyn AmInterface> {
        Box::new(crate::interface::InterfaceHandle(self.shared.clone()))
    }

    /// The configuration loader behind the cabinet-type callback.
    pub fn config_loader(&self) -> Box<dyn ConfigLoader> {
        Box::new(crate::config::FileConfigLoader::new())
    }

    /// `ErrorPort`.
    pub fn error(&self) -> Box<dyn ErrorPort> {
        Box::new(crate::error::ErrorHandle(self.shared.clone()))
    }

    /// `SequencePort`.
    pub fn sequence(&self) -> Box<dyn SequencePort> {
        Box::new(crate::sequence::SequenceHandle(self.shared.clone()))
    }

    /// `SystemInfoPort`.
    pub fn system_info(&self) -> Box<dyn SystemInfoPort> {
        Box::new(crate::platform::SystemInfoHandle(self.shared.clone()))
    }

    /// `ProcessPort`.
    pub fn process(&self) -> Box<dyn ProcessPort> {
        Box::new(crate::platform::ProcessHandle(self.shared.clone()))
    }

    /// `CommonPort`.
    pub fn common(&self) -> Box<dyn CommonPort> {
        Box::new(crate::platform::CommonHandle(self.shared.clone()))
    }

    /// `UsbIoPort`.
    pub fn usb_io(&self) -> Box<dyn UsbIoPort> {
        Box::new(crate::usbio::UsbIoHandle(self.shared.clone()))
    }

    /// `BoardIoPort`.
    pub fn board_io(&self) -> Box<dyn BoardIoPort> {
        Box::new(crate::usbio::BoardIoHandle(self.shared.clone()))
    }

    /// `UsbDevicePort`.
    pub fn usb_device(&self) -> Box<dyn UsbDevicePort> {
        Box::new(crate::usbio::UsbDeviceHandle(self.shared.clone()))
    }

    /// `JvsPort`.
    pub fn jvs(&self) -> Box<dyn JvsPort> {
        Box::new(crate::jvs::JvsHandle(self.shared.clone()))
    }

    /// `JvsNodePort`.
    pub fn jvs_node(&self) -> Box<dyn JvsNodePort> {
        Box::new(crate::jvs::JvsNodeHandle(self.shared.clone()))
    }

    /// `InputPort`.
    pub fn input(&self) -> Box<dyn InputPort> {
        Box::new(crate::jvs::JvsNodeHandle(self.shared.clone()))
    }

    /// `OutputPort`.
    pub fn output(&self) -> Box<dyn OutputPort> {
        Box::new(crate::jvs::JvsNodeHandle(self.shared.clone()))
    }

    /// `CanPort`.
    pub fn can(&self) -> Box<dyn CanPort> {
        Box::new(crate::can::CanHandle(self.shared.clone()))
    }

    /// `CreditPort`.
    pub fn credit(&self) -> Box<dyn CreditPort> {
        Box::new(crate::credit::CreditHandle(self.shared.clone()))
    }

    /// `BackupPort`.
    pub fn backup(&self) -> Box<dyn BackupPort> {
        Box::new(crate::nvram::BackupHandle(self.shared.clone()))
    }

    /// `AppImagePort`.
    pub fn app_image(&self) -> Box<dyn AppImagePort> {
        Box::new(crate::appimage::AppImageHandle(self.shared.clone()))
    }

    /// `ApmPort`.
    pub fn apm(&self) -> Box<dyn ApmPort> {
        Box::new(crate::apm::ApmHandle(self.shared.clone()))
    }

    /// `LogPort` (ABaaS log).
    pub fn log(&self) -> Box<dyn LogPort> {
        Box::new(crate::abaas::LogHandle(self.shared.clone()))
    }

    /// `AuthPort`.
    pub fn auth(&self) -> Box<dyn AuthPort> {
        Box::new(crate::allnet::AuthHandle(self.shared.clone()))
    }

    /// `AccountingPort`.
    pub fn accounting(&self) -> Box<dyn AccountingPort> {
        Box::new(crate::allnet::AccountingHandle(self.shared.clone()))
    }

    /// `AirPort`.
    pub fn air(&self) -> Box<dyn AirPort> {
        Box::new(crate::allnet::AirHandle)
    }

    /// `WiFiPort`.
    pub fn wifi(&self) -> Box<dyn WiFiPort> {
        Box::new(crate::allnet::WifiHandle(self.shared.clone()))
    }

    /// `NetworkPort`.
    pub fn network(&self) -> Box<dyn NetworkPort> {
        Box::new(crate::network::NetworkHandle(self.shared.clone()))
    }

    /// `LanInstallPort`.
    pub fn lan_install(&self) -> Box<dyn LanInstallPort> {
        Box::new(crate::network::LanInstallHandle(self.shared.clone()))
    }

    /// `NetDeliveryPort`.
    pub fn net_delivery(&self) -> Box<dyn NetDeliveryPort> {
        Box::new(crate::network::NetDeliveryHandle(self.shared.clone()))
    }

    /// `AimePort`.
    pub fn aime(&self) -> Box<dyn AimePort> {
        Box::new(crate::aime::AimeHandle(self.shared.clone()))
    }

    /// `EMoneyPort`.
    pub fn emoney(&self) -> Box<dyn EMoneyPort> {
        Box::new(crate::emoney::EMoneyHandle(self.shared.clone()))
    }

    /// `AimePayPort`.
    pub fn aime_pay(&self) -> Box<dyn AimePayPort> {
        Box::new(crate::aimepay::AimePayHandle(self.shared.clone()))
    }
}

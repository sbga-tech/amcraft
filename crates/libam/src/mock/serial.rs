//! The simulated COM ports of a cabinet.
//!
//! [`SimSerial`] is a map from COM number to a device. [`SerialPorts::open`] uses the device attached to that
//! number. A number nothing has claimed yet opens the healthy device the settings describe (a reader, the coin
//! selector or the panel), which is what the old per-device simulations did: every COM answered. [`SimSerial::detach`]
//! removes that device. A device's own `fail_open`, `set_port(None)` or `set_open_fails` still refuses the open.
//!
//! [`SimSerial::aime_reader`], [`SimSerial::as6db`] and [`SimSerial::vfd`] return the device on that COM, attaching
//! a fresh one when the number is free, so a test can script the device before the first open and inspect it after.

use std::any::Any;
use std::collections::HashMap;
use std::fmt::Debug;
use std::io;
use std::rc::Rc;

use backend::serial::{FlowControl, SerialPort, SerialPorts, SerialSettings};

use super::devices::{SimAimeReader, SimAs6db, SimVfd};

/// A device that can be attached to one COM number.
pub trait SerialDevice: Debug + Any {
    /// Opens this device with `settings`. The COM number was already chosen by the map.
    fn open(&self, settings: &SerialSettings) -> io::Result<Box<dyn SerialPort>>;

    /// Recovers the concrete device so a script handle can be handed back.
    fn as_any(&self) -> &dyn Any;
}

#[derive(Debug, Default)]
struct Map {
    devices: HashMap<u16, Rc<dyn SerialDevice>>,
}

/// The simulated serial ports. Clones share the map.
#[derive(Debug, Clone, Default)]
pub struct SimSerial(Rc<std::cell::RefCell<Map>>);

impl SimSerial {
    /// No device attached. The first open of a COM number attaches the healthy device its settings describe.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches `device` to `com`, replacing whatever was there.
    pub fn attach(&self, com: u16, device: Rc<dyn SerialDevice>) {
        self.0.borrow_mut().devices.insert(com, device);
    }

    /// Removes the device on `com`. The next open attaches a healthy one again.
    pub fn detach(&self, com: u16) {
        self.0.borrow_mut().devices.remove(&com);
    }

    /// The reader on `com`, attached now if the port was free.
    pub fn aime_reader(&self, com: u16) -> SimAimeReader {
        if let Some(reader) = self.existing::<SimAimeReader>(com) {
            return reader;
        }
        let reader = SimAimeReader::on_port(com);
        self.attach(com, Rc::new(reader.clone()));
        reader
    }

    /// The coin selector on `com`, attached now if the port was free.
    pub fn as6db(&self, com: u16) -> SimAs6db {
        if let Some(selector) = self.existing::<SimAs6db>(com) {
            return selector;
        }
        let selector = SimAs6db::on_port(com);
        self.attach(com, Rc::new(selector.clone()));
        selector
    }

    /// The panel on `com`, attached now if the port was free.
    pub fn vfd(&self, com: u16) -> SimVfd {
        if let Some(panel) = self.existing::<SimVfd>(com) {
            return panel;
        }
        let panel = SimVfd::on_port(com);
        self.attach(com, Rc::new(panel.clone()));
        panel
    }

    fn existing<T: Any + Clone>(&self, com: u16) -> Option<T> {
        self.0.borrow().devices.get(&com).and_then(|device| device.as_any().downcast_ref::<T>().cloned())
    }

    fn device_for(&self, com: u16, settings: &SerialSettings) -> Rc<dyn SerialDevice> {
        if let Some(device) = self.0.borrow().devices.get(&com) {
            return Rc::clone(device);
        }
        if settings.flow_control == FlowControl::Hardware {
            let panel = SimVfd::on_port(com);
            let device: Rc<dyn SerialDevice> = Rc::new(panel);
            self.attach(com, Rc::clone(&device));
            device
        } else if settings.baud == 9600 {
            let selector = SimAs6db::on_port(com);
            let device: Rc<dyn SerialDevice> = Rc::new(selector);
            self.attach(com, Rc::clone(&device));
            device
        } else {
            let reader = SimAimeReader::on_port(com);
            let device: Rc<dyn SerialDevice> = Rc::new(reader);
            self.attach(com, Rc::clone(&device));
            device
        }
    }
}

impl SerialPorts for SimSerial {
    fn open(&self, com: u16, settings: &SerialSettings) -> io::Result<Box<dyn SerialPort>> {
        self.device_for(com, settings).open(settings)
    }
}

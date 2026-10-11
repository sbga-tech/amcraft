//! `am::usbdevice::Manager` (`spec/libam/usbio.md` part 4): the USB device checker.
//!
//! Its only service is the reconnect of one `VID:PID` through the helper service ([`super::reconnect`]); the
//! "required devices" sets are accepted by the validator only when empty and nothing reads them (4 section 8), so
//! the manager keeps the configuration without interpreting it.

use crate::config::UsbDeviceConfig;
use crate::usbio::UsbDeviceEvent;

use super::reconnect::Job;
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};

/// `am::usbdevice::Manager` (`ctx+136576`).
#[derive(Debug, Default)]
pub struct UsbDevice {
    life: Lifecycle,
    config: UsbDeviceConfig,
    /// The reconnect in flight (`task == 1`).
    job: Option<Job>,
    events: Vec<UsbDeviceEvent>,
}

impl UsbDevice {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `OperatingState::enter` (`0x14024C9A0`): takes `AppConfig.usbdevice`, state 1. A running task and its
    /// outcome are not touched.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.config = cx.config.usbdevice.clone();
        self.life.begin();
    }

    /// `0x14024CB40(errorManagerReady)`, once per Operating frame (M.3): state 2 runs `operate`; states 0 and 1
    /// become 2 once the error manager is ready (without `operate` in that call).
    pub fn update(&mut self, cx: &mut Cx<'_>, error_manager_ready: bool) {
        // The helper conversation runs beside the manager (the original's worker thread): it also proceeds
        // while the manager is stopped.
        if let Some(job) = &mut self.job {
            job.advance(cx.backend, cx.clock);
        }
        if self.life.state == StateWord::READY {
            self.operate();
        } else if error_manager_ready {
            self.life.state = StateWord::READY;
        }
    }

    /// `Manager::operate` (`0x14024CEA0`, 5): a finished reconnect raises one completion event and clears the
    /// task. (`amMasterUpdate`, which every call also makes, only collects the outcome of that job.)
    fn operate(&mut self) {
        let Some(outcome) = self.job.as_ref().and_then(Job::outcome) else { return };
        self.events.push(UsbDeviceEvent::ReconnectComplete { code: outcome.listener_code() });
        self.job = None;
    }

    /// `Manager::reconnect` (`0x14024CBB0`, 4.2): `false` (result code 2) unless the manager is ready, the
    /// completion listener is installed, the master context is enabled (`master_enabled`) and no reconnect is
    /// in flight. Success starts the helper conversation.
    pub fn reconnect(&mut self, vendor_id: u16, product_id: u16, master_enabled: bool) -> bool {
        let accepted = self.life.state == StateWord::READY
            && self.config.app.listener_installed
            && master_enabled
            && self.job.is_none();
        if accepted {
            self.job = Some(Job::new(vendor_id, product_id));
        }
        accepted
    }

    /// Completion events since the last call.
    pub(crate) fn take_events(&mut self) -> Vec<UsbDeviceEvent> {
        std::mem::take(&mut self.events)
    }

    /// `0x14024CA60` (`finalize`): state 0, the task, the outcome and the configuration (the listener) are
    /// cleared. A reconnect still in flight is abandoned and raises no event.
    pub fn terminate(&mut self, _cx: &mut Cx<'_>) {
        let events = std::mem::take(&mut self.events);
        *self = Self::new();
        self.events = events;
    }

    /// Module restart (`stop`, `0x14024CB70`, 3.5): from state 2 back to 1 with the status record reset; the
    /// task, its outcome and the configuration stay. `operate` pauses until the next ready frame.
    pub fn restart(&mut self) {
        if self.life.state == StateWord::READY {
            self.life.begin();
        }
    }

    /// `ctx+138200`.
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// `allReady` term: state == 2.
    pub fn is_ready(&self) -> bool {
        self.life.state == StateWord::READY
    }

    /// The error cell (`ctx+136584`); this manager never sets it.
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// The configuration taken at initialize.
    pub fn config(&self) -> &UsbDeviceConfig {
        &self.config
    }
}

//! `am::systemio::Manager` (`spec/libam/usbio.md` part 3): the DIP / PUSH / LED words and the `DipSwitch` record.

use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use crate::nvram::{NvramContext, Record, RecordStatus};

/// Offset of the DIP byte inside the 512-byte `DipSwitch` record image (3.3 step 3).
const DIP_BYTE: usize = 8;

/// `am::systemio::Manager` (`ctx+135640`).
#[derive(Debug, Default)]
pub struct SystemIo {
    life: Lifecycle,
    dip: u64,
    push: u64,
    led: u64,
}

impl SystemIo {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `Manager::initialize(subStorage)` (`0x1402ABF70`, 3.3), from `CheckingCabinetTypeState::enter`: loads
    /// the DIP word from the `DipSwitch` record (byte 8). `sub_storage` is the platform module's flag.
    pub fn enter(&mut self, cx: &mut Cx<'_>, sub_storage: bool) {
        self.life.begin();
        self.dip = 0;
        self.load_dip(cx.nvram, sub_storage);
        self.life.state = StateWord::READY;
    }

    /// Steps 1 to 4 of `initialize` (3.3): the record object takes a handle on the NVRAM context (a context that
    /// is not open cannot be created here either: the platform's StaticInfo record would have failed the same
    /// way, so this is error `0x1009`), loads the `DipSwitch` record (a record with a bad CRC on both copies is
    /// reset to a zero payload and written back) and takes byte 8 as the DIP word. A failure leaves DIP 0 and
    /// latches `0x1009`, or `0x54` / `0x57` (by `sub_storage`) for a record I/O failure, with subcode 0.
    fn load_dip(&mut self, nvram: &NvramContext, sub_storage: bool) {
        let loaded = if nvram.is_open() {
            nvram.load_record(Record::DipSwitch, || Record::DipSwitch.blank_image())
        } else {
            Err(RecordStatus::ContextFailed)
        };
        match loaded {
            Ok(image) => {
                self.dip = u64::from(image[DIP_BYTE]);
                self.life.error.clear();
            }
            Err(status) => {
                if let Some((code, sub)) = NvramContext::status_error(status, sub_storage) {
                    self.life.error.set(code, sub);
                }
            }
        }
    }

    /// `Manager::update` (`0x1402AC0E0`, 3.4): evaluates the error slot only.
    pub fn update(&mut self, cx: &mut Cx<'_>) {
        let _ = cx;
    }

    /// `0x1402AC080`, terminate: words and error cleared, state 0.
    pub fn terminate(&mut self) {
        *self = Self::new();
    }

    /// `ctx+136544`: the DIP word (`BoardIoPort::dip`, `LoadCabinetType` argument).
    pub fn dip(&self) -> u64 {
        self.dip
    }

    /// The PUSH word (never written, 4.3).
    pub fn push(&self) -> u64 {
        self.push
    }

    /// The LED word (write-only storage, 4.4).
    pub fn led(&self) -> u64 {
        self.led
    }

    /// `set_led` (`0x140213ED0`).
    pub fn set_led(&mut self, value: u64) {
        self.led = value;
    }

    /// State word.
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// The error slot (`ctx+136240`), scanned by the Checking gate and the aggregator.
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }
}

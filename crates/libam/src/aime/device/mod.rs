//! The Aime card reader device layer (`spec/libam/aime.md` parts D and E): the serial port, the reader driver
//! (init, polling, firmware, LED) and the card semantics. The aimelib core (part C) drives it one tick per libam
//! frame; nothing here blocks or reads a clock: every stepper takes `now_ms`, a monotonic millisecond count.
//!
//! # Model
//!
//! * [`ReaderPorts`] is the reader manager (`0x140344780`): it opens one port per distinct COM number on
//!   [`backend::SerialPorts`], runs the per-port command queue (one outstanding command, 30 entries) and routes
//!   responses to the units by `dst`.
//! * [`Reader`] is one unit (`cAiMeNFCRW`): reader id = `com + 1000 * unit`. Several units may share a COM port.
//!   Steppers return the original's codes as enums: [`InitStep`] (`0x14034E310`), [`PollStep`] (`0x14034C440`),
//!   [`FirmwareStep`] (`0x14034DAF0`), [`LedFirmwareStep`] (`0x14034EBC0`).
//! * [`UnitCommands`] is the raw unit-command access of the e-money store-card reader (`spec/libam/payment.md`
//!   PayEmSvc 6.4) over the same port manager; the unit's [`UnitSlot`] keeps it and the aime sequences apart.
//! * A frame calls the stepper of the current core status once (or [`ReaderPorts::tick`] when the core only waits).
//!   Every stepper pumps the port first (sends the queue head, drains the line, dispatches responses).

mod felica;
mod firmware;
mod hub;
pub(crate) mod init;
mod lites;
mod poll;
#[cfg(test)]
mod tests;
mod unit_cmd;
pub(crate) mod wire;

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use hub::Hub;

use backend::SerialPorts;

pub use unit_cmd::{CommandError, CommandPoll, Holder, LedBlink, UnitCommand, UnitCommands, UnitSlot};
pub use wire::{Request, RequestAssembler, Response};

/// Outcome of [`ReaderPorts::open`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// COM numbers (deduplicated, `id % 1000`) that opened.
    pub opened: Vec<u16>,
    /// COM numbers that failed to open. They stay in the table; [`ReaderPorts::bind`] returns `None` for them.
    pub failed: Vec<u16>,
}

/// Timing and retry configuration of a unit (object offsets 16..36 of `cAiMeNFCRW`, 3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderConfig {
    /// Command response timeout (ms), clamped to 1000..=1900. Default 1000; am::aime uses 1900.
    pub command_timeout_ms: u32,
    /// Board-info (0xF0) timeout (ms), minimum 2000.
    pub board_timeout_ms: u32,
    /// Card read retries (default 3).
    pub card_retries: u8,
    /// Wait before re-polling after an empty scan (ms, default 100; am::aime passes its `cfg+4`).
    pub repoll_wait_ms: u32,
    /// Settle time between Start Polling and the first card detect (ms, default 300).
    pub detect_settle_ms: u32,
    /// Delay after Stop Polling (ms, default 0).
    pub post_stop_delay_ms: u32,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            command_timeout_ms: 1900,
            board_timeout_ms: 2000,
            card_retries: 3,
            repoll_wait_ms: 100,
            detect_settle_ms: 300,
            post_stop_delay_ms: 0,
        }
    }
}

/// Result of [`Reader::init_step`] (`0x14034E310`: 0 running, 1 done, 2 error, 3 interrupted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitStep {
    /// Still running.
    Running,
    /// Reader initialised.
    Done,
    /// A command timed out or answered garbage (the core raises its internal error 1).
    Failed,
    /// The stop request was set during init.
    Interrupted,
}

/// Result of [`Reader::poll_step`] (`0x14034C440`: 0 running, 1 done, 2 unit error, 3 done after stop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStep {
    /// Still scanning.
    Running,
    /// Scan finished: read [`Reader::scan`].
    Finished,
    /// Unit error or command timeout (the core raises its internal error 32).
    UnitError,
    /// Finished because a stop was requested.
    Stopped,
}

/// Result of [`Reader::firmware_step`] (`0x14034DAF0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareStep {
    /// Running.
    Running,
    /// Updated (core error 0).
    Done,
    /// Update failed (core error 30).
    Failed,
    /// The reader already runs the file's version (core error 3 text "no need update").
    AlreadyNew,
    /// Failed in the version phase or no firmware file for the generation (core error 1).
    VersionFault,
}

/// Result of [`Reader::led_firmware_step`] (`0x14034EBC0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedFirmwareStep {
    /// Running.
    Running,
    /// Done.
    Done,
    /// Failed (core error 30).
    Failed,
}

/// Firmware image bytes read by the core through the backend file system (`./firm/...`, 3.3).
#[derive(Debug, Clone, Default)]
pub struct FirmwareFiles {
    /// `TN32MSEC003S_V12.hex` (first generation, Intel-HEX text), if present.
    pub hex: Option<Vec<u8>>,
    /// `update_15286_6691_94.bin` (second generation, exactly 12288 bytes), if present.
    pub bin_gen2: Option<Vec<u8>>,
    /// `update_15396_6728_94.bin` (third generation, exactly 12288 bytes), if present.
    pub bin_gen3: Option<Vec<u8>>,
}

/// Why [`Reader::load_firmware`] / [`Reader::load_led_firmware`] rejected the files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareFileError {
    /// No file given.
    Missing,
    /// Hex record checksum, line format or size error.
    BadHex,
    /// A `.bin` image is not 12288 bytes.
    BadImage,
}

/// What a card scan ends with (`+328` detect state aside): the card records and the reader flags the core copies.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// Number of cards the reader reported (`+372`). More than one is the core's error 7.
    pub card_count: u32,
    /// The card records (`+41312`, two at most); entry 0 is the current card.
    pub cards: Vec<Card>,
    /// Auth kind (`+41872`): 1 Aime-style (key slot B), 2 Banapass-style (key slot A / FeliCa Standard), else 0.
    pub auth_kind: u8,
    /// FeliCa sub-kind (`+41960`) once a FeliCa card was classified.
    pub felica_kind: Option<FelicaKind>,
    /// FeliCa read error code (`+41964`, 0 ok; 1 R/W firmware, 14 message too long, ...).
    pub felica_error: u32,
    /// Read retry counter (`+41905`).
    pub retry_counter: u8,
}

/// Card record kind (`rec+0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CardKind {
    /// Nothing detected.
    #[default]
    None,
    /// MIFARE / ISO14443A with a 4-byte UID (Aime card, Banapass).
    Mifare,
    /// FeliCa.
    FeliCa,
    /// Amiibo (7-byte UID, amiibo mode).
    Amiibo,
    /// Detect failure (unsupported type).
    DetectFailure,
}

/// FeliCa classification (4.2): the sub-kind the core maps to its flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FelicaKind {
    /// FeliCa Lite (core flag `8`).
    Lite,
    /// FeliCa Standard, system code `8CA2` (core flag `0x10`).
    Standard,
    /// FeliCa Lite-S (core flag `0x80`).
    LiteS,
    /// Mobile FeliCa; the raw kind is 3, 6 or 7 (core flag `0x20`).
    Mobile(u8),
    /// Anything else (core flag `0x40`, incompatible).
    Other(u8),
}

/// Amiibo identity bytes (`rec+60..68`, 4.5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AmiiboId {
    /// Block 21 bytes 0..2 big-endian (`rec+60`).
    pub head: u32,
    /// Byte 3 (`rec+64`).
    pub byte3: u8,
    /// Bytes 4..5 big-endian (`rec+66`).
    pub word: u16,
    /// Byte 6 (`rec+68`).
    pub byte6: u8,
}

/// Lite-S blocks and descrambled value (`rec+73..162`, 5.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiteS {
    /// Random challenge RC used (`rec+73`).
    pub rc: [u8; 16],
    /// Blocks 0x82, 0x86 and 0x90 (`rec+89`, 16 bytes each) and the first 8 bytes of 0x91.
    pub blocks: [[u8; 16]; 3],
    /// First 8 bytes of block 0x91 (`rec+137`).
    pub block91: [u8; 8],
    /// ID block bytes 8..9, the DFC (`rec+145`).
    pub dfc: [u8; 2],
    /// Descrambled 10-byte value of block 0 (`rec+148`).
    pub value: [u8; 10],
    /// Low nibble of the descramble (`rec+158`).
    pub nibble: u32,
}

/// One card record (180 bytes in the original, 4.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Card {
    /// `rec+0`.
    pub kind: CardKind,
    /// `rec+4`: the access code is valid.
    pub valid: bool,
    /// `rec+8`: length of the detect entry.
    pub detect_len: u32,
    /// `rec+12`: UID (first 4 bytes).
    pub uid: [u8; 4],
    /// `rec+16`: 4-character card name from block 1 (unprintable -> space).
    pub name: [u8; 4],
    /// `rec+20`: block 1 bytes 12..15, big-endian.
    pub value: u32,
    /// `rec+24`: access code, 20 digits as packed BCD.
    pub access_code: [u8; 10],
    /// `rec+34`: FeliCa IDm (8) and PMm (8).
    pub idm_pmm: [u8; 16],
    /// `rec+52`: UID copy (amiibo).
    pub uid_copy: [u8; 8],
    /// `rec+60..68`.
    pub amiibo: AmiiboId,
    /// `rec+73..162`.
    pub lite_s: LiteS,
    /// `rec+164`: MIFARE block 0.
    pub block0: [u8; 16],
}

/// Reader identification the core copies after a successful init (`0x140327860` status 7/8, r = 1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReaderInfo {
    /// Hardware version text (`+41672`).
    pub hw_version: String,
    /// Firmware version text (`+41736`): two hex digits for generation 2/3 readers.
    pub fw_version: String,
    /// LED board name (`+41800`, "No LED Board" when absent).
    pub led_board_name: String,
    /// LED board firmware version byte (`+41864`).
    pub led_fw_version: u8,
    /// LED board present (`+41865`).
    pub led_present: bool,
    /// Firmware kind byte (`+41913`): 0 none, 1 "1.0", 2 "1.2", 9 other text, or the version byte of a 2nd/3rd
    /// generation reader.
    pub fw_kind: u8,
    /// Hardware generation (`+41876`): 0 unknown, 1, 2, 3.
    pub generation: u8,
}

/// Follow-up action stored before a scan (`0x14034A330`, modes 1 and 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowUp {
    /// Open the mailer with a `<MAILTO>..` parameter string.
    Mail(String),
    /// Open the browser with a `<URL>..` parameter string.
    Web(String),
}

/// A command could not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// The driver is not active (`-1`).
    Inactive,
    /// The queue holds 30 entries (`-5`, LED `-4`).
    QueueFull,
}

/// LED pattern table (`0x14071C5D0`): index to RGB.
const LED_PATTERNS: [[u8; 3]; 8] = [
    [0x00, 0x00, 0x00],
    [0xFF, 0x00, 0x00],
    [0xFF, 0xC8, 0x00],
    [0x00, 0xFF, 0x00],
    [0x00, 0xFF, 0xC8],
    [0x00, 0x96, 0xFF],
    [0xC8, 0x00, 0xFF],
    [0xFF, 0xFF, 0xB4],
];

/// The reader manager: the COM ports, their queues and the response routing (`0x1403440E0`, `0x140344780`).
#[derive(Clone)]
pub struct ReaderPorts {
    hub: Rc<RefCell<Hub>>,
}

impl fmt::Debug for ReaderPorts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReaderPorts").finish_non_exhaustive()
    }
}

impl ReaderPorts {
    /// Creates the manager over `serial`; `high_baud` is the `aime.high_baudrate` flag (115200 instead of 38400).
    pub fn new(serial: Rc<dyn SerialPorts>, high_baud: bool) -> Self {
        Self { hub: Rc::new(RefCell::new(Hub::new(serial, high_baud))) }
    }

    /// Opens the distinct COM ports of the reader ids (`id % 1000`). Ports that fail stay in the table.
    pub fn open(&self, ids: &[u32]) -> OpenReport {
        let (opened, failed) = self.hub.borrow_mut().open(ids);
        OpenReport { opened, failed }
    }

    /// Binds a unit to its port (`0x140348D90` + `0x140344600`): `None` when the port did not open.
    pub fn bind(&self, id: u32, config: ReaderConfig) -> Option<Reader> {
        let index = self.hub.borrow_mut().bind(id, config)?;
        Some(Reader { hub: Rc::clone(&self.hub), index })
    }

    /// The raw unit-command access of the unit bound with `id` (`None` when no such unit is bound); `slot` is the
    /// unit's owner slot shared with the aime sequences.
    pub fn unit_commands(&self, id: u32, slot: UnitSlot) -> Option<UnitCommands> {
        let index = self.hub.borrow().units.iter().position(|unit| unit.id == id)?;
        Some(UnitCommands::new(Rc::clone(&self.hub), index, id, slot))
    }

    /// Pumps every port once without stepping a unit (send queue heads, drain, dispatch, command timeouts).
    pub fn tick(&self, now_ms: u64) {
        self.hub.borrow_mut().pump(now_ms);
    }

    /// Closes all ports and drops the queues (`0x140344440`). Returns the original's bit mask (0 clean).
    pub fn close(&self) -> u32 {
        self.hub.borrow_mut().close()
    }
}

/// One reader unit (`cAiMeNFCRW`).
pub struct Reader {
    hub: Rc<RefCell<Hub>>,
    index: usize,
}

impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader").field("id", &self.id()).finish_non_exhaustive()
    }
}

impl Reader {
    /// Runs `f` on the manager and this unit's slot.
    fn with<R>(&self, f: impl FnOnce(&mut Hub, usize) -> R) -> R {
        f(&mut self.hub.borrow_mut(), self.index)
    }

    /// Queues one LED frame `[board][R][G][B]`; dropped when no board answered the probe.
    fn led(&mut self, rgb: [u8; 3]) -> Result<(), SendError> {
        self.with(|hub, u| {
            let unit = &hub.units[u];
            if !unit.info.led_present || (rgb != [0; 3] && unit.led_cancelled) {
                return Ok(());
            }
            let payload = [unit.addr, rgb[0], rgb[1], rgb[2]];
            hub.send(u, wire::cmd::LED_COLOUR, &payload)
        })
    }

    /// The reader id this unit was bound with (`com + 1000 * unit`).
    pub fn id(&self) -> u32 {
        self.with(|hub, u| hub.units[u].id)
    }

    /// One step of the reader init sequence (`0x14034E310`): 0x62, 0x30, 0x32, LED probe 0xF5/0xF0, key B, key A.
    pub fn init_step(&mut self, now_ms: u64) -> InitStep {
        self.with(|hub, u| hub.init_step(u, now_ms))
    }

    /// Reader identification, valid after [`InitStep::Done`].
    pub fn info(&self) -> ReaderInfo {
        self.with(|hub, u| hub.units[u].info.clone())
    }

    /// Starts a card scan (`0x1403317D0`): clears the card records, sends 0x40 with the card-type `filter`
    /// (bit0 ISO14443A, bit1 FeliCa). `timeout_ms == 0` scans without a time limit.
    pub fn start_polling(&mut self, filter: u8, timeout_ms: u64, now_ms: u64) {
        self.with(|hub, u| hub.start_polling(u, filter, timeout_ms, now_ms));
    }

    /// Amiibo mode (`+41912`): UID 7 detect and block 21 read, set before [`Reader::start_polling`] by mode 3.
    pub fn set_amiibo_mode(&mut self, on: bool) {
        self.with(|hub, u| hub.units[u].amiibo_mode = on);
    }

    /// Stores the mailer/web action run after the card (`0x14034A330`). `Err(())` for an empty string (`-1`).
    #[allow(clippy::result_unit_err)]
    pub fn set_follow_up(&mut self, action: FollowUp) -> Result<(), ()> {
        let (FollowUp::Mail(text) | FollowUp::Web(text)) = &action;
        if text.is_empty() {
            return Err(());
        }
        self.with(|hub, u| hub.units[u].follow_up = Some(action));
        Ok(())
    }

    /// One step of the scan state machine (`0x14034C440`).
    pub fn poll_step(&mut self, now_ms: u64) -> PollStep {
        self.with(|hub, u| hub.poll_step(u, now_ms))
    }

    /// The scan result (valid after [`PollStep::Finished`] / [`PollStep::Stopped`]).
    pub fn scan(&self) -> ScanResult {
        self.with(|hub, u| hub.scan_result(u))
    }

    /// Requests a stop (`0x14032D2D0`: `+310 = 1`); the current stepper finishes with `Interrupted` / `Stopped`.
    pub fn request_stop(&mut self) {
        self.with(|hub, u| hub.units[u].stop = true);
    }

    /// Unit error flag (`+305`).
    pub fn unit_error(&self) -> bool {
        self.with(|hub, u| hub.units[u].unit_error)
    }

    /// Sets the unit error flag (`0x14032DDB0`).
    pub fn set_unit_error(&mut self) {
        self.with(|hub, u| hub.units[u].unit_error = true);
    }

    /// Clears the unit error flag (`0x14032DDB0` with 0): the core does this after a finished scan.
    pub fn clear_unit_error(&mut self) {
        self.with(|hub, u| hub.units[u].unit_error = false);
    }

    /// The unit still has queued or outstanding commands (the rw flag bit of its port): the core finishes status 28
    /// only when this is false.
    pub fn busy(&self) -> bool {
        self.with(|hub, u| hub.units[u].rw || hub.units[u].pending)
    }

    /// Puts the LED queue in the cancelled state (`+16850`): non-zero colours are dropped until the next
    /// [`Reader::start_polling`]; "all off" still goes out.
    pub fn cancel_led(&mut self) {
        self.with(|hub, u| hub.units[u].led_cancelled = true);
    }

    /// Sets the LED colour (`0x14032D000`, command 0x81); dropped when no LED board answered.
    pub fn set_led_colour(&mut self, rgb: [u8; 3]) -> Result<(), SendError> {
        self.led(rgb)
    }

    /// Sets the LED pattern index 0..7 (`0x14032CF70`, table at `0x14071C5D0`).
    pub fn set_led_pattern(&mut self, index: u8) -> Result<(), SendError> {
        self.led(LED_PATTERNS[usize::from(index & 7)])
    }

    /// Terminates the unit (`0x14034C1D0`): LED all off if a board is present and the driver is deactivated.
    pub fn terminate(&mut self, now_ms: u64) {
        let _ = self.led([0; 3]);
        self.with(|hub, u| {
            hub.pump(now_ms);
            hub.units[u].active = false;
        });
    }

    /// Loads the reader firmware files (`0x140347E90`, flag 0) for [`Reader::firmware_step`].
    pub fn load_firmware(&mut self, files: &FirmwareFiles, force: bool) -> Result<(), FirmwareFileError> {
        self.with(|hub, u| hub.load_firmware(u, files, force))
    }

    /// Loads the LED board firmware hex (`./firm/837_15084_1_01.hex`, flag 1) for [`Reader::led_firmware_step`].
    /// `name` is the file name (the version byte comes from its tail).
    pub fn load_led_firmware(&mut self, name: &str, hex: &[u8], force: bool) -> Result<(), FirmwareFileError> {
        self.with(|hub, u| hub.load_led_firmware(u, name, hex, force))
    }

    /// One step of the reader firmware update (`0x14034DAF0`).
    pub fn firmware_step(&mut self, now_ms: u64) -> FirmwareStep {
        self.with(|hub, u| hub.firmware_step(u, now_ms))
    }

    /// One step of the LED board firmware update (`0x14034EBC0`).
    pub fn led_firmware_step(&mut self, now_ms: u64) -> LedFirmwareStep {
        self.with(|hub, u| hub.led_firmware_step(u, now_ms))
    }

    /// Firmware update progress `(current block, total blocks)` (`+41308`, `+424`).
    pub fn firmware_progress(&self) -> (u32, u32) {
        self.with(|hub, u| hub.firmware_progress(u))
    }
}

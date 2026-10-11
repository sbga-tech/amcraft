//! The Aime card reader module (`am::aime::Manager`, `spec/libam/aime.md`): reader units, scans, the Aime DB
//! client and the play log.
//!
//! The module is layered: [`device`] (parts D and E: the reader driver and cards), [`lib`]
//! (parts B and C: the aimelib core and the Aime DB client) and the facade in this directory (part A: units from
//! `AppConfig`, command sequences, events, the module lifecycle, the play log).
//!
//! # Cross-module methods
//!
//! * [`Aime::update`] (`0x140228FD0`, then `0x140229520`, M.3): runs after laninstall.
//! * [`Aime::log`] (`0x14022A0D0`): what `SequencePort::aime_log` forwards to (sequence.md 5.4, aime.md send
//!   play log).
//! * [`Aime::set_permit`] (`0x14022A530`): the idle/error refresh `0x140254C40` and the Operating step apply; the
//!   readers accept cards only while `permit` and not `suspended`.
//! * [`Aime::clear_play_log`] (`0x14022A5E0`): the first-launch step of the main flow (sequence.md 5.4).
//! * [`Aime::unit_com_ports`]: the `aime.unit` list the e-money module reads (`core.md` 7, row 22).
//! * [`Aime::card_id`]: the FeliCa card id of an Aime id (the Aime user-info list the Aime Pay module reads).
//! * [`Aime::set_board`]: the game id and keychip serial every DB request carries (`0x1402293D0`).

pub(crate) mod code;
pub mod device;
mod facade;
mod facade_error;
mod facade_log;
mod facade_unit;
pub mod lib;
mod port;
pub(crate) mod tables;

use crate::config::AimeConfig;

use self::device::ReaderPorts;
use self::facade_log::LogQueue;
use self::facade_unit::Unit;
use self::lib::{LibManager, NetworkClient};
use crate::module::{ErrorCell, Lifecycle};

pub use port::{
    AccessCode, AimeAppConfig, AimeEvent, AimeHandle, AimePort, CAMPAIGN_NAME_UNITS, CampaignInfoEntry,
    CampaignInfoList, CampaignProgressEntry, CampaignProgressList, CardInfo, FinishedStatus, FinishedTask, LedPattern,
    LibamAimeId, PlayLogStatus, ReaderInfo, RelatedSegaId, UnitTaskFinished,
};

/// `am::aime::Manager` (`ctx+172272`).
#[derive(Debug, Default)]
pub struct Aime {
    life: Lifecycle,
    config: AimeConfig,
    permit: bool,
    suspended: bool,
    /// `module+0` (A.7): 0 unconstructed, 1 configured, 2 start-up pass, 3 operational (also `enable = 0`), 4 fault.
    phase: u32,
    /// `module+4`: the start-up step.
    step: u32,
    /// `module+8`: the unit the start-up pass works on.
    index: usize,
    /// `module+16`: the terminate flag of the refresh sweep (the sequence's test flag); the start-up detect then
    /// runs without a firmware update.
    terminate: bool,
    /// The global flag byte (`byte_14095A32E`): fatal 87 instead of 84.
    develop: bool,
    /// `module+24`, the APM `pending` flag of `0x14022A670`.
    pending: bool,
    /// The APM stop of the periodic network work is in effect.
    stopped: bool,
    game_id: String,
    serial: String,
    /// `set_board` stored the game id and serial (`+45688`).
    board_set: bool,
    ports: Option<ReaderPorts>,
    manager: LibManager,
    units: Vec<Unit>,
    net: Option<NetworkClient>,
    /// The network client's listener holds the DB-dead record (`AIME+46044`).
    db_dead: bool,
    log: LogQueue,
    log_loaded: bool,
    /// The Aime user-info list: `(aime id, FeliCa card id)`.
    user_info: Vec<(u32, [u8; 8])>,
    events: Vec<AimeEvent>,
    now_ms: u64,
    /// ALL.Net location id (`module+45708`; the original calls it region), set by `0x140229520`.
    region: u32,
    /// `NvramContext::app_generation` the play-log queue was loaded under.
    log_generation: u32,
}

impl Aime {
    /// The configured reader units as `(com_port, unit_index)`.
    pub fn unit_com_ports(&self) -> Vec<(u64, u64)> {
        self.config.units.iter().map(|unit| (unit.com_port, unit.unit_index)).collect()
    }

    /// `aime.enable`.
    pub fn enabled(&self) -> bool {
        self.config.enable
    }

    /// `allReady` term (`0x140229300`): the module state is 2, 3 or 4.
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The error cell (`0x140229360`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }
}

//! Raw unit commands for the e-money store-card reader (`spec/libam/payment.md` PayEmSvc 6.4, `spec/libam/aime.md`
//! D.4): the seven commands `StartPolling`, `SetLedBlink`, `CardDetect`, `MifareKeySetA`, `MifareAuthenticationA`,
//! `MifareRead` and `StopPolling`, sent one at a time over the port hub the aime module's own readers use.
//!
//! The original runs them on a private serial object (path B); here the unit is shared, so the unit's owner slot
//! (`0x1402563B0` push, `0x1402563D0` pop, `0x140259670` availability) keeps the two users from interleaving:
//! [`UnitSlot`] is the slot, the aime module holds it while one of its sequences owns the unit, and
//! [`UnitCommands`] sends nothing unless e-money holds it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::SendError;
use super::hub::Hub;
use super::wire::cmd;

/// Who holds a reader unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Holder {
    /// Nobody: both modules may start.
    #[default]
    Free,
    /// An aime sequence owns the unit.
    Aime,
    /// The e-money store-card reader owns the unit.
    EMoney,
}

/// The owner slot of one unit (`module+2680+24*i`, reduced to its holder). Clones share the slot.
#[derive(Debug, Clone, Default)]
pub struct UnitSlot(Rc<Cell<Holder>>);

impl UnitSlot {
    /// The current holder.
    pub fn holder(&self) -> Holder {
        self.0.get()
    }

    /// `0x140259670`: the unit is not held by somebody else than `who`.
    pub fn available_to(&self, who: Holder) -> bool {
        matches!(self.0.get(), Holder::Free) || self.0.get() == who
    }

    /// `0x1402563B0`: `who` takes the unit; `false` while another module holds it.
    pub fn acquire(&self, who: Holder) -> bool {
        let free = self.available_to(who);
        if free {
            self.0.set(who);
        }
        free
    }

    /// `0x1402563D0`: `who` gives the unit back (nothing happens when somebody else holds it).
    pub fn release(&self, who: Holder) {
        if self.0.get() == who {
            self.0.set(Holder::Free);
        }
    }
}

/// The arguments of `SetLedBlink` (`0x1402BA7A0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedBlink {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Blink mode.
    pub mode: u16,
    /// On time (ms).
    pub on_ms: u16,
    /// Off time (ms).
    pub off_ms: u16,
}

impl LedBlink {
    /// Blue blinking 500 ms on, 500 ms off: the card reader waits for a card.
    pub const BLUE: Self = Self { r: 0, g: 0, b: 255, mode: 0, on_ms: 500, off_ms: 500 };
    /// Solid red: the card was refused.
    pub const RED: Self = Self { r: 255, g: 0, b: 0, mode: 0, on_ms: 0, off_ms: 0 };
    /// Off.
    pub const OFF: Self = Self { r: 0, g: 0, b: 0, mode: 0, on_ms: 0, off_ms: 0 };
}

/// One unit command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitCommand {
    /// `0x40`: start polling for ISO 14443A cards.
    StartPolling,
    /// `0x82` on the LED board of the unit (no answer).
    SetLedBlink(LedBlink),
    /// `0x42`: the answer is `count, {type, idlen, id}..`.
    CardDetect,
    /// `0x50`: the key A of every sector.
    MifareKeySetA([u8; 6]),
    /// `0x51`: authenticates `sector` (its trailer block) for the card with key diversifier `uid4`.
    MifareAuthA {
        /// Sector number.
        sector: u8,
        /// The last four bytes of the card ID.
        uid4: [u8; 4],
    },
    /// `0x52`: reads one block of `sector`; the answer is the 16 data bytes.
    MifareRead {
        /// Sector number.
        sector: u8,
        /// Block within the sector (0..=2).
        block: u8,
        /// The last four bytes of the card ID.
        uid4: [u8; 4],
    },
    /// `0x41`.
    StopPolling,
}

/// Why a command was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    /// The unit is not held by the caller, or no longer exists (the aime module restarted).
    NotHeld,
    /// The previous command has not been answered.
    Busy,
    /// The port refused the frame.
    Send(SendError),
}

/// The state of the command in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPoll {
    /// No command in flight.
    Idle,
    /// Waiting for the answer.
    Pending,
    /// The reader answered with status 0; the answer payload.
    Done(Vec<u8>),
    /// The reader refused the command or did not answer in time.
    Failed,
}

/// Unit commands over the port hub of the aime module. One command is in flight at a time; the reader answers it
/// within the unit's command timeout or the command fails.
pub struct UnitCommands {
    hub: Rc<RefCell<Hub>>,
    index: usize,
    id: u32,
    slot: UnitSlot,
    in_flight: Option<u8>,
}

impl std::fmt::Debug for UnitCommands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnitCommands").field("id", &self.id).field("in_flight", &self.in_flight).finish_non_exhaustive()
    }
}

impl UnitCommands {
    pub(super) fn new(hub: Rc<RefCell<Hub>>, index: usize, id: u32, slot: UnitSlot) -> Self {
        Self { hub, index, id, slot, in_flight: None }
    }

    fn live(&self, hub: &Hub) -> bool {
        hub.units.get(self.index).is_some_and(|unit| unit.id == self.id)
    }

    /// The unit is still bound to an open port of the manager (the aime module did not restart or close).
    pub fn bound(&self) -> bool {
        self.live(&self.hub.borrow())
    }

    /// The aime module has probed the unit: its init sequence ended, with success or with the driver gone inactive.
    pub fn probed(&self) -> bool {
        let hub = self.hub.borrow();
        self.live(&hub) && (hub.units[self.index].initialised || !hub.units[self.index].active)
    }

    /// The reader is initialised and its driver active: it takes commands (the lease state byte `+17 == 3`).
    pub fn usable(&self) -> bool {
        let hub = self.hub.borrow();
        self.live(&hub) && hub.units[self.index].initialised && hub.units[self.index].active
    }

    /// The unit's polling gap (`aime.polling_interval`) in milliseconds.
    pub fn repoll_wait_ms(&self) -> u64 {
        let hub = self.hub.borrow();
        if self.live(&hub) { u64::from(hub.units[self.index].config.repoll_wait_ms) } else { 0 }
    }

    /// Takes the unit (`0x1402563B0`); `false` while an aime sequence holds it.
    pub fn acquire(&self) -> bool {
        self.slot.acquire(Holder::EMoney)
    }

    /// Whether e-money holds the unit.
    pub fn held(&self) -> bool {
        self.slot.holder() == Holder::EMoney
    }

    /// No aime sequence holds the unit (`0x140259670`).
    pub fn available(&self) -> bool {
        self.slot.available_to(Holder::EMoney)
    }

    /// Drops the command in flight (its answer, if it still arrives, is ignored).
    pub fn cancel(&mut self) {
        if let Some(command) = self.in_flight.take() {
            let mut hub = self.hub.borrow_mut();
            if self.live(&hub) {
                hub.clear_command(self.index, command);
                let unit = &mut hub.units[self.index];
                unit.timer_armed = false;
                unit.forced_expiry = false;
            }
        }
    }

    /// Gives the unit back (`0x1402563D0`); a command still in flight is dropped.
    pub fn release(&mut self) {
        self.cancel();
        self.slot.release(Holder::EMoney);
    }

    /// Sends `command`. Everything but the LED frame is answered; poll with [`UnitCommands::poll`].
    pub fn send(&mut self, command: UnitCommand) -> Result<(), CommandError> {
        if !self.held() {
            return Err(CommandError::NotHeld);
        }
        if self.in_flight.is_some() {
            return Err(CommandError::Busy);
        }
        let mut hub = self.hub.borrow_mut();
        if !self.live(&hub) {
            return Err(CommandError::NotHeld);
        }
        let u = self.index;
        let sector_block = |sector: u8, block: u8| sector * 4 + block;
        let (code, payload): (u8, Vec<u8>) = match command {
            UnitCommand::StartPolling => (cmd::START_POLLING, vec![POLLING_TYPE]),
            UnitCommand::SetLedBlink(led) => {
                // Like every LED frame: dropped when no LED board answered the probe.
                if !hub.units[u].info.led_present {
                    return Ok(());
                }
                let [mode_hi, mode_lo] = led.mode.to_be_bytes();
                let [on_hi, on_lo] = led.on_ms.to_be_bytes();
                let [off_hi, off_lo] = led.off_ms.to_be_bytes();
                let board = hub.units[u].addr;
                (cmd::LED_BLINK, vec![board, led.r, led.g, led.b, mode_hi, mode_lo, on_hi, on_lo, off_hi, off_lo])
            }
            UnitCommand::CardDetect => (cmd::CARD_DETECT, Vec::new()),
            UnitCommand::MifareKeySetA(key) => (cmd::SET_KEY_A, key.to_vec()),
            UnitCommand::MifareAuthA { sector, uid4 } => {
                (cmd::AUTH_KEY_A, [&uid4[..], &[sector_block(sector, 3)]].concat())
            }
            UnitCommand::MifareRead { sector, block, uid4 } => {
                (cmd::READ_BLOCK, [&uid4[..], &[sector_block(sector, block)]].concat())
            }
            UnitCommand::StopPolling => (cmd::STOP_POLLING, Vec::new()),
        };
        // A flag or an answer an earlier command of the unit left behind is not this command's.
        hub.units[u].unit_error = false;
        hub.units[u].response = None;
        if code == cmd::LED_BLINK {
            return hub.send(u, code, &payload).map_err(CommandError::Send);
        }
        let timeout = hub.units[u].config.command_timeout_ms;
        hub.units[u].arm(timeout);
        hub.send(u, code, &payload).map_err(CommandError::Send)?;
        self.in_flight = Some(code);
        Ok(())
    }

    /// One manager cycle of the unit's port and the state of the command in flight.
    pub fn poll(&mut self, now_ms: u64) -> CommandPoll {
        let mut hub = self.hub.borrow_mut();
        if !self.live(&hub) {
            self.in_flight = None;
            return CommandPoll::Failed;
        }
        hub.pump(now_ms);
        let Some(command) = self.in_flight else { return CommandPoll::Idle };
        let u = self.index;
        if let Some(response) = hub.take_response(u, command) {
            self.in_flight = None;
            return if response.status == 0 { CommandPoll::Done(response.payload) } else { CommandPoll::Failed };
        }
        if hub.units[u].unit_error || hub.units[u].expired(now_ms) {
            hub.clear_command(u, command);
            let unit = &mut hub.units[u];
            unit.unit_error = false;
            unit.timer_armed = false;
            unit.forced_expiry = false;
            self.in_flight = None;
            return CommandPoll::Failed;
        }
        CommandPoll::Pending
    }
}

/// The polling type byte of `StartPolling` (`unit+312`, taken from `impl+1213`: 0/1/2 in the original). The e-money
/// call passes no argument, so the byte is [INFERENCE]: the card-type mask of ISO 14443A (bit 0), the family of a
/// MIFARE Classic card, as the scan of the aime module sets it.
const POLLING_TYPE: u8 = 0x01;

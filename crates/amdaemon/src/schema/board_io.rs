//! BoardIO blocks: `ServerData+0x478C` (0x8) and `ClientData+0x1A1C` (0x8).
//!
//! Evidence: `spec/controllers/can_boardio_usbdevice.md` §1.2. The request kind is a single constant "LED", so
//! neither block carries a task field, and the response is only the generation counter (no `succeeded` byte,
//! `checkResult` `0x1400255E0`).

use num_enum::TryFromPrimitive;
use zero_schema::zero;

use crate::schema::{client_block, server_block};

/// `boardio::ServerLedResponse` (`0x4`): the executor response is only the generation counter.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerLedResponse {
    /// Request generation mirrored by `checkResult`.
    pub generation: u32,
}

/// `ServerData.boardIO`: the LED response, the latched DIP switches and the push switches.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerBoardIo {
    /// LED request response.
    pub led_response: ServerLedResponse,
    /// `(u8)` of the libam DIP word, latched once per process (`clearServerData` `0x1400244D0`).
    pub dip_switch_bits: u8,
    /// Byte load of the **same** libam DIP address as [`Self::dip_switch_bits`], so both bytes are always equal
    /// (whether libam meant a different byte is unresolved).
    pub dip_switch_app_value: u8,
    /// `(u8)` of the libam push-switch word, refreshed on every update (`0x140024600`).
    pub push_switch_bits: u8,
}

/// `boardio::ClientLedRequest` (`0x8`).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientLedRequest<'b> {
    /// Request generation; a difference to the response starts a request.
    pub generation: u32,
    /// Raw [`LedCommand`] bytes; element `i` controls LED bit `i`.
    pub commands: &'b [u8; 2],
}

/// `ClientData.boardIO`: the only request kind, LED.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientBoardIo<'b> {
    /// The LED request.
    pub led_request: ClientLedRequest<'b>,
}

server_block!(board_io, board_io_mut: ServerBoardIo, ServerBoardIoRef, ServerBoardIoMut @ 0x478C);
client_block!(board_io, board_io_mut: ClientBoardIo, ClientBoardIoRef, ClientBoardIoMut @ 0x1A1C);

/// `boardio::LedCommand` (one byte; name table `0x1405EEE40`: `None`, `On`, `Off`).
///
/// The wire byte is an arbitrary `u8`; only `1` and `2` change an LED bit, every other value is treated like
/// `None` (`BoardIoController::accept` `0x140024780` tests `== 1` then `== 2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u8)]
pub enum LedCommand {
    /// `0`: leave the LED bit unchanged.
    None = 0,
    /// `1`: set the LED bit.
    On = 1,
    /// `2`: clear the LED bit.
    Off = 2,
}

impl LedCommand {
    /// Name from the enum-to-string table `0x1405EEE40`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::On => "On",
            Self::Off => "Off",
        }
    }

    /// Applies the command to bit `bit` of the libam LED word: `On` sets it, `Off` clears it, `None` leaves the
    /// word untouched. All other bits are never modified.
    pub const fn apply(self, led: u64, bit: u32) -> u64 {
        match self {
            Self::None => led,
            Self::On => led | (1 << bit),
            Self::Off => led & !(1 << bit),
        }
    }
}

impl ClientLedRequestRef<'_> {
    /// Typed command `index` (`None` when the index or the wire byte is out of range).
    pub fn command(&self, index: usize) -> Option<LedCommand> {
        self.commands().get(index).and_then(|&raw| LedCommand::try_from(raw).ok())
    }
}

const _: () = {
    assert!(ServerBoardIo::SCHEMA_SIZE == 0x8);
    assert!(ClientBoardIo::SCHEMA_SIZE == 0x8);
};

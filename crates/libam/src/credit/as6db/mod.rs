//! The Sega AS-6DB coin selector (`spec/libam/credit.md` part II).
//!
//! * `frame`: the wire format: header, the two CRC-8 variants, the keystream scrambler and the receive validator.
//! * `driver`: the board record, the non-blocking transfer state machine and the command handlers.
//! * `device`: the special device manager the credit module owns: lockout, fault supervision, coin cross-check.
//!   The COM port comes from [`backend::SerialPorts`].

pub mod device;
pub mod driver;
pub mod frame;

pub use device::{InputNode, Io, SlotCondition, SpecialDevice};

//! The devices a simulated cabinet plugs into its COM ports.
//!
//! Each one is attached by COM number on [`super::SimSerial`] and speaks the protocol of the module that opens
//! that port. The protocol code that only the simulation needs (the Banapass block encoder, the reader's scripted
//! cards) lives here rather than in the module.

mod aime_reader;
mod as6db;
mod store_card;
mod vfd;

pub use aime_reader::{SIM_CARD_NAME, SIM_CARD_VALUE, SIM_LED_VERSION, SimAimeReader, SimCard, SimRequest};
pub use as6db::{SimAs6db, WireFrame, Yen};
pub use store_card::encrypt_card;
pub use vfd::SimVfd;

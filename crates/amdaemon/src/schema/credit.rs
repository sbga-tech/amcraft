//! Credit blocks (`ServerData+0x1B88`, 0xE0 bytes; `ClientData+0xC40`, 0x44 bytes).
//!
//! Evidence: `spec/controllers/credit_emoney_aimepay.md` §2.2, claims `credit_emoney_aimepay.credit.sd.*` and
//! `credit_emoney_aimepay.credit.cd.layout`.

use num_enum::TryFromPrimitive;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

/// `CreditSound` published with a coin-in event: `1` when libam's node event kind is the coin chute, `2`
/// otherwise (values direct, names inferred, claim `credit_emoney_aimepay.credit.coin_in.sound_enum`). The field
/// holds neither value until the first event is published.
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CreditSound {
    /// Coin chute (libam kind flag 0).
    Coin = 1,
    /// Service switch (libam kind flag non-zero).
    Service = 2,
}

/// `ServerCoinInData` (`+0x0C`, 0x10 bytes): the latest published insert-coin event.
///
/// `sound` and `players` are valid together with the *new* `generation`; the client acknowledges by copying
/// `generation` into [`ClientCoinIn::generation`].
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerCoinIn {
    /// Event generation (`SD+0x1B94`), incremented per published event.
    pub generation: u32,
    /// [`CreditSound`] of the event; `None` until the first event.
    pub sound: Option<CreditSound>,
    /// One flag per player the event applies to.
    pub players: [bool; 8],
}

/// `CreditBookkeeping` (`+0x1C`, 0x28 bytes): ten counters copied from libam `credit+0x5F0`.
///
/// Field names are inferred from libam's own setter pairing (`credit_emoney_aimepay.credit.sd.bookkeeping`).
#[zero(endian = "little")]
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CreditBookkeeping {
    /// Coin counters of chutes 1..4 (libam keeps eight, only four are exported).
    pub coin: [u32; 4],
    /// libam `+0x20`.
    pub total_coin: u32,
    /// libam `+0x28`.
    pub coin_credit: u32,
    /// libam `+0x2C`.
    pub service_credit: u32,
    /// libam `+0x24`.
    pub e_money_coin: u32,
    /// libam `+0x30`.
    pub e_money_credit: u32,
    /// libam `+0x34`.
    pub total_credit: u32,
}

/// `CreditConfig` (`+0x44`, 0x38 bytes): the libam credit configuration as published at clear time. The
/// rejected `Config` task carries the same layout (see [`ClientCreditRequestRef::config_info`]).
#[zero(endian = "little")]
#[derive(Debug, PartialEq, Eq)]
pub struct CreditConfig {
    /// libam `chuteShareType == 1`.
    pub coin_chute_common: bool,
    /// libam `serviceShareType == 1`.
    pub service_common: bool,
    /// libam `operation == 2`.
    pub freeplay: bool,
    /// libam `cfg+0xC+4i`.
    pub multipliers: [u32; 2],
    /// libam `cfg+0x18` (validator string "Invalid bonus adder").
    pub bonus_adder: u32,
    /// libam `cfg+0x14` (validator string "Invalid coin to credit").
    pub coin_to_credit: u32,
    /// libam `cfg+0x1C+4i`.
    pub game_cost: [u32; 8],
    /// libam `cfg+0x3C`.
    pub coin_amount: u32,
}

/// `ServerPlayerData` (0xC bytes, stride of `players[]`).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerPlayer {
    /// `CreditValue.credit`.
    pub credit: u32,
    /// `CreditValue.remain`.
    pub remain: u32,
    /// Coins the player can still insert.
    pub addable_coin: u32,
}

/// `ServerSpecialDeviceData` (`+0xDC`, 3 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerSpecialDevice {
    /// Written only at clear time.
    pub available: bool,
    /// Lockout state, refreshed every update.
    pub lockout_on: bool,
    /// Whether a lockout change is allowed right now, refreshed every update.
    pub can_lockout: bool,
}

/// `ServerData.credit`: the executor response, the insert-coin channel and the published libam credit state.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerCredit {
    /// libam credit module enabled flag; written only at clear time.
    pub available: bool,
    /// Executor response (`SD+0x1B8C`).
    pub response: Response,
    /// Insert-coin event channel.
    pub coin_in: ServerCoinIn,
    /// Bookkeeping counters.
    pub bookkeeping: CreditBookkeeping,
    /// Configuration (refreshed only at clear time).
    pub config: CreditConfig,
    /// Per-player credit state.
    pub players: [ServerPlayer; 8],
    /// Special (lockout) device.
    pub special: ServerSpecialDevice,
    /// libam coin-in-ignore flag.
    pub coin_in_ignored: bool,
}

/// `ClientRequest` of the credit executor (`CD+0xC40`, 0x40 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientCreditRequest<'b> {
    /// Request generation.
    pub generation: u32,
    /// Raw [`CreditTask`].
    pub task: u32,
    /// Per-task info union, decoded by the [`ClientCreditRequestRef`] payload accessors.
    pub info: &'b [u8; 0x38],
}

/// `ClientCoinInData` (`CD+0xC80`): the client's acknowledgement of [`ServerCoinIn::generation`].
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientCoinIn {
    /// Last coin-in generation the client has consumed.
    pub generation: u32,
}

/// `ClientData.credit`: the executor request and the coin-in acknowledgement.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientCredit<'b> {
    /// Executor request.
    pub request: ClientCreditRequest<'b>,
    /// Coin-in acknowledgement.
    pub coin_in: ClientCoinIn,
}

server_block!(credit, credit_mut: ServerCredit, ServerCreditRef, ServerCreditMut @ 0x1B88);
client_block!(credit, credit_mut: ClientCredit, ClientCreditRef, ClientCreditMut @ 0xC40);

/// `ipc::credit::Task` (name table `0x1405EEE60`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum CreditTask {
    /// Rejected by the daemon with `am::Result` code 6 (`0x14002BAAD`).
    Config = 0,
    /// `credit.clear()`.
    Clear = 1,
    /// `credit.pay(player, gameCostIndex, count)`.
    Pay = 2,
    /// `credit.specialLockout(on)`.
    SpecialLockout = 3,
    /// `credit.setCoinInIgnore(flag)`.
    CoinInIgnore = 4,
}

/// Info of [`CreditTask::Pay`] (8 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct PayInfo {
    /// Player index.
    pub player_index: u32,
    /// Index into the libam game cost table.
    pub game_cost_index: u16,
    /// Number of plays to pay.
    pub count: u16,
}

/// Info of [`CreditTask::SpecialLockout`].
#[zero(endian = "little")]
#[derive(Debug)]
pub struct SpecialLockoutInfo {
    /// Requested lockout state.
    pub lockout: bool,
}

/// Info of [`CreditTask::CoinInIgnore`].
#[zero(endian = "little")]
#[derive(Debug)]
pub struct CoinInIgnoreInfo {
    /// New coin-in-ignore flag.
    pub ignore: bool,
}

impl<'w> ClientCreditRequestRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<CreditTask> {
        CreditTask::try_from(self.task()).ok()
    }

    /// Info of [`CreditTask::Pay`].
    pub fn pay_info(&self) -> PayInfoRef<'w> {
        let info: &'w [u8; 0x38] = self.info();
        PayInfo::access_prefix(info).expect("an integer payload accepts every bit pattern").0
    }

    /// Info of [`CreditTask::SpecialLockout`]: the requested state, `None` when the byte is not a boolean.
    pub fn special_lockout(&self) -> Option<bool> {
        let info: &'w [u8; 0x38] = self.info();
        SpecialLockoutInfo::access_prefix(info).ok().map(|(payload, _)| payload.lockout())
    }

    /// Info of [`CreditTask::CoinInIgnore`]: the new flag, `None` when the byte is not a boolean.
    pub fn coin_in_ignore(&self) -> Option<bool> {
        let info: &'w [u8; 0x38] = self.info();
        CoinInIgnoreInfo::access_prefix(info).ok().map(|(payload, _)| payload.ignore())
    }

    /// Info of [`CreditTask::Config`] (request trace only): a [`CreditConfig`], `None` when a flag is not a
    /// boolean.
    pub fn config_info(&self) -> Option<CreditConfigRef<'w>> {
        let info: &'w [u8; 0x38] = self.info();
        CreditConfig::access_prefix(info).ok().map(|(payload, _)| payload)
    }
}

const _: () = {
    assert!(ServerCoinIn::SCHEMA_SIZE == 0x10);
    assert!(CreditBookkeeping::SCHEMA_SIZE == 0x28);
    assert!(CreditConfig::SCHEMA_SIZE == 0x38);
    assert!(ServerPlayer::SCHEMA_SIZE == 0xC);
    assert!(ServerSpecialDevice::SCHEMA_SIZE == 3);
    assert!(ServerCredit::SCHEMA_SIZE == 0xE0);
    assert!(ClientCreditRequest::SCHEMA_SIZE == 0x40);
    assert!(ClientCoinIn::SCHEMA_SIZE == 4);
    assert!(ClientCredit::SCHEMA_SIZE == 0x44);
    assert!(PayInfo::SCHEMA_SIZE == 8);
    assert!(SpecialLockoutInfo::SCHEMA_SIZE == 1);
    assert!(CoinInIgnoreInfo::SCHEMA_SIZE == 1);
};

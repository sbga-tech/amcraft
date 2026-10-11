//! EMoney blocks: `ServerData+0x1C68` (0x1810 bytes) and `ClientData+0xC84` (0x38 bytes).
//!
//! Evidence: `spec/controllers/credit_emoney_aimepay.md` §3.2, claims `credit_emoney_aimepay.emoney.sd.*`
//! and `credit_emoney_aimepay.emoney.cd.layout`.

use num_enum::TryFromPrimitive;
use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

/// `ipc::emoney::Task` (name table `0x1405EEE88`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum EMoneyTask {
    /// Cancel the running operation (display test or payment).
    Cancel = 0,
    /// Start the display self test.
    DisplayCheck = 1,
    /// Terminal authentication.
    TerminalAuth = 2,
    /// Terminal removal.
    TerminalRemove = 3,
    /// Balance inquiry for a brand.
    Balance = 4,
    /// Pay an amount for an item.
    AmountPay = 5,
    /// Pay with coins.
    CoinPay = 6,
}

impl EMoneyTask {
    /// Name from the task table (`0x1405EEE88`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cancel => "Cancel",
            Self::DisplayCheck => "DisplayCheck",
            Self::TerminalAuth => "TerminalAuth",
            Self::TerminalRemove => "TerminalRemove",
            Self::Balance => "Balance",
            Self::AmountPay => "AmountPay",
            Self::CoinPay => "CoinPay",
        }
    }
}

/// `amdaemon::EMoneyBrandId` (names table `0x1405EBDE0`); libam numbers the brands differently.
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum EMoneyBrandId {
    /// nanaco.
    Nanaco = 0,
    /// Rakuten Edy.
    Edy = 1,
    /// Transport IC cards.
    Transport = 2,
    /// WAON.
    Waon = 3,
    /// PASELI.
    Paseli = 4,
    /// iD.
    Id = 5,
    /// SAPICA.
    Sapica = 6,
}

/// Number of brand slots published in `brands[]`.
pub const BRAND_COUNT: usize = 7;
/// Number of history slots in [`ServerDealResultData::items`].
pub const DEAL_RESULT_CAPACITY: usize = 20;

impl EMoneyBrandId {
    /// Every brand in id order (the slot order of `brands[]`).
    pub const ALL: [Self; BRAND_COUNT] =
        [Self::Nanaco, Self::Edy, Self::Transport, Self::Waon, Self::Paseli, Self::Id, Self::Sapica];

    /// Display name used by the request trace (`0x1405EBDE0`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Nanaco => "Nanaco",
            Self::Edy => "Edy",
            Self::Transport => "Transport",
            Self::Waon => "Waon",
            Self::Paseli => "Paseli",
            Self::Id => "iD",
            Self::Sapica => "Sapica",
        }
    }
}

/// `ServerSoundData` (`+0x0C`, 0x198 bytes): the latest published sound request.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerSound<'b> {
    /// Sound generation (`SD+0x1C74`), incremented per published sound.
    pub generation: u32,
    /// File path of the sound to play (at most 200 characters).
    #[zero(capacity = 201)]
    pub file_path: &'b U16CStr,
}

/// `ServerPayCompletionData` (`+0x1A4`): incremented when an AmountPay ends with an incomplete deal.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerPayCompletion {
    /// Pay-completion generation (`SD+0x1E0C`).
    pub generation: u32,
}

/// `ServerBrandData` (0x1B4 bytes, stride of `brands[]`; indexed by [`EMoneyBrandId`]).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerBrand<'b> {
    /// Brand name (at most 15 characters).
    #[zero(capacity = 16)]
    pub name: &'b U16CStr,
    /// Icon file path (at most 200 characters).
    #[zero(capacity = 201)]
    pub icon_file_path: &'b U16CStr,
    /// libam `balanceFlag`.
    pub balance: u8,
}

/// `ServerResult` (0x78 bytes): one deal, either the current operation result or a history item.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerResult<'b> {
    /// The slot holds a converted deal.
    pub valid: bool,
    /// `EMoneyResultStatus` (names unresolved): `0..=4` mapped from the libam deal status
    /// (`0→1, 2→4, 3→2, 4→3`, otherwise `0`); `completePay` also writes `1`.
    pub status: u32,
    /// Deal time (0 when libam's time is invalid).
    pub time: i64,
    /// Brand of the deal.
    pub brand_id: EMoneyBrandId,
    /// Deal number (at most 20 characters).
    #[zero(capacity = 21)]
    pub deal_number: &'b U16CStr,
    /// Card number (at most 20 characters).
    #[zero(capacity = 21)]
    pub card_number: &'b U16CStr,
    /// Deal amount.
    pub amount: i32,
    /// Balance before the deal.
    pub balance_before: i32,
    /// Balance after the deal.
    pub balance_after: i32,
}

/// `ServerOperationData` (`+0xE20`, 0x80 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerOperation<'b> {
    /// libam `dealAvailable`, refreshed every update.
    pub deal_available: bool,
    /// libam `canOperate(4)`, refreshed every update.
    pub can_operate_deal: bool,
    /// An operation is running.
    pub busy: bool,
    /// The running operation can be cancelled.
    pub cancellable: bool,
    /// The card must stay on the reader.
    pub held_over: bool,
    /// The last deal ended in error.
    pub error_occurred: bool,
    /// Result of the running / last operation.
    pub result: ServerResult<'b>,
}

/// `ServerDealResultData` (`+0xEA0`, 0x968 bytes): deal history, newest first.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerDealResultData<'b> {
    /// Number of converted items (libam keeps eight, so at most 8).
    pub count: u32,
    /// History slots.
    pub items: [ServerResult<'b>; DEAL_RESULT_CAPACITY],
}

/// `ServerData.eMoney` (`+0x1C68`, 0x1810 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerEMoney<'b> {
    /// libam emoney module enabled flag; written only at clear time.
    pub available: bool,
    /// Executor response (`SD+0x1C6C`).
    pub response: Response,
    /// Sound channel.
    pub sound: ServerSound<'b>,
    /// Pay-completion channel.
    pub pay_completion: ServerPayCompletion,
    /// libam `alive` (three libam flags all set).
    pub alive: bool,
    /// Terminal id (at most 30 characters).
    #[zero(capacity = 31)]
    pub terminal_id: &'b U16CStr,
    /// Terminal serial (at most 20 characters).
    #[zero(capacity = 21)]
    pub terminal_serial: &'b U16CStr,
    /// Number of valid entries in `available_brand_ids` (at most 7).
    pub available_brand_count: u8,
    /// Brands the terminal reports.
    pub available_brand_ids: [EMoneyBrandId; BRAND_COUNT],
    /// Per-brand data indexed by [`EMoneyBrandId`].
    pub brands: [ServerBrand<'b>; BRAND_COUNT],
    /// Operation state.
    pub operation: ServerOperation<'b>,
    /// Deal history.
    pub deal_result: ServerDealResultData<'b>,
    /// The `aime_unit` process-config byte (`SD+0x3470`), written by the config observer.
    pub aime_unit_index: u32,
}

/// `ClientData.eMoney` (`+0xC84`, 0x38 bytes): the executor request (`CD+0xC84`, 0x30 bytes) and the client's
/// two acknowledgement channels. The four bytes behind it (`CD+0xCBC`) are never accessed.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientEMoney<'b> {
    /// Request generation.
    pub generation: u32,
    /// Raw [`EMoneyTask`].
    pub task: u32,
    /// Per-task info union (0x28 bytes), decoded by the [`ClientEMoneyRef`] payload accessors.
    pub info: &'b [u8; 0x28],
    /// `ClientSoundData` (`CD+0xCB4`): last sound generation the client has played, the acknowledgement of
    /// [`ServerSound::generation`].
    pub sound_generation: u32,
    /// `ClientPayCompletionData` (`CD+0xCB8`): last pay-completion generation the client has shown, the
    /// acknowledgement of [`ServerPayCompletion::generation`].
    pub pay_completion_generation: u32,
}

/// Info of [`EMoneyTask::Balance`] (4 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct BalanceInfo {
    /// Brand to query.
    pub brand_id: EMoneyBrandId,
}

/// Info of [`EMoneyTask::AmountPay`] (0x28 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct AmountPayInfo<'b> {
    /// Brand to pay with.
    pub brand_id: EMoneyBrandId,
    /// Item id (libam requires exactly 13 characters).
    #[zero(capacity = 14)]
    pub item_id: &'b U16CStr,
    /// Amount.
    pub amount: i32,
    /// Item count.
    pub item_count: u32,
}

/// Info of [`EMoneyTask::CoinPay`] (12 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct CoinPayInfo {
    /// Brand to pay with.
    pub brand_id: EMoneyBrandId,
    /// Number of coins.
    pub coin: u32,
    /// Player receiving the coins.
    pub player_index: u32,
}

server_block!(emoney, emoney_mut: ServerEMoney, ServerEMoneyRef, ServerEMoneyMut @ 0x1C68);
client_block!(emoney, emoney_mut: ClientEMoney, ClientEMoneyRef, ClientEMoneyMut @ 0xC84);

impl<'w> ClientEMoneyRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<EMoneyTask> {
        EMoneyTask::try_from(self.task()).ok()
    }

    /// Balance payload, `None` when it fails its proof (unknown brand).
    pub fn balance_info(&self) -> Option<BalanceInfoRef<'w>> {
        let info: &'w [u8; 0x28] = self.info();
        BalanceInfo::access_prefix(info).ok().map(|(payload, _)| payload)
    }

    /// AmountPay payload, `None` when it fails its proof (unknown brand, item id not terminated within its
    /// 14 units).
    pub fn amount_pay_info(&self) -> Option<AmountPayInfoRef<'w>> {
        let info: &'w [u8; 0x28] = self.info();
        AmountPayInfo::access_prefix(info).ok().map(|(payload, _)| payload)
    }

    /// CoinPay payload, `None` when it fails its proof (unknown brand).
    pub fn coin_pay_info(&self) -> Option<CoinPayInfoRef<'w>> {
        let info: &'w [u8; 0x28] = self.info();
        CoinPayInfo::access_prefix(info).ok().map(|(payload, _)| payload)
    }
}

const _: () = {
    assert!(ServerEMoney::SCHEMA_SIZE == 0x1810);
    assert!(ClientEMoney::SCHEMA_SIZE == 0x38);
    assert!(BalanceInfo::SCHEMA_SIZE == 4);
    assert!(AmountPayInfo::SCHEMA_SIZE == 0x28);
    assert!(CoinPayInfo::SCHEMA_SIZE == 0x0C);
};

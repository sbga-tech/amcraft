//! AimePay blocks: `ServerData+0x3478` (0x440 bytes) and `ClientData+0xCC0` (0x40 bytes).
//!
//! Evidence: `spec/controllers/credit_emoney_aimepay.md` §4.2, claims `credit_emoney_aimepay.aimepay.sd.*`
//! and `credit_emoney_aimepay.aimepay.cd.layout`.

use num_enum::TryFromPrimitive;
use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

/// `ipc::aimepay::Task` (name table `0x1405EEDA0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum AimePayTask {
    /// Cancel the running operation.
    Cancel = 0,
    /// Answer a pending acceptation question.
    Confirm = 1,
    /// Start the display self test.
    DisplayCheck = 2,
    /// Activate Aime Pay.
    Activate = 3,
    /// Deactivate Aime Pay.
    Deactivate = 4,
    /// Query the user setting of an Aime.
    UserSetting = 5,
    /// Pay with coins.
    CoinPay = 6,
    /// Pay an amount for an item.
    AmountPay = 7,
}

impl AimePayTask {
    /// Name from the task table (`0x1405EEDA0`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cancel => "Cancel",
            Self::Confirm => "Confirm",
            Self::DisplayCheck => "DisplayCheck",
            Self::Activate => "Activate",
            Self::Deactivate => "Deactivate",
            Self::UserSetting => "UserSetting",
            Self::CoinPay => "CoinPay",
            Self::AmountPay => "AmountPay",
        }
    }
}

/// `amdaemon::AimePayConfirm`: the acceptation question libam is waiting for. The names describe the action
/// the `Confirm` task takes (value names are unresolved in the binary).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AimePayConfirm {
    /// No question pending.
    None = 0,
    /// Answered with `confirmAccept()`.
    Accept = 1,
    /// Answered with `confirmWithParam(param)`; the request must carry the 5-character `param`.
    WithParam = 2,
}

/// Number of history slots in [`ServerDealResultData::items`].
pub const DEAL_RESULT_CAPACITY: usize = 5;
/// Number of day slots in [`ServerDealSummaryData::items`].
pub const DEAL_SUMMARY_CAPACITY: usize = 7;

/// `ServerLocationData` (0x7E bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerLocation<'b> {
    /// All libam location fields are non-empty.
    pub valid: bool,
    /// Location name (at most 20 characters).
    #[zero(capacity = 21)]
    pub name: &'b U16CStr,
    /// Company name (at most 40 characters).
    #[zero(capacity = 41)]
    pub company_name: &'b U16CStr,
}

/// `ServerUserSetting` (8 bytes, `ServerOperationData+0x0C`).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerUserSetting {
    /// Aime id.
    pub aime_id: u32,
    /// Credit card payment is available.
    pub credit_card_available: bool,
    /// The Aime hold step can be skipped.
    pub aime_hold_skip: bool,
}

/// `ServerDealResult` (0x70 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerDealResult<'b> {
    /// The slot holds a converted deal.
    pub valid: bool,
    /// The deal was a coin payment.
    pub coin: bool,
    /// `AimePayDealResultStatus` (names unresolved): libam status `0→1, 2→3, 3→2`, otherwise `0`.
    pub status: u32,
    /// Deal time (0 when libam's time is invalid).
    pub time: i64,
    /// `"<code> - <message>"` for failed deals (at most 18 characters), otherwise empty.
    #[zero(capacity = 19)]
    pub error_code: &'b U16CStr,
    /// Packed-BCD access code (zeros when libam's code is not valid BCD).
    pub access_code: &'b [u8; 10],
    /// Item id (at most 13 characters).
    #[zero(capacity = 14)]
    pub item_id: &'b U16CStr,
    /// Item count.
    pub item_count: u32,
    /// Amount, `-1` when libam reports `0xFFFFFFFF`.
    pub amount: i32,
    /// Receipt id.
    pub receipt_id: u64,
}

/// `ServerOperationData` (`+0x108`, 0x88 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerOperation<'b> {
    /// libam `dealAvailable`, refreshed every update.
    pub deal_available: bool,
    /// An operation is running.
    pub busy: bool,
    /// The running operation can be cancelled.
    pub cancellable: bool,
    /// The acceptation question that is pending.
    pub confirm: AimePayConfirm,
    /// The last operation ended in error.
    pub error_occurred: bool,
    /// User setting of the last queried Aime.
    pub user_setting: ServerUserSetting,
    /// Result of the running / last operation.
    pub deal_result: ServerDealResult<'b>,
}

/// `ServerDealResultData` (`+0x190`, 0x238 bytes): deal history, newest first.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerDealResultData<'b> {
    /// Number of converted items (at most 5).
    pub count: u32,
    /// History slots.
    pub items: [ServerDealResult<'b>; DEAL_RESULT_CAPACITY],
}

/// `ServerDealSummary` (0x10 bytes): one day of the deal summary.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerDealSummary {
    /// Day.
    pub date: i64,
    /// Total amount.
    pub amount: u32,
    /// Number of deals.
    pub count: u32,
}

/// `ServerDealSummaryData` (`+0x3C8`, 0x78 bytes): deal summary, newest first.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerDealSummaryData {
    /// Number of converted items (at most 7).
    pub count: u32,
    /// Summary slots.
    pub items: [ServerDealSummary; DEAL_SUMMARY_CAPACITY],
}

/// `ServerData.aimePay` (`+0x3478`, 0x440 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAimePay<'b> {
    /// libam aimepay module enabled flag; written only at clear time.
    pub available: bool,
    /// Executor response (`SD+0x347C`).
    pub response: Response,
    /// Location the service is activated for.
    pub activated_location: ServerLocation<'b>,
    /// Location the cabinet is currently in.
    pub current_location: ServerLocation<'b>,
    /// Operation state.
    pub operation: ServerOperation<'b>,
    /// Deal history.
    pub deal_result: ServerDealResultData<'b>,
    /// Deal summary.
    pub deal_summary: ServerDealSummaryData,
}

/// `ClientData.aimePay` (`+0xCC0`, 0x40 bytes): the executor request is the whole block.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientAimePay<'b> {
    /// Request generation.
    pub generation: u32,
    /// Raw [`AimePayTask`].
    pub task: u32,
    /// Per-task info union (0x38 bytes), decoded by the [`ClientAimePayRef`] payload accessors.
    pub info: &'b [u8; 0x38],
}

/// Info of [`AimePayTask::Confirm`]: the confirmation parameter (5 characters, used when the pending question
/// needs one), terminated within 6 units.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ConfirmInfo<'b> {
    /// Confirmation parameter.
    #[zero(capacity = 6)]
    pub param: &'b U16CStr,
}

/// Info of [`AimePayTask::UserSetting`] (4 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct UserSettingInfo {
    /// Aime id to query.
    pub aime_id: u32,
}

/// Info of [`AimePayTask::CoinPay`] (0x18 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct CoinPayInfo<'b> {
    /// Aime id.
    pub aime_id: u32,
    /// Packed-BCD access code.
    pub access_code: &'b [u8; 10],
    /// Player receiving the coins.
    pub player_index: u32,
    /// Number of coins.
    pub coin: u32,
}

/// Info of [`AimePayTask::AmountPay`] (0x38 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct AmountPayInfo<'b> {
    /// Aime id.
    pub aime_id: u32,
    /// Packed-BCD access code.
    pub access_code: &'b [u8; 10],
    /// Item id.
    #[zero(capacity = 14)]
    pub item_id: &'b U16CStr,
    /// Item count.
    pub item_count: u32,
    /// Option number.
    pub option_number: i64,
}

server_block!(aime_pay, aime_pay_mut: ServerAimePay, ServerAimePayRef, ServerAimePayMut @ 0x3478);
client_block!(aime_pay, aime_pay_mut: ClientAimePay, ClientAimePayRef, ClientAimePayMut @ 0xCC0);

impl<'w> ClientAimePayRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<AimePayTask> {
        AimePayTask::try_from(self.task()).ok()
    }

    /// Confirm payload, `None` when the parameter is not terminated within its 6 units.
    pub fn confirm_info(&self) -> Option<ConfirmInfoRef<'w>> {
        let info: &'w [u8; 0x38] = self.info();
        ConfirmInfo::access_prefix(info).ok().map(|(payload, _)| payload)
    }

    /// UserSetting payload.
    pub fn user_setting_info(&self) -> UserSettingInfoRef<'w> {
        let info: &'w [u8; 0x38] = self.info();
        let (payload, _) = UserSettingInfo::access_prefix(info).expect("an integer payload accepts every bit pattern");
        payload
    }

    /// CoinPay payload.
    pub fn coin_pay_info(&self) -> CoinPayInfoRef<'w> {
        let info: &'w [u8; 0x38] = self.info();
        let (payload, _) =
            CoinPayInfo::access_prefix(info).expect("an integer and byte payload accepts every bit pattern");
        payload
    }

    /// AmountPay payload, `None` when the item id is not terminated within its 14 units.
    pub fn amount_pay_info(&self) -> Option<AmountPayInfoRef<'w>> {
        let info: &'w [u8; 0x38] = self.info();
        AmountPayInfo::access_prefix(info).ok().map(|(payload, _)| payload)
    }
}

const _: () = {
    assert!(ServerAimePay::SCHEMA_SIZE == 0x440);
    assert!(ClientAimePay::SCHEMA_SIZE == 0x40);
    assert!(ConfirmInfo::SCHEMA_SIZE <= 0x38);
    assert!(UserSettingInfo::SCHEMA_SIZE == 4);
    assert!(CoinPayInfo::SCHEMA_SIZE == 0x18);
    assert!(AmountPayInfo::SCHEMA_SIZE == 0x38);
};

//! The Thinca payment SDK boundary (`spec/libam/payment.md` part "thinca"): the SDK as a trait and the callback
//! rings the SDK threads fill.
//!
//! The original calls 21 vtable slots of `ThincaPayment.dll` and receives 17 callbacks. Here the slots are the
//! methods of [`ThincaSdk`] (return `0` = success, `202` = busy, anything else = failure), and the callbacks that
//! libam keeps are the methods of [`Callbacks`]: each only copies its payload into a ring of 4 that drops its
//! oldest element (section 4.2), and the Manager poll drains one element per ring per frame (section 5).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use libam_api::time::Tm;

use crate::Unsupported;

/// `apiVersion` passed to `ThincaPaymentGetInstance`.
pub const API_VERSION: u32 = 0x0104_0B00;
/// Slots of every ring.
pub const RING_CAPACITY: usize = 4;
/// SDK code: busy, try again (`setTerminalSerial`, `setClientCertificate`, `cancelRequest`).
pub const CODE_BUSY: u32 = 202;
/// The return code of an SDK method without an SDK instance (any non-zero value is a failure).
const NO_SDK: u32 = 1;

/// Command kinds the SDK reports in a result (`O+0x28`) and the commands use.
pub mod kind {
    /// `InitAuthTerm`.
    pub const INIT_AUTH_TERM: u32 = 0x0101_0001;
    /// `RemoveTerm`.
    pub const REMOVE_TERM: u32 = 0x0102_0001;
    /// `Payment` (and `CheckBrands`).
    pub const PAYMENT: u32 = 0x0201_0001;
    /// `BalanceInquiry` / `VerifyUnconfirmedDeal`.
    pub const BALANCE: u32 = 0x0205_0001;
}

/// What `ThincaPaymentGetInstance` + the init calls of section 7.2 need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThincaInit {
    /// Resource directory (`setResource`).
    pub resource_dir: String,
    /// Mapped log level (0..=5) and the `icasclient.log` path (`setIcasClientLog`), when the step is not skipped.
    pub icas_log: Option<(u32, String)>,
    /// Mapped log level and the `thincapayment.log` path (slot `+0x28`, rotate 10, `"20M"`).
    pub thinca_log: Option<(u32, String)>,
    /// The Aime reader adapter text (7.3).
    pub adapter: String,
}

/// Why [`ThincaSdk::open`] failed: the name of the failed method and the SDK code (`0` when no instance exists).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{method} failed with code {code}")]
pub struct ThincaError {
    /// libam's log name of the method.
    pub method: &'static str,
    /// The SDK return code.
    pub code: u32,
}

/// One element of the error list of an `errs` callback (`E+0x18`, `E+0x20`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdkError {
    /// Numeric error code.
    pub code: u32,
    /// Message.
    pub message: String,
}

/// Detail record `T` of a result (3.3), already decoded: the SDK adapter reads the type-specific offsets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SdkDetail {
    /// `T+0x10`.
    pub kind: u32,
    /// `T+0x08`.
    pub type_: u32,
    /// `T+0x18` (only consulted for kind 2 / type 3).
    pub status: u32,
    /// `T+0x28`: "balance fields present" (1).
    pub has_balance: u32,
    /// Device time (`None` when the record carries none).
    pub time: Option<Tm>,
    /// Card text.
    pub card: String,
    /// Amount.
    pub amount: i32,
    /// Balance before (kind 1 type 1: `T+0x60`; type 3: `T+0x68`).
    pub balance_before: i32,
    /// Balance after (kind 1 type 1: `T+0x68`; type 5: `T+0x30`).
    pub balance_after: i32,
}

/// Deal sub-object `D` of a result (3.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SdkDeal {
    /// Card number.
    pub card: String,
    /// The value stored at DealInfo `+0x80`.
    pub balance_after: i32,
}

/// Result object `O` of the result callbacks (3.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SdkResult {
    /// Result code (`0` = ok).
    pub code: u32,
    /// Second code: the Thinca brand number of the result (or 0).
    pub brand: u32,
    /// Text (deal number), at most 15 characters.
    pub text: String,
    /// Brand numbers (each `< 32`) of the brand mask.
    pub brands: Vec<u32>,
    /// Deal sub-object.
    pub deal: Option<SdkDeal>,
    /// Detail records.
    pub details: Vec<SdkDetail>,
    /// Kind id of the originating command.
    pub kind: u32,
}

#[derive(Debug, Default)]
struct Rings {
    a0: VecDeque<Option<String>>,
    a1: VecDeque<String>,
    p0: VecDeque<Option<String>>,
    p1: VecDeque<String>,
    buttons: VecDeque<u32>,
    cancellable: VecDeque<bool>,
    hold: VecDeque<bool>,
    sound: VecDeque<Option<String>>,
    deal_id: VecDeque<String>,
    progress: VecDeque<SdkResult>,
    finish: VecDeque<SdkResult>,
}

fn push<T>(ring: &mut VecDeque<T>, value: T) {
    if ring.len() == RING_CAPACITY {
        ring.pop_front();
    }
    ring.push_back(value);
}

/// The line builder of the `errs` callbacks (`0x1402685B0`): the last error as `<message> \<code>`, the code in
/// groups of three digits separated by commas; empty without errors; at most 255 characters.
pub fn error_line(errors: &[SdkError]) -> String {
    let Some(last) = errors.last() else { return String::new() };
    let digits = last.code.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{} \\{}", last.message, grouped).chars().take(255).collect()
}

/// The listener object the SDK calls back into (`ThincapaymentEventListener`). Cloning shares the rings; the
/// callbacks may be called from any thread.
#[derive(Debug, Clone, Default)]
pub struct Callbacks(Arc<Mutex<Rings>>);

impl Callbacks {
    fn rings(&self) -> MutexGuard<'_, Rings> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `+0x30`: activation/deactivation screen line 0.
    pub fn activation_line0(&self, text: Option<&str>) {
        push(&mut self.rings().a0, text.map(str::to_owned));
    }

    /// `+0x38`: activation/deactivation screen line 1.
    pub fn activation_errors(&self, errors: &[SdkError]) {
        push(&mut self.rings().a1, error_line(errors));
    }

    /// `+0x18`: payment screen line 0.
    pub fn payment_line0(&self, text: Option<&str>) {
        push(&mut self.rings().p0, text.map(str::to_owned));
    }

    /// `+0x20`: payment screen line 1.
    pub fn payment_errors(&self, errors: &[SdkError]) {
        push(&mut self.rings().p1, error_line(errors));
    }

    /// `+0x40`: the buttons on offer; the last one counts, `0` when there is none.
    pub fn buttons(&self, ids: &[u32]) {
        push(&mut self.rings().buttons, ids.last().copied().unwrap_or(0));
    }

    /// `+0x58`: the SDK allows (or no longer allows) cancelling.
    pub fn cancellable(&self, value: bool) {
        push(&mut self.rings().cancellable, value);
    }

    /// `+0x60`: the card has to be held over the reader.
    pub fn hold_card(&self, value: bool) {
        push(&mut self.rings().hold, value);
    }

    /// `+0x68`: play a sound file.
    pub fn play_sound(&self, path: Option<&str>) {
        push(&mut self.rings().sound, path.map(str::to_owned));
    }

    /// `+0x78`: the deal id; `false` (nothing queued) for `None`. Longer than 15 characters it is cut.
    pub fn deal_id(&self, id: Option<&str>) -> bool {
        let Some(id) = id else { return false };
        push(&mut self.rings().deal_id, id.chars().take(15).collect());
        true
    }

    /// `+0x80`: progress result.
    pub fn progress(&self, result: &SdkResult) {
        push(&mut self.rings().progress, result.clone());
    }

    /// `+0x88`: final result.
    pub fn finished(&self, result: &SdkResult) {
        push(&mut self.rings().finish, result.clone());
    }

    /// The oldest queued activation line 0, `None` when nothing is queued.
    pub fn pop_activation_line0(&self) -> Option<Option<String>> {
        self.rings().a0.pop_front()
    }
    /// The oldest queued activation line 1.
    pub fn pop_activation_line1(&self) -> Option<String> {
        self.rings().a1.pop_front()
    }
    /// The oldest queued payment line 0.
    pub fn pop_payment_line0(&self) -> Option<Option<String>> {
        self.rings().p0.pop_front()
    }
    /// The oldest queued payment line 1.
    pub fn pop_payment_line1(&self) -> Option<String> {
        self.rings().p1.pop_front()
    }
    /// The oldest queued button id.
    pub fn pop_button(&self) -> Option<u32> {
        self.rings().buttons.pop_front()
    }
    /// The oldest queued cancellable flag.
    pub fn pop_cancellable(&self) -> Option<bool> {
        self.rings().cancellable.pop_front()
    }
    /// The oldest queued hold-card flag.
    pub fn pop_hold(&self) -> Option<bool> {
        self.rings().hold.pop_front()
    }
    /// The oldest queued sound file.
    pub fn pop_sound(&self) -> Option<Option<String>> {
        self.rings().sound.pop_front()
    }
    /// The oldest queued deal id.
    pub fn pop_deal_id(&self) -> Option<String> {
        self.rings().deal_id.pop_front()
    }
    /// The oldest queued progress result.
    pub fn pop_progress(&self) -> Option<SdkResult> {
        self.rings().progress.pop_front()
    }
    /// The oldest queued final result.
    pub fn pop_finished(&self) -> Option<SdkResult> {
        self.rings().finish.pop_front()
    }
}

/// The 21 slots libam calls on the SDK object. `0` = success; see section 2.2 for the codes.
pub trait ThincaSdk {
    /// `GetInstance`, then `initialize`, `setThincaEventInterface(callbacks)`, `setResource`, the optional log
    /// steps, `setIcasClientConfig(adapter, 60000)` and `setGoodsCode("0990")` (7.2); returns the SDK version
    /// (`GetVersion()`, `0x01040B00` = 1.4.11.0).
    fn open(&mut self, init: &ThincaInit, callbacks: &Callbacks) -> Result<u32, ThincaError>;
    /// `dispose` (`+0x18`); a no-op when the SDK was not opened.
    fn dispose(&mut self) -> u32;
    /// `+0x08`: the prelude of every command.
    fn prelude(&mut self) -> u32;
    /// `+0x50 setClientCertificate(p12 path, password)`.
    fn set_client_certificate(&mut self, p12: &str, password: &str) -> u32;
    /// `+0x58 setTerminalSerial`.
    fn set_terminal_serial(&mut self, serial: &str) -> u32;
    /// `+0x98 initAuthTerm(serial, aux)`.
    fn init_auth_term(&mut self, serial: &str, aux: &str) -> u32;
    /// `+0xA0 removeTerm(1, brand)`.
    fn remove_term(&mut self, brand: u32) -> u32;
    /// `+0xA8 checkBrands(0x02010001)`.
    fn check_brands(&mut self) -> u32;
    /// `+0xB0 checkDeal(deal number)`.
    fn check_deal(&mut self, deal_number: &str) -> u32;
    /// `+0xD8 payment(slip, amount, 0, brand)`.
    fn payment(&mut self, slip: u32, amount: u32, brand: u32) -> u32;
    /// `+0x178 balanceInquiry(slip, brand)`.
    fn balance_inquiry(&mut self, slip: u32, brand: u32) -> u32;
    /// `+0x200 cancelRequest`.
    fn cancel_request(&mut self) -> u32;
    /// `+0x208 selectButton`.
    fn select_button(&mut self, id: u32) -> u32;
    /// `+0x220`: display name of a Thinca brand.
    fn brand_name(&mut self, brand: u32) -> Option<String>;
    /// `+0x228`: icon path of a Thinca brand.
    fn brand_icon(&mut self, brand: u32) -> Option<String>;
}

/// No `ThincaPayment.dll`: like the Windows backend when the DLL is missing.
impl ThincaSdk for Unsupported {
    fn open(&mut self, _init: &ThincaInit, _callbacks: &Callbacks) -> Result<u32, ThincaError> {
        Err(ThincaError { method: "ThincaPaymentGetInstance", code: 0 })
    }

    fn dispose(&mut self) -> u32 {
        0
    }

    fn prelude(&mut self) -> u32 {
        NO_SDK
    }

    fn set_client_certificate(&mut self, _p12: &str, _password: &str) -> u32 {
        NO_SDK
    }

    fn set_terminal_serial(&mut self, _serial: &str) -> u32 {
        NO_SDK
    }

    fn init_auth_term(&mut self, _serial: &str, _aux: &str) -> u32 {
        NO_SDK
    }

    fn remove_term(&mut self, _brand: u32) -> u32 {
        NO_SDK
    }

    fn check_brands(&mut self) -> u32 {
        NO_SDK
    }

    fn check_deal(&mut self, _deal_number: &str) -> u32 {
        NO_SDK
    }

    fn payment(&mut self, _slip: u32, _amount: u32, _brand: u32) -> u32 {
        NO_SDK
    }

    fn balance_inquiry(&mut self, _slip: u32, _brand: u32) -> u32 {
        NO_SDK
    }

    fn cancel_request(&mut self) -> u32 {
        NO_SDK
    }

    fn select_button(&mut self, _id: u32) -> u32 {
        NO_SDK
    }

    fn brand_name(&mut self, _brand: u32) -> Option<String> {
        None
    }

    fn brand_icon(&mut self, _brand: u32) -> Option<String> {
        None
    }
}

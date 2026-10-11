//! The simulated Thinca payment SDK: a scriptable `ThincaPayment.dll` that logs the calls it receives and fires
//! the scripted callbacks.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::ThincaSdk;
use crate::thinca::{API_VERSION, Callbacks, SdkError, SdkResult, ThincaError, ThincaInit};

/// A call the simulated SDK received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdkCall {
    /// `open` with the init data.
    Open(ThincaInit),
    /// `dispose`.
    Dispose,
    /// `+0x08`.
    Prelude,
    /// `setClientCertificate(p12, password)`.
    ClientCertificate(String, String),
    /// `setTerminalSerial`.
    TerminalSerial(String),
    /// `initAuthTerm(serial, aux)`.
    InitAuthTerm(String, String),
    /// `removeTerm(brand)`.
    RemoveTerm(u32),
    /// `checkBrands`.
    CheckBrands,
    /// `checkDeal(number)`.
    CheckDeal(String),
    /// `payment(slip, amount, brand)`.
    Payment(u32, u32, u32),
    /// `balanceInquiry(slip, brand)`.
    BalanceInquiry(u32, u32),
    /// `cancelRequest`.
    CancelRequest,
    /// `selectButton(id)`.
    SelectButton(u32),
}

/// What the SDK does in answer to a request: callbacks fired right after the call returns `0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    /// `+0x18` payment line 0.
    PaymentLine0(Option<String>),
    /// `+0x20` payment line 1.
    PaymentErrors(Vec<SdkError>),
    /// `+0x30` activation line 0.
    ActivationLine0(Option<String>),
    /// `+0x40` buttons.
    Buttons(Vec<u32>),
    /// `+0x58` cancellable.
    Cancellable(bool),
    /// `+0x60` hold card.
    HoldCard(bool),
    /// `+0x68` sound.
    Sound(Option<String>),
    /// `+0x78` deal id.
    DealId(String),
    /// `+0x80` progress result.
    Progress(SdkResult),
    /// `+0x88` final result.
    Finished(SdkResult),
}

#[derive(Debug, Default)]
struct SimState {
    calls: Vec<SdkCall>,
    /// Return codes for the next calls of one method, keyed by [`Method`].
    returns: Vec<(Method, VecDeque<u32>)>,
    /// Callback bursts, one per accepted request in order.
    bursts: VecDeque<Vec<Callback>>,
    callbacks: Option<Callbacks>,
    open_error: Option<ThincaError>,
    version: u32,
    brands: Vec<(u32, String, String)>,
}

/// The methods whose return code can be scripted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `setClientCertificate`.
    ClientCertificate,
    /// `setTerminalSerial`.
    TerminalSerial,
    /// `initAuthTerm`.
    InitAuthTerm,
    /// `removeTerm`.
    RemoveTerm,
    /// `checkBrands`.
    CheckBrands,
    /// `checkDeal`.
    CheckDeal,
    /// `payment`.
    Payment,
    /// `balanceInquiry`.
    BalanceInquiry,
    /// `cancelRequest`.
    CancelRequest,
}

/// Scriptable simulated SDK. Clones share the script and the call log.
#[derive(Debug, Clone)]
pub struct SimThinca {
    state: Rc<RefCell<SimState>>,
}

impl Default for SimThinca {
    fn default() -> Self {
        Self::new()
    }
}

impl SimThinca {
    /// An SDK of version 1.4.11.0 that knows no brands and accepts everything.
    pub fn new() -> Self {
        Self { state: Rc::new(RefCell::new(SimState { version: API_VERSION, ..SimState::default() })) }
    }

    /// Adds a brand the SDK reports (`+0x220` / `+0x228`); `checkBrands` answers with the brand list.
    pub fn add_brand(&self, thinca_brand: u32, name: &str, icon: &str) {
        self.state.borrow_mut().brands.push((thinca_brand, name.to_owned(), icon.to_owned()));
    }

    /// Makes `open` fail.
    pub fn fail_open(&self, method: &'static str, code: u32) {
        self.state.borrow_mut().open_error = Some(ThincaError { method, code });
    }

    /// Queues return codes for the next calls of `method`; with none queued the call returns 0.
    pub fn script_return(&self, method: Method, codes: &[u32]) {
        let mut state = self.state.borrow_mut();
        match state.returns.iter_mut().find(|(known, _)| *known == method) {
            Some((_, queue)) => queue.extend(codes),
            None => state.returns.push((method, codes.iter().copied().collect())),
        }
    }

    /// Queues the callbacks the SDK fires after the next accepted request that has none queued before it.
    pub fn script_burst(&self, callbacks: Vec<Callback>) {
        self.state.borrow_mut().bursts.push_back(callbacks);
    }

    /// Fires callbacks as if the SDK thread did, outside any request.
    pub fn fire(&self, callbacks: &[Callback]) {
        let target = self.state.borrow().callbacks.clone();
        if let Some(target) = target {
            Self::deliver(&target, callbacks);
        }
    }

    /// Every call made so far.
    pub fn calls(&self) -> Vec<SdkCall> {
        self.state.borrow().calls.clone()
    }

    fn deliver(target: &Callbacks, callbacks: &[Callback]) {
        for callback in callbacks {
            match callback {
                Callback::PaymentLine0(text) => target.payment_line0(text.as_deref()),
                Callback::PaymentErrors(errors) => target.payment_errors(errors),
                Callback::ActivationLine0(text) => target.activation_line0(text.as_deref()),
                Callback::Buttons(ids) => target.buttons(ids),
                Callback::Cancellable(value) => target.cancellable(*value),
                Callback::HoldCard(value) => target.hold_card(*value),
                Callback::Sound(path) => target.play_sound(path.as_deref()),
                Callback::DealId(id) => {
                    target.deal_id(Some(id));
                }
                Callback::Progress(result) => target.progress(result),
                Callback::Finished(result) => target.finished(result),
            }
        }
    }

    fn code(&self, method: Method) -> u32 {
        let mut state = self.state.borrow_mut();
        state
            .returns
            .iter_mut()
            .find(|(known, _)| *known == method)
            .and_then(|(_, queue)| queue.pop_front())
            .unwrap_or(0)
    }

    /// A request slot: log, return the scripted code, and on success fire the next burst.
    fn request(&self, call: SdkCall, method: Method) -> u32 {
        self.state.borrow_mut().calls.push(call);
        let code = self.code(method);
        if code == 0 {
            let (burst, target) = {
                let mut state = self.state.borrow_mut();
                (state.bursts.pop_front(), state.callbacks.clone())
            };
            if let (Some(burst), Some(target)) = (burst, target) {
                Self::deliver(&target, &burst);
            }
        }
        code
    }

    fn plain(&self, call: SdkCall, method: Method) -> u32 {
        self.state.borrow_mut().calls.push(call);
        self.code(method)
    }
}

impl ThincaSdk for SimThinca {
    fn open(&mut self, init: &ThincaInit, callbacks: &Callbacks) -> Result<u32, ThincaError> {
        let mut state = self.state.borrow_mut();
        state.calls.push(SdkCall::Open(init.clone()));
        if let Some(error) = state.open_error.clone() {
            return Err(error);
        }
        state.callbacks = Some(callbacks.clone());
        Ok(state.version)
    }

    fn dispose(&mut self) -> u32 {
        let mut state = self.state.borrow_mut();
        state.calls.push(SdkCall::Dispose);
        state.callbacks = None;
        0
    }

    fn prelude(&mut self) -> u32 {
        self.state.borrow_mut().calls.push(SdkCall::Prelude);
        0
    }

    fn set_client_certificate(&mut self, p12: &str, password: &str) -> u32 {
        self.plain(SdkCall::ClientCertificate(p12.to_owned(), password.to_owned()), Method::ClientCertificate)
    }

    fn set_terminal_serial(&mut self, serial: &str) -> u32 {
        self.plain(SdkCall::TerminalSerial(serial.to_owned()), Method::TerminalSerial)
    }

    fn init_auth_term(&mut self, serial: &str, aux: &str) -> u32 {
        self.request(SdkCall::InitAuthTerm(serial.to_owned(), aux.to_owned()), Method::InitAuthTerm)
    }

    fn remove_term(&mut self, brand: u32) -> u32 {
        self.request(SdkCall::RemoveTerm(brand), Method::RemoveTerm)
    }

    fn check_brands(&mut self) -> u32 {
        self.request(SdkCall::CheckBrands, Method::CheckBrands)
    }

    fn check_deal(&mut self, deal_number: &str) -> u32 {
        self.request(SdkCall::CheckDeal(deal_number.to_owned()), Method::CheckDeal)
    }

    fn payment(&mut self, slip: u32, amount: u32, brand: u32) -> u32 {
        self.request(SdkCall::Payment(slip, amount, brand), Method::Payment)
    }

    fn balance_inquiry(&mut self, slip: u32, brand: u32) -> u32 {
        self.request(SdkCall::BalanceInquiry(slip, brand), Method::BalanceInquiry)
    }

    fn cancel_request(&mut self) -> u32 {
        self.plain(SdkCall::CancelRequest, Method::CancelRequest)
    }

    fn select_button(&mut self, id: u32) -> u32 {
        self.state.borrow_mut().calls.push(SdkCall::SelectButton(id));
        0
    }

    fn brand_name(&mut self, brand: u32) -> Option<String> {
        self.state.borrow().brands.iter().find(|entry| entry.0 == brand).map(|entry| entry.1.clone())
    }

    fn brand_icon(&mut self, brand: u32) -> Option<String> {
        self.state.borrow().brands.iter().find(|entry| entry.0 == brand).map(|entry| entry.2.clone())
    }
}

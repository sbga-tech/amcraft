//! `ThincaPayment.dll` bound at run time (`spec/libam/payment.md` part "thinca", sections 1 to 4 and 7.2).
//!
//! The DLL is a C++ library: `ThincaPaymentGetInstance(0x01040B00)` returns the process-wide SDK object, whose
//! virtual table libam calls through 21 slots ([`WindowsThinca`] implements [`ThincaSdk`] over them), and libam hands
//! the SDK a listener object with an 18-entry virtual table (the destructor and 17 callbacks, section 4.1). The
//! SDK threads call the listener; every callback copies its payload into the [`Callbacks`] rings, exactly as the
//! original does (section 4.2), and returns.
//!
//! This module holds all the `unsafe` code of the SDK binding: raw vtable calls, getters on the SDK's result
//! objects, and the listener object whose layout the SDK expects (a vtable pointer first).

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::{from_ref, null_mut};

use libam_api::time::Tm;
use libloading::Library;

use crate::ThincaSdk;
use crate::thinca::{API_VERSION, Callbacks, SdkDeal, SdkDetail, SdkError, SdkResult, ThincaError, ThincaInit, kind};

/// Return code of a slot libam cannot call (no DLL, no instance): any non-zero value is a failure.
const NO_SDK: u32 = 1;
/// `setIcasClientConfig` timeout (7.2 step 7).
const ICAS_TIMEOUT_MS: u32 = 60_000;
/// Thinca log rotation count and size (7.2 step 6).
const THINCA_LOG_ROTATE: u32 = 10;
const THINCA_LOG_SIZE: &CStr = c"20M";
/// `setGoodsCode` (7.2 step 8).
const GOODS_CODE: &str = "0990";

type Object = *mut c_void;

/// The vtable slot at byte `offset` of the C++ object `object`.
///
/// # Safety
///
/// `object` must point to a live C++ object whose first word is a vtable pointer with a function of type `F`
/// (`extern "C"`, `this` first) at `offset`.
unsafe fn slot<F: Copy>(object: Object, offset: usize) -> F {
    const { assert!(size_of::<F>() == size_of::<usize>()) };
    // SAFETY: the caller guarantees the object layout; the entry is read as a pointer-sized value and
    // reinterpreted as the function type, which has the same size (asserted above).
    unsafe {
        let vtable = object.cast::<*const usize>().read();
        let entry = vtable.add(offset / size_of::<usize>()).read();
        std::mem::transmute_copy::<usize, F>(&entry)
    }
}

/// A getter slot: `fn(this) -> R`.
///
/// # Safety
///
/// As [`slot`], with a getter taking only `this` at `offset`.
unsafe fn getter<R>(object: Object, offset: usize) -> R {
    // SAFETY: forwarded to the caller.
    unsafe { slot::<unsafe extern "C" fn(Object) -> R>(object, offset)(object) }
}

/// A getter slot with a count out-parameter: `fn(this, int* count) -> *const E`; the elements as a slice.
///
/// # Safety
///
/// As [`getter`]; the returned array must hold `count` valid elements until the end of the callback.
unsafe fn array<'a, E>(object: Object, offset: usize) -> &'a [E] {
    let mut count = 0i32;
    // SAFETY: the SDK returns an array of `count` elements (3.1, 3.3) that stays valid during the callback.
    unsafe {
        let items = slot::<unsafe extern "C" fn(Object, *mut i32) -> *const E>(object, offset)(object, &mut count);
        match usize::try_from(count) {
            Ok(len) if len > 0 && !items.is_null() => std::slice::from_raw_parts(items, len),
            _ => &[],
        }
    }
}

/// UTF-8 text of an SDK `const char*`; `None` for NULL.
///
/// # Safety
///
/// `text` is NULL or a NUL-terminated string valid for the call.
unsafe fn text(text: *const c_char) -> Option<String> {
    // SAFETY: forwarded to the caller.
    (!text.is_null()).then(|| unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned())
}

/// The C string handed to the SDK; a text with an interior NUL arrives empty (the original's conversion fails
/// and the call site passes the zero-filled buffer, 1.2).
fn c_text(value: &str) -> CString {
    CString::new(value).unwrap_or_default()
}

fn check(method: &'static str, code: u32) -> Result<(), ThincaError> {
    if code == 0 { Ok(()) } else { Err(ThincaError { method, code }) }
}

/// A broken-down time as the SDK holds it: nine `i32` in the order of `struct tm`.
///
/// # Safety
///
/// `time` is NULL or points to nine readable `i32`.
unsafe fn read_tm(time: *const i32) -> Option<Tm> {
    if time.is_null() {
        return None;
    }
    // SAFETY: forwarded to the caller; the read is unaligned-safe.
    let raw = unsafe { time.cast::<[i32; 9]>().read_unaligned() };
    Some(Tm {
        sec: raw[0],
        min: raw[1],
        hour: raw[2],
        mday: raw[3],
        mon: raw[4],
        year: raw[5],
        wday: raw[6],
        yday: raw[7],
        isdst: raw[8],
    })
}

/// The detail record `T` of a result (3.3): the getters of its kind and type.
///
/// # Safety
///
/// `detail` is a live detail object of the SDK.
unsafe fn read_detail(detail: Object) -> SdkDetail {
    // SAFETY: the offsets are those of the spec's table 3.3; each getter takes only `this`.
    unsafe {
        let kind = getter::<u32>(detail, 0x10);
        let type_ = getter::<u32>(detail, 0x08);
        let mut read = SdkDetail { kind, type_, ..SdkDetail::default() };
        match (kind, type_) {
            (1, 1) => {
                read.time = read_tm(getter::<*const i32>(detail, 0x40));
                read.card = text(getter::<*const c_char>(detail, 0x30)).unwrap_or_default();
                read.amount = getter::<i32>(detail, 0x38);
                read.has_balance = getter::<u32>(detail, 0x28);
                if read.has_balance == 1 {
                    read.balance_before = getter::<i32>(detail, 0x60);
                    read.balance_after = getter::<i32>(detail, 0x68);
                }
            }
            (1 | 2, 3) => {
                read.status = getter::<u32>(detail, 0x18);
                read.time = read_tm(getter::<*const i32>(detail, 0x48));
                read.card = text(getter::<*const c_char>(detail, 0x38)).unwrap_or_default();
                read.amount = getter::<i32>(detail, 0x40);
                read.has_balance = getter::<u32>(detail, 0x28);
                if read.has_balance == 1 {
                    read.balance_before = getter::<i32>(detail, 0x68);
                }
            }
            (1, 5) => {
                read.time = read_tm(getter::<*const i32>(detail, 0x38));
                read.card = text(getter::<*const c_char>(detail, 0x28)).unwrap_or_default();
                read.balance_after = getter::<i32>(detail, 0x30);
            }
            _ => {}
        }
        read
    }
}

/// The result object `O` of the result callbacks (3.1), read into an [`SdkResult`].
///
/// # Safety
///
/// `result` is a live result object of the SDK.
unsafe fn read_result(result: Object) -> SdkResult {
    // SAFETY: the offsets are those of the spec's tables 3.1 and 3.2; the arrays and objects are valid during the
    // callback that received `result`.
    unsafe {
        let deal = getter::<Object>(result, 0x48);
        SdkResult {
            code: getter::<u32>(result, 0x10),
            brand: getter::<u32>(result, 0x30),
            text: text(getter::<*const c_char>(result, 0x78)).unwrap_or_default(),
            brands: array::<u32>(result, 0x38).to_vec(),
            deal: (!deal.is_null()).then(|| SdkDeal {
                card: text(getter::<*const c_char>(deal, 0x00)).unwrap_or_default(),
                balance_after: getter::<i32>(deal, 0x08),
            }),
            details: array::<Object>(result, 0x80)
                .iter()
                .filter(|detail| !detail.is_null())
                .map(|&detail| read_detail(detail))
                .collect(),
            kind: getter::<u32>(result, 0x28),
        }
    }
}

/// The last element of an error list (`errs[count - 1]`, 3.4); the line builder uses nothing else.
///
/// # Safety
///
/// `errors` is NULL or an array of `count` error objects of the SDK.
unsafe fn last_error(errors: *const Object, count: u32) -> Vec<SdkError> {
    let Some(last) = count.checked_sub(1).filter(|_| !errors.is_null()) else { return Vec::new() };
    // SAFETY: forwarded to the caller; `E+0x18` is the code and `E+0x20` the message.
    unsafe {
        let error = errors.add(last as usize).read();
        if error.is_null() {
            return Vec::new();
        }
        vec![SdkError {
            code: getter::<u32>(error, 0x18),
            message: text(getter::<*const c_char>(error, 0x20)).unwrap_or_default(),
        }]
    }
}

/// The listener object (`ThincapaymentEventListener`): a vtable pointer first, as the SDK's C++ interface needs.
#[repr(C)]
struct Listener {
    vtable: &'static ListenerVtable,
    callbacks: Callbacks,
}

/// The listener's virtual table: the deleting destructor and the 17 callback slots (4.1, 4.3), `8 * k` bytes in.
#[repr(C)]
struct ListenerVtable {
    destructor: unsafe extern "C" fn(*const Listener, u32) -> *mut c_void,
    slot_08: unsafe extern "C" fn(*const Listener),
    slot_10: unsafe extern "C" fn(*const Listener, u8),
    payment_line0: unsafe extern "C" fn(*const Listener, *const c_char),
    payment_errors: unsafe extern "C" fn(*const Listener, *const Object, u32),
    slot_28: unsafe extern "C" fn(*const Listener, u8),
    activation_line0: unsafe extern "C" fn(*const Listener, *const c_char),
    activation_errors: unsafe extern "C" fn(*const Listener, *const Object, u32),
    buttons: unsafe extern "C" fn(*const Listener, *const Object, i32),
    slot_48: unsafe extern "C" fn(*const Listener),
    slot_50: unsafe extern "C" fn(*const Listener),
    cancellable: unsafe extern "C" fn(*const Listener, u8),
    hold_card: unsafe extern "C" fn(*const Listener, u8),
    play_sound: unsafe extern "C" fn(*const Listener, *const c_char),
    slot_70: unsafe extern "C" fn(*const Listener),
    deal_id: unsafe extern "C" fn(*const Listener, *const c_char) -> u8,
    progress: unsafe extern "C" fn(*const Listener, Object),
    finished: unsafe extern "C" fn(*const Listener, Object),
}

static LISTENER_VTABLE: ListenerVtable = ListenerVtable {
    destructor: listener_destructor,
    slot_08: listener_noop,
    slot_10: listener_flag,
    payment_line0: listener_payment_line0,
    payment_errors: listener_payment_errors,
    slot_28: listener_flag,
    activation_line0: listener_activation_line0,
    activation_errors: listener_activation_errors,
    buttons: listener_buttons,
    slot_48: listener_noop,
    slot_50: listener_noop,
    cancellable: listener_cancellable,
    hold_card: listener_hold_card,
    play_sound: listener_play_sound,
    slot_70: listener_noop,
    deal_id: listener_deal_id,
    progress: listener_progress,
    finished: listener_finished,
};

/// The deleting destructor: the listener is owned by [`WindowsThinca`], so the SDK never frees it.
unsafe extern "C" fn listener_destructor(_: *const Listener, _: u32) -> *mut c_void {
    null_mut()
}

/// The no-op slots `+0x08`, `+0x48`, `+0x50` and `+0x70`.
unsafe extern "C" fn listener_noop(_: *const Listener) {}

/// The flag slots `+0x10` and `+0x28`: written by the original but never read.
unsafe extern "C" fn listener_flag(_: *const Listener, _: u8) {}

unsafe extern "C" fn listener_payment_line0(this: *const Listener, line: *const c_char) {
    // SAFETY: `this` is the listener libam registered; `line` is NULL or a C string valid for the call.
    unsafe { (*this).callbacks.payment_line0(text(line).as_deref()) }
}

unsafe extern "C" fn listener_payment_errors(this: *const Listener, errors: *const Object, count: u32) {
    // SAFETY: as above; `errors` holds `count` error objects.
    unsafe { (*this).callbacks.payment_errors(&last_error(errors, count)) }
}

unsafe extern "C" fn listener_activation_line0(this: *const Listener, line: *const c_char) {
    // SAFETY: `this` is the listener libam registered; `line` is NULL or a C string valid for the call.
    unsafe { (*this).callbacks.activation_line0(text(line).as_deref()) }
}

unsafe extern "C" fn listener_activation_errors(this: *const Listener, errors: *const Object, count: u32) {
    // SAFETY: as above; `errors` holds `count` error objects.
    unsafe { (*this).callbacks.activation_errors(&last_error(errors, count)) }
}

unsafe extern "C" fn listener_buttons(this: *const Listener, buttons: *const Object, count: i32) {
    let count = usize::try_from(count).unwrap_or(0);
    let ids: Vec<u32> = if buttons.is_null() {
        Vec::new()
    } else {
        // SAFETY: the SDK passes `count` button objects, each with its id at `B+0x10` (3.5).
        (0..count)
            .map(|index| unsafe { buttons.add(index).read() })
            .filter(|button| !button.is_null())
            .map(|button| unsafe { getter::<i32>(button, 0x10) }.cast_unsigned())
            .collect()
    };
    // SAFETY: `this` is the listener libam registered.
    unsafe { (*this).callbacks.buttons(&ids) }
}

unsafe extern "C" fn listener_cancellable(this: *const Listener, value: u8) {
    // SAFETY: `this` is the listener libam registered.
    unsafe { (*this).callbacks.cancellable(value != 0) }
}

unsafe extern "C" fn listener_hold_card(this: *const Listener, value: u8) {
    // SAFETY: `this` is the listener libam registered.
    unsafe { (*this).callbacks.hold_card(value != 0) }
}

unsafe extern "C" fn listener_play_sound(this: *const Listener, path: *const c_char) {
    // SAFETY: `this` is the listener libam registered; `path` is NULL or a C string valid for the call.
    unsafe { (*this).callbacks.play_sound(text(path).as_deref()) }
}

unsafe extern "C" fn listener_deal_id(this: *const Listener, id: *const c_char) -> u8 {
    // SAFETY: `this` is the listener libam registered; `id` is NULL or a C string valid for the call.
    u8::from(unsafe { (*this).callbacks.deal_id(text(id).as_deref()) })
}

unsafe extern "C" fn listener_progress(this: *const Listener, result: Object) {
    if result.is_null() {
        return;
    }
    // SAFETY: `this` is the listener libam registered; `result` is a live result object during the call.
    unsafe { (*this).callbacks.progress(&read_result(result)) }
}

unsafe extern "C" fn listener_finished(this: *const Listener, result: Object) {
    if result.is_null() {
        return;
    }
    // SAFETY: as above.
    unsafe { (*this).callbacks.finished(&read_result(result)) }
}

/// `ThincaPayment.dll` loaded at run time. Without the DLL (or its two exports) every call fails, like a library
/// whose `ThincaPaymentGetInstance` returns NULL.
#[derive(Debug)]
pub struct WindowsThinca {
    /// Declared first: dropped first, so the listener outlives the SDK threads' last callback.
    library: Option<Library>,
    listener: Option<Box<Listener>>,
    opened: bool,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener").finish_non_exhaustive()
    }
}

impl WindowsThinca {
    /// Loads `ThincaPayment.dll` from the loader's search path (the original links it at process start).
    pub fn load() -> Self {
        // SAFETY: loading the vendor's SDK runs its initialisers, exactly as the original's import does.
        let library = unsafe { Library::new("ThincaPayment.dll") }.ok();
        Self { library, listener: None, opened: false }
    }

    /// `ThincaPaymentGetInstance(0x01040B00)`: the process-wide SDK object; NULL when the DLL or the export is
    /// missing. libam calls it again at every use (1.1).
    fn instance(&self) -> Object {
        let Some(library) = &self.library else { return null_mut() };
        // SAFETY: the export is `ThincaPayment* GetInstance(u32 apiVersion)` (1.1).
        unsafe {
            library
                .get::<unsafe extern "C" fn(u32) -> Object>(b"ThincaPaymentGetInstance\0")
                .map_or(null_mut(), |get| get(API_VERSION))
        }
    }

    /// `ThincaPaymentGetVersion()`.
    fn version(&self) -> u32 {
        let Some(library) = &self.library else { return 0 };
        // SAFETY: the export is `u32 GetVersion()` (1.1).
        unsafe { library.get::<unsafe extern "C" fn() -> u32>(b"ThincaPaymentGetVersion\0").map_or(0, |get| get()) }
    }

    /// Runs a slot call on the instance; [`NO_SDK`] without one.
    fn call(&self, slot_call: impl FnOnce(Object) -> u32) -> u32 {
        let object = self.instance();
        if object.is_null() { NO_SDK } else { slot_call(object) }
    }

    /// A slot taking one `u32`.
    fn call_u32(&self, offset: usize, value: u32) -> u32 {
        // SAFETY: the offset names a `fn(this, u32) -> u32` slot of the SDK object (2.1).
        self.call(|object| unsafe { slot::<unsafe extern "C" fn(Object, u32) -> u32>(object, offset)(object, value) })
    }

    /// A slot taking one C string.
    fn call_text(&self, offset: usize, value: &str) -> u32 {
        let value = c_text(value);
        // SAFETY: the offset names a `fn(this, const char*) -> u32` slot; the string lives across the call.
        self.call(|object| unsafe {
            slot::<unsafe extern "C" fn(Object, *const c_char) -> u32>(object, offset)(object, value.as_ptr())
        })
    }

    /// A slot taking two C strings.
    fn call_text_text(&self, offset: usize, first: &str, second: &str) -> u32 {
        let (first, second) = (c_text(first), c_text(second));
        // SAFETY: the offset names a `fn(this, const char*, const char*) -> u32` slot; the strings live across
        // the call.
        self.call(|object| unsafe {
            slot::<unsafe extern "C" fn(Object, *const c_char, *const c_char) -> u32>(object, offset)(
                object,
                first.as_ptr(),
                second.as_ptr(),
            )
        })
    }

    /// A slot taking nothing.
    fn call_none(&self, offset: usize) -> u32 {
        // SAFETY: the offset names a `fn(this) -> u32` slot of the SDK object (2.1).
        self.call(|object| unsafe { slot::<unsafe extern "C" fn(Object) -> u32>(object, offset)(object) })
    }

    /// A slot taking a brand number and answering a C string (`+0x220`, `+0x228`).
    fn brand_text(&self, offset: usize, brand: u32) -> Option<String> {
        let object = self.instance();
        if object.is_null() {
            return None;
        }
        // SAFETY: the offset names a `fn(this, u32) -> const char*` slot; the string is read before returning.
        unsafe { text(slot::<unsafe extern "C" fn(Object, u32) -> *const c_char>(object, offset)(object, brand)) }
    }
}

impl ThincaSdk for WindowsThinca {
    fn open(&mut self, init: &ThincaInit, callbacks: &Callbacks) -> Result<u32, ThincaError> {
        let object = self.instance();
        if object.is_null() {
            return Err(ThincaError { method: "ThincaPaymentGetInstance", code: 0 });
        }
        check("initialize", self.call_u32(0x10, 0))?;
        // From here the follow-up shutdown disposes the SDK (7.2).
        self.opened = true;
        let listener =
            self.listener.insert(Box::new(Listener { vtable: &LISTENER_VTABLE, callbacks: callbacks.clone() }));
        let listener = from_ref::<Listener>(listener).cast::<c_void>().cast_mut();
        // SAFETY: slot `+0x70` is `setThincaEventInterface(ThincaEventInterface*)`; the listener is boxed in
        // `self` and stays at this address until `self` is dropped.
        let code = self.call(|object| unsafe {
            slot::<unsafe extern "C" fn(Object, Object) -> u32>(object, 0x70)(object, listener)
        });
        check("setThincaEventInterface", code)?;
        check("setResource", self.call_text(0x20, &init.resource_dir))?;
        if let Some((level, path)) = &init.icas_log {
            let path = c_text(path);
            // SAFETY: slot `+0x30` is `setIcasClientLog(u32 level, const char* path)`.
            let code = self.call(|object| unsafe {
                slot::<unsafe extern "C" fn(Object, u32, *const c_char) -> u32>(object, 0x30)(
                    object,
                    *level,
                    path.as_ptr(),
                )
            });
            check("setIcasClientLog", code)?;
        }
        if let Some((level, path)) = &init.thinca_log {
            let path = c_text(path);
            // SAFETY: slot `+0x28` is the Thinca log set-up `(u32 level, const char* path, u32 rotate, const char*
            // maxSize)`; the strings live across the call.
            let code = self.call(|object| unsafe {
                slot::<unsafe extern "C" fn(Object, u32, *const c_char, u32, *const c_char) -> u32>(object, 0x28)(
                    object,
                    *level,
                    path.as_ptr(),
                    THINCA_LOG_ROTATE,
                    THINCA_LOG_SIZE.as_ptr(),
                )
            });
            check("setIcasClientLog", code)?;
        }
        let adapter = c_text(&init.adapter);
        // SAFETY: slot `+0x38` is `setIcasClientConfig(const char* connect, u32 timeoutMs)`.
        let code = self.call(|object| unsafe {
            slot::<unsafe extern "C" fn(Object, *const c_char, u32) -> u32>(object, 0x38)(
                object,
                adapter.as_ptr(),
                ICAS_TIMEOUT_MS,
            )
        });
        check("setIcasClientConfig", code)?;
        check("setGoodsCode", self.call_text(0x60, GOODS_CODE))?;
        Ok(self.version())
    }

    fn dispose(&mut self) -> u32 {
        if !self.opened {
            return 0;
        }
        self.opened = false;
        let object = self.instance();
        if object.is_null() {
            return 0;
        }
        // SAFETY: slot `+0x18` is `dispose()`.
        unsafe { slot::<unsafe extern "C" fn(Object) -> u32>(object, 0x18)(object) }
    }

    fn prelude(&mut self) -> u32 {
        self.call_none(0x08)
    }

    fn set_client_certificate(&mut self, p12: &str, password: &str) -> u32 {
        self.call_text_text(0x50, p12, password)
    }

    fn set_terminal_serial(&mut self, serial: &str) -> u32 {
        self.call_text(0x58, serial)
    }

    fn init_auth_term(&mut self, serial: &str, aux: &str) -> u32 {
        self.call_text_text(0x98, serial, aux)
    }

    fn remove_term(&mut self, brand: u32) -> u32 {
        let object = self.instance();
        if object.is_null() {
            return NO_SDK;
        }
        // SAFETY: slot `+0xA0` is `removeTerm(u32 kind, u32 brandNumber)`; the kind is the literal 1.
        unsafe { slot::<unsafe extern "C" fn(Object, u32, u32) -> u32>(object, 0xA0)(object, 1, brand) }
    }

    fn check_brands(&mut self) -> u32 {
        // The payment operation id `0x02010001`.
        self.call_u32(0xA8, kind::PAYMENT)
    }

    fn check_deal(&mut self, deal_number: &str) -> u32 {
        self.call_text(0xB0, deal_number)
    }

    fn payment(&mut self, slip: u32, amount: u32, brand: u32) -> u32 {
        let object = self.instance();
        if object.is_null() {
            return NO_SDK;
        }
        // SAFETY: slot `+0xD8` is `payment(u32 slipNo, u32 amount, u32 reserved, u32 brandNumber)`.
        unsafe {
            slot::<unsafe extern "C" fn(Object, u32, u32, u32, u32) -> u32>(object, 0xD8)(
                object, slip, amount, 0, brand,
            )
        }
    }

    fn balance_inquiry(&mut self, slip: u32, brand: u32) -> u32 {
        let object = self.instance();
        if object.is_null() {
            return NO_SDK;
        }
        // SAFETY: slot `+0x178` is `balanceInquiry(u32 slipNo, u32 brandNumber)`.
        unsafe { slot::<unsafe extern "C" fn(Object, u32, u32) -> u32>(object, 0x178)(object, slip, brand) }
    }

    fn cancel_request(&mut self) -> u32 {
        self.call_none(0x200)
    }

    fn select_button(&mut self, id: u32) -> u32 {
        self.call_u32(0x208, id)
    }

    fn brand_name(&mut self, brand: u32) -> Option<String> {
        self.brand_text(0x220, brand)
    }

    fn brand_icon(&mut self, brand: u32) -> Option<String> {
        self.brand_text(0x228, brand)
    }
}

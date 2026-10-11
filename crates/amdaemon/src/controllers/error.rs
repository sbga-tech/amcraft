//! `amdaemon::server::ErrorController` (vtable `0x1405CF3C8`, ctor `0x1400344E0`) and its
//! `ErrorEventListener` (vtable `0x1405CF3B0`).
//!
//! Publishes the last fatal error and libam's error logs (`history`, `times`, `systemHistory`) and executes the
//! client's `Reset` / `Set` / `LogClear` requests. Evidence: `spec/controllers/error_sequence.md` Part 1
//! (claims `error_sequence.error.*`).

use std::fmt::Write as _;

use arrayvec::ArrayVec;

use crate::executor::{RequestExecutor, RequestState};
use crate::schema::error::{
    ClientErrorRef, ErrorTask, ErrorTimesItemPatch, HISTORY_CAPACITY, HistoryLogMut, TIMES_CAPACITY, TimesLogMut,
};
use crate::schema::shared::{ErrorDataMut, ErrorDataPatch, ErrorResetType as SchemaResetType};
use crate::schema::text::fit_wide;
use libam::error::{ErrorOccurrence, ErrorPort, FatalError, HistoryRing};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};

/// `0x1400524E0`: publishes a libam fatal error as `ErrorData`, replacing the whole record.
///
/// If the libam timestamp is not a valid calendar date, the record is left with every field zero, including
/// `number`. The message is cut to 199 units (`wcsncpy_s(dst, 200, src, 199)`).
pub fn publish_fatal_error(error: &FatalError, data: &mut ErrorDataMut<'_>) {
    data.clear();
    let Some(time) = error.time.to_date_time() else {
        return;
    };
    let message = fit_wide(error.message_units(), data.message_mut().capacity());
    let patch = ErrorDataPatch {
        number: Some(error.number),
        sub_number: Some(error.sub_number),
        message: Some(&message),
        reset_type: Some(map_reset_type(error.reset_type).into()),
        time: Some(time.0),
    };
    data.copy_from(&patch).expect("scalar fields, a closed enum value and a fitted message");
}

/// libam reset type → `ErrorData.resetType` (`0x1400524E0`): 1→2, 2→3, 3→4, 4→1, anything else 0.
fn map_reset_type(libam: u32) -> SchemaResetType {
    match libam {
        1 => SchemaResetType::Shutdown,
        2 => SchemaResetType::Operation,
        3 => SchemaResetType::Test,
        4 => SchemaResetType::Any,
        _ => SchemaResetType::None,
    }
}

/// `amdaemon::server::ErrorController`.
pub struct ErrorController {
    port: Box<dyn ErrorPort>,
    /// `lastLanguage` (`this+0x28`, `wchar_t[16]`), initially empty.
    last_language: [u16; 16],
    executor: RequestExecutor,
}

impl std::fmt::Debug for ErrorController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErrorController")
            .field("last_language", &self.last_language)
            .field("executor", &self.executor)
            .finish_non_exhaustive()
    }
}

impl ErrorController {
    /// Constructs the controller (`0x1400344E0`). The original also registers its listener with the Manager's
    /// fatal-error slot (`Manager+0x3B8`); here the Manager hub delivers to [`Controller::on_fatal_error`].
    pub fn new(port: Box<dyn ErrorPort>) -> Self {
        Self { port, last_language: [0; 16], executor: RequestExecutor::default() }
    }

    /// `wcsncpy_s(lastLanguage, 16, lang, 15)`: at most 15 units, then a terminator.
    fn remember_language(&mut self, language: &[u16; 16]) {
        let fitted = fit_wide(language, self.last_language.len());
        self.last_language = [0; 16];
        self.last_language[..fitted.len()].copy_from_slice(fitted.as_slice());
    }
}

/// The units before the first NUL (the whole array when unterminated): what the language comparison sees.
fn language_units(language: &[u16; 16]) -> &[u16] {
    let end = language.iter().position(|&unit| unit == 0).unwrap_or(language.len());
    &language[..end]
}

impl Controller for ErrorController {
    fn id(&self) -> ControllerId {
        ControllerId::Error
    }

    /// `0x1400346C0`.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor.is_busy(ctx.client.error().generation(), ctx.server.error().response().generation())
    }

    /// vf5 (`0x1400346F0`): seeds `lastLanguage`; no `ServerData` field is cleared.
    fn clear_server_data(&mut self, _ctx: &mut Ctx<'_>) {
        if let Some(language) = self.port.language() {
            self.remember_language(&language);
        }
    }

    /// vf6 (`0x140034780`): republishes the logs when the language changed, then `checkResult` (`checkResult_9`,
    /// `0x140035C80`).
    ///
    /// The original compares unconditionally and would dereference a null language string without a libam
    /// context; without one the comparison is skipped here.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(language) = self.port.language()
            && language_units(&language) != language_units(&self.last_language)
        {
            self.remember_language(&language);
            update_log(self.port.as_ref(), ctx);
        }
        // The checker is the constant lambda `0x140035990`: an accepted request always succeeds.
        let request_generation = ctx.client.error().generation();
        let mut block = ctx.server.error_mut();
        let response_generation = block.response().generation();
        let outcome = self.executor.check_result(request_generation, response_generation, || RequestState::Succeeded);
        commit_check(outcome, &mut block.response_mut());
    }

    /// vf7 (`0x140034830`): the accept edge.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let Self { port, executor, .. } = self;
        let request_generation = ctx.client.error().generation();
        let response_generation = ctx.server.error().response().generation();
        executor.accept(request_generation, response_generation, || accept(port.as_mut(), ctx));
    }

    /// `ErrorEventListener::onOccurFatalError` (`0x140033D10`).
    fn on_fatal_error(&mut self, error: &FatalError, ctx: &mut Ctx<'_>) {
        tracing::info!(number = error.number, "fatal error");
        publish_fatal_error(error, &mut ctx.server.error_mut().data_mut());
        update_log(self.port.as_ref(), ctx);
    }
}

/// `ErrorController::accept` (`0x1400348A0`), the executor acceptor.
fn accept(port: &mut dyn ErrorPort, ctx: &mut Ctx<'_>) -> RequestState {
    let request = ctx.client.error();
    if ctx.manager.debug_dump() {
        tracing::info!("{}", trace_line(&request));
    }
    if !port.error_facade_available() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    match request.task_kind() {
        Some(ErrorTask::Reset) => port.reset(),
        Some(ErrorTask::Set) => {
            let set = request.set_info();
            port.set(set.number, set.sub_number);
        }
        Some(ErrorTask::LogClear) => {
            port.clear_logs();
            update_log(port, ctx);
        }
        // `Result::setCode(5)` → the request fails.
        None => return RequestState::Failed,
    }
    RequestState::Pending
}

/// Request trace (`0x140034A90`): `Error - task: <name>` plus `, number: n(s)` for `Set`.
///
/// A task without a name puts the original's stream into the failed state right after the prefix (it would
/// read a null name pointer); only the prefix is logged here.
fn trace_line(request: &ClientErrorRef<'_>) -> String {
    let mut line = String::from("Error - task: ");
    if let Some(task) = request.task_kind() {
        line.push_str(task.name());
        if task == ErrorTask::Set {
            let set = request.set_info();
            // Writing into a `String` cannot fail.
            let _ = write!(line, ", number: {}({})", set.number, set.sub_number);
        }
    }
    line
}

/// `ErrorEventListener::updateLog` (`0x140033EC0`): rebuilds all three logs and publishes them only when every
/// libam fetch succeeded, so the previous coherent snapshot survives any failure. `data` is never touched.
fn update_log(port: &dyn ErrorPort, ctx: &mut Ctx<'_>) {
    if !port.error_facade_available() {
        tracing::warn!("libam is not ready");
        return;
    }
    let Some(history) = fetch_history(port, HistoryRing::Application) else {
        tracing::warn!("libamw version does not match");
        return;
    };
    let Some(times) = fetch_times(port) else {
        tracing::warn!("libamw version does not match");
        return;
    };
    let Some(system_history) = fetch_history(port, HistoryRing::System) else {
        tracing::warn!("libamw version does not match");
        return;
    };
    let mut block = ctx.server.error_mut();
    write_history(&mut block.history_mut(), &history);
    write_times(&mut block.times_mut(), &times);
    write_history(&mut block.system_history_mut(), &system_history);
}

/// One libam history ring as fetched: the count libam reports and its rows.
struct History {
    count: u32,
    rows: ArrayVec<FatalError, HISTORY_CAPACITY>,
}

/// The libam occurrence table as fetched: the count libam reports and its rows.
struct Times {
    count: u32,
    rows: ArrayVec<ErrorOccurrence, TIMES_CAPACITY>,
}

/// Fetches one history ring; `None` when a row fetch fails.
///
/// libam keeps `count <= 15`; the original trusts it and would overrun its stack buffer otherwise, here rows
/// beyond the 15 slots are not fetched.
fn fetch_history(port: &dyn ErrorPort, ring: HistoryRing) -> Option<History> {
    let count = port.history_count(ring);
    let mut rows = ArrayVec::new();
    for index in (0..count).take(HISTORY_CAPACITY) {
        rows.push(port.history_item(ring, index).ok()?);
    }
    Some(History { count, rows })
}

/// Fetches the occurrence-count table; `None` when a row fetch fails (rows beyond the 222 slots are not fetched).
fn fetch_times(port: &dyn ErrorPort) -> Option<Times> {
    let count = port.times_count();
    let mut rows = ArrayVec::new();
    for index in (0..count).take(TIMES_CAPACITY) {
        rows.push(port.times_item(index).ok()?);
    }
    Some(Times { count, rows })
}

/// Publishes a fetched history ring (`ServerHistoryLogData`), rows through [`FatalError::publish`].
fn write_history(log: &mut HistoryLogMut<'_>, history: &History) {
    // The original assigns a freshly built log: rows past `count` are zero.
    log.clear();
    log.count_mut().set(history.count).expect("u32 accepts every value");
    let mut rows = log.items_mut();
    for (index, error) in history.rows.iter().enumerate() {
        let mut row = rows.get_mut(index).expect("fetched rows are limited to the ring capacity");
        publish_fatal_error(error, &mut row);
    }
}

/// Publishes a fetched occurrence table (`ServerTimesLogData`).
fn write_times(log: &mut TimesLogMut<'_>, times: &Times) {
    // The original assigns a freshly built table: rows past `count` are zero.
    log.clear();
    log.count_mut().set(times.count).expect("u32 accepts every value");
    let mut rows = log.items_mut();
    for (index, item) in times.rows.iter().enumerate() {
        let mut row = rows.get_mut(index).expect("fetched rows are limited to the table capacity");
        let patch = ErrorTimesItemPatch { number: Some(item.number), times: Some(item.times) };
        row.copy_from(&patch).expect("scalar fields accept every value");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ops::Range;
    use std::rc::Rc;

    use crate::schema::error::{ClientErrorPatch, ErrorTimesItem, HistoryLogRef, ServerErrorRef};
    use crate::schema::image::ServerImage;
    use crate::schema::shared::ErrorResetType;
    use libam::result::{AmError, AmResult};
    use widestring::U16CStr;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::{Harness, civil};
    use libam::time::Tm;

    // `ServerData.error` (`+0x56B0`) and its fields, read raw to compare whole records and the bytes between them.
    const BLOCK: usize = 0x56B0;
    const DATA: Range<usize> = BLOCK + 0x0008..BLOCK + 0x01A8;
    const HISTORY: Range<usize> = BLOCK + 0x01A8..BLOCK + 0x1A10;
    const TIMES: Range<usize> = BLOCK + 0x1A10..BLOCK + 0x2104;
    const PAD_2104: Range<usize> = BLOCK + 0x2104..BLOCK + 0x2108;
    const SYSTEM_HISTORY: Range<usize> = BLOCK + 0x2108..BLOCK + 0x3970;
    /// Offset of the first row in a history ring, and the size of a row (`ErrorData`).
    const ROWS: usize = 0x08;
    const ROW: usize = 0x1A0;

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Reset,
        Set(u16, u16),
        ClearLogs,
    }

    #[derive(Debug, Default)]
    struct State {
        language: Option<[u16; 16]>,
        application: Vec<FatalError>,
        system: Vec<FatalError>,
        times: Vec<ErrorTimesItem>,
        application_count: Option<u32>,
        fail_application: bool,
        fail_times: bool,
        fail_system: bool,
        calls: Vec<Call>,
    }

    struct FakePort(Rc<RefCell<State>>);

    impl ErrorPort for FakePort {
        fn error_facade_available(&self) -> bool {
            true
        }
        fn language(&self) -> Option<[u16; 16]> {
            self.0.borrow().language
        }
        fn reset(&mut self) {
            self.0.borrow_mut().calls.push(Call::Reset);
        }
        fn set(&mut self, number: u16, sub_number: u16) {
            self.0.borrow_mut().calls.push(Call::Set(number, sub_number));
        }
        fn clear_logs(&mut self) {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::ClearLogs);
            state.application.clear();
            state.system.clear();
            state.times.clear();
        }
        fn history_count(&self, ring: HistoryRing) -> u32 {
            let state = self.0.borrow();
            match ring {
                HistoryRing::Application => state.application_count.unwrap_or(state.application.len() as u32),
                HistoryRing::System => state.system.len() as u32,
            }
        }
        fn history_item(&self, ring: HistoryRing, index: u32) -> AmResult<FatalError> {
            let state = self.0.borrow();
            let (rows, fail) = match ring {
                HistoryRing::Application => (&state.application, state.fail_application),
                HistoryRing::System => (&state.system, state.fail_system),
            };
            match rows.get(index as usize) {
                Some(item) if !fail => Ok(item.clone()),
                _ => Err(AmError::code(AmError::INVALID_ARGUMENT)),
            }
        }
        fn times_count(&self) -> u32 {
            self.0.borrow().times.len() as u32
        }
        fn times_item(&self, index: u32) -> AmResult<ErrorOccurrence> {
            let state = self.0.borrow();
            match state.times.get(index as usize) {
                Some(item) if !state.fail_times => Ok(ErrorOccurrence { number: item.number, times: item.times }),
                _ => Err(AmError::code(AmError::INVALID_ARGUMENT)),
            }
        }
    }

    fn language(text: &str) -> [u16; 16] {
        let mut value = [0; 16];
        for (unit, source) in value.iter_mut().zip(text.encode_utf16().take(15)) {
            *unit = source;
        }
        value
    }

    fn times_item(number: u16, times: u32) -> ErrorTimesItem {
        ErrorTimesItem { number, times }
    }

    /// 2024-05-20 12:34:56 as a libam `tm`.
    fn tm_2024() -> Tm {
        Tm { sec: 56, min: 34, hour: 12, mday: 20, mon: 4, year: 124, ..Tm::default() }
    }

    fn fatal(number: u16, reset_type: u32) -> FatalError {
        FatalError::new(number, number + 1, tm_2024(), "message", reset_type)
    }

    /// A fresh image whose Error block holds only `error`, published as the last fatal error.
    fn published(error: &FatalError) -> ServerImage {
        let mut image = ServerImage::new();
        publish_fatal_error(error, &mut image.error_mut().data_mut());
        image
    }

    /// Message unit `index` of the last fatal error in `image` (`ErrorData` `+0x04`), behind any terminator.
    fn message_unit(image: &ServerImage, index: usize) -> u16 {
        let at = DATA.start + 4 + 2 * index;
        u16::from_le_bytes([image.bytes()[at], image.bytes()[at + 1]])
    }

    fn is_zero(bytes: &[u8]) -> bool {
        bytes.iter().all(|&byte| byte == 0)
    }

    fn text(string: &U16CStr) -> String {
        String::from_utf16_lossy(string.as_slice())
    }

    /// `number` of history row `index`.
    fn number(log: &HistoryLogRef<'_>, index: usize) -> u16 {
        log.items().get(index).expect("a row of the ring").number()
    }

    struct Rig {
        harness: Harness,
        slot: ControllerSlot,
        state: Rc<RefCell<State>>,
    }

    impl Rig {
        fn new() -> Self {
            let state = Rc::new(RefCell::new(State { language: Some(language("ja")), ..State::default() }));
            let controller = ErrorController::new(Box::new(FakePort(Rc::clone(&state))));
            let harness = Harness::running();
            Self { harness, slot: ControllerSlot::new(Box::new(controller)), state }
        }

        /// One `App::execute` iteration as far as this controller is concerned.
        fn frame(&mut self) {
            let mut ctx = self.harness.ctx();
            self.slot.pre_execute(&mut ctx);
            self.slot.post_execute(&mut ctx);
        }

        fn send(&mut self, generation: u32, task: u32, number: u16, sub_number: u16) {
            let [n0, n1] = number.to_le_bytes();
            let [s0, s1] = sub_number.to_le_bytes();
            let patch =
                ClientErrorPatch { generation: Some(generation), task: Some(task), info: Some(&[n0, n1, s0, s1]) };
            self.harness.client.error_mut().copy_from(&patch).expect("plain fields");
        }

        fn is_busy(&mut self) -> bool {
            let ctx = self.harness.ctx();
            self.slot.controller().is_busy(&ctx)
        }

        fn fatal_error(&mut self, error: &FatalError) {
            let mut ctx = self.harness.ctx();
            self.slot.controller_mut().on_fatal_error(error, &mut ctx);
        }

        fn calls(&self) -> Vec<Call> {
            std::mem::take(&mut self.state.borrow_mut().calls)
        }

        fn error(&self) -> ServerErrorRef<'_> {
            self.harness.server.error()
        }

        /// The published `(generation, succeeded)`.
        fn response(&self) -> (u32, bool) {
            let response = self.error().response();
            (response.generation(), response.succeeded())
        }

        /// The three published `count` fields: history, times, system history.
        fn counts(&self) -> (u32, u32, u32) {
            let block = self.error();
            (block.history().count(), block.times().count(), block.system_history().count())
        }

        fn bytes(&self, range: Range<usize>) -> &[u8] {
            &self.harness.server.bytes()[range]
        }

        /// Raw bytes of row `index` of the history ring that starts at `ring`.
        fn row(&self, ring: &Range<usize>, index: usize) -> &[u8] {
            let start = ring.start + ROWS + index * ROW;
            self.bytes(start..start + ROW)
        }
    }

    #[test]
    fn set_task_reaches_libam_at_accept_and_completes_on_next_update() {
        let mut rig = Rig::new();
        rig.send(1, 1, 42, 3);
        rig.frame();
        assert_eq!(rig.calls(), vec![Call::Set(42, 3)]);
        assert_eq!(rig.response().0, 0);
        assert!(rig.is_busy());
        rig.frame();
        assert_eq!(rig.response(), (1, true));
        assert!(!rig.is_busy());
        assert!(rig.calls().is_empty(), "a completed request is not accepted again");
    }

    #[test]
    fn log_clear_task_clears_libam_logs_then_republishes_wire_logs() {
        let mut rig = Rig::new();
        {
            let mut state = rig.state.borrow_mut();
            state.application = vec![fatal(100, 1)];
            state.system = vec![fatal(200, 2)];
            state.times = vec![times_item(100, 4)];
        }
        rig.fatal_error(&fatal(100, 1));
        assert_eq!(rig.counts(), (1, 1, 1));
        rig.send(1, 2, 0, 0);
        rig.frame();
        assert_eq!(rig.calls(), vec![Call::ClearLogs]);
        // The republish happens inside accept, before the response is written.
        assert_eq!(rig.counts(), (0, 0, 0));
        assert!(is_zero(rig.row(&HISTORY, 0)));
        assert_eq!(rig.response().0, 0);
        rig.frame();
        assert_eq!(rig.response(), (1, true));
        assert_eq!(rig.error().data().number(), 100, "LogClear does not clear the last fatal error");
    }

    #[test]
    fn unknown_task_fails_without_touching_libam() {
        let mut rig = Rig::new();
        rig.send(4, 3, 9, 9);
        rig.frame();
        assert!(rig.is_busy());
        rig.frame();
        assert!(rig.calls().is_empty());
        assert_eq!(rig.response(), (4, false));
    }

    #[test]
    fn fatal_error_publishes_data_and_logs() {
        let mut rig = Rig::new();
        {
            let mut state = rig.state.borrow_mut();
            state.application = vec![fatal(1234, 2), fatal(77, 4)];
            state.system = vec![fatal(9, 1)];
            state.times = vec![times_item(1234, 2), times_item(77, 5)];
        }
        rig.fatal_error(&fatal(1234, 2));

        let block = rig.error();
        assert_eq!(rig.bytes(DATA), &published(&fatal(1234, 2)).bytes()[DATA]);
        assert_eq!(block.data().number(), 1234);
        assert_eq!(block.data().sub_number(), 1235);
        assert_eq!(block.history().count(), 2);
        assert_eq!(number(&block.history(), 0), 1234, "row 0 is the newest");
        assert_eq!(number(&block.history(), 1), 77);
        assert!(is_zero(rig.row(&HISTORY, 2)));
        assert_eq!(block.times().count(), 2);
        assert_eq!(block.times().items().get(0).expect("row").copy_into(), times_item(1234, 2));
        assert_eq!(block.times().items().get(1).expect("row").copy_into(), times_item(77, 5));
        assert_eq!(block.system_history().count(), 1);
        assert_eq!(number(&block.system_history(), 0), 9);
        assert!(is_zero(rig.bytes(PAD_2104)));
        assert_eq!(rig.response(), (0, false), "publishing does not complete a request");
    }

    #[test]
    fn fatal_error_conversion_maps_every_field() {
        let mut error = FatalError::new(0x1234, 0x0007, tm_2024(), "disk failure", 1);
        error.message[13] = 0x00FF; // beyond the terminator at index 12: never copied
        let image = published(&error);
        let data = image.error().data();
        assert_eq!((data.number(), data.sub_number()), (0x1234, 7));
        assert_eq!(text(data.message()), "disk failure");
        assert_eq!(message_unit(&image, 13), 0);
        assert_eq!(data.time(), civil(2024, 5, 20, 12, 34, 56).0);
        assert_eq!(data.reset_type(), ErrorResetType::Shutdown);
    }

    #[test]
    fn fatal_error_reset_types_map_one_two_three_four_to_two_three_four_one() {
        let mapped: Vec<u32> =
            (0..=5).map(|raw| published(&fatal(1, raw)).error().data().reset_type() as u32).collect();
        assert_eq!(mapped, vec![0, 2, 3, 4, 1, 0]);
    }

    #[test]
    fn fatal_error_with_invalid_time_publishes_an_all_zero_record() {
        let mut error = fatal(1234, 2);
        error.time.mon = 12; // month 13
        assert!(is_zero(&published(&error).bytes()[DATA]));
        let mut rig = Rig::new();
        rig.fatal_error(&error);
        assert!(is_zero(rig.bytes(DATA)));
    }

    #[test]
    fn failed_row_fetch_keeps_the_previous_snapshot() {
        for (ring, break_it) in [
            ("application", (|s: &mut State| s.fail_application = true) as fn(&mut State)),
            ("times", |s| s.fail_times = true),
            ("system", |s| s.fail_system = true),
        ] {
            let mut rig = Rig::new();
            {
                let mut state = rig.state.borrow_mut();
                state.application = vec![fatal(1, 1)];
                state.system = vec![fatal(2, 1)];
                state.times = vec![times_item(1, 1)];
            }
            rig.fatal_error(&fatal(1, 1));
            let before = [rig.bytes(HISTORY).to_vec(), rig.bytes(TIMES).to_vec(), rig.bytes(SYSTEM_HISTORY).to_vec()];
            {
                let mut state = rig.state.borrow_mut();
                state.application = vec![fatal(10, 1), fatal(11, 1)];
                state.system = vec![fatal(20, 1)];
                state.times = vec![times_item(10, 1), times_item(11, 1)];
                break_it(&mut state);
            }
            rig.fatal_error(&fatal(3, 1));
            assert_eq!(rig.bytes(HISTORY), before[0], "{ring}");
            assert_eq!(rig.bytes(TIMES), before[1], "{ring}");
            assert_eq!(rig.bytes(SYSTEM_HISTORY), before[2], "{ring}");
            assert_eq!(
                rig.bytes(DATA),
                &published(&fatal(3, 1)).bytes()[DATA],
                "data is written before updateLog runs"
            );
        }
    }

    #[test]
    fn history_count_beyond_the_ring_capacity_is_not_fetched_past_the_slots() {
        let mut rig = Rig::new();
        rig.state.borrow_mut().application = (0..15).map(|n| fatal(n + 1, 1)).collect();
        rig.state.borrow_mut().application_count = Some(16);
        rig.fatal_error(&fatal(1, 1));
        assert_eq!(rig.error().history().count(), 16, "the count is published as libam reports it");
        assert_eq!(number(&rig.error().history(), 14), 15);
    }

    #[test]
    fn ready_edge_seeds_the_language_so_no_logs_are_published_until_it_changes() {
        let mut rig = Rig::new();
        rig.state.borrow_mut().application = vec![fatal(8, 1)];
        rig.frame();
        rig.frame();
        assert_eq!(rig.error().history().count(), 0, "the first ready edge must not run updateLog");

        rig.state.borrow_mut().language = Some(language("en"));
        rig.frame();
        assert_eq!(rig.error().history().count(), 1);
        assert_eq!(number(&rig.error().history(), 0), 8);
        assert!(is_zero(rig.bytes(DATA)), "updateLog never writes `data`");

        rig.state.borrow_mut().application.clear();
        rig.frame();
        assert_eq!(rig.error().history().count(), 1, "an unchanged language does not republish");
    }
}

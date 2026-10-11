//! `amdaemon::server::abaas::LogController` (vtable `0x1405E11F8`, ctor `0x14004CCB0`).
//!
//! Publishes the libabaas log state (`alive`, `fileCount`, `upload`, `rootPath`) and forwards client error
//! messages to ABaaS. Evidence: `spec/controllers/abaas_apm.md` §1 (claims `abaas_apm.log.*`).

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::Manager;
use crate::schema::abaas::{ClientAbaasLogRef, LogTask, ServerAbaasLogPatch};
use crate::schema::text::fit_wide;
use libam_api::abaas::LogPort;

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};

/// `amdaemon::server::abaas::LogController`.
pub struct LogController {
    port: Box<dyn LogPort>,
    executor: RequestExecutor,
}

impl std::fmt::Debug for LogController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogController").field("executor", &self.executor).finish_non_exhaustive()
    }
}

impl LogController {
    /// Constructs the controller (`0x14004CCB0`).
    pub fn new(port: Box<dyn LogPort>) -> Self {
        Self { port, executor: RequestExecutor::default() }
    }

    /// `LogController::loadRootPath` (`0x14004D170`): copies the current log folder into `rootPath`, at most 200
    /// characters (`wcsncpy_s(dst, 0xC9, src, 0xC8)`).
    fn load_root_path(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.abaas_handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let root = self.port.current_log_root();
        let mut log = ctx.server.abaas_log_mut();
        let mut field = log.root_path_mut();
        let fitted = fit_wide(root.as_ref().map(|root| root.as_slice()).unwrap_or_default(), field.capacity());
        field.set(&fitted).expect("fitted to the field");
    }
}

impl Controller for LogController {
    fn id(&self) -> ControllerId {
        ControllerId::AbaasLog
    }

    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor.is_busy(ctx.client.abaas_log().generation(), ctx.server.abaas_log().response().generation())
    }

    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        self.load_root_path(ctx);
    }

    /// `LogController::updateServerData` (`0x14004CE30`).
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.abaas_handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let request_generation = ctx.client.abaas_log().generation();
        let mut log = ctx.server.abaas_log_mut();
        let patch = ServerAbaasLogPatch {
            alive: Some(self.port.is_alive()),
            file_count: Some(self.port.file_count()),
            upload: Some(!self.port.upload_idle()),
            ..ServerAbaasLogPatch::default()
        };
        log.copy_from(&patch).expect("scalar fields accept every value");

        // The checker is the constant lambda `0x14004DFE0`: an accepted request always succeeds. The completion
        // check is `checkResult_16` (`0x14004E290`).
        let response_generation = log.response().generation();
        let outcome = self.executor.check_result(request_generation, response_generation, || RequestState::Succeeded);
        commit_check(outcome, &mut log.response_mut());
    }

    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let Self { port, executor } = self;
        let request = ctx.client.abaas_log();
        let manager = &mut *ctx.manager;
        let response_generation = ctx.server.abaas_log().response().generation();
        executor.accept(request.generation(), response_generation, || accept(port.as_mut(), &request, manager));
    }

    fn on_app_state(&mut self, _state: u32, ctx: &mut Ctx<'_>) {
        // Listener `0x14004DFD0`: runs on every application-state broadcast and ignores the state value.
        self.load_root_path(ctx);
    }
}

/// `LogController::accept` (`0x14004CFC0`), the executor acceptor.
fn accept(port: &mut dyn LogPort, request: &ClientAbaasLogRef<'_>, manager: &mut Manager) -> RequestState {
    if manager.debug_dump() {
        let text = trace_line(request);
        tracing::info!("{text}");
    }
    if port.abaas_handle().is_err() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    match request.task_kind() {
        Some(LogTask::Error) => match request.error_message() {
            Some(message) => {
                port.send_error_message(message.as_slice());
                RequestState::Pending
            }
            // The strict payload proof failed: the message is not terminated within its field (the original
            // has no bound at all and reads on).
            None => RequestState::Failed,
        },
        // `Result::setCode(5, "Requested unknown task.")` → Failed.
        None => RequestState::Failed,
    }
}

/// Trace line emitted before every accept when the Manager request trace is on (`0x14004D290`).
///
/// A non-zero task has no name, which puts the original stream into the failed state right after the prefix.
fn trace_line(request: &ClientAbaasLogRef<'_>) -> String {
    let mut line = String::from("ABaaS Log - task: ");
    if let Some(task) = request.task_kind() {
        line.push_str(task.name());
        line.push_str(", message: ");
        if let Some(message) = request.error_message() {
            line.push_str(&String::from_utf16_lossy(message.as_slice()));
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::schema::abaas::{ClientAbaasLogPatch, ServerAbaasLogRef};
    use libam_api::result::{AmError, AmResult};
    use widestring::U16CStr;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::{Harness, buffer};

    /// `ServerData.abaas_log` (`+0x3D40`): `root_path` follows the 8-byte response.
    const ROOT_PATH: usize = 0x3D40 + 8;

    /// State behind the fake libam facade.
    struct Libam {
        initialised: bool,
        alive: bool,
        file_count: i32,
        upload_idle: bool,
        root: Option<Vec<u16>>,
        sent: Vec<Vec<u16>>,
    }

    struct FakePort {
        libam: Rc<RefCell<Libam>>,
    }

    impl LogPort for FakePort {
        fn abaas_handle(&mut self) -> AmResult<()> {
            if self.libam.borrow().initialised {
                Ok(())
            } else {
                Err(AmError::with_message(AmError::NOT_INITIALIZED, "amw is uninitialized\n"))
            }
        }

        fn is_alive(&mut self) -> bool {
            self.libam.borrow().alive
        }

        fn file_count(&mut self) -> i32 {
            self.libam.borrow().file_count
        }

        fn upload_idle(&mut self) -> bool {
            self.libam.borrow().upload_idle
        }

        fn current_log_root(&mut self) -> Option<[u16; 260]> {
            self.libam.borrow().root.as_deref().map(buffer)
        }

        fn send_error_message(&mut self, message: &[u16]) {
            self.libam.borrow_mut().sent.push(message.to_vec());
        }
    }

    struct Rig {
        harness: Harness,
        slot: ControllerSlot,
        libam: Rc<RefCell<Libam>>,
    }

    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn text(string: &U16CStr) -> String {
        String::from_utf16_lossy(string.as_slice())
    }

    /// The request's info bytes: `units` little-endian from the start, zeros after (so a shorter text is terminated).
    fn info(units: &[u16]) -> [u8; 2050] {
        let mut info = [0; 2050];
        for (pair, unit) in info.chunks_exact_mut(2).zip(units) {
            pair.copy_from_slice(&unit.to_le_bytes());
        }
        info
    }

    impl Rig {
        fn new() -> Self {
            Self::with(Harness::running())
        }

        fn with(harness: Harness) -> Self {
            let libam = Rc::new(RefCell::new(Libam {
                initialised: true,
                alive: false,
                file_count: -1,
                upload_idle: true,
                root: None,
                sent: Vec::new(),
            }));
            let port = FakePort { libam: Rc::clone(&libam) };
            let slot = ControllerSlot::new(Box::new(LogController::new(Box::new(port))));
            Self { harness, slot, libam }
        }

        fn pre(&mut self) {
            let mut ctx = self.harness.ctx();
            self.slot.pre_execute(&mut ctx);
        }

        fn post(&mut self) {
            let mut ctx = self.harness.ctx();
            self.slot.post_execute(&mut ctx);
        }

        fn broadcast(&mut self, state: u32) {
            let mut ctx = self.harness.ctx();
            self.slot.controller_mut().on_app_state(state, &mut ctx);
        }

        fn is_busy(&mut self) -> bool {
            let ctx = self.harness.ctx();
            self.slot.controller().is_busy(&ctx)
        }

        /// The client writes a request whose info bytes are `info`.
        fn request_raw(&mut self, generation: u32, task: u32, info: &[u8; 2050]) {
            let patch = ClientAbaasLogPatch { generation: Some(generation), task: Some(task), info: Some(info) };
            self.harness.client.abaas_log_mut().copy_from(&patch).expect("plain fields");
        }

        /// The client writes a request carrying `message`, cut to 1024 units and terminated.
        fn request(&mut self, generation: u32, task: u32, message: &str) {
            let units: Vec<u16> = message.encode_utf16().take(1024).collect();
            self.request_raw(generation, task, &info(&units));
        }

        fn published(&self) -> ServerAbaasLogRef<'_> {
            self.harness.server.abaas_log()
        }

        /// The published response `(generation, succeeded)`.
        fn response(&self) -> (u32, bool) {
            let response = self.published().response();
            (response.generation(), response.succeeded())
        }

        fn sent(&self) -> Vec<Vec<u16>> {
            self.libam.borrow().sent.clone()
        }

        fn set_root(&self, root: Option<Vec<u16>>) {
            self.libam.borrow_mut().root = root;
        }

        fn set_initialised(&self, initialised: bool) {
            self.libam.borrow_mut().initialised = initialised;
        }
    }

    #[test]
    fn error_task_is_accepted_after_the_tick_and_succeeds_on_the_next_pre_execute() {
        let mut rig = Rig::new();
        rig.request(1, 0, "disk full");
        rig.pre();
        rig.post();
        assert_eq!(rig.sent(), vec![utf16("disk full")]);
        assert_eq!(rig.response(), (0, false));
        assert!(rig.is_busy());

        rig.pre();
        assert_eq!(rig.response(), (1, true));
        assert!(!rig.is_busy());
        assert_eq!(rig.sent().len(), 1);
    }

    #[test]
    fn unknown_tasks_fail_without_contacting_abaas() {
        for task in [1, 2, u32::MAX] {
            let mut rig = Rig::new();
            rig.request(1, 0, "ok");
            rig.pre();
            rig.post();
            rig.pre();
            assert!(rig.response().1);

            rig.request(2, task, "ignored");
            rig.post();
            rig.pre();
            assert_eq!(rig.response(), (2, false));
            assert_eq!(rig.sent(), vec![utf16("ok")], "task {task}");
        }
    }

    #[test]
    fn unterminated_message_fails_the_request_without_contacting_abaas() {
        // Strict payload decoding: a message filling all 1025 units has no terminator, so the request fails
        // (the lenient reader used to send the 1025 units).
        let mut rig = Rig::new();
        rig.request_raw(1, 0, &info(&[u16::from(b'a'); 1025]));
        rig.pre();
        rig.post();
        assert!(rig.sent().is_empty());
        rig.pre();
        assert_eq!(rig.response(), (1, false));
    }

    #[test]
    fn accept_without_libam_fails() {
        let mut rig = Rig::new();
        rig.pre();
        rig.set_initialised(false);
        rig.request(1, 0, "lost");
        rig.post();
        assert!(rig.sent().is_empty());

        rig.set_initialised(true);
        rig.pre();
        assert_eq!(rig.response(), (1, false));
    }

    #[test]
    fn update_without_libam_skips_the_completion_check() {
        let mut rig = Rig::new();
        rig.pre();
        rig.request(1, 0, "pending");
        rig.post();
        rig.set_initialised(false);
        rig.pre();
        assert_eq!(rig.response().0, 0);

        rig.set_initialised(true);
        rig.pre();
        assert_eq!(rig.response(), (1, true));
    }

    #[test]
    fn update_publishes_alive_file_count_and_inverted_upload_idle() {
        let mut rig = Rig::new();
        {
            let mut libam = rig.libam.borrow_mut();
            libam.alive = true;
            libam.file_count = 7;
            libam.upload_idle = false;
        }
        rig.pre();
        let log = rig.published();
        assert_eq!((log.alive(), log.file_count(), log.upload()), (true, 7, true));

        {
            let mut libam = rig.libam.borrow_mut();
            libam.alive = false;
            libam.file_count = -1;
            libam.upload_idle = true;
        }
        rig.pre();
        let log = rig.published();
        assert_eq!((log.alive(), log.file_count(), log.upload()), (false, -1, false));
    }

    #[test]
    fn root_path_loads_on_the_ready_edge_but_not_on_plain_updates() {
        let mut rig = Rig::new();
        rig.set_root(Some(utf16(r"D:\log\")));
        assert_eq!(text(rig.published().root_path()), "");
        rig.pre();
        assert_eq!(text(rig.published().root_path()), r"D:\log\");

        rig.set_root(Some(utf16(r"D:\log\SDEY\")));
        rig.pre();
        assert_eq!(text(rig.published().root_path()), r"D:\log\");
    }

    #[test]
    fn root_path_follows_every_application_state_broadcast() {
        let mut rig = Rig::new();
        rig.set_root(Some(utf16(r"D:\log\")));
        rig.pre();

        rig.set_root(Some(utf16(r"D:\log\SDEY\")));
        rig.broadcast(1);
        assert_eq!(text(rig.published().root_path()), r"D:\log\SDEY\");

        rig.set_root(None);
        rig.broadcast(2);
        assert_eq!(text(rig.published().root_path()), "");
    }

    #[test]
    fn shorter_root_path_overwrites_only_up_to_the_terminator() {
        // `wcsncpy_s` writes the text plus one NUL; the previous tail stays on the wire.
        let mut rig = Rig::new();
        rig.set_root(Some(utf16("ABCDEF")));
        rig.pre();
        rig.set_root(Some(utf16("xy")));
        rig.broadcast(1);
        let head: Vec<u16> = rig.harness.server.bytes()[ROOT_PATH..ROOT_PATH + 14]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(head, utf16("xy\0DEF\0"));
    }

    #[test]
    fn broadcast_updates_root_path_even_while_not_ready() {
        let mut rig = Rig::with(Harness::new());
        rig.set_root(Some(utf16(r"D:\log\")));
        rig.broadcast(1);
        assert_eq!(text(rig.published().root_path()), r"D:\log\");
    }

    #[test]
    fn root_path_is_cut_at_200_characters_and_stays_terminated() {
        for (length, expected) in [(199, 199), (200, 200), (201, 200), (400, 200)] {
            let mut rig = Rig::new();
            rig.set_root(Some(vec![u16::from(b'x'); length]));
            rig.pre();
            // A published string is always terminated within its field, so the length is the cut.
            assert_eq!(rig.published().root_path().len(), expected, "source length {length}");
        }
    }
}

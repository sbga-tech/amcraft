//! App test doubles and fixtures: recording controllers and host, scripted and recording libam
//! interfaces, config loader, server exchange, main-loop clock, window, console, traces and assembly.
//! Doubles of external interfaces live in [`backend::mock`]; protocol emulators live in `libam::mock`.

mod console;
mod recorded;
mod trace;
mod window;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use crate::controllers::controller::{Controller, ControllerId, Ctx};
use crate::ipc::server::RootsMut;
use crate::manager::ManagerState;
use crate::schema::image::{ClientImage, ServerImage, ServerIoImage};
use crate::schema::process::ServerProcessPatch;
use libam_api::config::{AppConfig, ProcessConfig};
use libam_api::error::FatalError;
use libam_api::result::{AmError, AmResult};

pub(crate) use self::console::FakeConsole;
pub(crate) use self::recorded::{Recorded, Seen};
pub(crate) use self::trace::{Trace, lines, new_trace, push};
pub(crate) use self::window::FakeWindow;
use crate::daemon::app::{AppServices, ServerExchange, SystemServices};
use crate::daemon::arcade::HubHost;
use crate::daemon::clock::Clock;
use crate::daemon::log::LogPath;
use libam_api::interface::{
    AmInterface, CabinetAppConfig, ConfigLoadRequest, ConfigLoader, LibamEvent, LibamwConfig, LoadedConfig,
};

/// A module path inside a directory that does not exist: the default log file can never be written.
pub(crate) const MODULE_PATH: &str = "/amdaemon-daemon-tests-missing-dir/amdaemon.exe";

/// The registry position of `id`.
pub(crate) fn index_of(id: ControllerId) -> usize {
    ControllerId::ORDER.iter().position(|candidate| *candidate == id).expect("every id is in ORDER")
}

/// Test-side switches of one [`Probe`].
#[derive(Debug, Clone, Default)]
pub(crate) struct ProbeHandle {
    /// Number of `is_busy` calls that still answer `true`.
    pub busy: Rc<Cell<u32>>,
    /// Overrides `is_ready` when set.
    pub ready: Rc<Cell<Option<bool>>>,
    /// Written to `ServerData.process.boot_clock` by the next `update_server_data`.
    pub boot_clock: Rc<Cell<Option<i64>>>,
    /// Broadcast through `Manager::emit_app_state` by the next `update_server_data`.
    pub emit_app_state: Rc<Cell<Option<u32>>>,
    /// Manager state observed by every `update_server_data`.
    pub states: Rc<RefCell<Vec<ManagerState>>>,
    /// `Manager::debug_dump` as observed by every `on_process_config`.
    pub debug_dump_seen: Rc<RefCell<Vec<bool>>>,
    /// Makes `accept_requests` call `Manager::kill` (an accepted Kill request).
    pub kill_on_accept: Rc<Cell<bool>>,
}

/// A controller that records every hook as `<Id>:<hook>` in the trace.
#[derive(Debug)]
pub(crate) struct Probe {
    id: ControllerId,
    name: String,
    trace: Trace,
    handle: ProbeHandle,
}

impl Probe {
    pub(crate) fn new(id: ControllerId, trace: &Trace, handle: ProbeHandle) -> Self {
        Self { id, name: format!("{id:?}"), trace: Rc::clone(trace), handle }
    }

    fn note(&self, hook: impl std::fmt::Display) {
        push(&self.trace, format!("{}:{hook}", self.name));
    }
}

impl Controller for Probe {
    fn id(&self) -> ControllerId {
        self.id
    }

    fn is_busy(&self, _ctx: &Ctx<'_>) -> bool {
        self.note("is_busy");
        let remaining = self.handle.busy.get();
        self.handle.busy.set(remaining.saturating_sub(1));
        remaining > 0
    }

    fn is_ready(&self, ctx: &Ctx<'_>) -> bool {
        self.handle.ready.get().unwrap_or_else(|| ctx.manager.controllers_ready())
    }

    fn clear_server_data(&mut self, _ctx: &mut Ctx<'_>) {
        self.note("clear");
    }

    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        self.note("update");
        self.handle.states.borrow_mut().push(ctx.manager.state());
        if let Some(clock) = self.handle.boot_clock.take() {
            let patch = ServerProcessPatch { boot_clock: Some(clock), ..ServerProcessPatch::default() };
            ctx.server.process_mut().copy_from(&patch).expect("plain fields");
        }
        if let Some(state) = self.handle.emit_app_state.take() {
            ctx.manager.emit_app_state(state);
        }
    }

    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        self.note("accept");
        if self.handle.kill_on_accept.get() {
            ctx.manager.kill();
        }
    }

    fn on_libam_events(&mut self, _ctx: &mut Ctx<'_>) {
        self.note("libam_events");
    }

    fn on_process_config(&mut self, _config: &ProcessConfig, ctx: &mut Ctx<'_>) {
        self.note("process_config");
        self.handle.debug_dump_seen.borrow_mut().push(ctx.manager.debug_dump());
    }

    fn on_app_config(&mut self, _config: &mut AppConfig, _ctx: &mut Ctx<'_>) {
        self.note("app_config");
    }

    fn on_display_complete(&mut self, _ctx: &mut Ctx<'_>) {
        self.note("display_complete");
    }

    fn on_jvs_detect_node(&mut self, _ctx: &mut Ctx<'_>) {
        self.note("jvs_detect_node");
    }

    fn on_usb_io_detect_node(&mut self, _ctx: &mut Ctx<'_>) {
        self.note("usb_io_detect_node");
    }

    fn on_need_to_exit(&mut self, reason: u32, _ctx: &mut Ctx<'_>) {
        self.note(format!("need_to_exit({reason})"));
    }

    fn on_fatal_error(&mut self, _error: &FatalError, _ctx: &mut Ctx<'_>) {
        self.note("fatal_error");
    }

    fn on_app_state(&mut self, state: u32, _ctx: &mut Ctx<'_>) {
        self.note(format!("app_state({state})"));
    }
}

/// Zeroed IPC roots with a scripted `execute`.
pub(crate) struct FakeServer {
    trace: Trace,
    server: ServerImage,
    client: ClientImage,
    server_io: ServerIoImage,
    executes: usize,
    /// The 1-based `execute` call from which the client echoes the boot clock; 0 = never.
    pub echo_from_execute: usize,
}

impl std::fmt::Debug for FakeServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeServer").field("executes", &self.executes).finish_non_exhaustive()
    }
}

impl FakeServer {
    pub(crate) fn new(trace: &Trace) -> Self {
        Self {
            trace: Rc::clone(trace),
            server: ServerImage::new(),
            client: ClientImage::new(),
            server_io: ServerIoImage::new(),
            executes: 0,
            echo_from_execute: 0,
        }
    }
}

impl ServerExchange for FakeServer {
    fn execute(&mut self) {
        self.executes += 1;
        push(&self.trace, "server.execute");
        if self.echo_from_execute != 0 && self.executes >= self.echo_from_execute {
            let boot_clock = self.server.process().boot_clock();
            self.client.process_mut().server_boot_clock_mut().set(boot_clock).expect("plain scalar");
        }
    }

    fn roots_mut(&mut self) -> RootsMut<'_> {
        RootsMut { server: &mut self.server, client: &self.client, server_io: &mut self.server_io }
    }

    fn relation_index(&self) -> u32 {
        7
    }
}

/// Reads from the interface that tests need after the double moved into the App.
#[derive(Debug, Clone, Default)]
pub(crate) struct InterfaceHandles {
    /// `(accepted, config)` answers of the cabinet-type callback.
    pub cabinet_results: Rc<RefCell<Vec<(bool, CabinetAppConfig)>>>,
    /// The libamw configuration `initialize` received.
    pub libamw: Rc<RefCell<Option<LibamwConfig>>>,
}

/// Scripted `am::Interface`.
#[derive(Debug)]
pub(crate) struct FakeInterface {
    trace: Trace,
    pub init: Option<AmResult<Vec<LibamEvent>>>,
    ticks: VecDeque<Vec<LibamEvent>>,
    pub develop: AmResult<bool>,
    pub app_root: AmResult<Option<String>>,
    pub handles: InterfaceHandles,
}

impl FakeInterface {
    pub(crate) fn new(trace: &Trace) -> Self {
        Self {
            trace: Rc::clone(trace),
            init: Some(Ok(vec![LibamEvent::ChangeState(libam_api::sequence::ProcessState(1))])),
            ticks: VecDeque::new(),
            develop: Ok(false),
            app_root: Ok(None),
            handles: InterfaceHandles::default(),
        }
    }

    /// Events returned by the next not yet scripted `execute`.
    pub(crate) fn script_tick(&mut self, events: Vec<LibamEvent>) {
        self.ticks.push_back(events);
    }
}

impl AmInterface for FakeInterface {
    fn initialize(&mut self, config: &LibamwConfig) -> AmResult<Vec<LibamEvent>> {
        push(&self.trace, "am.initialize");
        *self.handles.libamw.borrow_mut() = Some(config.clone());
        self.init.take().unwrap_or_else(|| Err(AmError::code(2)))
    }

    fn execute(&mut self) -> Vec<LibamEvent> {
        push(&self.trace, "am.execute");
        self.ticks.pop_front().unwrap_or_default()
    }

    fn load_cabinet_type_result(&mut self, accepted: bool, app_config: CabinetAppConfig) {
        push(&self.trace, format!("am.cabinet_result({accepted})"));
        self.handles.cabinet_results.borrow_mut().push((accepted, app_config));
    }

    fn finalize(&mut self) {
        push(&self.trace, "am.finalize");
    }

    fn is_develop(&self) -> AmResult<bool> {
        self.develop.clone()
    }

    fn app_root_path(&self) -> AmResult<Option<String>> {
        self.app_root.clone()
    }
}

/// Scripted configuration loader.
#[derive(Debug)]
pub(crate) struct FakeLoader {
    trace: Trace,
    pub loaded: Option<LoadedConfig>,
    pub convert_ok: bool,
    /// Written to `CabinetAppConfig::abaas_log_root` by `convert`.
    pub abaas_log_root: Option<String>,
    /// One line per `load` call.
    pub requests: Rc<RefCell<Vec<String>>>,
}

impl FakeLoader {
    pub(crate) fn new(trace: &Trace) -> Self {
        Self {
            trace: Rc::clone(trace),
            loaded: Some(LoadedConfig::default()),
            convert_ok: true,
            abaas_log_root: None,
            requests: Rc::default(),
        }
    }
}

impl ConfigLoader for FakeLoader {
    fn load(&mut self, request: &ConfigLoadRequest<'_>) -> Option<LoadedConfig> {
        push(&self.trace, "loader.load");
        self.requests.borrow_mut().push(format!(
            "dip={} model={} region={} c={:?} exe={} m={}",
            request.dip_switch,
            request.model,
            request.region,
            request.config_paths,
            request.exe_dir,
            request.merged_dump_path
        ));
        self.loaded.clone()
    }

    fn convert(&mut self, config: &LoadedConfig, _exe_dir: &str, app_config: &mut CabinetAppConfig) -> bool {
        push(&self.trace, format!("loader.convert(4ff8c={})", config.bytes.byte_4ff8c));
        app_config.abaas_log_root = self.abaas_log_root.clone();
        self.convert_ok
    }
}

/// [`HubHost`] that records console requests.
#[derive(Debug)]
pub(crate) struct RecordingHost {
    trace: Trace,
}

impl RecordingHost {
    pub(crate) fn new(trace: &Trace) -> Self {
        Self { trace: Rc::clone(trace) }
    }
}

impl HubHost for RecordingHost {
    fn apply_console(&mut self, show: bool) {
        push(&self.trace, format!("host.apply_console({show})"));
    }
}

/// Scripted main-loop clock. Every reading returns `now` and then advances it by `step`; sleeps are recorded.
#[derive(Debug)]
pub(crate) struct FakeClock {
    trace: Trace,
    pub frequency: i64,
    /// Value of the next reading.
    pub now: Cell<i64>,
    /// Added to `now` after every reading.
    pub step: i64,
}

impl FakeClock {
    pub(crate) fn new(trace: &Trace) -> Self {
        Self { trace: Rc::clone(trace), frequency: 10_000_000, now: Cell::new(1_000_000_000), step: 1_000_000 }
    }
}

impl Clock for FakeClock {
    fn frequency(&self) -> i64 {
        self.frequency
    }

    fn now(&self) -> i64 {
        let now = self.now.get();
        self.now.set(now + self.step);
        now
    }

    fn sleep(&self, milliseconds: u32) {
        push(&self.trace, format!("clock.sleep({milliseconds})"));
    }
}

/// Everything one App test needs; [`Self::into_services`] moves the doubles into an [`AppServices`].
pub(crate) struct Fixture {
    pub trace: Trace,
    pub clock: FakeClock,
    pub window: FakeWindow,
    pub console: FakeConsole,
    pub log: LogPath,
    pub server: FakeServer,
    pub interface: FakeInterface,
    pub loader: FakeLoader,
    pub probes: Vec<ProbeHandle>,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let trace = new_trace();
        let probes: Vec<ProbeHandle> = ControllerId::ORDER.iter().map(|_| ProbeHandle::default()).collect();
        // Only ProcessController overrides isReady (always true, 0x1400427A0).
        probes[index_of(ControllerId::Process)].ready.set(Some(true));
        Self {
            clock: FakeClock::new(&trace),
            window: FakeWindow::new(&trace),
            console: FakeConsole::new(&trace),
            log: LogPath::new(MODULE_PATH),
            server: FakeServer::new(&trace),
            interface: FakeInterface::new(&trace),
            loader: FakeLoader::new(&trace),
            probes,
            trace,
        }
    }

    pub(crate) fn probe(&self, id: ControllerId) -> &ProbeHandle {
        &self.probes[index_of(id)]
    }

    /// The 27 recording controllers in registry order.
    pub(crate) fn controllers(&self) -> Vec<Box<dyn Controller>> {
        ControllerId::ORDER
            .iter()
            .zip(&self.probes)
            .map(|(id, handle)| Box::new(Probe::new(*id, &self.trace, handle.clone())) as Box<dyn Controller>)
            .collect()
    }

    pub(crate) fn into_services(self) -> AppServices {
        AppServices {
            controllers: self.controllers(),
            system: SystemServices {
                clock: Box::new(self.clock),
                window: Box::new(self.window),
                console: Box::new(self.console),
            },
            server: Box::new(self.server),
            interface: Box::new(self.interface),
            config_loader: Box::new(self.loader),
            log: self.log,
        }
    }
}

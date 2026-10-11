//! `amdaemon::App`: the hidden window, the 66/120 Hz main loop and the shutdown sequence.
//!
//! Spec: `spec/controllers/lifecycle.md` §3. The loop runs, per due tick,
//! `Manager::execute` → hub events and `on_libam_events` → 27 × `preExecute` → `Server::execute` →
//! 27 × `postExecute`; the clock, window and console are internal services in [`SystemServices`], the
//! IPC server a [`ServerExchange`].

use std::fmt;

use crate::controllers::controller::{Controller, ControllerId, ControllerSlot, Ctx};
use crate::ipc::server::{RootsMut, Server};
use crate::manager::Manager;

use crate::daemon::arcade::{Arcade, Hub, HubHost, InitializeFailed};
use crate::daemon::args::ArgumentsParser;
use crate::daemon::clock::Clock;
use crate::daemon::console::Console;
use crate::daemon::log::LogPath;
use crate::daemon::stdio;
use crate::daemon::window::{MessagePump, MessageWindow};
use libam_api::interface::{AmInterface, ConfigLoader};

/// `amdaemonExe Ver.91f83d14 Build:May 20 2024 11:28:48\n`: the banner printed first by `App::run`
/// (`0x1405B2A79`), the console title (`0x140002A60`) and the hidden window's title.
pub const BANNER: &str = "amdaemonExe Ver.91f83d14 Build:May 20 2024 11:28:48\n";

/// Window class and application name passed to `Manager::initialize` (`0x1405B2A60`, `0x1405B2A48`).
pub const APP_NAME: &str = "amdaemonExe";

/// Application version passed to `Manager::initialize` (`0x1405B2A28`).
pub const APP_VERSION: &str = "Ver.91f83d14";

/// Frame rate without `-f`.
const FRAME_RATE: i64 = 66;
/// Frame rate with `-f`.
const FRAME_RATE_HIGH: i64 = 120;

/// The IPC server as the App uses it (`ipc::Server`, `App+0x480`).
pub trait ServerExchange {
    /// `Server::execute` (`0x14006BD20`): one transaction with the connected client.
    fn execute(&mut self);

    /// The three data roots handed to the controllers.
    fn roots_mut(&mut self) -> RootsMut<'_>;

    /// The claimed relation index as read by `0x14006BD00` (an unsigned read of the `i32` relation field, so
    /// `u32::MAX` when no relation slot was claimed).
    fn relation_index(&self) -> u32;
}

impl ServerExchange for Server {
    fn execute(&mut self) {
        Server::execute(self);
    }

    fn roots_mut(&mut self) -> RootsMut<'_> {
        Server::roots_mut(self)
    }

    fn relation_index(&self) -> u32 {
        Server::relation_index(self).unwrap_or(u32::MAX)
    }
}

/// `amdaemon::Console` (`App+0x08`): the console is allocated at most once and released with the App.
#[derive(Debug)]
struct AppConsole {
    console: Box<dyn Console>,
    opened: bool,
    allocated: bool,
}

impl AppConsole {
    fn new(console: Box<dyn Console>) -> Self {
        Self { console, opened: false, allocated: false }
    }

    /// `Console::open(title)` (`0x140052250`): the first call wins; an empty title allocates nothing.
    fn open(&mut self, title: &str) {
        if self.opened {
            return;
        }
        if !title.is_empty() {
            self.allocated = self.console.allocate(title);
        }
        self.opened = true;
    }

    /// `FreeConsole` of `Console::~Console` (`0x1400521F0`), when [`Self::open`] allocated the console.
    fn release(&mut self) {
        if self.allocated {
            self.console.free();
            self.allocated = false;
        }
    }
}

/// The App's side of the hub: the `preLoadHook` target.
impl HubHost for AppConsole {
    /// `applyConsole(show)` (`0x140002A60`): `show` opens a console titled [`BANNER`], otherwise an empty title.
    fn apply_console(&mut self, show: bool) {
        self.open(if show { BANNER } else { "" });
    }
}

/// The App's internal system services: its main-loop clock, hidden window and process console.
#[derive(Debug)]
pub struct SystemServices {
    /// Main-loop ticks and sleep ([`StdClock`](crate::daemon::clock::StdClock) outside tests).
    pub clock: Box<dyn Clock>,
    /// The hidden window and its message pump.
    pub window: Box<dyn MessageWindow>,
    /// The console that `-w` or a developer dongle opens.
    pub console: Box<dyn Console>,
}

/// Everything [`App::new`] needs.
pub struct AppServices {
    /// Operating-system services.
    pub system: SystemServices,
    /// The IPC server (already constructed: the Backup controller needs its relation index).
    pub server: Box<dyn ServerExchange>,
    /// The 27 controllers in [`ControllerId::ORDER`].
    pub controllers: Vec<Box<dyn Controller>>,
    /// libam.
    pub interface: Box<dyn AmInterface>,
    /// Configuration loader behind the cabinet-type callback.
    pub config_loader: Box<dyn ConfigLoader>,
    /// The log file the subscriber writes to; `-l` and the root paths libam reports move it.
    pub log: LogPath,
}

impl fmt::Debug for AppServices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppServices").field("controllers", &self.controllers.len()).finish_non_exhaustive()
    }
}

/// The controller list handed to [`App::new`] is not the 27 controllers in registry order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// Wrong number of controllers.
    #[error("expected 27 controllers, got {0}")]
    Count(usize),
    /// A controller is at the wrong registry position.
    #[error("registry position {position} must hold {expected:?}, found {found:?}")]
    Order {
        /// Zero-based position.
        position: usize,
        /// The controller the registry order requires there.
        expected: ControllerId,
        /// The controller that was supplied.
        found: ControllerId,
    },
}

/// `amdaemon::App` (`0x1210` bytes on `wWinMain`'s stack).
pub struct App {
    /// `std::vector<Controller*>` at `+0x11F0`; destroyed in reverse order.
    registry: Vec<ControllerSlot>,
    server: Box<dyn ServerExchange>,
    arcade: Arcade,
    console: AppConsole,
    clock: Box<dyn Clock>,
    window: Box<dyn MessageWindow>,
    /// `+0x00`: `-f` was applied by `initialize`. The original also raised the timer resolution with
    /// `timeBeginPeriod(1)`; the clock's sleep is precise without it.
    frame_high: bool,
    /// A fatal condition ended the process in the original; nothing runs after it.
    crashed: bool,
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("controllers", &self.registry.len())
            .field("frame_high", &self.frame_high)
            .field("crashed", &self.crashed)
            .finish_non_exhaustive()
    }
}

/// Builds a controller context from the server roots and the Manager.
fn make_ctx<'a>(server: &'a mut dyn ServerExchange, manager: &'a mut Manager) -> Ctx<'a> {
    let RootsMut { server, client, server_io } = server.roots_mut();
    Ctx { server, client, server_io, manager }
}

/// `Clock::calcSpan(deadline, now)` (`0x1400536D0`): `deadline - now` in microseconds, `None` when the checked
/// subtraction or the scaling by `1_000_000 / frequency` (reduced by their gcd, `0x140053860`) does not fit, or
/// when the frequency is not positive.
fn span_micros(deadline: i64, now: i64, frequency: i64) -> Option<i64> {
    if frequency <= 0 {
        return None;
    }
    let ticks = deadline.checked_sub(now)?;
    let gcd = {
        let (mut a, mut b) = (1_000_000_i64, frequency);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    };
    let numerator = i128::from(1_000_000 / gcd);
    let denominator = i128::from(frequency / gcd);
    i64::try_from(i128::from(ticks) * numerator / denominator).ok()
}

impl App {
    /// Constructor `0x140001000`: console, log path, Manager, then the controllers as the registry.
    pub fn new(services: AppServices) -> Result<Self, RegistryError> {
        let AppServices { system, server, controllers, interface, config_loader, log } = services;
        if controllers.len() != ControllerId::ORDER.len() {
            return Err(RegistryError::Count(controllers.len()));
        }
        for (position, (controller, expected)) in controllers.iter().zip(ControllerId::ORDER).enumerate() {
            if controller.id() != expected {
                return Err(RegistryError::Order { position, expected, found: controller.id() });
            }
        }
        let SystemServices { clock, window, console } = system;
        Ok(Self {
            registry: controllers.into_iter().map(ControllerSlot::new).collect(),
            server,
            arcade: Arcade::new(interface, config_loader, log),
            console: AppConsole::new(console),
            clock,
            window,
            frame_high: false,
            crashed: false,
        })
    }

    /// The Manager state shared with the controllers.
    pub fn manager(&self) -> &Manager {
        self.arcade.manager()
    }

    /// `App::run` (`0x140002340`): banner, [`Self::initialize`], the main loop and the shutdown sequence.
    /// Returns 1 when initialisation failed (no loop, no shutdown here: [`Drop`] cleans up), otherwise 0.
    pub fn run(&mut self, args: &ArgumentsParser) -> i32 {
        stdio::print_stdout(BANNER);
        if self.initialize(args).is_err() {
            return 1;
        }
        self.execute();
        if self.crashed {
            return 1;
        }
        self.shutdown();
        0
    }

    /// `App::initialize` (`0x1400023E0`).
    fn initialize(&mut self, args: &ArgumentsParser) -> Result<(), InitializeFailed> {
        if !self.window.create_hidden(APP_NAME, BANNER) {
            return Err(InitializeFailed);
        }
        if args.window_option_given() {
            self.console.apply_console(args.show_console());
        } else {
            self.arcade.set_console_from_dongle();
        }
        if !args.log_path().is_empty() {
            self.arcade.log().set_path(args.log_path());
        }
        {
            let Self { registry, server, arcade, console, .. } = &mut *self;
            let mut hub = Hub { controllers: registry.as_mut_slice(), roots: server.roots_mut(), host: console };
            arcade.initialize(args, APP_NAME, APP_VERSION, &mut hub)?;
        }
        self.frame_high = args.frame_high();
        Ok(())
    }

    /// Whether a fatal condition (`__debugbreak` in the original) ended the process.
    fn terminated(&self) -> bool {
        self.arcade.manager().terminated()
    }

    /// The loop's continue predicate (`0x1400025C0`, `0x140002840`): Manager state 1 or 2, else any busy
    /// controller in registry order.
    fn should_continue(&mut self) -> bool {
        if self.arcade.manager().loop_active() {
            return true;
        }
        let Self { registry, server, arcade, .. } = self;
        registry.iter().any(|slot| {
            let ctx = make_ctx(&mut **server, arcade.manager_mut());
            slot.controller().is_busy(&ctx)
        })
    }

    /// `App::execute` (`0x1400025C0`): the main loop.
    fn execute(&mut self) {
        let frequency = self.clock.frequency();
        let period = frequency / if self.frame_high { FRAME_RATE_HIGH } else { FRAME_RATE };
        let mut last_tick = 0_i64;
        let mut sleep_ms = 0_u32;
        let mut quit = false;
        loop {
            if self.terminated() {
                self.crashed = true;
                return;
            }
            if !self.should_continue() || quit {
                return;
            }
            if self.window.pump() == MessagePump::Quit {
                quit = true;
            }
            let now = self.clock.now();
            let due = now.saturating_sub(last_tick) >= period;
            if due {
                self.tick();
                last_tick = now;
                let after = self.clock.now();
                let remaining = span_micros(now.saturating_add(period), after, frequency).unwrap_or_else(|| {
                    tracing::error!(frequency, "the clock span of the frame does not fit in microseconds");
                    0
                });
                sleep_ms = u32::try_from((remaining / 1000).max(1)).unwrap_or(u32::MAX);
            }
            self.clock.sleep(if due { sleep_ms } else { 0 });
        }
    }

    /// One due tick: `Manager::execute`, hub events, `on_libam_events`, 27 × `preExecute`, `Server::execute`,
    /// 27 × `postExecute`.
    fn tick(&mut self) {
        let events = self.arcade.execute();
        {
            let Self { registry, server, arcade, console, .. } = &mut *self;
            let mut hub = Hub { controllers: registry.as_mut_slice(), roots: server.roots_mut(), host: console };
            arcade.dispatch(events, &mut hub);
        }
        if self.terminated() {
            return;
        }
        let Self { registry, server, arcade, .. } = self;
        for slot in registry.iter_mut() {
            let mut ctx = make_ctx(&mut **server, arcade.manager_mut());
            slot.controller_mut().on_libam_events(&mut ctx);
        }
        for index in 0..registry.len() {
            let mut ctx = make_ctx(&mut **server, arcade.manager_mut());
            registry[index].pre_execute(&mut ctx);
            Self::deliver_app_states(registry, &mut **server, arcade);
        }
        server.execute();
        for index in 0..registry.len() {
            let mut ctx = make_ctx(&mut **server, arcade.manager_mut());
            registry[index].post_execute(&mut ctx);
            Self::deliver_app_states(registry, &mut **server, arcade);
        }
    }

    /// Delivers the application-state broadcasts (`0x140006130`) a controller queued to every controller's
    /// `apmCbs` registrant, in registry order, before the next controller runs.
    fn deliver_app_states(registry: &mut [ControllerSlot], server: &mut dyn ServerExchange, arcade: &mut Arcade) {
        for state in arcade.manager_mut().take_app_state_events() {
            for slot in registry.iter_mut() {
                let mut ctx = make_ctx(&mut *server, arcade.manager_mut());
                slot.controller_mut().on_app_state(state, &mut ctx);
            }
        }
    }

    /// The shutdown sequence (`0x140002340` tail, `~App` steps 2-4): `Manager::finalize`, one
    /// `ProcessController::preExecute` (which publishes "not running" and answers a pending Kill request), one
    /// `Server::execute`. It runs at the end of [`Self::run`] and again when the App is dropped.
    fn shutdown(&mut self) {
        self.arcade.finalize();
        let Self { registry, server, arcade, .. } = self;
        let mut ctx = make_ctx(&mut **server, arcade.manager_mut());
        registry[0].pre_execute(&mut ctx);
        server.execute();
    }
}

impl Drop for App {
    /// `App::~App` (`0x140002100`): the shutdown sequence again, then the controllers are destroyed in reverse
    /// order (the Server, Manager and log path follow as fields), and the console is released.
    fn drop(&mut self) {
        if !self.crashed {
            self.shutdown();
        }
        while let Some(slot) = self.registry.pop() {
            drop(slot);
        }
        self.console.release();
    }
}

#[cfg(test)]
mod tests {
    use crate::manager::ManagerState;
    use libam_api::sequence::ProcessState;

    use super::*;
    use crate::mock::{self, Fixture, Trace};
    use libam_api::interface::{CabinetAppConfig, LibamEvent, LoadCabinetTypeEvent};

    fn parser(args: &[&str]) -> ArgumentsParser {
        let mut parser = ArgumentsParser::new();
        let mut argv = vec!["amdaemon.exe"];
        argv.extend_from_slice(args);
        parser.parse(argv, r"C:\amd\amdaemon.exe").expect("valid command line");
        parser
    }

    fn change_state(state: u32) -> LibamEvent {
        LibamEvent::ChangeState(ProcessState(state))
    }

    fn cabinet(model: Option<u8>) -> LibamEvent {
        LibamEvent::LoadCabinetType(Box::new(LoadCabinetTypeEvent {
            model,
            dip_switch: 0,
            region: Some(1),
            app_config: CabinetAppConfig::default(),
        }))
    }

    fn app(fixture: Fixture) -> App {
        App::new(fixture.into_services()).expect("27 controllers in registry order")
    }

    /// The trace split into the lines before the first tick and one chunk per `Manager::execute`; the shutdown
    /// sequence (from `Manager::finalize` on) belongs to no chunk.
    fn ticks(trace: &Trace) -> (Vec<String>, Vec<Vec<String>>) {
        let mut prefix = Vec::new();
        let mut ticks: Vec<Vec<String>> = Vec::new();
        for line in mock::lines(trace) {
            if line == "am.finalize" {
                break;
            }
            if line == "am.execute" {
                ticks.push(Vec::new());
            }
            match ticks.last_mut() {
                Some(tick) => tick.push(line),
                None => prefix.push(line),
            }
        }
        (prefix, ticks)
    }

    fn every(hook: &str) -> Vec<String> {
        ControllerId::ORDER.iter().map(|id| format!("{id:?}:{hook}")).collect()
    }

    fn of(id: ControllerId, lines: &[String]) -> Vec<String> {
        let prefix = format!("{id:?}:");
        lines.iter().filter(|line| line.starts_with(&prefix)).cloned().collect()
    }

    fn count(trace: &Trace, line: &str) -> usize {
        mock::lines(trace).iter().filter(|candidate| *candidate == line).count()
    }

    // ---- the tick ---------------------------------------------------------------------------------------

    #[test]
    fn a_tick_runs_manager_then_pre_then_server_then_post() {
        let mut fixture = Fixture::new();
        fixture.interface.init = Some(Ok(vec![change_state(3)]));
        fixture.interface.script_tick(vec![]);
        fixture.interface.script_tick(vec![change_state(0)]);
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        assert_eq!(app.run(&parser(&[])), 0);

        let (prefix, ticks) = ticks(&trace);
        assert_eq!(
            prefix,
            ["window.create", "am.initialize"],
            "window, then Manager::initialize; the console policy waits for the dongle"
        );
        let mut expected = vec!["am.execute".to_owned()];
        expected.extend(every("libam_events"));
        expected.extend(ControllerId::ORDER.iter().flat_map(|id| [format!("{id:?}:clear"), format!("{id:?}:update")]));
        expected.push("server.execute".into());
        expected.extend(every("accept"));
        expected.push("clock.sleep(1)".into());
        assert_eq!(ticks[0], expected);
    }

    #[test]
    fn loop_runs_while_the_manager_is_in_setup_or_running() {
        let mut fixture = Fixture::new();
        for events in [vec![], vec![], vec![change_state(3)], vec![], vec![change_state(0)]] {
            fixture.interface.script_tick(events);
        }
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        assert_eq!(app.run(&parser(&[])), 0);
        assert_eq!(count(&trace, "am.execute"), 5, "state 1, 1, 2, 2 keep it running, state 0 ends it");
        assert_eq!(app.manager().state(), ManagerState::Idle);
    }

    #[test]
    fn quit_message_bypasses_the_busy_drain() {
        let mut fixture = Fixture::new();
        fixture.interface.init = Some(Ok(vec![change_state(0)]));
        fixture.probe(ControllerId::Sequence).busy.set(1_000);
        fixture.window.pump = [MessagePump::Idle, MessagePump::Dispatched, MessagePump::Quit].into();
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        app.run(&parser(&[]));
        assert_eq!(
            count(&trace, "am.execute"),
            3,
            "three turns pumped, the fourth sees quit despite the busy controller"
        );
    }

    // ---- pacing -----------------------------------------------------------------------------------------

    fn paced(frequency: i64, now: i64, step: i64) -> Fixture {
        let mut fixture = Fixture::new();
        fixture.clock.frequency = frequency;
        fixture.clock.now.set(now);
        fixture.clock.step = step;
        fixture.interface.init = Some(Ok(vec![change_state(3)]));
        fixture
    }

    fn sleeps(trace: &Trace) -> Vec<String> {
        mock::lines(trace).into_iter().filter(|line| line.starts_with("clock.sleep")).collect()
    }

    #[test]
    fn due_ticks_sleep_the_rest_of_the_period_and_idle_turns_yield() {
        // 1 MHz clock: the 66 Hz period is 15151 ticks (= µs). Each clock read advances 5000.
        let mut fixture = paced(1_000_000, 1_000_000, 5_000);
        fixture.window.pump = [MessagePump::Idle, MessagePump::Idle, MessagePump::Idle, MessagePump::Quit].into();
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        app.run(&parser(&[]));
        assert_eq!(count(&trace, "am.execute"), 2, "ticks on turns 1 and 4 only");
        // turn 1: 15151 - 5000 = 10151 µs -> 10 ms; turns 2, 3: not due -> Sleep(0); turn 4: 10 ms again.
        assert_eq!(sleeps(&trace), ["clock.sleep(10)", "clock.sleep(0)", "clock.sleep(0)", "clock.sleep(10)"]);
    }

    #[test]
    fn first_turn_is_due_and_a_tick_that_overran_its_period_sleeps_one_millisecond() {
        let mut fixture = paced(1_000_000, 1_000_000, 100_000);
        fixture.interface.script_tick(vec![change_state(0)]);
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        app.run(&parser(&[]));
        assert_eq!(sleeps(&trace), ["clock.sleep(1)"]);
    }

    #[test]
    fn frame_option_switches_to_120_hz() {
        let ticks_at = |args: &[&str]| {
            let mut fixture = paced(1_000_000, 1_000_000, 5_000);
            fixture.window.pump = [MessagePump::Idle, MessagePump::Idle, MessagePump::Quit].into();
            let trace = fixture.trace.clone();
            let mut app = app(fixture);
            app.run(&parser(args));
            drop(app);
            trace
        };
        let normal = ticks_at(&[]);
        assert_eq!(count(&normal, "am.execute"), 1, "66 Hz: period 15151 ticks, only the first turn is due");

        let high = ticks_at(&["-f"]);
        assert_eq!(count(&high, "am.execute"), 3, "120 Hz: period 8333 ticks, every second read is due");
    }

    #[test]
    fn span_micros_scales_and_truncates_like_clock_calc_span() {
        assert_eq!(span_micros(1_050_000, 1_000_000, 10_000_000), Some(5_000));
        assert_eq!(span_micros(1_000_000, 1_050_000, 10_000_000), Some(-5_000));
        assert_eq!(span_micros(7, 0, 3), Some(2_333_333), "7 / 3 s = 2333333.33 µs, truncated");
        assert_eq!(span_micros(-7, 0, 3), Some(-2_333_333), "truncation is toward zero");
        assert_eq!(span_micros(i64::MIN, 1, 1000), None, "the checked subtraction overflows");
        assert_eq!(span_micros(i64::MAX, 0, 1), None, "the scaled value does not fit");
    }

    // ---- ready edges and the accept gate ----------------------------------------------------------------

    #[test]
    fn controllers_clear_then_update_on_the_ready_edge_and_update_once_more_when_it_falls() {
        let mut fixture = Fixture::new();
        for events in [vec![], vec![change_state(3)], vec![], vec![change_state(0)]] {
            fixture.interface.script_tick(events);
        }
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        app.run(&parser(&[]));
        let (_, ticks) = ticks(&trace);
        assert_eq!(ticks.len(), 4);

        let hooks = |tick: usize, id| -> Vec<String> {
            of(id, &ticks[tick])
                .into_iter()
                .filter(|line| !line.ends_with("libam_events") && !line.ends_with("is_busy"))
                .collect()
        };
        let common = ControllerId::Common;
        assert!(hooks(0, common).is_empty(), "state 1: not ready, nothing published");
        assert_eq!(hooks(1, common), ["Common:clear", "Common:update", "Common:accept"], "rising edge");
        assert_eq!(hooks(2, common), ["Common:update", "Common:accept"], "steady");
        assert_eq!(hooks(3, common), ["Common:update"], "falling edge: one final update, no accept");

        let process = ControllerId::Process;
        assert_eq!(hooks(0, process), ["Process:clear", "Process:update", "Process:accept"], "Process is always ready");
        assert_eq!(hooks(3, process), ["Process:update", "Process:accept"]);
    }

    #[test]
    fn accept_edge_waits_for_the_client_to_echo_the_boot_clock() {
        let mut fixture = Fixture::new();
        fixture.interface.init = Some(Ok(vec![change_state(3)]));
        fixture.probe(ControllerId::Process).boot_clock.set(Some(42));
        fixture.server.echo_from_execute = 2;
        fixture.interface.script_tick(vec![]);
        fixture.interface.script_tick(vec![]);
        fixture.interface.script_tick(vec![change_state(0)]);
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        app.run(&parser(&[]));
        let (_, ticks) = ticks(&trace);
        let accepts = |tick: usize| ticks[tick].iter().filter(|line| line.ends_with(":accept")).count();
        assert_eq!(accepts(0), 0, "ServerData.bootClock (42) != ClientData echo (0)");
        assert_eq!(accepts(1), 27, "the echo arrived with the second server exchange");
        assert_eq!(accepts(2), 1, "only Process is still ready after the state fell to 0");
    }

    #[test]
    fn app_state_broadcasts_reach_every_controller_before_the_next_controller_runs() {
        let mut fixture = Fixture::new();
        fixture.interface.init = Some(Ok(vec![change_state(3)]));
        fixture.interface.script_tick(vec![]);
        fixture.interface.script_tick(vec![change_state(0)]);
        fixture.probe(ControllerId::Common).emit_app_state.set(Some(3));
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        app.run(&parser(&[]));
        let (_, ticks) = ticks(&trace);
        let tick = &ticks[0];
        let common = tick.iter().position(|line| line == "Common:update").expect("Common updated");
        assert_eq!(tick[common + 1..common + 28], every("app_state(3)"));
        assert_eq!(tick[common + 28], "SystemInfo:clear");
    }

    // ---- shutdown ---------------------------------------------------------------------------------------

    #[test]
    fn shutdown_runs_at_the_end_of_run_and_again_on_drop_each_time_answering_through_process() {
        let mut fixture = Fixture::new();
        fixture.interface.init = Some(Ok(vec![change_state(3)]));
        fixture.interface.script_tick(vec![]);
        fixture.interface.script_tick(vec![change_state(0)]);
        let process = fixture.probe(ControllerId::Process).clone();
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        assert_eq!(app.run(&parser(&[])), 0);

        let all = mock::lines(&trace);
        let end_of_run = all.iter().position(|line| line == "am.finalize").expect("Manager::finalize ran");
        let tail: Vec<&str> = all[end_of_run..].iter().map(String::as_str).collect();
        assert_eq!(
            tail,
            ["am.finalize", "Process:update", "server.execute"],
            "Manager::finalize, ProcessController::preExecute, Server::execute"
        );
        assert_eq!(app.manager().state(), ManagerState::Idle);

        let before_drop = all.len();
        drop(app);
        let after: Vec<String> = mock::lines(&trace)[before_drop..].to_vec();
        assert_eq!(
            &after[..3],
            ["am.finalize", "Process:update", "server.execute"],
            "~App repeats the shutdown sequence"
        );
        assert_eq!(count(&trace, "am.finalize"), 2);
        assert_eq!(
            *process.states.borrow(),
            [ManagerState::Running, ManagerState::Idle, ManagerState::Idle, ManagerState::Idle],
            "tick 1, tick 2 (state 0), shutdown, destructor: Process publishes 'not running' to answer Kill"
        );
    }

    #[test]
    fn kill_with_a_busy_controller_drains_before_exiting() {
        let mut fixture = Fixture::new();
        fixture.interface.init = Some(Ok(vec![change_state(3)]));
        fixture.probe(ControllerId::Process).kill_on_accept.set(true);
        fixture.probe(ControllerId::Network).busy.set(2);
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        app.run(&parser(&[]));
        assert_eq!(count(&trace, "am.execute"), 3, "Terminating keeps ticking while Network reports busy");
    }

    // ---- initialisation failures and the console policy -------------------------------------------------

    #[test]
    fn window_creation_failure_stops_before_any_other_initialisation_and_drop_still_cleans_up() {
        let mut fixture = Fixture::new();
        fixture.window.creatable = false;
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        assert_eq!(app.run(&parser(&["-w", "show", "-f"])), 1);
        assert_eq!(mock::lines(&trace), ["window.create"], "no console, no Manager::initialize, no timer");
        drop(app);
        let after = mock::lines(&trace);
        assert_eq!(
            &after[1..5],
            ["am.finalize", "Process:clear", "Process:update", "server.execute"],
            "the first Process::preExecute of the process is a rising edge: clear, then publish 'not running'"
        );
    }

    #[test]
    fn manager_initialize_failure_skips_the_loop() {
        let mut fixture = Fixture::new();
        fixture.interface.init = Some(Err(libam_api::result::AmError::code(2)));
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        assert_eq!(app.run(&parser(&[])), 1);
        assert_eq!(count(&trace, "am.execute"), 0);
        assert_eq!(count(&trace, "am.finalize"), 0, "the shutdown sequence is left to the destructor");
        drop(app);
        assert_eq!(count(&trace, "am.finalize"), 1);
    }

    const TITLE: &str = "console.allocate(\"amdaemonExe Ver.91f83d14 Build:May 20 2024 11:28:48\\n\")";

    #[test]
    fn explicit_show_allocates_the_console_first_and_ignores_the_dongle() {
        let mut fixture = Fixture::new();
        fixture.interface.develop = Ok(false);
        fixture.interface.script_tick(vec![cabinet(Some(1)), change_state(0)]);
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        app.run(&parser(&["-w", "show"]));
        let (prefix, _) = ticks(&trace);
        assert_eq!(prefix, ["window.create", TITLE, "am.initialize"]);
        assert_eq!(count(&trace, TITLE), 1);
        drop(app);
        assert_eq!(mock::lines(&trace).last().map(String::as_str), Some("console.free"));
    }

    #[test]
    fn explicit_hide_wins_over_a_developer_dongle() {
        let mut fixture = Fixture::new();
        fixture.interface.develop = Ok(true);
        fixture.interface.script_tick(vec![cabinet(Some(1)), change_state(0)]);
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        app.run(&parser(&["-w", "hide"]));
        drop(app);
        assert!(mock::lines(&trace).iter().all(|line| !line.starts_with("console.allocate") && line != "console.free"));
    }

    #[test]
    fn without_the_window_option_a_developer_dongle_opens_the_console_at_the_cabinet_event() {
        let mut fixture = Fixture::new();
        fixture.interface.develop = Ok(true);
        fixture.interface.script_tick(vec![cabinet(Some(1)), change_state(0)]);
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        app.run(&parser(&[]));
        let all = mock::lines(&trace);
        let alloc = all.iter().position(|line| line == TITLE).expect("console allocated");
        let initialize = all.iter().position(|line| line == "am.initialize").unwrap();
        let load = all.iter().position(|line| line == "loader.load").unwrap();
        assert!(initialize < alloc && alloc < load, "the pre-load hook fires at the start of the callback");
    }

    #[test]
    fn without_the_window_option_a_normal_dongle_leaves_the_console_closed() {
        let mut fixture = Fixture::new();
        fixture.interface.develop = Ok(false);
        fixture.interface.script_tick(vec![cabinet(Some(1)), change_state(0)]);
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        app.run(&parser(&[]));
        drop(app);
        assert!(mock::lines(&trace).iter().all(|line| !line.contains("console")));
    }

    // ---- fatal exceptions -------------------------------------------------------------------------------

    #[test]
    fn fatal_exception_ends_the_process_without_the_shutdown_sequence() {
        let mut fixture = Fixture::new();
        fixture.interface.script_tick(vec![cabinet(None)]);
        let trace = fixture.trace.clone();
        let mut app = app(fixture);
        assert_eq!(app.run(&parser(&[])), 1);
        let (_, ticks) = ticks(&trace);
        assert_eq!(ticks.len(), 1);
        assert!(
            ticks[0].iter().all(|line| !line.ends_with(":update")),
            "the rest of the tick never ran: {:?}",
            ticks[0]
        );
        drop(app);
        assert_eq!(count(&trace, "am.finalize"), 0, "__debugbreak ends the process: nothing runs afterwards");
    }
}

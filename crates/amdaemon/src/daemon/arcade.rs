//! `amdaemon::arcade::Manager` lifecycle and the `arcade::EventListener` hub around
//! [`crate::manager::Manager`].
//!
//! Spec: `spec/controllers/lifecycle.md` §M3 (`initialize`/`finalize`/`execute`), §M4.3 (`onChangeState`),
//! §M5/§M7.2 (listener sinks and their registrants), §M8 (helpers) and §9 (cabinet-type callback
//! `0x140005B40`).
//!
//! libam is a port ([`libam_api::interface`]). Its callbacks arrive as [`LibamEvent`]s and are routed here in
//! emission order; the hub vectors of the original (`configCbs`, `appConfigCbs`, `jvsDetectCbs`, …) are the
//! controllers' `on_*` hooks called in registry order, which equals the recovered registration order. libam's
//! own diagnostics are `tracing` events and do not pass through here.

use crate::controllers::controller::{Controller, ControllerSlot, Ctx};
use crate::ipc::server::RootsMut;
use crate::manager::Manager;

use crate::daemon::args::ArgumentsParser;
use crate::daemon::log::LogPath;
use crate::daemon::paths;
use libam_api::interface::{
    AmInterface, CabinetAppConfig, ConfigLoadRequest, ConfigLoader, LibamEvent, LibamwConfig, LoadCabinetTypeEvent,
};

/// `wchar_t logDir[128]` (`wcscpy_s(cfg+0, 128, …)` in `0x140212730`).
const LOG_DIR_CAPACITY: usize = 128;
/// `wchar_t appName[32]` / `appVersion[32]` (`wcsnlen(p, 32) >= 32` fails).
const NAME_CAPACITY: usize = 32;
/// Maximum length of the ABaaS log directory in `wchar_t` units (`0x1406A0328`).
const ABAAS_ROOT_LIMIT: usize = 200;

/// `Manager::initialize` returned `Status` 1; the reason was logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("arcade::Manager::initialize failed")]
pub struct InitializeFailed;

/// Services the hub calls back into the App while routing an event.
pub trait HubHost: std::fmt::Debug {
    /// `preLoadHook` target (`App` lambda `0x140003C90`): `App::applyConsole(show)`.
    fn apply_console(&mut self, show: bool);
}

/// Everything the `EventListener` sinks reach: the controllers (in registry order), the IPC roots and the App.
#[derive(Debug)]
pub struct Hub<'a> {
    /// The 27 controllers in registry order.
    pub controllers: &'a mut [ControllerSlot],
    /// Live `ServerData`/`ClientData`/ServerIO roots.
    pub roots: RootsMut<'a>,
    /// App callbacks.
    pub host: &'a mut dyn HubHost,
}

/// `arcade::Manager` (`App+0x48`) with its libam port and the `EventListener` state (`Manager+0x1C0`).
#[derive(Debug)]
pub struct Arcade {
    interface: Box<dyn AmInterface>,
    loader: Box<dyn ConfigLoader>,
    manager: Manager,
    /// `Manager+0x00 owner`.
    log: LogPath,
    /// `EventListener+0x40` (`argsC`).
    config_paths: Vec<String>,
    /// `EventListener+0x58` (`exeDir`).
    exe_dir: String,
    /// `EventListener+0x78` (`argM`).
    merged_dump_path: String,
    /// `preLoadHook` is set: apply the console policy from the dongle at the start of every config callback.
    console_from_dongle: bool,
}

impl Arcade {
    /// Constructor `0x14000A020`: idle Manager; `log` is the log destination the App root paths move.
    pub fn new(interface: Box<dyn AmInterface>, loader: Box<dyn ConfigLoader>, log: LogPath) -> Self {
        Self {
            interface,
            loader,
            manager: Manager::new(),
            log,
            config_paths: Vec::new(),
            exe_dir: String::new(),
            merged_dump_path: String::new(),
            console_from_dongle: false,
        }
    }

    /// The Manager runtime state shared with the controllers.
    pub fn manager(&self) -> &Manager {
        &self.manager
    }

    /// The Manager runtime state, mutably.
    pub fn manager_mut(&mut self) -> &mut Manager {
        &mut self.manager
    }

    /// The log path object (`Manager+0x00 owner`).
    pub fn log(&self) -> &LogPath {
        &self.log
    }

    /// Registers the `preLoadHook` (`0x140005340`, from `App::initialize` @`0x140002476`): at the start of every
    /// cabinet-type callback the console policy follows `Manager::isDevelop()`.
    pub fn set_console_from_dongle(&mut self) {
        self.console_from_dongle = true;
    }

    /// `Manager::initialize` (`0x14000A2C0`). Failures are logged and yield [`InitializeFailed`]; `hub`
    /// receives the callbacks libam fires synchronously (`ChangeState(1)`).
    pub fn initialize(
        &mut self,
        args: &ArgumentsParser,
        app_name: &str,
        app_version: &str,
        hub: &mut Hub<'_>,
    ) -> Result<(), InitializeFailed> {
        self.config_paths = args.config_paths().to_vec();
        self.exe_dir = args.exe_dir().to_owned();
        self.merged_dump_path = args.merged_dump_path().to_owned();

        let mut config = LibamwConfig::default();
        if !args.log_path().is_empty() {
            let mut dir = paths::parent_dir(args.log_path()).to_owned();
            paths::ensure_trailing_separator(&mut dir);
            if paths::utf16_len(&dir) >= LOG_DIR_CAPACITY {
                tracing::error!(dir, "the directory of the -l log path is too long for libam");
                return Err(InitializeFailed);
            }
            config.log_dir = dir;
        }
        if paths::utf16_len(app_name) >= NAME_CAPACITY {
            tracing::warn!(app_name, "the application name is too long for libam");
            return Err(InitializeFailed);
        }
        config.app_name = app_name.to_owned();
        if paths::utf16_len(app_version) >= NAME_CAPACITY {
            tracing::warn!(app_version, "the application version is too long for libam");
            return Err(InitializeFailed);
        }
        config.app_version = app_version.to_owned();

        match self.interface.initialize(&config) {
            Ok(events) => self.dispatch(events, hub),
            Err(error) => {
                tracing::error!(code = error.code, "libam initialisation failed");
                return Err(InitializeFailed);
            }
        }
        Ok(())
    }

    /// `Manager::execute` (`0x14000A6C0`): one libam step; the callbacks it fired, in emission order. Hand them
    /// to [`Self::dispatch`].
    pub fn execute(&mut self) -> Vec<LibamEvent> {
        self.interface.execute()
    }

    /// `Manager::finalize` (`0x14000A690`): shuts libam down (idempotent) and sets the state to idle; the
    /// `testMode` and `debugDump` flags are kept.
    pub fn finalize(&mut self) {
        self.interface.finalize();
        self.manager.mark_finalized();
    }

    /// Routes libam callbacks in order to the Manager and the controller hooks.
    pub fn dispatch(&mut self, events: Vec<LibamEvent>, hub: &mut Hub<'_>) {
        for event in events {
            self.deliver(event, hub);
        }
    }

    fn deliver(&mut self, event: LibamEvent, hub: &mut Hub<'_>) {
        match event {
            LibamEvent::ChangeState(state) => self.manager.on_change_state(state),
            LibamEvent::LoadCabinetType(event) => {
                let (accepted, app_config) = self.load_cabinet_type(*event, hub);
                self.interface.load_cabinet_type_result(accepted, app_config);
            }
            LibamEvent::FatalError(error) => {
                Self::broadcast(&mut self.manager, hub, |controller, ctx| controller.on_fatal_error(&error, ctx));
            }
            LibamEvent::NeedToExit(reason) => {
                if reason < 3 {
                    Self::broadcast(&mut self.manager, hub, |controller, ctx| controller.on_need_to_exit(reason, ctx));
                }
            }
            LibamEvent::JvsDetectNode => {
                Self::broadcast(&mut self.manager, hub, |controller, ctx| controller.on_jvs_detect_node(ctx));
            }
            LibamEvent::UsbIoDetectNode => {
                Self::broadcast(&mut self.manager, hub, |controller, ctx| controller.on_usb_io_detect_node(ctx));
            }
            LibamEvent::DisplayCompleteTask => {
                Self::broadcast(&mut self.manager, hub, |controller, ctx| controller.on_display_complete(ctx));
            }
            LibamEvent::FixAppRootFolderPath(path) => {
                if let Some(path) = path {
                    self.log.set_app_root(&path);
                }
            }
        }
    }

    /// Calls `call` for every controller in registry order with a fresh context.
    fn broadcast(manager: &mut Manager, hub: &mut Hub<'_>, mut call: impl FnMut(&mut dyn Controller, &mut Ctx<'_>)) {
        for slot in hub.controllers.iter_mut() {
            let mut ctx = Ctx {
                server: &mut *hub.roots.server,
                client: hub.roots.client,
                server_io: &mut *hub.roots.server_io,
                manager: &mut *manager,
            };
            call(slot.controller_mut(), &mut ctx);
        }
    }

    /// `Manager::isDevelop` (`0x14000A7F0`).
    fn is_develop(&mut self) -> bool {
        match self.interface.is_develop() {
            Ok(develop) => develop,
            Err(_) => {
                tracing::warn!("libam is not ready to tell whether the dongle is a developer dongle");
                false
            }
        }
    }

    /// `Manager::generateAppRootPath` (`0x14000A8F0`): the libam-resolved app root, empty until resolved.
    fn generate_app_root_path(&mut self) -> String {
        match self.interface.app_root_path() {
            Ok(path) => path.unwrap_or_default(),
            Err(_) => {
                tracing::warn!("libam is not ready to resolve the application root folder");
                String::new()
            }
        }
    }

    /// `Manager::setAbaasLogRootPath` (`0x14000ACB0`): `false` vetoes the configuration.
    fn set_abaas_log_root_path(&mut self, relative: &str) -> bool {
        let mut path = self.generate_app_root_path();
        paths::join(&mut path, relative);
        paths::ensure_trailing_separator(&mut path);
        if paths::utf16_len(&path) > ABAAS_ROOT_LIMIT {
            tracing::error!(path, "abaas_log.root_path is too long");
            return false;
        }
        if std::fs::metadata(&path).is_err()
            && let Err(error) = std::fs::create_dir_all(&path)
        {
            tracing::error!(path, %error, "creating the ABaaS log directory failed");
        }
        self.log.set_abaas_root(&path);
        true
    }

    /// `LoadCabinetTypeEventListener::operator()` (`0x140005B40`), steps numbered as in spec §M6.2/§9. Returns
    /// the callback's `bool` and the `AppConfig` it filled.
    fn load_cabinet_type(&mut self, event: LoadCabinetTypeEvent, hub: &mut Hub<'_>) -> (bool, CabinetAppConfig) {
        let LoadCabinetTypeEvent { model, dip_switch, region, mut app_config } = event;

        // 1. preLoadHook: App::applyConsole(Manager::isDevelop()).
        if self.console_from_dongle {
            let develop = self.is_develop();
            hub.host.apply_console(develop);
        }

        // 2-3. `new Config` + anon::load (0x140006230).
        let Some(model) = model else {
            self.manager.terminate("libam reported a cabinet without a model type");
            return (false, app_config);
        };
        let request = ConfigLoadRequest {
            dip_switch,
            model,
            region: region.filter(|region| (1..=3).contains(region)).unwrap_or(0),
            config_paths: &self.config_paths,
            exe_dir: &self.exe_dir,
            merged_dump_path: &self.merged_dump_path,
        };
        let Some(mut loaded) = self.loader.load(&request) else {
            return (false, app_config);
        };

        // 4. Config[0x4FF8C] = max(Config[0x4FF8C], Config[0x4FF98], Config[0x4FF9D]).
        let bytes = &mut loaded.bytes;
        bytes.byte_4ff8c = bytes.byte_4ff8c.max(bytes.byte_4ff98).max(bytes.byte_4ff9d);

        // 5. Config -> AppConfig conversion (0x140006380).
        if !self.loader.convert(&loaded, &self.exe_dir, &mut app_config) {
            return (false, app_config);
        }

        // 6. configCbs: the Manager's own debugDump copy first, then the registrants in registry order.
        self.manager.set_debug_dump(loaded.process.debug_dump);
        Self::broadcast(&mut self.manager, hub, |controller, ctx| controller.on_process_config(&loaded.process, ctx));

        // 7. ALL.Net Air must stay disabled.
        if loaded.bytes.air_enabled != 0 {
            tracing::error!("ALL.Net Air is enabled in the configuration; it must be disabled");
            return (false, app_config);
        }

        // 8. Display fields and sink.
        let bytes = &loaded.bytes;
        app_config.display_flag = bytes.display_flag_507e0 != 0 || bytes.display_flag_50b14 != 0;
        app_config.display_value = bytes.display_value_50974.max(bytes.display_value_50ca8);
        app_config.sinks.display_complete = true;

        // 9. abaasRootCb: the optional abaas_log.root_path may veto the configuration.
        if let Some(relative) = app_config.abaas_log_root.as_deref()
            && !self.set_abaas_log_root_path(relative)
        {
            return (false, app_config);
        }

        // 10. NeedToExit, Jvs and UsbIO sinks.
        app_config.sinks.need_to_exit = true;
        app_config.sinks.jvs_detect_node = true;
        app_config.sinks.usb_io_detect_node = true;

        // 11. appConfigCbs.
        Self::broadcast(&mut self.manager, hub, |controller, ctx| {
            controller.on_app_config(&mut app_config.controllers, ctx);
        });
        (true, app_config)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    use crate::controllers::controller::ControllerId;
    use crate::manager::ManagerState;
    use libam_api::sequence::ProcessState;

    use super::*;
    use crate::daemon::app::ServerExchange;
    use crate::mock::{self, Fixture, InterfaceHandles, ProbeHandle, RecordingHost, Trace};
    use libam_api::interface::{ConfigBytes, ListenerSinks, LoadedConfig};

    struct Rig {
        arcade: Arcade,
        slots: Vec<ControllerSlot>,
        server: mock::FakeServer,
        host: RecordingHost,
        trace: Trace,
        handles: InterfaceHandles,
        requests: Rc<RefCell<Vec<String>>>,
        probes: Vec<ProbeHandle>,
    }

    impl Rig {
        fn new(fixture: Fixture) -> Self {
            let slots = fixture.controllers().into_iter().map(ControllerSlot::new).collect();
            Self {
                handles: fixture.interface.handles.clone(),
                requests: Rc::clone(&fixture.loader.requests),
                host: RecordingHost::new(&fixture.trace),
                arcade: Arcade::new(Box::new(fixture.interface), Box::new(fixture.loader), fixture.log),
                slots,
                server: fixture.server,
                trace: fixture.trace,
                probes: fixture.probes,
            }
        }

        fn dispatch(&mut self, events: Vec<LibamEvent>) {
            let mut hub = Hub { controllers: &mut self.slots, roots: self.server.roots_mut(), host: &mut self.host };
            self.arcade.dispatch(events, &mut hub);
        }

        fn initialize(&mut self, args: &ArgumentsParser, name: &str, version: &str) -> Result<(), InitializeFailed> {
            let mut hub = Hub { controllers: &mut self.slots, roots: self.server.roots_mut(), host: &mut self.host };
            self.arcade.initialize(args, name, version, &mut hub)
        }

        /// Trace lines `<Id>:<hook>` (or `<Id>:<hook>(…)`) in order.
        fn hook(&self, hook: &str) -> Vec<String> {
            mock::lines(&self.trace)
                .into_iter()
                .filter(|line| {
                    line.split_once(':').is_some_and(|(_, rest)| rest == hook || rest.starts_with(&format!("{hook}(")))
                })
                .collect()
        }
    }

    fn every_controller(hook: &str) -> Vec<String> {
        ControllerId::ORDER.iter().map(|id| format!("{id:?}:{hook}")).collect()
    }

    fn parser(args: &[&str]) -> ArgumentsParser {
        let mut parser = ArgumentsParser::new();
        let mut argv = vec!["amdaemon.exe"];
        argv.extend_from_slice(args);
        parser.parse(argv, r"C:\amd\amdaemon.exe").expect("valid command line");
        parser
    }

    fn cabinet(model: Option<u8>, region: Option<u32>) -> LibamEvent {
        LibamEvent::LoadCabinetType(Box::new(LoadCabinetTypeEvent {
            model,
            dip_switch: 0x1234,
            region,
            app_config: CabinetAppConfig::default(),
        }))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("amdaemon-daemon-arcade-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    // ---- initialize -------------------------------------------------------------------------------------

    #[test]
    fn initialize_hands_libamw_the_log_directory_name_and_version_and_applies_the_synchronous_state() {
        let mut rig = Rig::new(Fixture::new());
        let args = parser(&["-l", r"D:\amd\log\a.log", "-c", "a.json", "-m", "dump.json"]);
        rig.initialize(&args, "amdaemonExe", "Ver.91f83d14").unwrap();

        let config = rig.handles.libamw.borrow().clone().expect("am::Interface::initialize was called");
        assert_eq!(
            config,
            LibamwConfig {
                log_dir: r"D:\amd\log\".into(),
                app_name: "amdaemonExe".into(),
                app_version: "Ver.91f83d14".into()
            }
        );
        assert_eq!(rig.arcade.manager().state(), ManagerState::Setup, "ChangeState(1) fired inside initialize");
    }

    #[test]
    fn log_directory_follows_the_parent_directory_rules() {
        for (log_path, expected) in
            [("a.log", ".\\"), (r"C:\a.log", ".\\"), (r"D:\x\\y.log", r"D:\x\"), ("/var/log/a.log", "/var/log\\")]
        {
            let mut rig = Rig::new(Fixture::new());
            rig.initialize(&parser(&["-l", log_path]), "n", "v").unwrap();
            assert_eq!(rig.handles.libamw.borrow().clone().unwrap().log_dir, expected, "{log_path}");
        }
        let mut rig = Rig::new(Fixture::new());
        rig.initialize(&parser(&[]), "n", "v").unwrap();
        assert_eq!(
            rig.handles.libamw.borrow().clone().unwrap().log_dir,
            "",
            "without -l libamw keeps its empty log dir"
        );
    }

    #[test]
    fn log_directory_of_128_wide_characters_is_rejected() {
        let long_ok = format!(r"D:\{}\a.log", "d".repeat(122)); // `D:\` + 122 + `\` = 126 + ... fits below
        let mut rig = Rig::new(Fixture::new());
        rig.initialize(&parser(&["-l", &long_ok]), "n", "v").unwrap();

        let dir_127 = format!(r"D:\{}\", "d".repeat(123)); // 3 + 123 + 1 = 127 units
        assert_eq!(dir_127.len(), 127);
        let mut rig = Rig::new(Fixture::new());
        rig.initialize(&parser(&["-l", &format!("{dir_127}a.log")]), "n", "v").unwrap();
        assert_eq!(rig.handles.libamw.borrow().clone().unwrap().log_dir, dir_127);

        let dir_128 = format!(r"D:\{}\", "d".repeat(124));
        let mut rig = Rig::new(Fixture::new());
        let result = rig.initialize(&parser(&["-l", &format!("{dir_128}a.log")]), "n", "v");
        assert_eq!(result, Err(InitializeFailed));
        assert!(rig.handles.libamw.borrow().is_none(), "libam is never initialised");
    }

    // ---- finalize and state -----------------------------------------------------------------------------

    #[test]
    fn finalize_shuts_libam_down_and_idles_the_manager_but_keeps_the_flags() {
        let mut rig = Rig::new(Fixture::new());
        rig.dispatch(vec![LibamEvent::ChangeState(ProcessState(5))]);
        assert_eq!(rig.arcade.manager().state(), ManagerState::Running);
        assert!(rig.arcade.manager().test_mode());
        rig.arcade.finalize();
        assert_eq!(rig.arcade.manager().state(), ManagerState::Idle);
        assert!(rig.arcade.manager().test_mode(), "testMode is not cleared by finalize");
        assert!(mock::lines(&rig.trace).contains(&"am.finalize".to_owned()));
    }

    // ---- hub routing ------------------------------------------------------------------------------------

    #[test]
    fn controller_events_reach_every_controller_in_registry_order() {
        let mut rig = Rig::new(Fixture::new());
        rig.dispatch(vec![LibamEvent::JvsDetectNode]);
        assert_eq!(rig.hook("jvs_detect_node"), every_controller("jvs_detect_node"));
        rig.dispatch(vec![LibamEvent::UsbIoDetectNode]);
        assert_eq!(rig.hook("usb_io_detect_node"), every_controller("usb_io_detect_node"));
        rig.dispatch(vec![LibamEvent::DisplayCompleteTask]);
        assert_eq!(rig.hook("display_complete"), every_controller("display_complete"));
        rig.dispatch(vec![LibamEvent::FatalError(Box::default())]);
        assert_eq!(rig.hook("fatal_error"), every_controller("fatal_error"));
    }

    #[test]
    fn need_to_exit_is_routed_only_for_reasons_below_three() {
        let mut rig = Rig::new(Fixture::new());
        rig.dispatch(vec![LibamEvent::NeedToExit(3), LibamEvent::NeedToExit(u32::MAX)]);
        assert!(rig.hook("need_to_exit").is_empty());
        for reason in 0..3 {
            rig.dispatch(vec![LibamEvent::NeedToExit(reason)]);
        }
        let expected: Vec<String> = (0..3)
            .flat_map(|reason| ControllerId::ORDER.iter().map(move |id| format!("{id:?}:need_to_exit({reason})")))
            .collect();
        assert_eq!(rig.hook("need_to_exit"), expected);
    }

    #[test]
    fn app_root_folder_event_updates_the_log_path_at_priority_one() {
        let mut rig = Rig::new(Fixture::new());
        assert_eq!(rig.arcade.log().priority(), 0);
        rig.dispatch(vec![LibamEvent::FixAppRootFolderPath(None)]);
        assert_eq!(rig.arcade.log().priority(), 0, "a null path is ignored");

        rig.dispatch(vec![LibamEvent::FixAppRootFolderPath(Some("/tmp/amd_root".into()))]);
        assert_eq!(rig.arcade.log().priority(), 1);
        assert!(rig.arcade.log().path().contains("amd_root"));
    }

    // ---- cabinet-type callback --------------------------------------------------------------------------

    fn loaded(bytes: ConfigBytes, debug_dump: bool) -> LoadedConfig {
        let mut loaded = LoadedConfig { bytes, ..LoadedConfig::default() };
        loaded.process.debug_dump = debug_dump;
        loaded
    }

    fn cabinet_trace(rig: &Rig) -> Vec<String> {
        mock::lines(&rig.trace)
            .into_iter()
            .filter(|line| {
                line.starts_with("host.")
                    || line.starts_with("loader.")
                    || line.starts_with("am.cabinet")
                    || line.ends_with(":process_config")
                    || line.ends_with(":app_config")
            })
            .collect()
    }

    #[test]
    fn config_callback_runs_the_eleven_steps_in_order() {
        let mut fixture = Fixture::new();
        fixture.loader.loaded = Some(loaded(
            ConfigBytes {
                byte_4ff8c: 3,
                byte_4ff98: 9,
                byte_4ff9d: 5,
                air_enabled: 0,
                display_flag_507e0: 0,
                display_flag_50b14: 1,
                display_value_50974: 7,
                display_value_50ca8: 11,
            },
            true,
        ));
        fixture.interface.develop = Ok(true);
        let mut rig = Rig::new(fixture);
        rig.arcade.set_console_from_dongle();
        rig.initialize(&parser(&["-c", "a.json", "b.json", "-m", "dump.json"]), "n", "v").unwrap();
        rig.dispatch(vec![cabinet(Some(2), Some(3))]);

        let mut expected =
            vec!["host.apply_console(true)".to_owned(), "loader.load".into(), "loader.convert(4ff8c=9)".into()];
        expected.extend(every_controller("process_config"));
        expected.extend(every_controller("app_config"));
        expected.push("am.cabinet_result(true)".into());
        assert_eq!(cabinet_trace(&rig), expected);

        assert!(rig.arcade.manager().debug_dump(), "the Manager's own config callback ran first");
        let seen: Vec<bool> = rig.probes.iter().flat_map(|probe| probe.debug_dump_seen.borrow().clone()).collect();
        assert_eq!(seen, [true; 27], "debugDump is set before any registrant sees the config");

        let results = rig.handles.cabinet_results.borrow();
        let (accepted, config) = &results[0];
        assert!(*accepted);
        assert!(config.display_flag);
        assert_eq!(config.display_value, 11);
        assert_eq!(
            config.sinks,
            ListenerSinks {
                display_complete: true,
                need_to_exit: true,
                jvs_detect_node: true,
                usb_io_detect_node: true
            }
        );
    }

    #[test]
    fn loader_request_carries_cabinet_identity_and_command_line_copies() {
        let mut rig = Rig::new(Fixture::new());
        rig.initialize(&parser(&["-c", "a.json", "b.json", "-m", "dump.json"]), "n", "v").unwrap();
        rig.dispatch(vec![
            cabinet(Some(2), Some(3)),
            cabinet(Some(2), Some(9)),
            cabinet(Some(2), None),
            cabinet(Some(2), Some(1)),
        ]);
        let exe_dir = r"C:\amd\";
        let expected: Vec<String> = [3, 0, 0, 1]
            .iter()
            .map(|region| {
                format!("dip=4660 model=2 region={region} c=[\"a.json\", \"b.json\"] exe={exe_dir} m=dump.json")
            })
            .collect();
        assert_eq!(*rig.requests.borrow(), expected, "regions other than 1, 2, 3 are passed as 0");
    }

    #[test]
    fn conversion_failure_returns_false_before_the_config_callbacks() {
        let mut fixture = Fixture::new();
        fixture.loader.convert_ok = false;
        let mut rig = Rig::new(fixture);
        rig.dispatch(vec![cabinet(Some(1), Some(1))]);
        assert_eq!(cabinet_trace(&rig), ["loader.load", "loader.convert(4ff8c=0)", "am.cabinet_result(false)"]);
        assert!(!rig.arcade.manager().debug_dump());
    }

    #[test]
    fn enabled_air_runs_the_config_callbacks_then_rejects_without_installing_sinks() {
        let mut fixture = Fixture::new();
        fixture.loader.loaded = Some(loaded(
            ConfigBytes { air_enabled: 1, display_flag_507e0: 1, display_value_50974: 5, ..ConfigBytes::default() },
            true,
        ));
        let mut rig = Rig::new(fixture);
        rig.dispatch(vec![cabinet(Some(1), Some(1))]);

        let mut expected = vec!["loader.load".to_owned(), "loader.convert(4ff8c=0)".into()];
        expected.extend(every_controller("process_config"));
        expected.push("am.cabinet_result(false)".into());
        assert_eq!(cabinet_trace(&rig), expected, "configCbs run before the Air check; appConfigCbs never run");
        let results = rig.handles.cabinet_results.borrow();
        assert_eq!(results[0].1.sinks, ListenerSinks::default());
        assert!(!results[0].1.display_flag && results[0].1.display_value == 0, "step 8 is skipped");
        assert!(rig.arcade.manager().debug_dump(), "the Manager callback is the first configCb");
    }

    #[test]
    fn too_long_abaas_root_vetoes_before_the_remaining_sinks_and_app_config_callbacks() {
        let mut fixture = Fixture::new();
        fixture.loader.abaas_log_root = Some("a".repeat(250));
        let mut rig = Rig::new(fixture);
        rig.dispatch(vec![cabinet(Some(1), Some(1))]);

        assert!(rig.hook("app_config").is_empty());
        assert_eq!(rig.hook("process_config").len(), 27);
        let results = rig.handles.cabinet_results.borrow();
        let (accepted, config) = &results[0];
        assert!(!accepted);
        assert_eq!(
            config.sinks,
            ListenerSinks { display_complete: true, ..ListenerSinks::default() },
            "the display sink (step 8) is installed, NeedToExit/Jvs/UsbIO (step 10) are not"
        );
        assert_eq!(rig.arcade.log().priority(), 0);
    }

    /// The app root as libam reports it: a directory path ending in a separator that is valid on every host.
    fn root_in(base: &std::path::Path) -> String {
        format!("{}/", base.display())
    }

    #[test]
    fn abaas_root_limit_is_200_wide_characters_including_the_trailing_separator() {
        let base = temp_dir("abaas-limit");
        let root = root_in(&base);
        let fill = ABAAS_ROOT_LIMIT - paths::utf16_len(&root) - 1;
        for (extra, accepted) in [(0, true), (1, false)] {
            let mut fixture = Fixture::new();
            fixture.interface.app_root = Ok(Some(root.clone()));
            fixture.loader.abaas_log_root = Some("q".repeat(fill + extra));
            let mut rig = Rig::new(fixture);
            rig.dispatch(vec![cabinet(Some(1), Some(1))]);
            assert_eq!(rig.handles.cabinet_results.borrow()[0].0, accepted, "{extra} characters over the limit");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn accepted_abaas_root_is_created_and_becomes_the_log_directory() {
        let base = temp_dir("abaas-ok");
        let mut fixture = Fixture::new();
        fixture.interface.app_root = Ok(Some(root_in(&base)));
        fixture.loader.abaas_log_root = Some(r"\abaas\log".into());
        let mut rig = Rig::new(fixture);
        rig.dispatch(vec![cabinet(Some(1), Some(1))]);

        assert!(rig.handles.cabinet_results.borrow()[0].0);
        assert_eq!(rig.hook("app_config").len(), 27);
        assert_eq!(rig.arcade.log().priority(), 2);
        assert_eq!(std::fs::read_dir(&base).expect("the directory was created").count(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn directory_creation_failure_does_not_veto() {
        let base = temp_dir("abaas-file");
        std::fs::create_dir_all(&base).unwrap();
        let blocker = base.join("blocker");
        std::fs::write(&blocker, b"file").unwrap();
        let mut fixture = Fixture::new();
        fixture.interface.app_root = Ok(Some(root_in(&blocker)));
        fixture.loader.abaas_log_root = Some("sub".into());
        let mut rig = Rig::new(fixture);
        rig.dispatch(vec![cabinet(Some(1), Some(1))]);

        assert!(rig.handles.cabinet_results.borrow()[0].0, "the callback still returns true");
        assert_eq!(rig.arcade.log().priority(), 2, "the log path is applied even though the directory is missing");
        let _ = std::fs::remove_dir_all(&base);
    }
}

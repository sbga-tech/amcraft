//! The main flow through the public ports on the simulated backend (`spec/libam/core.md` M, C).

use std::fs;

use backend::RegValue;
use backend::mock::SimRegistry;
use libam::Libam;
use libam::config::WideStr;
use libam::interface::{AmInterface, CabinetAppConfig, LibamEvent, LibamwConfig};
use libam::mock::SimBackend;
use libam::sequence::ProcessState;
use tempfile::TempDir;

/// A libam on the simulated healthy cabinet, with an English error resource set under `R:\res`.
struct Cabinet {
    libam: Libam,
    interface: Box<dyn AmInterface>,
    registry: SimRegistry,
    _root: TempDir,
}

impl Cabinet {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary directory");
        let (backend, sim) = SimBackend::new(root.path());
        let resources = root.path().join("r").join("res");
        fs::create_dir_all(&resources).expect("resource directory");
        let errors = r#"[{"number": 23, "sub": 1, "reset": "operation", "message": "Coin jam", "tag": ""}]"#;
        fs::write(resources.join("error.en.json"), errors).expect("error resource");
        fs::write(resources.join("warning.en.json"), "[]").expect("warning resource");
        let libam = Libam::new(backend, Box::new(sim.clock.clone()));
        let interface = libam.interface();
        Self { libam, interface, registry: sim.registry, _root: root }
    }

    fn initialize(&mut self) -> Vec<LibamEvent> {
        self.interface.initialize(&LibamwConfig::default()).expect("initialize")
    }

    /// Executes frames until `LoadCabinetType` arrives and returns every event seen, the request included.
    fn run_to_cabinet_request(&mut self) -> Vec<LibamEvent> {
        let mut seen = Vec::new();
        for _ in 0..20 {
            let events = self.interface.execute();
            let requested = events.iter().any(|event| matches!(event, LibamEvent::LoadCabinetType(_)));
            seen.extend(events);
            if requested {
                return seen;
            }
        }
        panic!("no LoadCabinetType request: {seen:?}");
    }

    /// Executes `frames` frames and returns every event.
    fn run(&mut self, frames: usize) -> Vec<LibamEvent> {
        (0..frames).flat_map(|_| self.interface.execute()).collect()
    }
}

/// The configuration a daemon answers with: the defaults plus the resource folder the error manager needs.
fn valid_config() -> CabinetAppConfig {
    let mut config = CabinetAppConfig::default();
    config.am.common.resource.resource_path = WideStr::new(r"R:\res").expect("resource path");
    config.am.common.resource.language = WideStr::new("en").expect("language");
    config
}

fn change_states(events: &[LibamEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match event {
            LibamEvent::ChangeState(ProcessState(state)) => Some(*state),
            _ => None,
        })
        .collect()
}

fn fatal_errors(events: &[LibamEvent]) -> Vec<u16> {
    events
        .iter()
        .filter_map(|event| match event {
            LibamEvent::FatalError(error) if error.number != 0 => Some(error.number),
            _ => None,
        })
        .collect()
}

#[test]
fn initialize_twice_reports_code_two() {
    let mut cabinet = Cabinet::new();
    let events = cabinet.initialize();
    assert_eq!(change_states(&events), [1]);
    let error = cabinet.interface.initialize(&LibamwConfig::default()).expect_err("second initialize");
    assert_eq!(error.code, 2);
    cabinet.interface.finalize();
    cabinet.initialize();
}

#[test]
fn default_config_reaches_operating_and_all_ready() {
    let mut cabinet = Cabinet::new();
    cabinet.initialize();
    cabinet.run_to_cabinet_request();
    cabinet.interface.load_cabinet_type_result(true, valid_config());
    let events = cabinet.run(20);
    assert!(fatal_errors(&events).is_empty(), "unexpected errors: {events:?}");
    assert_eq!(change_states(&events), [3], "allReady moves the sequence to advertise: {events:?}");
    assert!(cabinet.interface.is_develop().is_ok());
}

#[test]
fn invalid_config_raises_4104_once() {
    let mut cabinet = Cabinet::new();
    cabinet.initialize();
    cabinet.run_to_cabinet_request();
    let mut config = valid_config();
    config.am.common.max_player = 9;
    cabinet.interface.load_cabinet_type_result(true, config);
    let events = cabinet.run(20);
    assert_eq!(fatal_errors(&events), [4104], "{events:?}");
    cabinet.libam.error().set(23, 1);
    assert!(fatal_errors(&cabinet.interface.execute()).is_empty(), "a later error is dropped while 4104 is current");
}

#[test]
fn refused_config_raises_4104() {
    let mut cabinet = Cabinet::new();
    cabinet.initialize();
    cabinet.run_to_cabinet_request();
    cabinet.interface.load_cabinet_type_result(false, CabinetAppConfig::default());
    let events = cabinet.run(20);
    assert_eq!(fatal_errors(&events), [4104], "{events:?}");
}

#[test]
fn missing_platform_id_bails_out_silently() {
    let mut cabinet = Cabinet::new();
    cabinet.registry.set(r"System\SEGA\SystemProperty\Static", "PlatformId", RegValue::Text(String::new()));
    let events = cabinet.initialize();
    assert!(events.is_empty());
    assert!(cabinet.run(10).is_empty(), "the main flow never starts");
}

#[test]
fn events_raised_between_frames_come_back_from_the_next_execute() {
    let mut cabinet = Cabinet::new();
    cabinet.initialize();
    cabinet.run_to_cabinet_request();
    cabinet.interface.load_cabinet_type_result(true, valid_config());
    cabinet.run(20);
    cabinet.libam.error().set(23, 1);
    let events = cabinet.interface.execute();
    assert_eq!(fatal_errors(&events), [23], "{events:?}");
    assert!(cabinet.interface.execute().is_empty(), "delivered once");
}

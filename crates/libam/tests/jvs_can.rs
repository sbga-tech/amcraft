//! The disabled-feature JVS and CAN managers through the public ports (`spec/libam/jvs.md`, `spec/libam/can.md` 2).

use libam::Libam;
use libam::mock::{SIMULATED_RESOURCE_PATH, SimBackend};
use libam_api::am_config::WideStr;
use libam_api::can::CanPacket;
use libam_api::interface::{AmInterface, CabinetAppConfig, LibamEvent, LibamwConfig};
use libam_api::result::AmError;
use tempfile::TempDir;

struct Cabinet {
    libam: Libam,
    interface: Box<dyn AmInterface>,
    _root: TempDir,
}

impl Cabinet {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary directory");
        let (backend, sim) = SimBackend::new(root.path());
        let libam = Libam::new(backend, Box::new(sim.clock.clone()));
        let interface = libam.interface();
        Self { libam, interface, _root: root }
    }

    /// Initialises, answers the cabinet-type request with `config` and runs the Operating frames; returns every event.
    fn run_with(&mut self, config: CabinetAppConfig) -> Vec<LibamEvent> {
        self.interface.initialize(&LibamwConfig::default()).expect("initialize");
        let mut seen = Vec::new();
        for _ in 0..20 {
            let events = self.interface.execute();
            let requested = events.iter().any(|event| matches!(event, LibamEvent::LoadCabinetType(_)));
            seen.extend(events);
            if requested {
                break;
            }
        }
        self.interface.load_cabinet_type_result(true, config);
        for _ in 0..10 {
            seen.extend(self.interface.execute());
        }
        seen
    }
}

fn valid_config() -> CabinetAppConfig {
    let mut config = CabinetAppConfig::default();
    config.am.common.resource.resource_path = WideStr::new(SIMULATED_RESOURCE_PATH).expect("resource path");
    config.am.common.resource.language = WideStr::new("en").expect("language");
    config
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

fn detect_nodes(events: &[LibamEvent]) -> usize {
    events.iter().filter(|event| matches!(event, LibamEvent::JvsDetectNode)).count()
}

#[test]
fn ports_fail_with_code_two_before_initialize() {
    let cabinet = Cabinet::new();
    assert_eq!(cabinet.libam.jvs().handle().unwrap_err().code, AmError::NOT_INITIALIZED);
    assert_eq!(cabinet.libam.jvs_node().jvs_handle().unwrap_err().code, AmError::NOT_INITIALIZED);
    assert_eq!(cabinet.libam.can().can_facade().unwrap_err().code, AmError::NOT_INITIALIZED);
}

#[test]
fn jvs_has_no_nodes_and_every_indexed_call_fails_with_code_one() {
    let mut cabinet = Cabinet::new();
    cabinet.run_with(valid_config());
    let mut jvs = cabinet.libam.jvs();
    assert!(jvs.handle().is_ok());
    assert_eq!(jvs.node_count(), 0);
    for node in [0, 1, 3, usize::MAX] {
        for error in [
            jvs.node_auto_output_zero(node).unwrap_err(),
            jvs.set_general_output(node, 0xFFFF).unwrap_err(),
            jvs.node_input(node).map(|_| ()).unwrap_err(),
        ] {
            assert_eq!(error.code, AmError::INVALID_ARGUMENT, "node {node}");
        }
    }
    let mut nodes = cabinet.libam.jvs_node();
    assert!(nodes.jvs_handle().is_ok());
    assert_eq!(nodes.jvs_node_count(), 0);
}

#[test]
fn can_has_no_ports_and_send_fails_with_code_one() {
    let mut cabinet = Cabinet::new();
    cabinet.run_with(valid_config());
    let mut can = cabinet.libam.can();
    assert!(can.can_facade().is_ok());
    assert_eq!(can.port_count(), 0);
    assert_eq!(can.target_count(0), 0);
    assert_eq!(can.target(0, 0).unwrap_err().code, AmError::INVALID_ARGUMENT);
    let packet = CanPacket::new(0x10, &[1, 2, 3, 4, 5, 6, 7, 8], 8);
    for priority in [0, 7, 255] {
        assert_eq!(can.send(0, 0, priority, &packet).unwrap_err().code, AmError::INVALID_ARGUMENT);
    }
    assert!(can.take_events().is_empty());
}

#[test]
fn disabled_jvs_and_can_raise_nothing() {
    let mut cabinet = Cabinet::new();
    let events = cabinet.run_with(valid_config());
    assert!(fatal_errors(&events).is_empty(), "{events:?}");
    assert_eq!(detect_nodes(&events), 0);
}

#[test]
fn enabled_jvs_raises_4104_and_detect_node_once() {
    let mut cabinet = Cabinet::new();
    let mut config = valid_config();
    config.am.jvs.enable = true;
    config.sinks.jvs_detect_node = true;
    let events = cabinet.run_with(config);
    assert_eq!(fatal_errors(&events), [4104], "{events:?}");
    assert_eq!(detect_nodes(&events), 1, "{events:?}");
}

#[test]
fn enabled_jvs_without_listener_still_raises_4104_but_no_event() {
    let mut cabinet = Cabinet::new();
    let mut config = valid_config();
    config.am.jvs.enable = true;
    let events = cabinet.run_with(config);
    assert_eq!(fatal_errors(&events), [4104], "{events:?}");
    assert_eq!(detect_nodes(&events), 0, "{events:?}");
}

#[test]
fn enabled_can_raises_4104() {
    let mut cabinet = Cabinet::new();
    let mut config = valid_config();
    config.am.can.enable = true;
    config.am.can.app.send_listener_installed = true;
    config.am.can.app.recv_listener_installed = true;
    let events = cabinet.run_with(config);
    assert_eq!(fatal_errors(&events), [4104], "{events:?}");
}

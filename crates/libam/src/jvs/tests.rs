//! The disabled-feature state machine: setup, the re-arm and the shutdown (`jvs.md` 3).

use crate::config::AmConfig;
use crate::interface::LibamEvent;

use super::*;
use crate::mock::{SimBackend, SimClock};
use crate::module::EventQueue;
use backend::Backend;

struct Rig {
    backend: Backend,
    clock: SimClock,
    config: AmConfig,
    events: EventQueue,
    jvs: Jvs,
}

impl Rig {
    fn new(enable: bool) -> Self {
        let (backend, sim) = SimBackend::new("/unused");
        let mut rig = Self {
            backend,
            clock: sim.clock,
            config: AmConfig::default(),
            events: EventQueue::default(),
            jvs: Jvs::new(),
        };
        rig.config.jvs.enable = enable;
        rig.config.jvs.detect_node_listener = true;
        rig.with_cx(|jvs, cx| jvs.initialize(cx));
        rig
    }

    fn with_cx<R>(&mut self, f: impl FnOnce(&mut Jvs, &mut Cx<'_>) -> R) -> R {
        let nvram = crate::nvram::NvramContext::new();
        let mut cx = Cx {
            backend: &mut self.backend,
            clock: &self.clock,
            events: &mut self.events,
            config: &self.config,
            nvram: &nvram,
        };
        f(&mut self.jvs, &mut cx)
    }

    fn frame(&mut self) -> usize {
        self.with_cx(|jvs, cx| jvs.update(cx));
        self.events.take().iter().filter(|event| matches!(event, LibamEvent::JvsDetectNode)).count()
    }
}

#[test]
fn a_disabled_module_is_ready_at_once_and_silent() {
    let mut rig = Rig::new(false);
    assert_eq!(rig.jvs.state(), StateWord::READY);
    assert_eq!(rig.frame(), 0);
    assert!(rig.jvs.error_cell().is_empty());
    rig.jvs.restart();
    assert_eq!(rig.jvs.state(), StateWord::READY);
}

#[test]
fn the_first_tick_of_an_enabled_module_fails_with_4104_and_detects_once() {
    let mut rig = Rig::new(true);
    assert_eq!(rig.jvs.state(), StateWord::INITIALIZED);
    assert!(!rig.jvs.is_ready());
    assert_eq!(rig.frame(), 1);
    assert_eq!(rig.jvs.state(), StateWord::FAILED);
    assert_eq!(rig.jvs.error_cell().get(), Some((NOT_SUPPORTED_ERROR, 0)));
    assert!(rig.jvs.is_ready());
    assert_eq!(rig.frame(), 0, "a finished module does not run the setup again");
}

#[test]
fn a_re_arm_repeats_the_setup_and_the_detect_event() {
    let mut rig = Rig::new(true);
    assert_eq!(rig.frame(), 1);
    rig.jvs.restart();
    assert_eq!(rig.jvs.state(), StateWord::INITIALIZED);
    assert!(rig.jvs.error_cell().is_empty());
    assert_eq!(rig.frame(), 1);
    assert_eq!(rig.jvs.error_cell().get(), Some((NOT_SUPPORTED_ERROR, 0)));
}

#[test]
fn without_the_daemon_listener_no_event_is_raised() {
    let mut rig = Rig::new(true);
    rig.config.jvs.detect_node_listener = false;
    rig.with_cx(|jvs, cx| jvs.terminate(cx));
    rig.with_cx(|jvs, cx| jvs.initialize(cx));
    assert_eq!(rig.frame(), 0);
    assert_eq!(rig.jvs.error_cell().get(), Some((NOT_SUPPORTED_ERROR, 0)));
}

#[test]
fn terminate_returns_to_state_zero_and_clears_the_error() {
    let mut rig = Rig::new(true);
    rig.frame();
    rig.with_cx(|jvs, cx| jvs.terminate(cx));
    assert_eq!(rig.jvs.state(), StateWord::UNINITIALIZED);
    assert!(rig.jvs.error_cell().is_empty());
}

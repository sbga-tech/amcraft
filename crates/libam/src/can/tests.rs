//! The disabled-feature state machine: setup, the restart gate and the shutdown (`can.md` 2.5).

use crate::config::AmConfig;

use super::*;
use crate::mock::{SimBackend, SimClock};
use crate::module::EventQueue;
use backend::Backend;

struct Rig {
    backend: Backend,
    clock: SimClock,
    config: AmConfig,
    events: EventQueue,
    can: Can,
}

impl Rig {
    fn new(enable: bool) -> Self {
        let (backend, sim) = SimBackend::new("/unused");
        let mut rig = Self {
            backend,
            clock: sim.clock,
            config: AmConfig::default(),
            events: EventQueue::default(),
            can: Can::new(),
        };
        rig.config.can.enable = enable;
        rig.with_cx(|can, cx| can.initialize(cx));
        rig
    }

    fn with_cx<R>(&mut self, f: impl FnOnce(&mut Can, &mut Cx<'_>) -> R) -> R {
        let nvram = crate::nvram::NvramContext::new();
        let mut cx = Cx {
            backend: &mut self.backend,
            clock: &self.clock,
            events: &mut self.events,
            config: &self.config,
            nvram: &nvram,
        };
        f(&mut self.can, &mut cx)
    }

    fn frame(&mut self) {
        self.with_cx(|can, _cx| can.update());
    }
}

#[test]
fn a_disabled_module_is_ready_without_an_error() {
    let mut rig = Rig::new(false);
    assert_eq!(rig.can.state(), StateWord::READY);
    rig.frame();
    assert!(rig.can.is_ready());
    assert!(rig.can.error_cell().is_empty());
}

#[test]
fn an_enabled_module_raises_4104_and_is_ready_in_state_two() {
    let mut rig = Rig::new(true);
    assert_eq!(rig.can.state(), StateWord::INITIALIZED);
    assert!(!rig.can.is_ready());
    rig.frame();
    assert_eq!(rig.can.state(), StateWord::READY);
    assert_eq!(rig.can.error_cell().get(), Some((NOT_SUPPORTED_ERROR, 0)));
}

#[test]
fn the_restart_gate_re_raises_the_error_only_after_an_error() {
    let mut rig = Rig::new(true);
    rig.frame();
    rig.can.restart();
    assert_eq!(rig.can.state(), StateWord::INITIALIZED);
    assert!(rig.can.error_cell().is_empty());
    rig.frame();
    assert_eq!(rig.can.error_cell().get(), Some((NOT_SUPPORTED_ERROR, 0)));

    let mut clean = Rig::new(false);
    clean.can.restart();
    assert_eq!(clean.can.state(), StateWord::READY);
    assert!(clean.can.error_cell().is_empty());
}

#[test]
fn terminate_returns_to_state_zero() {
    let mut rig = Rig::new(true);
    rig.frame();
    rig.with_cx(|can, cx| can.terminate(cx));
    assert_eq!(rig.can.state(), StateWord::UNINITIALIZED);
    assert!(rig.can.error_cell().is_empty());
}

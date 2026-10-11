use crate::config::AmConfig;

use super::*;
use crate::mock::SimBackend;
use crate::module::{Cx, EventQueue};
use crate::nvram::NvramContext;
use crate::util::game_id;

fn apm(enable: bool) -> Apm {
    let root = tempfile::tempdir().unwrap();
    let (mut backend, sim) = SimBackend::new(root.path());
    let mut config = AmConfig::default();
    config.apm.enable = enable;
    let mut events = EventQueue::default();
    let nvram = NvramContext::new();
    let mut cx = Cx { backend: &mut backend, clock: &sim.clock, events: &mut events, config: &config, nvram: &nvram };
    let mut apm = Apm::new();
    apm.initialize(&mut cx);
    apm
}

#[test]
fn a_disabled_apm_is_ready_at_once_and_refuses_applications() {
    let mut apm = apm(false);
    assert!(apm.is_ready());
    assert!(!apm.start(game_id("SDEY").unwrap(), 0x0001_0000, false));
    assert!(!apm.end(false, false));
}

#[test]
fn an_enabled_apm_waits_for_the_other_modules() {
    let mut apm = apm(true);
    assert!(!apm.is_ready());
    apm.update(false);
    assert!(!apm.is_ready());
    apm.update(true);
    assert!(apm.is_ready());
    assert!(!apm.is_started());
}

#[test]
fn the_end_state_follows_the_test_flags() {
    let mut apm = apm(true);
    apm.update(true);
    let id = game_id("SDEY").unwrap();
    for (in_test, test, state) in [(false, false, 2), (true, true, 3), (true, false, 4), (false, true, 5)] {
        assert!(apm.start(id, 0x0001_0203, true));
        assert!(apm.is_started());
        assert_eq!(apm.sub_game_id(), Some(id));
        assert!(apm.with_aime());
        let property = apm.property().unwrap();
        assert_eq!(property.version, 0x0001_0203);
        assert!(property.is_valid());
        assert!(apm.end(in_test, test));
        assert_eq!(apm.state(), state);
        assert_eq!(apm.sub_game_id(), None);
        assert_eq!(apm.property(), None);
    }
}

#[test]
fn a_restart_silently_ends_a_running_application() {
    let mut apm = apm(true);
    apm.update(true);
    assert!(apm.start(game_id("SDEY").unwrap(), 0, false));
    apm.restart();
    assert!(!apm.is_started());
    assert!(!apm.is_ready());
    assert_eq!(apm.sub_game_id(), None);
}

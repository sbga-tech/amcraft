//! The error manager and the sequence manager through `ErrorPort` / `SequencePort` on the simulated backend
//! (`spec/libam/error.md`, `spec/libam/sequence.md`).

use std::fs;

use backend::keychip::Property;
use libam::Libam;
use libam::config::WideStr;
use libam::error::{ErrorPort, FatalError, HistoryRing};
use libam::interface::{AmInterface, CabinetAppConfig, LibamEvent, LibamwConfig};
use libam::mock::SimBackend;
use libam::sequence::{ProcessState, SequencePort};
use libam::time::Tm;
use tempfile::TempDir;

const ERROR_JSON: &str = r#"[
  {"number": 100, "sub": 0, "reset": "operation", "message": "Operation error", "tag": ""},
  {"number": 103, "sub": 0, "reset": "operation", "message": "Second operation error", "tag": ""},
  {"number": 101, "sub": 0, "reset": "shutdown", "message": "Shutdown error", "tag": ""},
  {"number": 102, "sub": 0, "reset": "test", "message": "Test error", "tag": ""},
  {"number": 4103, "sub": 0, "reset": "shutdown", "message": "Unknown error", "tag": ""}
]"#;

struct Cabinet {
    interface: Box<dyn AmInterface>,
    error: Box<dyn ErrorPort>,
    sequence: Box<dyn SequencePort>,
    _root: TempDir,
}

impl Cabinet {
    /// A libam at Operating with two players, the English resources and an advertise-state sequence.
    fn operating() -> Self {
        let root = tempfile::tempdir().expect("temporary directory");
        let (backend, sim) = SimBackend::new(root.path());
        let resources = root.path().join("r").join("res");
        fs::write(resources.join("error.en.json"), ERROR_JSON).expect("error resource");
        fs::create_dir_all(root.path().join("c").join("appdata").join("SBZZ")).expect("APPDATA directory");
        sim.keychip.set_property("gameId", Property::Binary(b"SBZZ".to_vec()));
        let libam = Libam::new(backend, Box::new(sim.clock.clone()));
        let mut interface = libam.interface();
        interface.initialize(&LibamwConfig::default()).expect("initialize");
        let mut cabinet = Self { error: libam.error(), sequence: libam.sequence(), interface, _root: root };
        cabinet.run_until(|event| matches!(event, LibamEvent::LoadCabinetType(_)));
        let mut config = CabinetAppConfig::default();
        config.am.common.resource.resource_path = WideStr::new(r"R:\res").expect("resource path");
        config.am.common.resource.language = WideStr::new("en").expect("language");
        config.am.common.max_player = 2;
        cabinet.interface.load_cabinet_type_result(true, config);
        let events = cabinet.run_until(|event| matches!(event, LibamEvent::ChangeState(ProcessState(3))));
        assert!(fatal_numbers(&events).iter().all(|&number| number == 0), "{events:?}");
        cabinet
    }

    fn run_until(&mut self, found: impl Fn(&LibamEvent) -> bool) -> Vec<LibamEvent> {
        let mut seen = Vec::new();
        for _ in 0..40 {
            let events = self.interface.execute();
            let done = events.iter().any(&found);
            seen.extend(events);
            if done {
                return seen;
            }
        }
        panic!("the awaited event never arrived: {seen:?}");
    }

    fn run(&mut self, frames: usize) -> Vec<LibamEvent> {
        (0..frames).flat_map(|_| self.interface.execute()).collect()
    }
}

fn fatal_numbers(events: &[LibamEvent]) -> Vec<u16> {
    events
        .iter()
        .filter_map(|event| match event {
            LibamEvent::FatalError(error) => Some(error.number),
            _ => None,
        })
        .collect()
}

fn fatal_items(events: &[LibamEvent]) -> Vec<FatalError> {
    events
        .iter()
        .filter_map(|event| match event {
            LibamEvent::FatalError(error) => Some((**error).clone()),
            _ => None,
        })
        .collect()
}

fn states(events: &[LibamEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match event {
            LibamEvent::ChangeState(ProcessState(state)) => Some(*state),
            _ => None,
        })
        .collect()
}

fn text(item: &FatalError) -> String {
    String::from_utf16_lossy(item.message_units())
}

#[test]
fn raised_error_carries_resource_text_and_reset_type() {
    let mut cabinet = Cabinet::operating();
    cabinet.error.set(100, 0);
    let items = fatal_items(&cabinet.run(2));
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!((items[0].number, items[0].reset_type, text(&items[0]).as_str()), (100, 2, "Operation error"));
    assert_eq!(cabinet.error.language().map(|units| units[..2].to_vec()), Some(vec![u16::from(b'e'), u16::from(b'n')]));
}

#[test]
fn reset_clears_only_operation_and_any_errors_and_announces_the_empty_item() {
    let mut cabinet = Cabinet::operating();
    cabinet.error.set(101, 0);
    cabinet.run(2);
    cabinet.error.reset();
    assert!(fatal_numbers(&cabinet.run(2)).is_empty(), "a shutdown error is not resettable");
    assert!(cabinet.sequence.fatal_error_pending());

    let mut cabinet = Cabinet::operating();
    cabinet.error.set(100, 0);
    cabinet.run(2);
    assert!(cabinet.sequence.fatal_error_pending());
    cabinet.error.reset();
    assert!(!cabinet.sequence.fatal_error_pending());
    assert_eq!(fatal_numbers(&cabinet.run(1)), [0], "the listener hears the cleared item");
}

#[test]
fn a_second_error_is_dropped_while_one_is_current() {
    let mut cabinet = Cabinet::operating();
    cabinet.error.set(101, 0);
    cabinet.error.set(100, 0);
    assert_eq!(fatal_numbers(&cabinet.run(3)), [101]);
}

#[test]
fn unknown_error_numbers_become_error_4103() {
    let mut cabinet = Cabinet::operating();
    cabinet.error.set(777, 0);
    let items = fatal_items(&cabinet.run(2));
    assert_eq!((items[0].number, text(&items[0]).as_str()), (4103, "Unknown error"));
}

#[test]
fn rings_and_occurrence_counters_follow_logged_errors() {
    let mut cabinet = Cabinet::operating();
    assert_eq!(cabinet.error.history_count(HistoryRing::System), 0);
    cabinet.error.set(100, 0);
    cabinet.run(2);
    assert_eq!(cabinet.error.history_count(HistoryRing::System), 1);
    assert_eq!(cabinet.error.history_count(HistoryRing::Application), 1);
    assert_eq!(cabinet.error.history_item(HistoryRing::System, 0).expect("newest").number, 100);
    assert_eq!(cabinet.error.history_item(HistoryRing::System, 1).expect_err("past the end").code, 1);
    let rows: Vec<_> =
        (0..cabinet.error.times_count()).map(|row| cabinet.error.times_item(row).expect("row")).collect();
    let row = rows.iter().find(|row| row.number == 100).expect("a row for the raised number");
    assert_eq!(row.times, 1);

    cabinet.error.reset();
    cabinet.error.set(100, 0);
    cabinet.run(2);
    assert_eq!(cabinet.error.history_count(HistoryRing::System), 1, "the same error after a reset is not logged again");
    let times_of = |cabinet: &Cabinet, number: u16| {
        (0..cabinet.error.times_count())
            .map(|row| cabinet.error.times_item(row).expect("row"))
            .find(|row| row.number == number)
    };
    assert_eq!(times_of(&cabinet, 100).expect("row").times, 1);

    cabinet.error.reset();
    cabinet.error.set(103, 0);
    cabinet.run(2);
    assert_eq!(cabinet.error.history_count(HistoryRing::System), 2);
    assert_eq!(cabinet.error.history_item(HistoryRing::System, 0).expect("newest").number, 103);
    assert_eq!(times_of(&cabinet, 103).expect("row").times, 1);
}

#[test]
fn clear_logs_empties_rings_and_counters() {
    let mut cabinet = Cabinet::operating();
    cabinet.error.set(100, 0);
    cabinet.run(2);
    cabinet.error.clear_logs();
    assert_eq!(cabinet.error.history_count(HistoryRing::System), 0);
    assert_eq!(cabinet.error.history_count(HistoryRing::Application), 0);
    let rows: Vec<_> =
        (0..cabinet.error.times_count()).map(|row| cabinet.error.times_item(row).expect("row")).collect();
    assert!(rows.iter().all(|row| row.times == 0), "{rows:?}");
}

#[test]
fn plays_move_between_advertise_and_play() {
    let mut cabinet = Cabinet::operating();
    assert!(!cabinet.sequence.is_playing(0));
    cabinet.sequence.begin_play(0).expect("begin");
    assert!(cabinet.sequence.is_playing(0));
    assert_eq!(states(&cabinet.run(1)), [4]);
    cabinet.sequence.end_play(0).expect("end");
    assert!(!cabinet.sequence.is_playing(0));
    assert_eq!(states(&cabinet.run(1)), [3]);
    assert_eq!(cabinet.sequence.end_play(0).expect_err("not playing").code, 6);
    assert_eq!(cabinet.sequence.begin_play(2).expect_err("slot beyond the player count").code, 6);
}

#[test]
fn no_play_starts_while_an_error_is_current() {
    let mut cabinet = Cabinet::operating();
    cabinet.error.set(101, 0);
    cabinet.run(2);
    assert_eq!(cabinet.sequence.begin_play(0).expect_err("error pending").code, 6);
}

#[test]
fn toggle_mode_goes_to_test_and_back() {
    let mut cabinet = Cabinet::operating();
    cabinet.sequence.toggle_mode();
    let events = cabinet.run_until(|event| matches!(event, LibamEvent::ChangeState(ProcessState(5))));
    assert_eq!(states(&events), [2, 5]);
    cabinet.sequence.toggle_mode();
    let events = cabinet.run_until(|event| matches!(event, LibamEvent::ChangeState(ProcessState(3))));
    assert_eq!(states(&events), [1, 3]);
}

#[test]
fn local_time_is_a_valid_broken_down_time() {
    let cabinet = Cabinet::operating();
    let Tm { mon, mday, .. } = cabinet.sequence.local_time();
    assert!((0..12).contains(&mon) && (1..=31).contains(&mday));
}

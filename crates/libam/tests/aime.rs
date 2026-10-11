//! The Aime module through `AimePort` on a running libam: one reader on a simulated COM port, the
//! Aime DB on the simulated server (`spec/libam/aime.md` A.3, A.4, A.8).

use std::fs;
use std::thread;
use std::time::Duration;

use libam::Libam;
use libam::mock::SimBackend;
use libam_api::aime::{AimeEvent, AimePort, FinishedStatus, FinishedTask};
use libam_api::am_config::{AimeUnit, WideStr};
use libam_api::interface::{AmInterface, CabinetAppConfig, LibamwConfig};
use tempfile::TempDir;

/// The access code of the card on the reader and of the account on the server.
const ACCESS_CODE: [u8; 10] = [0x00, 0x03, 0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56];
/// The Aime id the server holds for [`ACCESS_CODE`].
const AIME_ID: i32 = 4_711_001;
/// COM port of the one configured reader.
const COM_PORT: u16 = 1;

/// A libam in Operating with one enabled Aime reader.
struct Cabinet {
    interface: Box<dyn AmInterface>,
    aime: Box<dyn AimePort>,
    sim: SimBackend,
    _root: TempDir,
}

impl Cabinet {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary directory");
        let (backend, sim) = SimBackend::new(root.path());
        let resources = root.path().join("r").join("res");
        fs::create_dir_all(&resources).expect("resource directory");
        fs::write(resources.join("error.en.json"), "[]").expect("error resource");
        fs::write(resources.join("warning.en.json"), "[]").expect("warning resource");
        let libam = Libam::new(backend, Box::new(sim.clock.clone()));
        let mut interface = libam.interface();

        let mut config = CabinetAppConfig::default();
        config.am.common.resource.resource_path = WideStr::new(r"R:\res").expect("resource path");
        config.am.common.resource.language = WideStr::new("en").expect("language");
        config.am.aime.enable = true;
        config.controllers.aime.install_listeners();
        config
            .am
            .aime
            .units
            .push(AimeUnit { com_port: u64::from(COM_PORT), unit_index: 1, flag: false })
            .expect("unit");

        interface.initialize(&LibamwConfig::default()).expect("initialize");
        for _ in 0..20 {
            let requested = interface
                .execute()
                .iter()
                .any(|event| matches!(event, libam_api::interface::LibamEvent::LoadCabinetType(_)));
            if requested {
                break;
            }
        }
        interface.load_cabinet_type_result(true, config);
        Self { interface, aime: libam.aime(), sim, _root: root }
    }

    /// One frame: 20 ms of simulated time, then a short real pause for the Aime DB worker thread.
    fn frame(&mut self) {
        self.sim.clock.advance_ms(20);
        self.interface.execute();
        thread::sleep(Duration::from_millis(1));
    }

    /// Frames until `done` holds on the events taken from the port so far; returns every event taken.
    fn run_until(&mut self, mut done: impl FnMut(&[AimeEvent]) -> bool) -> Vec<AimeEvent> {
        let mut seen = Vec::new();
        for _ in 0..3000 {
            self.frame();
            seen.extend(self.aime.take_events());
            if done(&seen) {
                return seen;
            }
        }
        let kinds: Vec<_> = seen
            .iter()
            .map(|event| match event {
                AimeEvent::UnitTaskFinished(f) => format!("{:?}/{:?}/e{}", f.task, f.status, f.error_code),
                other => format!("{other:?}").chars().take(60).collect(),
            })
            .collect();
        panic!("condition not reached, events: {kinds:#?}");
    }
}

/// `(task, status)` of every task-finished event of unit 0, in order.
fn tasks(events: &[AimeEvent]) -> Vec<(FinishedTask, FinishedStatus)> {
    events
        .iter()
        .filter_map(|event| match event {
            AimeEvent::UnitTaskFinished(finished) if finished.unit == 0 => Some((finished.task, finished.status)),
            _ => None,
        })
        .collect()
}

#[test]
fn scan_reads_the_card_and_resolves_the_aime_id() {
    let mut cabinet = Cabinet::new();
    cabinet.sim.aime_db.add_account(ACCESS_CODE, AIME_ID, 1);
    cabinet.sim.serial.aime_reader(COM_PORT).place_aime_card(ACCESS_CODE);

    // The start-up pass detects the reader; then the unit is available.
    let startup = cabinet
        .run_until(|events| tasks(events).contains(&(FinishedTask::SequenceFinished, FinishedStatus::Succeeded)));
    assert_eq!(cabinet.aime.unit_count(), 1);
    assert!(cabinet.aime.reader_info(0).expect("reader info").detected);
    assert!(cabinet.aime.card_info(0).is_none(), "the detect sequence reads no card");
    assert!(
        startup.iter().any(|event| matches!(event, AimeEvent::ChangeLedLightPattern { unit: 0, pattern: 0 })),
        "the LED controller reports pattern 0 at construction: {startup:?}"
    );

    // The start-up pass ends one frame after its last unit finished; scans are refused until then.
    assert!(cabinet.aime.start_scan(0).is_err(), "a scan during the start-up pass is refused");
    cabinet.frame();
    cabinet.aime.start_scan(0).expect("start scan");
    let events = cabinet
        .run_until(|events| tasks(events).contains(&(FinishedTask::SequenceFinished, FinishedStatus::Succeeded)));

    use FinishedStatus::{Queued, Started, Succeeded};
    use FinishedTask::{
        CheckAimeIdRegistered, CheckFelicaRegistered, DetectCardReader, RegisterAimeId, RegisterFelica, ScanNfcChip,
        SequenceFinished, UpdateFirmware,
    };
    assert_eq!(
        tasks(&events),
        [
            // The unit reset queues every command.
            (DetectCardReader, Queued),
            (ScanNfcChip, Queued),
            (CheckFelicaRegistered, Queued),
            (RegisterFelica, Queued),
            (CheckAimeIdRegistered, Queued),
            (RegisterAimeId, Queued),
            (UpdateFirmware, Queued),
            // Then the chain {Detect, Scan, CheckFelica, RegisterFelica, CheckAime, RegisterAime} in order.
            (DetectCardReader, Started),
            (DetectCardReader, Succeeded),
            (ScanNfcChip, Started),
            (ScanNfcChip, Succeeded),
            (CheckFelicaRegistered, Started),
            (CheckFelicaRegistered, Succeeded),
            (RegisterFelica, Started),
            (RegisterFelica, Succeeded),
            (CheckAimeIdRegistered, Started),
            (CheckAimeIdRegistered, Succeeded),
            (RegisterAimeId, Started),
            (RegisterAimeId, Succeeded),
            (SequenceFinished, Succeeded),
        ]
    );
    assert!(
        !events.iter().any(|event| matches!(event, AimeEvent::NeedAcceptation { .. })),
        "a registered account asks no question: {events:?}"
    );

    let aime_id = cabinet.aime.aime_id(0).expect("aime id");
    assert_eq!(aime_id.id, AIME_ID as u32);
    let access_code = cabinet.aime.access_code(0).expect("access code");
    assert_eq!(access_code.0, ACCESS_CODE);
    assert!(cabinet.aime.card_info(0).is_some());
}

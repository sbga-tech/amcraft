use std::fs;

use libam_api::am_config::AmConfig;

use super::icf::{build, record};
use super::*;
use crate::mock::{SimBackend, SimClock};
use crate::module::EventQueue;
use crate::nvram::NvramContext;
use crate::registry_keys::MOUNT_KEY;
use backend::{Backend, RegValue};

struct Rig {
    root: tempfile::TempDir,
    backend: Backend,
    clock: SimClock,
    events: EventQueue,
    config: AmConfig,
    nvram: NvramContext,
    images: AppImage,
}

fn rig() -> Rig {
    let root = tempfile::tempdir().unwrap();
    let (backend, sim) = SimBackend::new(root.path());
    sim.registry.set(MOUNT_KEY, "AMFS", RegValue::Text(r"Y:\".to_owned()));
    sim.registry.set(MOUNT_KEY, "APPDATA", RegValue::Text(r"Z:\".to_owned()));
    Rig {
        root,
        backend,
        clock: sim.clock,
        events: EventQueue::default(),
        config: AmConfig::default(),
        nvram: NvramContext::new(),
        images: AppImage::new(),
    }
}

impl Rig {
    fn write_icf(&self, bytes: &[u8]) {
        let path = self.root.path().join("y/ICF1");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn start(&mut self) {
        let mut cx = Cx {
            backend: &mut self.backend,
            clock: &self.clock,
            events: &mut self.events,
            config: &self.config,
            nvram: &self.nvram,
        };
        self.images.initialize(&mut cx);
        self.images.update(&mut cx);
    }
}

const TIME: (u16, u8, u8, u8, u8, u8) = (2024, 5, 20, 12, 30, 15);

#[test]
fn applications_are_listed_by_ascending_version_and_the_highest_is_current() {
    let mut rig = rig();
    rig.write_icf(&build(&[
        record(2, 1, 1, [3, 2, 1, 0], TIME),
        record(2, 1, 1, [0, 1, 1, 0], TIME),
        record(2, 1, 1, [1, 0, 2, 0], TIME),
        // A record whose status is not 2 is not installed.
        record(1, 1, 1, [0, 0, 9, 0], TIME),
    ]));
    rig.start();
    assert!(rig.images.is_ready());
    assert_eq!(rig.images.app_count(), 3);
    let versions: Vec<u32> = (0..3).map(|index| rig.images.app_at(index).unwrap().version).collect();
    assert_eq!(versions, [0x0001_0100, 0x0001_0203, 0x0002_0001]);
    assert_eq!(rig.images.current_app().unwrap().version, 0x0002_0001);
    // `Tm::year` counts from 1900.
    assert_eq!(rig.images.current_app().unwrap().creation.year, 124);
}

#[test]
fn a_missing_index_file_is_an_empty_catalogue() {
    let mut rig = rig();
    rig.start();
    assert!(rig.images.is_ready());
    assert!(rig.images.error_cell().is_empty());
    assert_eq!(rig.images.app_count(), 0);
    assert!(rig.images.current_app().is_none());
}

#[test]
fn a_damaged_index_file_latches_error_4105() {
    let mut rig = rig();
    rig.write_icf(&[0x5A; 40]);
    rig.start();
    // State 3 (failed) still counts as "done" for allReady; the latched error tells the difference.
    assert_eq!(rig.images.error_cell().get(), Some((4105, 0)));
    assert!(rig.images.current_app().is_none());
}

#[test]
fn a_restart_rebuilds_the_catalogue_without_reading_the_file_again() {
    let mut rig = rig();
    rig.write_icf(&build(&[record(2, 1, 1, [3, 2, 1, 0], TIME)]));
    rig.start();
    assert_eq!(rig.images.app_count(), 1);
    fs::remove_file(rig.root.path().join("y/ICF1")).unwrap();
    rig.images.restart();
    assert!(!rig.images.is_ready());
    assert_eq!(rig.images.app_count(), 0);
    let mut cx = Cx {
        backend: &mut rig.backend,
        clock: &rig.clock,
        events: &mut rig.events,
        config: &rig.config,
        nvram: &rig.nvram,
    };
    rig.images.update(&mut cx);
    assert_eq!(rig.images.app_count(), 1);
}

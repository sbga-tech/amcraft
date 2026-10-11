use libam_api::am_config::{AmConfig, WideStr};

use super::*;
use crate::mock::{SimBackend, SimClock};
use crate::module::EventQueue;
use crate::nvram::NvramContext;
use crate::util::game_id;
use backend::mock::SimHttp;
use backend::{Backend, HttpResponse};

struct Rig {
    root: tempfile::TempDir,
    backend: Backend,
    clock: SimClock,
    http: SimHttp,
    events: EventQueue,
    config: AmConfig,
    nvram: NvramContext,
    abaas: Abaas,
}

fn rig(enable: bool) -> Rig {
    let root = tempfile::tempdir().unwrap();
    let (backend, sim) = SimBackend::new(root.path());
    let mut config = AmConfig::default();
    config.abaas_log.enable = enable;
    config.abaas_log.root_path = WideStr::new("abaas").unwrap();
    let mut rig = Rig {
        root,
        backend,
        clock: sim.clock,
        http: sim.http,
        events: EventQueue::default(),
        config,
        nvram: NvramContext::new(),
        abaas: Abaas::new(),
    };
    let mut cx = Cx {
        backend: &mut rig.backend,
        clock: &rig.clock,
        events: &mut rig.events,
        config: &rig.config,
        nvram: &rig.nvram,
    };
    rig.abaas.initialize(&mut cx);
    rig
}

fn inputs() -> LogInputs<'static> {
    LogInputs {
        setup_ready: true,
        app_allowed: false,
        game_id: game_id("SDEY"),
        keychip: Some("ABCD0123456"),
        main_id: Some("EFGH0123456"),
        base_path: Some(r"Y:\SDEY\"),
        version: Some((1, 5)),
        place_id: Some(1234),
    }
}

impl Rig {
    fn update(&mut self, inputs: &LogInputs<'_>) {
        let mut cx = Cx {
            backend: &mut self.backend,
            clock: &self.clock,
            events: &mut self.events,
            config: &self.config,
            nvram: &self.nvram,
        };
        self.abaas.update(&mut cx, inputs);
    }

    fn ready(&mut self) {
        self.update(&inputs());
        assert!(self.abaas.is_ready() && self.abaas.error_cell().is_empty());
    }

    fn host(&self, cabinet: &str) -> std::path::PathBuf {
        self.backend.fs.map(cabinet)
    }
}

#[test]
fn setup_creates_the_main_folder_and_configures_libabaas() {
    let mut rig = rig(true);
    assert_eq!(rig.abaas.state(), StateWord::INITIALIZED);
    rig.update(&LogInputs { setup_ready: false, ..inputs() });
    assert_eq!(rig.abaas.state(), StateWord::INITIALIZED);
    assert_eq!(rig.abaas.main_folder(), None);

    rig.ready();
    assert_eq!(rig.abaas.main_folder(), Some(r"Y:\SDEY\abaas\"));
    assert!(rig.host(r"Y:\SDEY\abaas\").is_dir());
    assert!(rig.abaas.engine.is_configured());
    assert!(rig.root.path().join("y/SDEY/abaas").is_dir());
}

#[test]
fn a_system_info_failure_latches_error_4105_and_stays_set_up() {
    let missing_main_id = LogInputs { main_id: None, ..inputs() };
    let too_long = LogInputs { base_path: Some(&"Y:\\".repeat(130)), ..inputs() };
    let no_game_id = LogInputs { game_id: None, ..inputs() };
    let empty_base = LogInputs { base_path: Some(""), ..inputs() };
    for failing in [missing_main_id, too_long, no_game_id, empty_base] {
        let mut rig = rig(true);
        rig.update(&failing);
        assert_eq!(rig.abaas.state(), StateWord::READY);
        assert_eq!(rig.abaas.error_cell().get(), Some((4105, 0)));
        assert_eq!(rig.abaas.main_folder(), None);
        assert_eq!(rig.abaas.file_count(), -1);
        assert!(!rig.abaas.is_alive());
        // Operating continues on the next frames without requests.
        rig.update(&LogInputs { app_allowed: true, ..inputs() });
        assert!(rig.http.requests().is_empty());
    }
}

#[test]
fn a_disabled_manager_is_ready_and_does_nothing() {
    let mut rig = rig(false);
    assert!(rig.abaas.is_ready());
    rig.update(&LogInputs { app_allowed: true, ..inputs() });
    assert!(rig.abaas.main_folder().is_none());
    assert!(!rig.host(r"Y:\SDEY\abaas\").exists());
    assert!(rig.http.requests().is_empty());
    assert!(!rig.abaas.enabled());
}

#[test]
fn the_server_is_polled_only_while_the_link_is_allowed_and_the_application_runs() {
    let mut rig = rig(true);
    rig.ready();
    rig.update(&inputs());
    assert!(rig.http.requests().is_empty());

    rig.update(&LogInputs { app_allowed: true, ..inputs() });
    let requests = rig.http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "http://SDEY.amlog.sys-all.net/api/polling");

    // `setAppFlag(false)` (the application is running) stops the access: the retry wait of the failed poll is
    // abandoned and no further request is made while it lasts.
    rig.abaas.refresh_idle(false);
    rig.update(&LogInputs { app_allowed: true, ..inputs() });
    rig.clock.advance_ms(3_600_000);
    rig.update(&LogInputs { app_allowed: true, ..inputs() });
    assert_eq!(rig.http.requests().len(), 1);

    rig.abaas.refresh_idle(true);
    rig.update(&LogInputs { app_allowed: true, ..inputs() });
    assert_eq!(rig.http.requests().len(), 2);
}

#[test]
fn the_location_id_comes_from_the_allnet_result_when_it_fits_sixteen_bits() {
    let mut rig = rig(true);
    rig.ready();
    rig.update(&LogInputs { app_allowed: true, place_id: Some(0x1_0000), ..inputs() });
    assert_eq!(rig.abaas.sys.location, 0);
    rig.update(&inputs());
    rig.update(&LogInputs { app_allowed: true, place_id: Some(0xFFFF), ..inputs() });
    assert_eq!(rig.abaas.sys.location, 0xFFFF);
}

#[test]
fn upload_idle_is_set_after_240_seconds_without_a_live_connection() {
    let mut rig = rig(true);
    rig.ready();
    let allowed = LogInputs { app_allowed: true, ..inputs() };
    rig.update(&allowed);
    assert!(!rig.abaas.upload_idle());
    rig.clock.advance_ms(239_000);
    rig.update(&allowed);
    assert!(!rig.abaas.upload_idle());
    rig.clock.advance_ms(2_000);
    rig.update(&allowed);
    assert!(rig.abaas.upload_idle());

    // A server that answers with nothing to upload keeps the byte at "idle" (`count == 0`).
    let key = wire::derive_key("SDEY");
    rig.http.push_answer(Ok(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: wire::encode_response(&key, 0, b"{}"),
    }));
    rig.clock.advance_ms(200_000);
    for _ in 0..4 {
        rig.update(&allowed);
    }
    assert_eq!(rig.abaas.file_count(), 0);
    assert!(rig.abaas.upload_idle());
}

#[test]
fn change_app_builds_the_sub_application_folder() {
    let mut rig = rig(true);
    // Before the set-up nothing can be changed.
    assert!(!rig.abaas.change_app(rig.backend.fs.as_ref(), game_id("SDEZ")));
    rig.ready();
    assert_eq!(rig.abaas.sub_folder(), None);

    assert!(rig.abaas.change_app(rig.backend.fs.as_ref(), game_id("SDEZ")));
    assert_eq!(rig.abaas.sub_folder(), Some(r"Y:\SDEZ\abaas\"));
    assert!(rig.host(r"Y:\SDEZ\abaas\").is_dir());

    // An invalid game id ends the application: the folder is emptied.
    assert!(rig.abaas.change_app(rig.backend.fs.as_ref(), None));
    assert_eq!(rig.abaas.sub_folder(), None);
}

#[test]
fn error_messages_are_dropped_until_ready_and_queued_after() {
    let mut rig = rig(true);
    let message: Vec<u16> = "boom".encode_utf16().collect();
    rig.abaas.send_error_message(&message, &rig.clock);
    assert!(rig.abaas.engine.app_messages().is_empty());
    rig.ready();
    rig.abaas.send_error_message(&message, &rig.clock);
    assert_eq!(rig.abaas.engine.app_messages().len(), 1);
    assert_eq!(rig.abaas.engine.app_messages()[0].content, "boom");
    // A lone surrogate cannot be converted: dropped.
    rig.abaas.send_error_message(&[0xD800, 0x41], &rig.clock);
    assert_eq!(rig.abaas.engine.app_messages().len(), 1);
}

#[test]
fn error_message_conversion_truncates_like_the_original() {
    assert_eq!(convert_message(&[0x61, 0x62, 0, 0x63]).as_deref(), Some("ab"));
    assert_eq!(convert_message(&[0xDC00]), None);
    let ascii: Vec<u16> = vec![u16::from(b'x'); 2000];
    assert_eq!(convert_message(&ascii).unwrap().len(), 0x401);
    // 400 three-byte characters: 1025 bytes are kept, the cut character is dropped again.
    let kana: Vec<u16> = vec![0x3042; 400];
    assert_eq!(convert_message(&kana).unwrap().len(), 1023);
    // More than the 6145-byte buffer: the conversion fails.
    let huge: Vec<u16> = vec![0x3042; 2049];
    assert_eq!(convert_message(&huge), None);
}

#[test]
fn terminate_returns_to_state_zero_and_stops_the_access() {
    let mut rig = rig(true);
    rig.ready();
    rig.update(&LogInputs { app_allowed: true, ..inputs() });
    assert!(rig.abaas.running);
    let mut cx = Cx {
        backend: &mut rig.backend,
        clock: &rig.clock,
        events: &mut rig.events,
        config: &rig.config,
        nvram: &rig.nvram,
    };
    rig.abaas.terminate(&mut cx);
    assert_eq!(rig.abaas.state(), StateWord::UNINITIALIZED);
    assert!(!rig.abaas.running && !rig.abaas.enabled());
}

#[test]
fn restart_re_arms_the_setup() {
    let mut rig = rig(true);
    rig.update(&LogInputs { main_id: None, ..inputs() });
    assert_eq!(rig.abaas.error_cell().get(), Some((4105, 0)));
    rig.abaas.restart(rig.clock.tick_ms());
    assert_eq!(rig.abaas.state(), StateWord::INITIALIZED);
    assert!(rig.abaas.error_cell().is_empty());
    rig.ready();
}

#[test]
fn the_version_block_of_an_option_image_is_not_used() {
    use libam_api::app_image::{AppImageProperty, PropertyKind};
    let app = AppImageProperty {
        kind: PropertyKind::App,
        initialized: true,
        version: 0x0001_0502,
        option_id: [0; 5],
        creation: libam_api::time::Tm::default(),
    };
    assert_eq!(LogInputs::version_of(Some(app)), Some((1, 5)));
    assert_eq!(LogInputs::version_of(Some(AppImageProperty { kind: PropertyKind::Option, ..app })), None);
    assert_eq!(LogInputs::version_of(Some(AppImageProperty { initialized: false, ..app })), None);
    assert_eq!(LogInputs::version_of(None), None);
}

//! The credit module: the AS-6DB wire protocol against the executable model of `credit.md` part II
//! (`as6db_model.py`), the credit manager from coin counter to credit, and `CreditPort` on a running libam.

use std::fs;
use std::path::Path;
use std::rc::Rc;

use backend::Backend;
use libam::Libam;
use libam::core::Core;
use libam::credit::as6db::driver::Driver;
use libam::credit::as6db::frame::{self, Key};
use libam::credit::{CoinSlots, CreditMode};
use libam::mock::devices::{SimAs6db, Yen};
use libam::mock::{SimBackend, SimClock};
use libam::module::Cx;
use libam_api::am_config::{CreditAs6db, UNSET_U64, WideStr};
use libam_api::credit::PlayerCredit;
use libam_api::interface::{AmInterface, CabinetAppConfig, LibamEvent, LibamwConfig};
use tempfile::TempDir;

fn bytes(text: &str) -> Vec<u8> {
    text.split_whitespace().map(|byte| u8::from_str_radix(byte, 16).expect("hex byte")).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------------------------------------
// AS-6DB wire protocol
// ---------------------------------------------------------------------------------------------------------

/// Plain frames of the model (`python3 as6db_model.py`), bus address 2.
#[test]
fn plain_frames_match_the_model() {
    let setup = frame::setup_body(&[0x00, 0x0D, 0x7D, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0A, 0x32]);
    let cases: [(Vec<u8>, &str); 9] = [
        (
            frame::build(2, 2, &setup, Key::Plain),
            "4A FC 02 01 12 02 05 00 0D 7D 00 00 00 00 00 00 00 00 00 00 00 00 0A 32 D3",
        ),
        (frame::build(2, 3, &[6, 1, 0], Key::Plain), "4A FC 02 01 03 03 06 01 00 52"),
        (frame::build(2, 3, &[6, 0, 0], Key::Plain), "4A FC 02 01 03 03 06 00 00 47"),
        (frame::build(2, 4, &[9], Key::Plain), "4A FC 02 01 01 04 09 6B"),
        (frame::build(2, 5, &[7], Key::Plain), "4A FC 02 01 01 05 07 54"),
        (frame::build(2, 6, &[1, 0x10, 8, 0], Key::Plain), "4A FC 02 01 04 06 01 10 08 00 57"),
        (frame::build(2, 7, &[0x10, 0x55, 0xAA], Key::Plain), "4A FC 02 01 03 07 10 55 AA D2"),
        (frame::build(0, 8, &[4], Key::Plain), "4A FC 00 01 01 08 04 B4"),
        (frame::new_seed(0, 9, 0xBEEF).to_vec(), "4A FC 00 01 00 09 EF BE 89"),
    ];
    for (built, expected) in cases {
        assert_eq!(hex(&built), expected);
    }
    assert_eq!(hex(&frame::new_seed(2, 1, 0xBEEF)), "4A FC 02 01 00 01 EF BE D8");
}

/// The scrambled receive and transmit vectors of the model: seed `BEEF`, initial keystream state `EFBE`.
#[test]
fn scrambled_frames_match_the_model() {
    let mut state = 0xEFBE;
    let mut received = bytes("4A FC 01 02 04 9B EE 2F FB 4B F7");
    assert!(frame::open(&mut received, Some(&mut state)), "the checksum of the model's frame is valid");
    assert_eq!(hex(&received[5..]), "20 01 01 00 00 49");
    assert_eq!(state, 0x4BBE);
    let sent = frame::build(2, 2, &[7], Key::Scrambled(&mut state));
    assert_eq!(hex(&sent), "4A FC 02 01 01 D0 E8 CE");
    assert_eq!(state, 0xF4BB);
}

/// The driver started with a seed generator that yields `BEEF`: the whole session (NEW_SEED, SETUP, CONTROL
/// lock, INFO, STATUS with one 100 yen coin, the acknowledgement that clears it) is byte-identical to the
/// model's frames, in both directions.
#[test]
fn driver_session_matches_the_model() {
    let sim = SimAs6db::new();
    let mut driver = Driver::new(1, &[1, 2, 3], 0, 0xBEEF_0000);
    let mut now = 1000;
    let mut counted = 0;
    for frame in 0..24 {
        if frame == 8 {
            sim.insert_coin(Yen::Hundred);
        }
        driver.frame(&sim, now);
        counted += driver.stats().yen_100;
        now += 20;
    }
    let expected = [
        ('M', "4A FC 02 01 00 01 EF BE D8"),
        ('D', "4A FC 01 02 04 AB EE 2F FB 4B 01"),
        ('M', "4A FC 02 01 12 D0 EA F4 B6 80 2E 7F 4B 9F D2 E7 F4 B9 FD EE 7F 7B 95 2C 1F"),
        ('D', "4A FC 01 02 04 16 B8 03 EE 00 BB"),
        ('M', "4A FC 02 01 03 03 18 41 07 28"),
        ('D', "4A FC 01 02 04 16 01 42 00 50 E7"),
        ('M', "4A FC 02 01 01 90 19 C0"),
        ('D', "4A FC 01 02 19 1A 4B 2F 51 50 94 54 24 94 09 28 3F 89 50 22 54 48 95 D2 25 74 89 5D 22 5D 7A 87"),
        ('M', "4A FC 02 01 04 C0 75 22 57 8C F2"),
        ('M', "4A FC 02 01 01 25 12 61"),
        ('D', "4A FC 01 02 0A 36 39 09 8C 82 23 60 89 18 23 46 25"),
        ('M', "4A FC 02 01 04 D6 83 60 68 1D 13"),
        ('M', "4A FC 02 01 01 CF 41 83"),
        ('D', "4A FC 01 02 0A 99 7C E2 1D B8 C7 2E 31 CB 8D 72 BA"),
    ];
    let wire = sim.wire();
    assert!(wire.len() >= expected.len(), "only {} frames on the wire", wire.len());
    for (frame, (side, text)) in wire.iter().zip(expected) {
        assert_eq!(frame.from_master, side == 'M');
        assert_eq!(hex(&frame.bytes), text);
    }
    assert_eq!(counted, 1, "the coin of the first STATUS is counted once");
    assert!(sim.lockout(), "the device boots locked out and the first CONTROL keeps it so");
    assert_eq!(hex(&sim.setup().expect("SETUP stored")), "00 0D 7D 00 00 00 00 00 00 00 00 00 00 00 00 0A 32");
}

#[test]
fn silent_selector_times_out_and_recovers() {
    let sim = SimAs6db::new();
    sim.set_answering(false);
    let mut driver = Driver::new(1, &[1], 0, 0xBEEF_0000);
    let mut now = 20;
    while now < 4900 {
        driver.frame(&sim, now);
        now += 20;
    }
    assert!(!driver.faults().comm_timeout, "the connect timeout is 4 s");
    while now < 6000 {
        driver.frame(&sim, now);
        now += 20;
    }
    assert!(driver.faults().comm_timeout);
    sim.set_answering(true);
    for _ in 0..200 {
        driver.frame(&sim, now);
        now += 20;
    }
    assert!(!driver.faults().comm_timeout, "an answering selector clears the timeout");
}

#[test]
fn missing_port_is_retried_after_two_seconds() {
    let sim = SimAs6db::new();
    sim.set_port(None);
    let mut driver = Driver::new(1, &[1], 0, 0xBEEF_0000);
    driver.frame(&sim, 1000);
    assert_eq!(sim.opens(), 0, "a failed open is not a successful open");
    sim.set_port(Some(1));
    driver.frame(&sim, 2000);
    assert_eq!(sim.opens(), 0, "the back-off after a failed open is 2 s");
    driver.frame(&sim, 3100);
    assert_eq!(sim.opens(), 1);
}

// ---------------------------------------------------------------------------------------------------------
// The credit manager
// ---------------------------------------------------------------------------------------------------------

/// A cabinet driven frame by frame on the simulated backend.
struct Machine {
    core: Core,
    clock: SimClock,
    root: TempDir,
}

fn app_config(configure: impl FnOnce(&mut CabinetAppConfig)) -> CabinetAppConfig {
    let mut config = CabinetAppConfig::default();
    config.am.common.resource.resource_path = WideStr::new(r"R:\res").expect("resource path");
    config.am.common.resource.language = WideStr::new("en").expect("language");
    config.am.common.max_player = 2;
    config.am.credit.enable = true;
    config.am.credit.coin_box_assign[0] = 0;
    config.controllers.credit.insert_coin_listener = true;
    config.am.credit.rule.chute_share_type = 1;
    config.am.credit.rule.coin_to_credit = 2;
    configure(&mut config);
    config
}

/// The simulated healthy cabinet plus an English error resource set holding the AS-6DB errors.
fn prepare(root: &Path) -> (Backend, SimBackend) {
    let (backend, sim) = SimBackend::new(root);
    let resources = root.join("r").join("res");
    fs::create_dir_all(&resources).expect("resource directory");
    let errors = r#"[{"number": 3991, "sub": 0, "reset": "operation", "message": "Coin selector", "tag": ""}]"#;
    fs::write(resources.join("error.en.json"), errors).expect("error resource");
    fs::write(resources.join("warning.en.json"), "[]").expect("warning resource");
    (backend, sim)
}

impl Machine {
    /// Boots libam to the operating state with `config` and runs until the credit manager is running.
    fn boot(root: TempDir, config: CabinetAppConfig) -> Self {
        let (backend, sim) = prepare(root.path());
        let clock = sim.clock.clone();
        let mut core = Core::new(backend, Box::new(sim.clock.clone()));
        core.initialize().expect("initialize");
        let mut requested = false;
        for _ in 0..20 {
            requested |= core.execute().iter().any(|event| matches!(event, LibamEvent::LoadCabinetType(_)));
            if requested {
                break;
            }
        }
        assert!(requested, "libam asks for the cabinet type");
        core.answer_cabinet_type(true, config);
        for _ in 0..30 {
            core.execute();
        }
        assert!(core.credit.is_ready(), "the credit manager runs");
        Self { core, clock, root }
    }

    /// One frame of the credit manager against `board`, 16 ms after the last.
    fn step(&mut self, board: &mut Board, mode: CreditMode) {
        self.clock.advance_ms(16);
        let core = &mut self.core;
        let mut cx = Cx {
            backend: &mut core.backend,
            clock: &*core.clock,
            events: &mut core.events,
            config: &core.config,
            nvram: &core.nvram,
        };
        core.credit.update(&mut cx, board, mode);
        core.events.take();
    }

    /// Frames until the manager has left the node-waiting substate and a coin counter baseline exists.
    fn settle(&mut self, board: &mut Board) {
        for _ in 0..3 {
            self.step(board, PLAY);
        }
    }

    fn player(&self, player: u32) -> Option<PlayerCredit> {
        self.core.credit.player_value(player)
    }
}

const PLAY: CreditMode = CreditMode { play: true, test: false };

/// An I/O board with one coin node of one slot.
#[derive(Default)]
struct Board {
    counter: u16,
}

impl CoinSlots for Board {
    fn enabled(&self) -> bool {
        true
    }

    fn ready(&self) -> bool {
        true
    }

    fn node_count(&self) -> usize {
        1
    }

    fn slot_count(&self, node: usize) -> usize {
        usize::from(node == 0)
    }

    fn share_coin_slot(&self, _node: usize) -> bool {
        false
    }

    fn jam(&self, _node: usize, _slot: usize) -> bool {
        false
    }

    fn disconnected(&self, _node: usize, _slot: usize) -> bool {
        false
    }

    fn coin_counter(&self, node: usize, slot: usize) -> u16 {
        if node == 0 && slot == 0 { self.counter } else { 0 }
    }

    fn switch_count(&self, _node: usize) -> usize {
        0
    }

    fn service(&self, _node: usize, _k: usize) -> bool {
        false
    }

    fn set_lockout(&mut self, _node: usize, _on: bool) {}
}

fn machine(configure: impl FnOnce(&mut CabinetAppConfig)) -> Machine {
    Machine::boot(tempfile::tempdir().expect("temporary directory"), app_config(configure))
}

/// Two coins per credit: the first coin is a partial credit, the second completes it; the insert-coin events,
/// the bookkeeping and the price check follow. `pay` takes the credit back.
#[test]
fn coins_become_credit_at_two_coins_per_credit() {
    let mut cabinet = machine(|_| {});
    let mut board = Board::default();
    cabinet.settle(&mut board);
    assert_eq!(cabinet.player(0), Some(PlayerCredit { credit: 0, remain: 0 }));
    assert_eq!(cabinet.core.credit.check(0, 0, 1).unwrap(), 0);

    board.counter = 1;
    cabinet.step(&mut board, PLAY);
    assert_eq!(cabinet.player(0), Some(PlayerCredit { credit: 0, remain: 1 }), "one coin of two");
    assert_eq!(cabinet.core.credit.check(0, 0, 1).unwrap(), 1, "a partial credit is held");
    let events = cabinet.core.credit.take_events();
    assert_eq!(events.len(), 1);
    assert!(!events[0].service, "a coin entered, no credit completed");
    assert!(events[0].players[0], "the event is for the player with the coin box");

    board.counter = 2;
    cabinet.step(&mut board, PLAY);
    assert_eq!(cabinet.player(0), Some(PlayerCredit { credit: 1, remain: 0 }));
    assert_eq!(cabinet.core.credit.check(0, 0, 1).unwrap(), 2, "affordable");
    assert_eq!(cabinet.core.credit.check(0, 0, 2).unwrap(), 1, "two games cost two credits");
    assert!(cabinet.core.credit.take_events()[0].service, "a credit was completed");
    let totals = cabinet.core.credit.bookkeeping_totals();
    assert_eq!((totals.total_coin, totals.coin_credit, totals.total_credit), (2, 1, 1));
    assert_eq!(cabinet.core.credit.bookkeeping_coin(0), Some(2), "coin counter of the slot");

    cabinet.core.credit.pay(0, 0, 1).expect("pay one credit");
    assert_eq!(cabinet.player(0), Some(PlayerCredit { credit: 0, remain: 0 }));
    assert!(cabinet.core.credit.pay(0, 0, 1).is_err(), "nothing left to pay");
}

/// A player without a coin box has no credit; the counter of a coin entered while input is ignored or the
/// cabinet is in test mode is only re-baselined.
#[test]
fn coins_are_ignored_without_coin_box_ignore_flag_and_in_test_mode() {
    let mut cabinet = machine(|_| {});
    let mut board = Board::default();
    cabinet.settle(&mut board);
    assert_eq!(cabinet.player(1), None, "player 2 has no coin box");
    assert_eq!(cabinet.core.credit.check(1, 0, 1).unwrap(), 0);
    assert_eq!(cabinet.core.credit.addable(1), 0);

    cabinet.core.credit.set_coin_in_ignore(true);
    board.counter = 3;
    cabinet.step(&mut board, PLAY);
    cabinet.core.credit.set_coin_in_ignore(false);
    board.counter = 4;
    cabinet.step(&mut board, CreditMode { play: true, test: false });
    assert_eq!(cabinet.player(0), Some(PlayerCredit { credit: 0, remain: 1 }), "only the coin after the flag counts");

    board.counter = 7;
    cabinet.step(&mut board, CreditMode { play: false, test: true });
    assert_eq!(
        cabinet.player(0),
        Some(PlayerCredit { credit: 0, remain: 0 }),
        "test mode re-baselines and discards the partial credit"
    );
    board.counter = 8;
    cabinet.step(&mut board, PLAY);
    assert_eq!(cabinet.player(0), Some(PlayerCredit { credit: 0, remain: 1 }));
}

/// The credit state is written to the sysfile by the frame after the change; a new libam on the same files
/// restores the credits but not the partial coin (`credit.md` 3.2).
#[test]
fn credit_survives_a_power_cycle_without_the_partial_coin() {
    let mut cabinet = machine(|_| {});
    let mut board = Board::default();
    cabinet.settle(&mut board);
    board.counter = 5;
    cabinet.step(&mut board, PLAY);
    cabinet.step(&mut board, PLAY);
    assert_eq!(cabinet.player(0), Some(PlayerCredit { credit: 2, remain: 1 }));
    cabinet.step(&mut board, PLAY);
    let Machine { core, root, .. } = cabinet;
    drop(core);

    let again = Machine::boot(root, app_config(|_| {}));
    assert_eq!(again.player(0), Some(PlayerCredit { credit: 2, remain: 0 }));
    let totals = again.core.credit.bookkeeping_totals();
    assert_eq!((totals.total_coin, totals.coin_credit, totals.total_credit), (5, 2, 2));
}

/// `max_credit` clamps the credits and drops the remainder; `addable` tells how many units still fit.
#[test]
fn max_credit_clamps_and_addable_counts_the_room() {
    let mut cabinet = machine(|config| config.am.credit.max_credit = 3);
    let mut board = Board::default();
    cabinet.settle(&mut board);
    assert_eq!(cabinet.core.credit.addable(0), 6, "three credits of two coins");
    board.counter = 5;
    cabinet.step(&mut board, PLAY);
    assert_eq!(cabinet.player(0), Some(PlayerCredit { credit: 2, remain: 1 }));
    assert_eq!(cabinet.core.credit.addable(0), 1);
    board.counter = 8;
    cabinet.step(&mut board, PLAY);
    assert_eq!(cabinet.player(0), Some(PlayerCredit { credit: 3, remain: 0 }), "clamped, the remainder is dropped");
    assert_eq!(cabinet.core.credit.addable(0), 0);
}

// ---------------------------------------------------------------------------------------------------------
// CreditPort on a running libam
// ---------------------------------------------------------------------------------------------------------

struct Cabinet {
    libam: Libam,
    interface: Box<dyn AmInterface>,
    clock: SimClock,
    _root: TempDir,
}

impl Cabinet {
    fn boot(selector: Option<&SimAs6db>, config: CabinetAppConfig) -> Self {
        let root = tempfile::tempdir().expect("temporary directory");
        let (backend, sim) = prepare(root.path());
        if let Some(selector) = selector {
            sim.serial.attach(1, Rc::new(selector.clone()));
        }
        let clock = sim.clock.clone();
        let libam = Libam::new(backend, Box::new(sim.clock.clone()));
        let mut interface = libam.interface();
        interface.initialize(&LibamwConfig::default()).expect("initialize");
        let mut cabinet = Self { libam, interface, clock, _root: root };
        let events = cabinet.frames(20);
        assert!(events.iter().any(|event| matches!(event, LibamEvent::LoadCabinetType(_))));
        cabinet.interface.load_cabinet_type_result(true, config);
        cabinet.frames(20);
        cabinet
    }

    fn frames(&mut self, count: usize) -> Vec<LibamEvent> {
        let mut events = Vec::new();
        for _ in 0..count {
            self.clock.advance_ms(20);
            events.extend(self.interface.execute());
        }
        events
    }
}

#[test]
fn credit_port_reports_the_effective_rule_and_player_values() {
    let cabinet = Cabinet::boot(None, app_config(|config| config.am.credit.rule.game_cost[1] = 3));
    let mut port = cabinet.libam.credit();
    assert!(port.handle().is_ok());
    assert!(port.is_enabled());
    assert!(!port.special_device_available());
    assert_eq!(port.config_chute_share_type(), 1);
    assert_eq!(port.config_coin_to_credit(), 2);
    assert_eq!(port.config_multiplier(0).unwrap(), 1);
    assert!(port.config_multiplier(2).is_err());
    assert_eq!(port.config_game_cost(0).unwrap(), 1);
    assert_eq!(port.config_game_cost(1).unwrap(), 3);
    assert!(port.config_game_cost(8).is_err());
    assert_eq!(port.player_value(0), Some(PlayerCredit { credit: 0, remain: 0 }));
    assert_eq!(port.player_value(1), None);
    assert_eq!(port.addable_coin(0), 48, "24 credits of two coins");
    assert_eq!(port.bookkeeping_coin(0).unwrap(), 0);
    assert!(port.bookkeeping_coin(8).is_err());
    assert!(!port.lockout_on() && !port.can_lockout(), "no special device");
    assert!(port.special_lockout(true).is_err());
    assert_eq!(port.pay(0, 0, 1).expect_err("no credit").code, 6);
    port.set_coin_in_ignore(true);
    assert!(port.coin_in_ignored());
    assert!(port.take_events().is_empty());
}

#[test]
fn disabled_credit_refuses_everything() {
    let cabinet = Cabinet::boot(None, app_config(|config| config.am.credit.enable = false));
    let mut port = cabinet.libam.credit();
    assert!(!port.is_enabled());
    assert_eq!(port.player_value(0), None);
    port.set_coin_in_ignore(true);
    assert!(!port.coin_in_ignored(), "only an enabled module stores the flag");
    assert!(port.pay(0, 0, 1).is_err());
}

fn with_selector(config: &mut CabinetAppConfig) {
    config.am.credit.coin_box_assign = [UNSET_U64; 8];
    config.am.credit.coin_box_assign[0] = 0;
    let mut selector = CreditAs6db { enable: true, arg: 1, ..CreditAs6db::default() };
    selector.types.push(2).expect("coin type");
    config.am.credit.coin_selector_as6db = selector;
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

/// The selector locks the coin chutes while it is set up and the STATUS reports it; `special_lockout(false)`
/// reaches the device as a CONTROL frame and the state shown to the game follows.
#[test]
fn credit_port_unlocks_the_selector() {
    let sim = SimAs6db::new();
    let mut cabinet = Cabinet::boot(Some(&sim), app_config(with_selector));
    let mut port = cabinet.libam.credit();
    assert!(port.special_device_available());
    let events = cabinet.frames(200);
    assert!(fatal_errors(&events).is_empty(), "{events:?}");
    assert!(sim.setup().is_some(), "SETUP reached the selector");
    assert!(sim.lockout());
    assert!(port.lockout_on(), "the STATUS reports the lockout");
    assert!(port.can_lockout());

    port.special_lockout(false).expect("unlock");
    assert!(!port.lockout_on(), "the requested state is shown at once");
    assert!(port.special_lockout(true).is_err(), "no change during the cooldown");
    cabinet.frames(200);
    assert!(!sim.lockout(), "the CONTROL frame unlocked the device");
    assert!(!port.lockout_on());
    assert!(port.can_lockout());
}

/// A selector that never answers raises credit error 3991 after the 4 s connect timeout.
#[test]
fn silent_selector_raises_error_3991() {
    let sim = SimAs6db::new();
    sim.set_answering(false);
    let mut cabinet = Cabinet::boot(Some(&sim), app_config(with_selector));
    assert!(fatal_errors(&cabinet.frames(100)).is_empty(), "2 s are within the timeout");
    let errors = fatal_errors(&cabinet.frames(400));
    assert_eq!(errors, [3991]);
}

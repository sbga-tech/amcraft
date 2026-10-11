//! Display behaviour on the simulated VFD: protocol bytes, the self-test timeline and `CompleteTask`, the health
//! tracker and the shutdown flush (`can.md` 3.3-3.10).

use crate::config::{AmConfig, DisplayConfig};
use crate::interface::LibamEvent;

use super::*;
use crate::mock::SimBackend;
use crate::module::Cx;
use backend::Backend;

const INIT: [u8; 8] = [0x1B, 0x0B, 0x1B, 0x21, 0x01, 0x1B, 0x32, 0x02];
const CLEAR: [u8; 4] = [0x1B, 0x0C, 0x1B, 0x52];
/// しばらくお待ちください in CP932.
const PLEASE_WAIT: [u8; 22] = [
    0x82, 0xB5, 0x82, 0xCE, 0x82, 0xE7, 0x82, 0xAD, 0x82, 0xA8, 0x91, 0xD2, 0x82, 0xBF, 0x82, 0xAD, 0x82, 0xBE, 0x82,
    0xB3, 0x82, 0xA2,
];
/// 電子決済できます in CP932.
const EPAY_OK: [u8; 16] =
    [0x93, 0x64, 0x8E, 0x71, 0x8C, 0x88, 0x8D, 0xCF, 0x82, 0xC5, 0x82, 0xAB, 0x82, 0xDC, 0x82, 0xB7];

/// A display module on the simulated backend, stepped frame by frame.
struct Rig {
    backend: Backend,
    sim: SimBackend,
    config: AmConfig,
    events: crate::module::EventQueue,
    display: Display,
}

impl Rig {
    fn new(port: u64) -> Self {
        let (backend, sim) = SimBackend::new("/unused");
        let mut rig = Self {
            backend,
            sim,
            config: AmConfig::default(),
            events: crate::module::EventQueue::default(),
            display: Display::new(),
        };
        rig.config.display =
            DisplayConfig { enable: true, display_port: port, complete_task_listener: true, message_listener: false };
        rig.with_cx(|display, cx| display.initialize(cx));
        rig
    }

    fn with_cx<R>(&mut self, f: impl FnOnce(&mut Display, &mut Cx<'_>) -> R) -> R {
        let nvram = crate::nvram::NvramContext::new();
        let mut cx = Cx {
            backend: &mut self.backend,
            clock: &self.sim.clock,
            events: &mut self.events,
            config: &self.config,
            nvram: &nvram,
        };
        f(&mut self.display, &mut cx)
    }

    fn frame(&mut self) {
        self.with_cx(|display, cx| display.update(cx));
    }

    fn advance(&mut self, ms: u64) {
        self.sim.clock.advance_ms(ms);
    }

    fn start_test(&mut self) -> crate::result::AmResult<()> {
        let now = self.sim.clock.tick_ms();
        self.display.start_test(now)
    }

    fn complete_events(&mut self) -> usize {
        self.events.take().iter().filter(|event| matches!(event, LibamEvent::DisplayCompleteTask)).count()
    }

    fn panel(&self) -> crate::mock::devices::SimVfd {
        self.sim.serial.vfd(self.config.display.display_port as u16)
    }

    fn last_packet(&self) -> Vec<u8> {
        self.panel().packets().pop().expect("a packet was written")
    }
}

/// The scroll block of the 80-bar test pattern on a line with its start command.
fn pattern_packet(position: u8) -> Vec<u8> {
    let mut packet = CLEAR.to_vec();
    packet.extend([0x1B, 0x40, 0x00, 0x00, position, 0x00, 0x9F, 0x02, 0x1B, 0x41, 0x00, 0x1B, 0x50, 0x50]);
    packet.extend([b'|'; 80]);
    packet.extend([0x1B, 0x51]);
    packet
}

#[test]
fn first_frame_opens_the_port_and_sends_the_init_packet() {
    let mut rig = Rig::new(3);
    rig.frame();
    assert_eq!(rig.panel().open_port(), Some(3));
    assert_eq!(rig.panel().packets(), vec![INIT.to_vec()]);
    assert!(rig.display.ready());
}

#[test]
fn a_missing_com_port_leaves_the_display_ready_but_unusable() {
    let mut rig = Rig::new(3);
    rig.panel().set_open_fails(true);
    rig.frame();
    assert!(rig.display.is_ready());
    assert!(!rig.display.ready());
    assert_eq!(rig.start_test().unwrap_err().code, 6);
}

#[test]
fn the_epayment_lane_draws_the_aimepay_page_in_one_packet() {
    let mut rig = Rig::new(3);
    rig.frame();
    rig.display.lane_clear(Lane::AimePay);
    rig.display.lane_set_entry(Lane::AimePay, 1, "AimePay");
    rig.display.lane_set_state(Lane::AimePay, 1);
    rig.frame();

    let mut expected = CLEAR.to_vec();
    expected.extend([0x1B, 0x40, 0x00, 0x00, 0x02, 0x00, 0x9F, 0x02, 0x1B, 0x41, 0x00, 0x1B, 0x50, 0x1B]);
    expected.extend(b"AimePay");
    expected.extend([b' '; 20]);
    expected.extend([0x1B, 0x30, 0x00, 0x00, 0x00]);
    expected.extend(EPAY_OK);
    expected.push(0x1B);
    expected.push(0x51);
    assert_eq!(rig.last_packet(), expected);
    assert_eq!(expected.len(), 68);
}

#[test]
fn a_transaction_text_scrolls_with_six_wrap_around_spaces() {
    let mut rig = Rig::new(3);
    rig.frame();
    rig.display.enter_mode_a();
    rig.display.set_line(0, "しばらくお待ちください", true);
    rig.display.set_line(1, "", false);
    rig.frame();

    let mut expected = CLEAR.to_vec();
    expected.extend([0x1B, 0x40, 0x00, 0x00, 0x00, 0x00, 0x9F, 0x02, 0x1B, 0x41, 0x00, 0x1B, 0x50, 0x1C]);
    expected.extend(PLEASE_WAIT);
    expected.extend([b' '; 6]);
    expected.extend([0x1B, 0x51]);
    assert_eq!(rig.last_packet(), expected);
}

#[test]
fn the_self_test_walks_both_lines_and_completes_once() {
    let mut rig = Rig::new(3);
    rig.frame();
    rig.start_test().unwrap();
    assert!(rig.display.is_mode_a());
    assert_eq!(rig.start_test().unwrap_err().code, 3, "the screen is in use");

    rig.frame();
    assert_eq!(rig.last_packet(), pattern_packet(0x00));

    rig.advance(4999);
    rig.frame();
    assert_eq!(rig.panel().packets().len(), 2, "the pattern stays for 5 s");

    rig.advance(1);
    rig.frame();
    assert_eq!(rig.last_packet(), pattern_packet(0x02));
    assert_eq!(rig.complete_events(), 0);

    rig.advance(5000);
    rig.frame();
    assert_eq!(rig.complete_events(), 1);
    assert!(!rig.display.is_mode_a());
    rig.frame();
    assert_eq!(rig.last_packet(), CLEAR.to_vec(), "screen B (blank) is drawn again");
    rig.advance(20_000);
    rig.frame();
    assert_eq!(rig.complete_events(), 0);
    rig.start_test().unwrap();
}

#[test]
fn cancel_ends_the_test_on_the_next_frame() {
    let mut rig = Rig::new(3);
    rig.frame();
    rig.start_test().unwrap();
    rig.frame();
    rig.display.cancel_test();
    assert_eq!(rig.complete_events(), 0);
    rig.frame();
    assert_eq!(rig.complete_events(), 1);
    rig.display.cancel_test();
    rig.frame();
    assert_eq!(rig.complete_events(), 0);
}

#[test]
fn a_dead_panel_releases_the_port_until_the_module_restarts() {
    let mut rig = Rig::new(3);
    rig.frame();
    rig.panel().set_write_fails(true);
    rig.display.lane_set_entry(Lane::EMoney, 7, "Suica");
    rig.display.lane_set_state(Lane::EMoney, 1);
    for _ in 0..4 {
        rig.frame();
    }
    assert!(!rig.display.ready());
    assert_eq!(rig.panel().open_port(), None, "two consecutive errors close the port");
    assert_eq!(rig.start_test().unwrap_err().code, 6);

    rig.panel().set_write_fails(false);
    rig.frame();
    assert_eq!(rig.panel().open_count(), 1, "nothing retries by itself");
    rig.display.restart();
    rig.frame();
    assert_eq!(rig.panel().open_count(), 2);
    assert!(rig.display.ready());
}

#[test]
fn a_panel_that_does_not_answer_the_init_packet_is_closed_at_once() {
    let mut rig = Rig::new(3);
    rig.panel().set_hang(true);
    rig.frame();
    assert_eq!(rig.panel().open_port(), None);
    assert!(!rig.display.ready());
    assert_eq!(rig.start_test().unwrap_err().code, 6);
}

#[test]
fn shutdown_cancels_the_test_blanks_the_panel_and_closes_the_port() {
    let mut rig = Rig::new(3);
    rig.frame();
    rig.start_test().unwrap();
    rig.frame();
    rig.with_cx(|display, cx| display.terminate(cx));
    assert_eq!(rig.complete_events(), 1);
    assert_eq!(rig.last_packet(), CLEAR.to_vec());
    assert_eq!(rig.panel().open_port(), None);
    assert!(!rig.display.is_ready());
}

//! The reader driver against the simulated reader: init sequence and a Mifare (Aime) card read, checked at the
//! byte level (`spec/libam/aime.md` D, E.3.4, E.4).

use std::rc::Rc;

use libam::aime::device::{CardKind, InitStep, PollStep, Reader, ReaderConfig, ReaderPorts};
use libam::mock::devices::{SIM_CARD_NAME, SIM_CARD_VALUE};
use libam::mock::serial::SimSerial;

const COM: u16 = 4;
const ACCESS_CODE: [u8; 10] = [0x00, 0x05, 0x21, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56];

/// Steps `step` every 20 ms of simulated time until it stops answering "running".
fn run<T: PartialEq>(reader: &mut Reader, now: &mut u64, running: T, step: impl Fn(&mut Reader, u64) -> T) -> T {
    for _ in 0..2000 {
        *now += 20;
        let result = step(reader, *now);
        if result != running {
            return result;
        }
    }
    panic!("stepper did not finish");
}

#[test]
fn init_and_mifare_card_read_match_the_simulated_reader() {
    let serial = SimSerial::new();
    let sim = serial.aime_reader(COM);
    sim.place_aime_card(ACCESS_CODE);
    let ports = ReaderPorts::new(Rc::new(serial), false);
    assert_eq!(ports.open(&[u32::from(COM)]).opened, vec![COM]);
    let mut reader = ports.bind(u32::from(COM), ReaderConfig::default()).expect("open port");
    let mut now = 1000;

    let init = run(&mut reader, &mut now, InitStep::Running, |reader, now| reader.init_step(now));
    assert_eq!(init, InitStep::Done);
    let info = reader.info();
    assert_eq!(info.hw_version, "TN32MSEC003S H/W Ver3.0");
    assert_eq!(info.fw_version, "TN32MSEC003S F/W Ver1.2");
    // Reset, firmware version, hardware version, LED probe and board info, then the two Mifare keys.
    let commands: Vec<u8> = sim.requests().iter().map(|entry| entry.request.cmd).collect();
    assert_eq!(commands, [0x62, 0x30, 0x32, 0xF5, 0xF0, 0x54, 0x50]);

    reader.start_polling(0x03, 0, now);
    let poll = run(&mut reader, &mut now, PollStep::Running, |reader, now| reader.poll_step(now));
    assert_eq!(poll, PollStep::Finished);

    let scan = reader.scan();
    assert_eq!((scan.card_count, scan.auth_kind), (1, 1));
    let card = &scan.cards[0];
    assert_eq!((card.kind, card.valid), (CardKind::Mifare, true));
    assert_eq!(card.uid, [0x04, 0xD0, 0xE0, 0x11]);
    assert_eq!(card.access_code, ACCESS_CODE);
    assert_eq!(card.name, SIM_CARD_NAME);
    assert_eq!(card.value, SIM_CARD_VALUE);

    // Start polling, detect, select, authenticate with key B, read blocks 0..2, stop polling.
    let sent = sim.requests();
    let tail: Vec<u8> = sent[7..].iter().map(|entry| entry.request.cmd).collect();
    assert_eq!(tail, [0x40, 0x42, 0x43, 0x55, 0x52, 0x52, 0x52, 0x41]);
    assert_eq!(sent[7].request.payload, [0x03]);
    assert_eq!(sent[10].request.payload, [0x04, 0xD0, 0xE0, 0x11, 0x03]);
}

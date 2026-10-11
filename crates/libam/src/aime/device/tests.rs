//! Driver tests over the simulated reader: card flows, firmware updates, timeouts.

use std::rc::Rc;

use super::lites::descramble;
use super::wire::cmd;
use super::*;
use crate::aime::code::access_code;
use crate::mock::devices::{SIM_CARD_NAME, SIM_CARD_VALUE, SIM_LED_VERSION, SimAimeReader, SimCard};
use crate::mock::serial::SimSerial;

const COM: u16 = 3;
const IDM: [u8; 8] = [0x01, 0x2E, 0x4F, 0x55, 0x66, 0x77, 0x88, 0x99];

struct Rig {
    sim: SimAimeReader,
    ports: ReaderPorts,
    reader: Reader,
    now: u64,
}

impl Rig {
    fn new() -> Self {
        Self::with(SimSerial::new())
    }

    fn with(serial: SimSerial) -> Self {
        let sim = serial.aime_reader(COM);
        let ports = ReaderPorts::new(Rc::new(serial), false);
        assert_eq!(ports.open(&[u32::from(COM)]).opened, vec![COM]);
        let reader = ports.bind(u32::from(COM), ReaderConfig::default()).expect("port open");
        Self { sim, ports, reader, now: 1000 }
    }

    /// Steps `f` every 20 ms until it stops answering `Running` (`pending` says so).
    fn run<T>(&mut self, mut step: impl FnMut(&mut Reader, u64) -> T, running: impl Fn(&T) -> bool) -> T {
        for _ in 0..2000 {
            self.now += 20;
            let result = step(&mut self.reader, self.now);
            if !running(&result) {
                return result;
            }
        }
        panic!("stepper did not finish");
    }

    fn init(&mut self) -> InitStep {
        self.run(|reader, now| reader.init_step(now), |step| *step == InitStep::Running)
    }

    fn scan(&mut self, filter: u8, timeout_ms: u64) -> (PollStep, ScanResult) {
        self.reader.start_polling(filter, timeout_ms, self.now);
        let step = self.run(|reader, now| reader.poll_step(now), |step| *step == PollStep::Running);
        (step, self.reader.scan())
    }

    fn commands(&self) -> Vec<u8> {
        self.sim.requests().iter().map(|entry| entry.request.cmd).collect()
    }
}

#[test]
fn init_identifies_the_reader_and_loads_both_keys() {
    let mut rig = Rig::new();
    assert_eq!(rig.init(), InitStep::Done);
    let info = rig.reader.info();
    assert_eq!(info.generation, 1);
    assert_eq!(info.fw_kind, 2);
    assert_eq!(info.fw_version, "TN32MSEC003S F/W Ver1.2");
    assert_eq!(info.hw_version, "TN32MSEC003S H/W Ver3.0");
    assert!(info.led_present);
    assert_eq!(info.led_board_name, "15084");
    assert_eq!(info.led_fw_version, SIM_LED_VERSION);
    assert_eq!(rig.commands(), [0x62, 0x30, 0x32, 0xF5, 0xF0, 0x54, 0x50]);
    let requests = rig.sim.requests();
    assert_eq!(requests[5].request.payload, b"WCCFv2");
    assert_eq!(requests[6].request.payload, [0x60, 0x90, 0xD0, 0x06, 0x32, 0xF5]);
}

#[test]
fn init_without_led_board_goes_on_and_keeps_the_default_name() {
    let serial = SimSerial::new();
    serial.aime_reader(COM).remove_led_board();
    let mut rig = Rig::with(serial);
    assert_eq!(rig.init(), InitStep::Done);
    let info = rig.reader.info();
    assert!(!info.led_present);
    assert_eq!(info.led_board_name, "No LED Board");
    assert!(rig.reader.set_led_colour([1, 2, 3]).is_ok());
    assert!(!rig.commands().contains(&cmd::LED_COLOUR));
}

#[test]
fn a_dead_reader_fails_init_with_a_unit_error() {
    let serial = SimSerial::new();
    serial.aime_reader(COM).silence();
    let mut rig = Rig::with(serial);
    assert_eq!(rig.init(), InitStep::Failed);
    assert!(rig.reader.unit_error());
    rig.reader.clear_unit_error();
    assert!(!rig.reader.unit_error());
}

#[test]
fn aime_card_is_read_through_key_b() {
    let mut rig = Rig::new();
    let code = [0x03, 0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56, 0x78];
    rig.sim.place_aime_card(code);
    assert_eq!(rig.init(), InitStep::Done);
    let (step, scan) = rig.scan(0x03, 0);
    assert_eq!(step, PollStep::Finished);
    assert_eq!(scan.card_count, 1);
    assert_eq!(scan.auth_kind, 1);
    let card = &scan.cards[0];
    assert_eq!((card.kind, card.valid), (CardKind::Mifare, true));
    assert_eq!(card.uid, [0x04, 0xD0, 0xE0, 0x11]);
    assert_eq!(card.access_code, code);
    assert_eq!(card.name, SIM_CARD_NAME);
    assert_eq!(card.value, SIM_CARD_VALUE);
    let commands = rig.commands();
    assert_eq!(commands[7..], [0x40, 0x42, 0x43, 0x55, 0x52, 0x52, 0x52, 0x41]);
    let requests = rig.sim.requests();
    assert_eq!(requests[7].request.payload, [0x03]);
    assert_eq!(requests[9].request.payload, [0x04, 0xD0, 0xE0, 0x11]);
    assert_eq!(requests[10].request.payload, [0x04, 0xD0, 0xE0, 0x11, 0x03]);
    assert_eq!(requests[13].request.payload, [0x04, 0xD0, 0xE0, 0x11, 0x02]);
}

#[test]
fn banapass_falls_back_to_key_a_and_decodes_block_1() {
    let mut rig = Rig::new();
    rig.sim.place_card(SimCard::Banapass { uid: [1, 2, 3, 4], value: 12_345_678, slot: 2 });
    assert_eq!(rig.init(), InitStep::Done);
    let (step, scan) = rig.scan(0x03, 0);
    assert_eq!(step, PollStep::Finished);
    assert_eq!(scan.auth_kind, 2);
    assert_eq!(scan.cards[0].access_code, access_code(12_345_678, 2));
    assert!(scan.cards[0].valid);
    // Key B was refused, the card was selected again and key A worked.
    assert_eq!(rig.commands()[7..], [0x40, 0x42, 0x43, 0x55, 0x42, 0x43, 0x51, 0x52, 0x52, 0x41]);
}

#[test]
fn an_empty_field_with_a_time_limit_ends_with_no_card() {
    let mut rig = Rig::new();
    assert_eq!(rig.init(), InitStep::Done);
    let (step, scan) = rig.scan(0x03, 600);
    assert_eq!(step, PollStep::Finished);
    assert_eq!(scan.card_count, 0);
    // The reader was polled again after the empty scans.
    assert!(rig.commands().iter().filter(|&&command| command == 0x40).count() > 1);
}

#[test]
fn removing_the_card_between_scans_is_seen() {
    let mut rig = Rig::new();
    rig.sim.place_aime_card([0x01; 10]);
    assert_eq!(rig.init(), InitStep::Done);
    assert_eq!(rig.scan(0x03, 0).1.card_count, 1);
    rig.sim.remove_card();
    assert_eq!(rig.scan(0x03, 300).1.card_count, 0);
}

#[test]
fn a_stop_request_ends_an_untimed_scan() {
    let mut rig = Rig::new();
    assert_eq!(rig.init(), InitStep::Done);
    rig.reader.start_polling(0x03, 0, rig.now);
    for _ in 0..30 {
        rig.now += 20;
        assert_eq!(rig.reader.poll_step(rig.now), PollStep::Running);
    }
    rig.reader.request_stop();
    let step = rig.run(|reader, now| reader.poll_step(now), |step| *step == PollStep::Running);
    assert_eq!(step, PollStep::Stopped);
}

#[test]
fn felica_lite_authenticates_with_the_mac_and_reads_the_banapass_block() {
    let mut rig = Rig::new();
    rig.sim.place_card(SimCard::FeliCaLite { idm: IDM, value: 2_000_000, slot: 5 });
    assert_eq!(rig.init(), InitStep::Done);
    let (step, scan) = rig.scan(0x02, 0);
    assert_eq!(step, PollStep::Finished);
    assert_eq!(scan.felica_error, 0);
    assert_eq!(scan.felica_kind, Some(FelicaKind::Lite));
    let card = &scan.cards[0];
    assert_eq!(card.kind, CardKind::FeliCa);
    assert_eq!(card.idm_pmm[..8], IDM);
    assert_eq!(card.access_code, access_code(2_000_000, 5));
    let commands = rig.commands();
    assert_eq!(commands[7..], [0x40, 0x42, 0x41, 0x71, 0x71, 0x71, 0x71, 0x71]);
    let frames: Vec<_> = rig
        .sim
        .requests()
        .into_iter()
        .filter(|entry| entry.request.cmd == 0x71)
        .map(|entry| entry.request.payload)
        .collect();
    // Every through frame starts with the IDm; then `len code ...`.
    assert!(frames.iter().all(|frame| frame[..8] == IDM));
    assert_eq!(frames[0][8..], [0x06, 0x00, 0xFF, 0xFF, 0x01, 0x0F]);
    assert_eq!(frames[1][8..10], [0x10, 0x06]);
    assert_eq!(frames[1][18..], [0x01, 0x0B, 0x00, 0x01, 0x80, 0x82]);
    assert_eq!(frames[2][8..10], [0x20, 0x08]);
    assert_eq!(frames[2].len(), 8 + 0x20);
    assert_eq!(frames[3][8..10], [0x12, 0x06]);
    assert_eq!(frames[4][18..], [0x01, 0x0B, 0x00, 0x01, 0x80, 0x0D]);
}

#[test]
fn felica_standard_is_read_from_block_0() {
    let mut rig = Rig::new();
    rig.sim.place_card(SimCard::FeliCaStandard { idm: IDM, value: 77, slot: 1 });
    assert_eq!(rig.init(), InitStep::Done);
    let (step, scan) = rig.scan(0x02, 0);
    assert_eq!(step, PollStep::Finished);
    assert_eq!(scan.felica_kind, Some(FelicaKind::Standard));
    assert_eq!(scan.auth_kind, 2);
    assert_eq!(scan.cards[0].access_code, access_code(77, 1));
    assert_eq!(rig.commands()[7..], [0x40, 0x42, 0x41, 0x71, 0x71]);
}

#[test]
fn felica_lite_s_keeps_the_descrambled_value_and_no_local_code() {
    let mut rig = Rig::new();
    let block0: [u8; 16] = std::array::from_fn(|index| (index * 17) as u8);
    rig.sim.place_card(SimCard::FeliCaLiteS { idm: IDM, block0 });
    assert_eq!(rig.init(), InitStep::Done);
    let (step, scan) = rig.scan(0x02, 0);
    assert_eq!(step, PollStep::Finished);
    assert_eq!(scan.felica_kind, Some(FelicaKind::LiteS));
    let lite_s = &scan.cards[0].lite_s;
    let (value, nibble) = descramble(&block0);
    assert_eq!((lite_s.value, lite_s.nibble), (value, nibble));
    assert_eq!(lite_s.blocks[1], [0x86; 16]);
    assert_eq!(lite_s.block91, [0x91; 8]);
    assert_eq!(lite_s.rc, rig.sim_rc_of_last_write());
    assert_eq!(scan.cards[0].access_code, [0; 10]);
}

impl Rig {
    /// The random challenge of the last `0x08` write the sim saw.
    fn sim_rc_of_last_write(&self) -> [u8; 16] {
        let requests = self.sim.requests();
        let write = requests
            .iter()
            .rev()
            .find(|entry| entry.request.cmd == 0x71 && entry.request.payload.get(9) == Some(&0x08))
            .expect("a write frame");
        write.request.payload[8 + 16..].try_into().expect("16 bytes")
    }
}

/// An Intel-HEX file of `lines` records, first generation layout: HW string at image offset 104, FW string after it.
fn hex_file(hw: &str, fw: &str, lines: usize) -> Vec<u8> {
    assert!(lines >= 10, "the version strings sit at image offset 104");
    let mut image = vec![0u8; lines * 16];
    image[104..104 + hw.len()].copy_from_slice(hw.as_bytes());
    image[105 + hw.len()..105 + hw.len() + fw.len()].copy_from_slice(fw.as_bytes());
    let mut text = String::new();
    for (index, chunk) in image.chunks(16).enumerate() {
        let mut record = vec![chunk.len() as u8, (index >> 4) as u8, (index << 4) as u8, 0];
        record.extend_from_slice(chunk);
        let sum = record.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte));
        record.push(sum.wrapping_neg());
        text.push(':');
        text.extend(record.iter().map(|byte| format!("{byte:02X}")));
        text.push('\n');
    }
    text.into_bytes()
}

fn drive_firmware(rig: &mut Rig) -> FirmwareStep {
    rig.run(|reader, now| reader.firmware_step(now), |step| *step == FirmwareStep::Running)
}

#[test]
fn first_generation_firmware_is_sent_line_by_line_when_the_versions_differ() {
    let mut rig = Rig::new();
    let files =
        FirmwareFiles { hex: Some(hex_file("TN32MSEC003S H/W Ver3.0", "F/W Ver1.3", 12)), ..Default::default() };
    rig.reader.load_firmware(&files, false).expect("valid files");
    assert_eq!(drive_firmware(&mut rig), FirmwareStep::Done);
    assert_eq!(
        rig.commands(),
        [0x62, 0x30, 0x32, 0x60, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61, 0x61]
    );
    let lines: Vec<String> =
        String::from_utf8(files.hex.expect("hex")).expect("ascii").lines().map(str::to_owned).collect();
    let sent: Vec<String> = rig
        .sim
        .requests()
        .iter()
        .filter(|entry| entry.request.cmd == 0x61)
        .map(|entry| String::from_utf8(entry.request.payload.clone()).expect("ascii"))
        .collect();
    assert_eq!(sent, lines);
    assert_eq!(rig.reader.firmware_progress(), (12, 12));
}

#[test]
fn first_generation_firmware_that_matches_is_left_alone() {
    let mut rig = Rig::new();
    let files = FirmwareFiles {
        hex: Some(hex_file("TN32MSEC003S H/W Ver3.0", "TN32MSEC003S F/W Ver1.2", 10)),
        ..Default::default()
    };
    rig.reader.load_firmware(&files, false).expect("valid files");
    assert_eq!(drive_firmware(&mut rig), FirmwareStep::Done);
    assert_eq!(rig.commands(), [0x62, 0x30, 0x32]);
}

fn image(version: u8) -> Vec<u8> {
    let mut image = vec![0u8; 12288];
    image[12285] = version;
    image
}

#[test]
fn second_generation_reader_with_the_same_version_byte_needs_no_update() {
    let serial = SimSerial::new();
    serial.aime_reader(COM).set_generation(2);
    let mut rig = Rig::with(serial);
    let files =
        FirmwareFiles { bin_gen2: Some(image(0x94)), hex: Some(hex_file("A H/W", "B", 10)), ..Default::default() };
    rig.reader.load_firmware(&files, true).expect("valid files");
    assert_eq!(drive_firmware(&mut rig), FirmwareStep::AlreadyNew);
}

#[test]
fn second_generation_image_goes_out_in_48_blocks() {
    let serial = SimSerial::new();
    serial.aime_reader(COM).set_generation(2);
    let mut rig = Rig::with(serial);
    let mut bin = image(0x95);
    bin[..256].fill(0xA5);
    let files = FirmwareFiles { bin_gen2: Some(bin), hex: Some(hex_file("A H/W", "B", 10)), ..Default::default() };
    rig.reader.load_firmware(&files, true).expect("valid files");
    assert_eq!(drive_firmware(&mut rig), FirmwareStep::Done);
    let blocks: Vec<_> = rig
        .sim
        .requests()
        .into_iter()
        .filter(|entry| entry.request.cmd == 0x64)
        .map(|entry| entry.request.payload)
        .collect();
    assert_eq!(blocks.len(), 48);
    assert!(blocks.iter().all(|block| block.len() == 256));
    assert_eq!(blocks[0], [0xA5; 256]);
    assert_eq!(rig.reader.firmware_progress().1, 48);
}

#[test]
fn firmware_files_are_validated() {
    let mut rig = Rig::new();
    assert_eq!(rig.reader.load_firmware(&FirmwareFiles::default(), false), Err(FirmwareFileError::Missing));
    let short = FirmwareFiles { bin_gen2: Some(vec![0; 100]), ..Default::default() };
    assert_eq!(rig.reader.load_firmware(&short, false), Err(FirmwareFileError::BadImage));
    let mut hex = hex_file("A", "B", 10);
    let last = hex.len() - 3;
    hex[last] = b'0';
    let broken = FirmwareFiles { hex: Some(hex), ..Default::default() };
    assert_eq!(rig.reader.load_firmware(&broken, false), Err(FirmwareFileError::BadHex));
}

#[test]
fn led_firmware_is_updated_when_the_board_version_differs() {
    let mut rig = Rig::new();
    let lines = hex_file("x", "y", 10);
    rig.reader.load_led_firmware("837_15084_1_02.hex", &lines, false).expect("valid");
    let step = rig.run(|reader, now| reader.led_firmware_step(now), |step| *step == LedFirmwareStep::Running);
    assert_eq!(step, LedFirmwareStep::Done);
    let mut expected = vec![0xF5, 0xF0, 0xF4];
    expected.extend([0xF3; 10]);
    assert_eq!(rig.commands(), expected);
    assert_eq!(rig.sim.requests()[3].request.payload[0], b':');
    assert_eq!(rig.reader.info().led_fw_version, 0x12);
}

#[test]
fn led_firmware_that_is_current_is_not_sent() {
    let mut rig = Rig::new();
    rig.reader.load_led_firmware("837_15084_1_01.hex", &hex_file("x", "y", 10), false).expect("valid");
    let step = rig.run(|reader, now| reader.led_firmware_step(now), |step| *step == LedFirmwareStep::Running);
    assert_eq!(step, LedFirmwareStep::Done);
    assert_eq!(rig.commands(), [0xF5, 0xF0]);
}

#[test]
fn led_commands_carry_the_board_and_the_colour() {
    let mut rig = Rig::new();
    assert_eq!(rig.init(), InitStep::Done);
    rig.reader.set_led_pattern(5).expect("queued");
    rig.ports.tick(rig.now + 20);
    let requests = rig.sim.requests();
    let led = requests.last().expect("a request");
    assert_eq!((led.request.cmd, led.request.dst), (0x81, 0x08));
    assert_eq!(led.request.payload, [0x00, 0x96, 0xFF]);
    // The cancelled state drops colours but not "all off".
    rig.reader.cancel_led();
    rig.reader.set_led_colour([9, 9, 9]).expect("dropped");
    rig.ports.tick(rig.now + 40);
    assert_eq!(rig.sim.requests().len(), requests.len());
    rig.reader.set_led_colour([0, 0, 0]).expect("queued");
    rig.ports.tick(rig.now + 60);
    assert_eq!(rig.sim.requests().last().map(|entry| entry.request.payload.clone()), Some(vec![0, 0, 0]));
}

#[test]
fn ports_that_fail_to_open_cannot_be_bound() {
    let serial = SimSerial::new();
    serial.aime_reader(7).fail_open();
    let ports = ReaderPorts::new(Rc::new(serial), true);
    let report = ports.open(&[3, 1007, 7, 1003]);
    assert_eq!(report.opened, vec![3]);
    assert_eq!(report.failed, vec![7]);
    assert!(ports.bind(1007, ReaderConfig::default()).is_none());
    assert!(ports.bind(1003, ReaderConfig::default()).is_some());
    assert_eq!(ports.close(), 0);
}

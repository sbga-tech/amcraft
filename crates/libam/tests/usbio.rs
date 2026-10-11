//! HID report bytes in and out of the USB I/O manager, against the simulated board (`spec/libam/usbio.md` 1.3-1.6).

use std::fs;

use backend::hid::{REPORT_LEN, Report};
use backend::mock::hid::product_string;
use backend::mock::{SimFs, SimHid, SimRegistry, SimServices};
use backend::{Fs, RegValue};
use libam::Libam;
use libam::config::{Pair, UsbIoFuncs, UsbIoNode, WideStr};
use libam::interface::{AmInterface, CabinetAppConfig, LibamEvent, LibamwConfig};
use libam::mock::{SIMULATED_RESOURCE_PATH, SimBackend, SimClock};
use libam::nvram::{NvramContext, Record};
use libam::result::AmError;
use libam::usbio::{BoardIoPort, FirmwareEvent, FirmwareStatus, UsbDeviceEvent, UsbDevicePort, UsbIoPort};
use tempfile::TempDir;

/// The functions the simulated board advertises (hexadecimal parameters): one player with 10 buttons, two coin
/// slots, two 10-bit analog channels, two rotary channels, 16 general outputs, 3 PWM outputs.
const FUNCTIONS: &str = "SWIN=1,A_COININ=2_ADIN=2,A_ROTIN=2_GOUT=10_PWMOUT=3";

/// The DIP byte of the `DipSwitch` record the cabinet starts with.
const DIP: u8 = 0x2A;

/// The key of the helper service the USB reconnect talks to.
const SGWORKER: &str = r"System\Sega\SystemProperty\sgworker";

/// Writes `value` as the DIP byte (offset 8) of the `DipSwitch` record of the system file below `root`, through
/// the same NVRAM layer libam reads it with (so both copies carry a valid CRC).
fn write_dip(root: &std::path::Path, value: u8) {
    let (mut backend, _sim) = SimBackend::new(root);
    let mut nvram = NvramContext::new();
    nvram.acquire(&mut backend).expect("system file");
    let mut image = Record::DipSwitch.blank_image();
    image[8] = value;
    nvram.write_record(Record::DipSwitch, &image).expect("DipSwitch record");
}

struct Cabinet {
    _libam: Libam,
    interface: Box<dyn AmInterface>,
    usb_io: Box<dyn UsbIoPort>,
    usb_device: Box<dyn UsbDevicePort>,
    board_io: Box<dyn BoardIoPort>,
    sim: SimHid,
    registry: SimRegistry,
    clock: SimClock,
    services: SimServices,
    _root: TempDir,
}

fn node(id: u8) -> UsbIoNode {
    UsbIoNode {
        id,
        comm_timeout: Some(10),
        pwm_cycle: Some(20),
        funcs: UsbIoFuncs {
            switch_input: Some(Pair { a: 1, b: 10 }),
            coin_input: Some(2),
            analog_input: Some(Pair { a: 2, b: 10 }),
            rotary_input: Some(2),
            general_output: Some(16),
            pwm_output: Some(3),
            ..UsbIoFuncs::default()
        },
        ..UsbIoNode::default()
    }
}

fn config() -> CabinetAppConfig {
    let mut config = CabinetAppConfig::default();
    config.am.common.resource.resource_path = WideStr::new(SIMULATED_RESOURCE_PATH).expect("resource path");
    config.am.common.resource.language = WideStr::new("en").expect("language");
    config.am.usbio.enable = true;
    config.am.usbio.nodes.push(node(1)).expect("node");
    config.sinks.usb_io_detect_node = true;
    config.controllers.usb_device.listener_installed = true;
    config
}

/// A board with node id 1 and the functions above.
fn add_board(sim: &SimHid) -> usize {
    sim.add_board("SEGA", &product_string("BD001", 1, 2, 0, "CHIP", 1, FUNCTIONS))
}

fn input_report() -> Report {
    let mut report = [0; REPORT_LEN];
    report[0] = 1;
    let p = &mut report[1..];
    p[0..2].copy_from_slice(&0x0123u16.to_le_bytes()); // analog 0
    p[2..4].copy_from_slice(&0x0345u16.to_le_bytes()); // analog 1
    p[16..18].copy_from_slice(&7u16.to_le_bytes()); // rotary 0
    p[24..28].copy_from_slice(&[0, 5, 1, 2]); // coin 0: flags 0 counter 5, coin 1: jam, counter 2
    p[28] = 0x81; // player 0, low byte
    p[29] = 0xFF; // player 0, high byte (the system bits are its low two)
    report
}

impl Cabinet {
    /// A cabinet with one board plugged in, run until usbio is set up.
    fn operating(sim_setup: impl FnOnce(&SimHid)) -> (Self, Vec<LibamEvent>) {
        Self::operating_with(|sim| sim_setup(&sim.hid), |_| {})
    }

    /// As [`Self::operating`], with the configuration adjusted by `tweak`.
    fn operating_with(
        sim_setup: impl FnOnce(&SimBackend),
        tweak: impl FnOnce(&mut CabinetAppConfig),
    ) -> (Self, Vec<LibamEvent>) {
        let root = tempfile::tempdir().expect("temporary directory");
        let (backend, sim) = SimBackend::new(root.path());
        write_dip(root.path(), DIP);
        let errors = r#"[
          {"number": 6401, "sub": 0, "reset": "test", "message": "I/O board communication error", "tag": ""},
          {"number": 6402, "sub": 0, "reset": "test", "message": "I/O board function missing", "tag": ""}
        ]"#;
        fs::write(root.path().join("r").join("res").join("error.en.json"), errors).expect("error resource");
        sim_setup(&sim);
        let (usbio_sim, registry, clock, services) =
            (sim.hid.clone(), sim.registry.clone(), sim.clock.clone(), sim.services.clone());
        let libam = Libam::new(backend, Box::new(sim.clock.clone()));
        let interface = libam.interface();
        let (usb_io, usb_device, board_io) = (libam.usb_io(), libam.usb_device(), libam.board_io());
        let mut cabinet = Self {
            _libam: libam,
            interface,
            usb_io,
            usb_device,
            board_io,
            sim: usbio_sim,
            registry,
            clock,
            services,
            _root: root,
        };
        cabinet.interface.initialize(&LibamwConfig::default()).expect("initialize");
        for _ in 0..20 {
            let events = cabinet.interface.execute();
            if events.iter().any(|event| matches!(event, LibamEvent::LoadCabinetType(_))) {
                break;
            }
        }
        let mut app_config = config();
        tweak(&mut app_config);
        cabinet.interface.load_cabinet_type_result(true, app_config);
        let events = cabinet.run(30);
        (cabinet, events)
    }

    fn run(&mut self, frames: usize) -> Vec<LibamEvent> {
        (0..frames).flat_map(|_| self.interface.execute()).collect()
    }
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

/// `(command, payload)` of every write, with the report id checked.
fn commands(writes: &[Report], payload_len: usize) -> Vec<(u8, Vec<u8>)> {
    writes
        .iter()
        .map(|report| {
            assert_eq!(report[0], 0x10, "output report id");
            (report[1], report[2..2 + payload_len].to_vec())
        })
        .collect()
}

#[test]
fn handshake_writes_the_setup_commands_and_input_reports_are_decoded() {
    let (mut cabinet, events) = Cabinet::operating(|sim| {
        let board = add_board(sim);
        sim.set_input(board, input_report());
    });
    assert!(fatal_errors(&events).is_empty(), "{events:?}");
    assert_eq!(
        events.iter().filter(|event| matches!(event, LibamEvent::UsbIoDetectNode)).count(),
        1,
        "DetectNode once the node is set up: {events:?}"
    );

    // ClearBoardStatus, comm timeout 0, sampling count 6, then the zeroed outputs (16 general bits, PWM cycle 20).
    let writes = commands(&cabinet.sim.interrupt_writes(0), 11);
    let sent: Vec<u8> = writes.iter().map(|(command, _)| *command).collect();
    assert_eq!(sent, [0x03, 0x01, 0x02, 0x04, 0x05], "{writes:?}");
    assert_eq!(writes[1].1[0], 0);
    assert_eq!(writes[2].1[0], 6);
    assert_eq!(writes[3].1[..2], [0, 0]);
    assert_eq!(writes[4].1[..10], [20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert!(cabinet.sim.control_writes(0).is_empty());

    assert_eq!(cabinet.usb_io.node_count(), 1);
    let switch = cabinet.usb_io.node_switch(0).expect("switch");
    assert_eq!(switch.player_count, 1);
    assert_eq!(switch.system, 3);
    assert_eq!(switch.player_masks[0], 0xC081, "10 buttons: the bits above bit 9 are cleared");
    let analog = cabinet.usb_io.node_analog(0).expect("analog");
    assert_eq!(analog.count, 2);
    assert_eq!([analog.channels[0].value, analog.channels[1].value], [0x0123, 0x0345]);
    assert_eq!(analog.channels[0].valid_bits, 10);
    let rotary = cabinet.usb_io.node_rotary(0).expect("rotary");
    assert_eq!((rotary.count, rotary.values[0]), (2, 7));
    let coins = cabinet.usb_io.node_unique_descriptors(0).expect("coins");
    assert_eq!(coins.count, 2);
    assert_eq!((coins.entries[0].flags, coins.entries[0].code), (0, 5));
    assert_eq!((coins.entries[1].flags, coins.entries[1].code), (1, 2));

    // A later report replaces the values on the next frame.
    let mut report = input_report();
    report[1] = 0x77;
    cabinet.sim.set_input(0, report);
    cabinet.run(1);
    assert_eq!(cabinet.usb_io.node_analog(0).expect("analog").channels[0].value, 0x0177);
}

#[test]
fn outputs_go_out_on_the_next_frame_in_command_order() {
    let (mut cabinet, _) = Cabinet::operating(|sim| {
        add_board(sim);
    });
    let before = cabinet.sim.interrupt_writes(0).len();
    cabinet.usb_io.send_general_output(0, 0x1234, 2).expect("general output");
    cabinet.usb_io.send_pwm_output(0, 1, 0x80).expect("pwm output");
    cabinet.run(1);

    let writes = commands(&cabinet.sim.interrupt_writes(0)[before..], 11);
    let sent: Vec<u8> = writes.iter().map(|(command, _)| *command).collect();
    assert_eq!(sent, [0x01, 0x04, 0x05], "the comm timeout is armed by the first output: {writes:?}");
    assert_eq!(writes[0].1[0], 10);
    assert_eq!(writes[1].1[..2], [0x34, 0x12]);
    assert_eq!(writes[2].1[..10], [20, 0, 0x80, 0, 0, 0, 0, 0, 0, 0]);

    // Nothing is pending: the next frame sends nothing.
    cabinet.run(1);
    assert_eq!(cabinet.sim.interrupt_writes(0).len(), before + 3);

    let error = cabinet.usb_io.send_general_output(3, 1, 1).expect_err("node 3 does not exist");
    assert_eq!(error.code, AmError::INVALID_ARGUMENT);
    // Slot 3 is beyond the three PWM outputs the node requires.
    assert!(cabinet.usb_io.send_pwm_output(0, 3, 1).is_err());
}

#[test]
fn sixty_consecutive_failed_frames_raise_the_communication_error() {
    let (mut cabinet, _) = Cabinet::operating(|sim| {
        add_board(sim);
    });
    cabinet.sim.with_board(0, |board| board.streaming = false);
    assert!(fatal_errors(&cabinet.run(59)).is_empty());
    assert_eq!(fatal_errors(&cabinet.run(1)), [0x1901], "board communication error on the 60th failed frame");
}

#[test]
fn failures_are_forgiven_by_a_good_frame_until_one_fifth_of_the_last_600_frames_failed() {
    let (mut cabinet, _) = Cabinet::operating(|sim| {
        add_board(sim);
    });
    let stream = |cabinet: &Cabinet, on: bool| cabinet.sim.with_board(0, |board| board.streaming = on);
    for _ in 0..2 {
        stream(&cabinet, false);
        assert!(fatal_errors(&cabinet.run(59)).is_empty());
        stream(&cabinet, true);
        assert!(fatal_errors(&cabinet.run(1)).is_empty());
    }
    // 118 of the last 600 frames failed; the limit is 120.
    stream(&cabinet, false);
    assert!(fatal_errors(&cabinet.run(1)).is_empty());
    assert_eq!(fatal_errors(&cabinet.run(1)), [0x1901]);
}

#[test]
fn a_missing_board_raises_6401_and_the_node_stays_empty() {
    let (cabinet, events) = Cabinet::operating(|_| {});
    assert_eq!(fatal_errors(&events), [0x1901], "{events:?}");
    assert_eq!(cabinet.usb_io.node_count(), 1, "the configured count is reported whatever boards exist");
    assert_eq!(cabinet.usb_io.node_info(0).expect("node").revision, 0);
}

#[test]
fn a_board_that_lacks_a_required_function_raises_6402() {
    let (_, events) = Cabinet::operating(|sim| {
        sim.add_board("SEGA", &product_string("BD001", 1, 2, 0, "CHIP", 1, "SWIN=1,A_COININ=2"));
    });
    assert_eq!(fatal_errors(&events), [0x1902], "{events:?}");
}

#[test]
fn boards_of_another_maker_are_ignored() {
    let (_, events) = Cabinet::operating(|sim| {
        sim.add_board("OTHER", &product_string("BD001", 1, 2, 0, "CHIP", 1, FUNCTIONS));
    });
    assert_eq!(fatal_errors(&events), [0x1901], "{events:?}");
}

#[test]
fn the_dip_word_comes_from_the_record_and_the_led_word_is_kept() {
    let (mut cabinet, _) = Cabinet::operating(|sim| {
        add_board(sim);
    });
    assert!(cabinet.board_io.systemio_facade().is_ok());
    assert_eq!(cabinet.board_io.dip(), u64::from(DIP));
    assert_eq!(cabinet.board_io.push(), 0);
    cabinet.board_io.set_led(0x55);
    assert_eq!(cabinet.board_io.led(), 0x55);
}

/// Runs frames of 100 ms until the reconnect completes and returns the events.
fn reconnect_events(cabinet: &mut Cabinet) -> Vec<UsbDeviceEvent> {
    let mut events = Vec::new();
    for _ in 0..40 {
        cabinet.clock.advance_ms(100);
        cabinet.run(1);
        events.extend(cabinet.usb_device.take_events());
    }
    events
}

#[test]
fn a_reconnect_drives_the_helper_service_and_reports_completion_once() {
    let (mut cabinet, _) = Cabinet::operating(|sim| {
        add_board(sim);
    });
    cabinet.registry.set(SGWORKER, "STATE", RegValue::Qword(0));
    cabinet.registry.set(SGWORKER, "LAST_RESULT_CODE", RegValue::Qword(0));

    cabinet.usb_device.usb_reconnect(0x0CA3, 0x0021).expect("accepted");
    let busy = cabinet.usb_device.usb_reconnect(0x0CA3, 0x0022).expect_err("one reconnect at a time");
    assert_eq!(busy.code, AmError::NOT_INITIALIZED);

    let events = reconnect_events(&mut cabinet);
    assert_eq!(events, [UsbDeviceEvent::ReconnectComplete { code: 1 }]);
    assert_eq!(cabinet.registry.get(SGWORKER, "VID"), Some(RegValue::Qword(0x0CA3)));
    assert_eq!(cabinet.registry.get(SGWORKER, "PID"), Some(RegValue::Qword(0x0021)));
    assert_eq!(cabinet.services.state.borrow().requests, [("sgmanageio".to_owned(), 0x80)]);

    // The task is cleared: a new reconnect is accepted.
    cabinet.usb_device.usb_reconnect(0x0CA3, 0x0021).expect("accepted again");
}

#[test]
fn the_service_result_code_selects_the_completion_code() {
    for (result, code) in [(3, 3), (1, 2), (0, 1)] {
        let (mut cabinet, _) = Cabinet::operating(|sim| {
            add_board(sim);
        });
        cabinet.registry.set(SGWORKER, "STATE", RegValue::Qword(0));
        cabinet.registry.set(SGWORKER, "LAST_RESULT_CODE", RegValue::Qword(result));
        cabinet.usb_device.usb_reconnect(0x0CA3, 0x0021).expect("accepted");
        assert_eq!(reconnect_events(&mut cabinet), [UsbDeviceEvent::ReconnectComplete { code }], "result {result}");
    }
}

#[test]
fn a_helper_service_that_reports_a_fatal_state_fails_the_reconnect_without_a_control_code() {
    let (mut cabinet, _) = Cabinet::operating(|sim| {
        add_board(sim);
    });
    cabinet.registry.set(SGWORKER, "STATE", RegValue::Qword(2));
    cabinet.registry.set(SGWORKER, "LAST_RESULT_CODE", RegValue::Qword(0));
    cabinet.usb_device.usb_reconnect(0x0CA3, 0x0021).expect("accepted");
    assert_eq!(reconnect_events(&mut cabinet), [UsbDeviceEvent::ReconnectComplete { code: 2 }]);
    assert!(cabinet.services.state.borrow().requests.is_empty());
}

/// One Motorola S-record line carrying `data` at address 0 (`usbio.md` 1.8.1).
fn s_record(data: &[u8]) -> String {
    let count = u8::try_from(data.len() + 3).expect("short record");
    let sum = data.iter().fold(count, |sum, byte| sum.wrapping_add(*byte));
    let hex: String = data.iter().map(|byte| format!("{byte:02X}")).collect();
    format!("S1{count:02X}0000{hex}{:02X}", !sum)
}

#[test]
fn a_board_with_other_firmware_is_updated_line_by_line_and_the_session_is_reported() {
    const PATH: &str = "firmware/fw_BD001_0000_02_0000.hex";
    let lines = [s_record(&[1, 2, 3, 4]), s_record(&[5, 6, 7, 8]), s_record(&[9, 10, 11, 12])];
    let (mut cabinet, _) = Cabinet::operating_with(
        |sim| {
            // Boot-loader mode (0), firmware revision 1: the file carries revision 2.
            let board = sim.hid.add_board("SEGA", &product_string("BD001", 0, 1, 0, "CHIP", 1, FUNCTIONS));
            sim.hid.with_board(board, |board| board.firmware_status = 0x10);
            let file = SimFs::new(sim.root.clone()).map(PATH);
            fs::create_dir_all(file.parent().expect("firmware folder")).expect("firmware folder");
            fs::write(file, lines.join("\r\n")).expect("firmware file");
        },
        |config| {
            config.am.usbio.firmware_path = WideStr::new(PATH).expect("firmware path");
            config.controllers.usb_io.firmware_listener = true;
        },
    );
    let mut events = cabinet.usb_io.take_firmware_events();
    for _ in 0..200 {
        cabinet.clock.advance_ms(100);
        cabinet.run(1);
        events.extend(cabinet.usb_io.take_firmware_events());
    }
    let status = |status| FirmwareEvent::NodeStatus { node: 0, status };
    assert_eq!(events.first(), Some(&FirmwareEvent::UpdateState { status: FirmwareStatus::Started }), "{events:?}");
    assert_eq!(events.get(1), Some(&status(FirmwareStatus::Started)), "{events:?}");
    assert!(events.contains(&status(FirmwareStatus::Finished)), "{events:?}");
    assert_eq!(events.last(), Some(&FirmwareEvent::UpdateState { status: FirmwareStatus::Finished }), "{events:?}");
    let percents: Vec<u32> = events
        .iter()
        .filter_map(|event| match event {
            FirmwareEvent::Progress { percent, .. } => Some(*percent),
            _ => None,
        })
        .collect();
    assert!(percents.windows(2).all(|pair| pair[0] <= pair[1]), "progress never goes back: {percents:?}");
    assert_eq!(percents.last(), Some(&100));

    // The transfer: start marker, every line with its length byte, then the reset command.
    let sent: Vec<(u8, Vec<u8>)> = cabinet
        .sim
        .interrupt_writes(0)
        .iter()
        .chain(&cabinet.sim.control_writes(0))
        .filter(|report| matches!(report[1], 0x85 | 0x88))
        .map(|report| (report[1], report[2..].to_vec()))
        .collect();
    let mut expected = vec![(0x85, vec![0])];
    for line in &lines {
        let text = format!("{line}\n");
        let mut data = vec![u8::try_from(text.len()).expect("length")];
        data.extend_from_slice(text.as_bytes());
        expected.push((0x85, data));
    }
    expected.push((0x88, vec![0xD9]));
    let trimmed: Vec<(u8, Vec<u8>)> = sent
        .into_iter()
        .map(|(command, mut data)| {
            let len = if command == 0x85 && data[0] != 0 { usize::from(data[0]) + 1 } else { 1 };
            data.truncate(len);
            (command, data)
        })
        .collect();
    assert_eq!(trimmed, expected);
}

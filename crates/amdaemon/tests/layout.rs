//! Executor placement: the response/request generation word of every `RequestExecutor` instance sits at its
//! recovered absolute address, reached through the checked block views (`spec/controllers/lifecycle.md` §E6.1,
//! 31 instances). Block offsets, sizes and alignment are compile-time asserts next to the schemas.

use amdaemon::schema::image::{ClientImage, ServerImage, ServerIoImage};

const SENTINEL: u32 = 0xA5C3_0F96;

/// Absolute byte offset of the sentinel written into a zeroed image.
fn located(bytes: &[u8]) -> usize {
    let needle = SENTINEL.to_le_bytes();
    let hits: Vec<usize> = bytes.windows(4).enumerate().filter(|(_, w)| *w == needle).map(|(i, _)| i).collect();
    assert_eq!(hits.len(), 1, "sentinel must appear exactly once");
    hits[0]
}

fn server_offset(write: impl FnOnce(&mut ServerImage)) -> usize {
    let mut image = ServerImage::new();
    write(&mut image);
    located(image.bytes())
}

fn client_offset(write: impl FnOnce(&mut ClientImage)) -> usize {
    let mut image = ClientImage::new();
    write(&mut image);
    located(image.bytes())
}

fn server_io_offset(write: impl FnOnce(&mut ServerIoImage)) -> usize {
    let mut image = ServerIoImage::new();
    write(&mut image);
    located(image.bytes())
}

/// Response generation words of all 31 executor instances (§E6.1 "Response ptr").
#[test]
fn every_executor_response_generation_sits_at_its_recovered_address() {
    let s = SENTINEL;
    let mut found = vec![
        (
            "accounting",
            server_offset(|d| {
                d.allnet_accounting_mut().response_mut().header_mut().generation_mut().set(s).unwrap();
            }),
            0x0000,
        ),
        ("sequence", server_offset(|d| d.sequence_mut().response_mut().generation_mut().set(s).unwrap()), 0x0058),
        ("backup", server_offset(|d| d.backup_mut().response_mut().generation_mut().set(s).unwrap()), 0x0268),
        (
            "net_delivery",
            server_offset(|d| d.net_delivery_mut().response_mut().generation_mut().set(s).unwrap()),
            0x0374,
        ),
        ("aime main", server_offset(|d| d.aime_mut().response_mut().generation_mut().set(s).unwrap()), 0x03D0),
        ("credit", server_offset(|d| d.credit_mut().response_mut().generation_mut().set(s).unwrap()), 0x1B8C),
        ("emoney", server_offset(|d| d.emoney_mut().response_mut().generation_mut().set(s).unwrap()), 0x1C6C),
        ("aime_pay", server_offset(|d| d.aime_pay_mut().response_mut().generation_mut().set(s).unwrap()), 0x347C),
        ("process/common", server_offset(|d| d.process_mut().response_mut().generation_mut().set(s).unwrap()), 0x38B8),
        ("network", server_offset(|d| d.network_mut().response_mut().generation_mut().set(s).unwrap()), 0x38D4),
        ("abaas_log", server_offset(|d| d.abaas_log_mut().response_mut().generation_mut().set(s).unwrap()), 0x3D40),
        ("board_io", server_offset(|d| d.board_io_mut().led_response_mut().generation_mut().set(s).unwrap()), 0x478C),
        ("usb_device", server_offset(|d| d.usb_device_mut().response_mut().generation_mut().set(s).unwrap()), 0x4794),
        (
            "air",
            server_offset(|d| d.allnet_air_mut().payload_mut().response_mut().generation_mut().set(s).unwrap()),
            0x47A8,
        ),
        ("error", server_offset(|d| d.error_mut().response_mut().generation_mut().set(s).unwrap()), 0x56B0),
        ("apm", server_offset(|d| d.apm_mut().response_mut().generation_mut().set(s).unwrap()), 0x9020),
    ];
    for (unit, expected) in [0x03E8, 0x08E0, 0x0DD8, 0x12D0].into_iter().enumerate() {
        let actual = server_offset(|d| {
            let mut aime = d.aime_mut();
            let mut units = aime.units_mut();
            units.get_mut(unit).unwrap().response_mut().generation_mut().set(s).unwrap();
        });
        found.push(("aime unit", actual, expected));
    }
    for (port, expected) in [0x3EE4, 0x4338].into_iter().enumerate() {
        let actual = server_offset(|d| {
            let mut can = d.can_mut();
            let mut ports = can.ports_mut();
            ports.get_mut(port).unwrap().response_mut().generation_mut().set(s).unwrap();
        });
        found.push(("can port", actual, expected));
    }
    for (name, actual, expected) in found {
        assert_eq!(actual, expected, "{name} response generation");
    }
    for (node, expected) in [0x000C, 0x0230, 0x0454, 0x0678].into_iter().enumerate() {
        let actual = server_io_offset(|d| {
            let mut usb_io = d.usb_io_mut();
            let mut nodes = usb_io.nodes_mut();
            nodes.get_mut(node).unwrap().response_mut().generation_mut().set(s).unwrap();
        });
        assert_eq!(actual, expected, "usbio node {node} response generation (ServerIO)");
    }
    for (node, expected) in [0x08A4, 0x0C14, 0x0F84, 0x12F4].into_iter().enumerate() {
        let actual = server_io_offset(|d| {
            let mut jvs = d.jvs_mut();
            let mut nodes = jvs.nodes_mut();
            nodes.get_mut(node).unwrap().generation_mut().set(s).unwrap();
        });
        assert_eq!(actual, expected, "jvs node {node} response generation (ServerIO)");
    }
}

/// Request generation words of all 31 executor instances (§E6.1 "Request ptr").
#[test]
fn every_executor_request_generation_sits_at_its_recovered_address() {
    let s = SENTINEL;
    let mut found = vec![
        ("backup", client_offset(|d| d.backup_mut().generation_mut().set(s).unwrap()), 0x0000),
        ("sequence", client_offset(|d| d.sequence_mut().generation_mut().set(s).unwrap()), 0x0BB8),
        ("aime main", client_offset(|d| d.aime_mut().request_mut().generation_mut().set(s).unwrap()), 0x0BF0),
        ("credit", client_offset(|d| d.credit_mut().request_mut().generation_mut().set(s).unwrap()), 0x0C40),
        ("emoney", client_offset(|d| d.emoney_mut().generation_mut().set(s).unwrap()), 0x0C84),
        ("aime_pay", client_offset(|d| d.aime_pay_mut().generation_mut().set(s).unwrap()), 0x0CC0),
        ("accounting", client_offset(|d| d.allnet_accounting_mut().generation_mut().set(s).unwrap()), 0x0D00),
        ("abaas_log", client_offset(|d| d.abaas_log_mut().generation_mut().set(s).unwrap()), 0x0E10),
        ("usb_device", client_offset(|d| d.usb_device_mut().generation_mut().set(s).unwrap()), 0x161C),
        ("network", client_offset(|d| d.network_mut().generation_mut().set(s).unwrap()), 0x1808),
        ("net_delivery", client_offset(|d| d.net_delivery_mut().generation_mut().set(s).unwrap()), 0x1810),
        ("board_io", client_offset(|d| d.board_io_mut().led_request_mut().generation_mut().set(s).unwrap()), 0x1A1C),
        ("process/common", client_offset(|d| d.process_mut().generation_mut().set(s).unwrap()), 0x1A28),
        ("error", client_offset(|d| d.error_mut().generation_mut().set(s).unwrap()), 0x1A58),
        ("air", client_offset(|d| d.allnet_air_mut().generation_mut().set(s).unwrap()), 0x1A64),
        ("apm", client_offset(|d| d.apm_mut().generation_mut().set(s).unwrap()), 0x1A6C),
    ];
    for (unit, expected) in [0x0C10, 0x0C1C, 0x0C28, 0x0C34].into_iter().enumerate() {
        let actual = client_offset(|d| {
            let mut aime = d.aime_mut();
            let mut units = aime.units_mut();
            units.get_mut(unit).unwrap().generation_mut().set(s).unwrap();
        });
        found.push(("aime unit", actual, expected));
    }
    for (port, expected) in [0x1628, 0x1718].into_iter().enumerate() {
        let actual = client_offset(|d| {
            let mut can = d.can_mut();
            let mut ports = can.ports_mut();
            ports.get_mut(port).unwrap().request_mut().generation_mut().set(s).unwrap();
        });
        found.push(("can port", actual, expected));
    }
    for (node, expected) in [0x0218, 0x0470, 0x06C8, 0x0920].into_iter().enumerate() {
        let actual = client_offset(|d| {
            let mut usb_io = d.usb_io_mut();
            let mut nodes = usb_io.nodes_mut();
            nodes.get_mut(node).unwrap().generation_mut().set(s).unwrap();
        });
        found.push(("usbio node", actual, expected));
    }
    for (node, expected) in [0x0B78, 0x0B88, 0x0B98, 0x0BA8].into_iter().enumerate() {
        let actual = client_offset(|d| {
            let mut jvs = d.jvs_mut();
            let mut nodes = jvs.nodes_mut();
            nodes.get_mut(node).unwrap().generation_mut().set(s).unwrap();
        });
        found.push(("jvs node", actual, expected));
    }
    for (name, actual, expected) in found {
        assert_eq!(actual, expected, "{name} request generation");
    }
}

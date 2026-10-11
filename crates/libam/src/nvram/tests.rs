use std::fs;

use super::*;
use crate::mock::SimBackend;
use crate::registry_keys::MOUNT_KEY;
use backend::{Backend, RegValue};

fn open(backend: &mut Backend, sim: &SimBackend) -> NvramContext {
    fs::create_dir_all(sim.root.join("y")).unwrap();
    sim.registry.set(MOUNT_KEY, "AMFS", RegValue::Text(r"Y:\amfs".to_owned()));
    let mut nvram = NvramContext::new();
    nvram.acquire(backend).unwrap();
    nvram
}

fn image(record: Record, fill: u8) -> Vec<u8> {
    let mut image = vec![fill; record.size()];
    image[..4].fill(0);
    image
}

fn stamped(record: Record, mut image: Vec<u8>) -> Vec<u8> {
    let crc = crc32fast::hash(&image[4..]);
    image[..4].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(image.len(), record.size());
    image
}

/// `sysfile.dat` is 0x6000 bytes; a record is written as `CRC-32(bytes 4..) || body` to the mirror and then to the
/// primary, at `0x0000` and `0x3000` for CreditInfo (`storage.md` 3.4, 3.5).
#[test]
fn a_record_is_stored_with_its_crc_in_both_copies() {
    let dir = tempfile::tempdir().unwrap();
    let (mut backend, sim) = SimBackend::new(dir.path());
    let nvram = open(&mut backend, &sim);
    let file = nvram.system_file().unwrap().to_path_buf();
    assert_eq!(fs::metadata(&file).unwrap().len(), 0x6000);
    assert!(fs::read(&file).unwrap().iter().all(|&byte| byte == 0));

    let body = image(Record::CreditInfo, 0xA5);
    assert_eq!(nvram.store_record(Record::CreditInfo, &body), RecordStatus::Ok);

    let expected = stamped(Record::CreditInfo, body.clone());
    let bytes = fs::read(&file).unwrap();
    assert_eq!(&bytes[0x0000..0x0200], expected.as_slice());
    assert_eq!(&bytes[0x3000..0x3200], expected.as_slice());
    assert!(bytes[0x0200..0x3000].iter().all(|&byte| byte == 0));
    assert_eq!(nvram.read_record(Record::CreditInfo).unwrap(), expected);
}

#[test]
fn a_damaged_primary_is_healed_from_the_mirror() {
    let dir = tempfile::tempdir().unwrap();
    let (mut backend, sim) = SimBackend::new(dir.path());
    let nvram = open(&mut backend, &sim);
    let body = image(Record::CreditInfo, 0x11);
    nvram.store_record(Record::CreditInfo, &body);
    let file = nvram.system_file().unwrap().to_path_buf();
    let mut bytes = fs::read(&file).unwrap();
    bytes[0x0010] ^= 0xFF;
    fs::write(&file, &bytes).unwrap();

    let expected = stamped(Record::CreditInfo, body);
    assert_eq!(nvram.read_record(Record::CreditInfo).unwrap(), expected);
    assert_eq!(&fs::read(&file).unwrap()[0x0000..0x0200], expected.as_slice());
}

#[test]
fn both_copies_failing_the_crc_load_the_default_and_write_it_twice() {
    let dir = tempfile::tempdir().unwrap();
    let (mut backend, sim) = SimBackend::new(dir.path());
    let nvram = open(&mut backend, &sim);
    let file = nvram.system_file().unwrap().to_path_buf();
    // Fresh zero bytes: the stored CRC (0) does not match, so the default image is written.
    let loaded = nvram.load_record(Record::CreditInfo, || Record::CreditInfo.blank_image()).unwrap();
    let mut default = Record::CreditInfo.blank_image();
    assert_eq!(&default[..4], &0x5555_AAAA_u32.to_le_bytes());
    default = stamped(Record::CreditInfo, default);
    assert_eq!(loaded, default);
    let bytes = fs::read(&file).unwrap();
    assert_eq!(&bytes[0x0000..0x0200], default.as_slice());
    assert_eq!(&bytes[0x3000..0x3200], default.as_slice());
}

#[test]
fn the_context_needs_the_amfs_mount_and_counts_handles() {
    let dir = tempfile::tempdir().unwrap();
    let (mut backend, sim) = SimBackend::new(dir.path());
    let mut nvram = NvramContext::new();
    sim.registry.remove(MOUNT_KEY, "AMFS");
    assert!(matches!(nvram.acquire(&mut backend), Err(NvramError::NoMountPath)));
    assert!(!nvram.is_open());
    fs::create_dir_all(sim.root.join("y")).unwrap();
    sim.registry.set(MOUNT_KEY, "AMFS", RegValue::Text(r"Y:\amfs".to_owned()));
    nvram.acquire(&mut backend).unwrap();
    nvram.acquire(&mut backend).unwrap();
    nvram.release();
    assert!(nvram.is_open());
    nvram.release();
    assert!(!nvram.is_open());
}

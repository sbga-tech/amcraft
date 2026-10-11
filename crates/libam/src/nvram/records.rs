//! The record directory (`storage.md` 3.5) and the backup-record library on top of the two device files
//! (framing, CRC, id, primary/mirror repair; `storage.md` 3.4, `spec/libam/usbio.md` 6.3-6.6).
//!
//! The library codes are the original's: `-22` range, `-23` CRC, `-24` id, `-28` library not initialised, and the
//! device codes `-1..-7` of [`super::device`].

use super::NvramContext;
use super::device::{ALIGNMENT, Device};

/// Id every id-checked record carries (`0x2D2D2D2D`, "----").
const RECORD_ID: u32 = 0x2D2D_2D2D;

/// Every `am::nvram` record class (`storage.md` 3.5). The address map is private to this module; callers only
/// name the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Record {
    /// `sysfile.dat` `0x0000` / `0x3000`, `0x200` bytes.
    CreditInfo,
    /// `sysfile.dat` `0x0200` / `0x3200`, `0x200` bytes.
    NetworkEth0Info,
    /// `sysfile.dat` `0x0600` / `0x3600`, `0x200` bytes.
    History,
    /// `sysfile.dat` `0x1000` / `0x4000`, `0x200` bytes.
    BackupInfo,
    /// `sysfile.dat` `0x1400` / `0x4400`, `0x200` bytes.
    TimezoneInfo,
    /// `sysfile.dat` `0x1600` / `0x4600`, `0x200` bytes.
    ErrorLog,
    /// `sysfile.dat` `0x2000` / `0x5000`, `0x200` bytes, id-checked.
    FirmwareUpdateStateInfo,
    /// `sysfile.dat` `0x2200` / `0x5200`, `0x600` bytes, id-checked.
    EmoneyInfo,
    /// `sysfile.dat` `0x2800` / `0x5800`, `0x200` bytes.
    DipSwitch,
    /// `sysfile.dat` `0x2C00` / `0x5C00`, `0x400` bytes, id-checked.
    AimePayInfo,
    /// `appfile.dat` `0x0000` / `0x2000`, `0x200` bytes.
    PlayInfo,
    /// `appfile.dat` `0x0200` / `0x2200`, `0x200` bytes: the legacy FirmwareUpdateStateInfo location, read once at
    /// construction by the sequence/error owner and never written.
    LegacyFirmwareUpdateState,
    /// `appfile.dat` `0x0400` / `0x2400`, `0x800` bytes.
    AppErrorLogInfo,
    /// `appfile.dat` `0x0C00` / `0x2C00`, `0x400` bytes.
    AppDataJournalInfo,
    /// `appfile.dat` `0x1000` / `0x3000`, `0x400` bytes.
    AimePlayLogInfo,
}

struct Descriptor {
    device: Device,
    primary: u32,
    mirror: u32,
    size: usize,
    id: Option<u32>,
}

const fn sys(primary: u32, size: usize, id: Option<u32>) -> Descriptor {
    Descriptor { device: Device::System, primary, mirror: primary + 0x3000, size, id }
}

const fn app(primary: u32, size: usize) -> Descriptor {
    Descriptor { device: Device::Application, primary, mirror: primary + 0x2000, size, id: None }
}

impl Record {
    const fn descriptor(self) -> Descriptor {
        match self {
            Self::CreditInfo => sys(0x0000, 0x200, None),
            Self::NetworkEth0Info => sys(0x0200, 0x200, None),
            Self::History => sys(0x0600, 0x200, None),
            Self::BackupInfo => sys(0x1000, 0x200, None),
            Self::TimezoneInfo => sys(0x1400, 0x200, None),
            Self::ErrorLog => sys(0x1600, 0x200, None),
            Self::FirmwareUpdateStateInfo => sys(0x2000, 0x200, Some(RECORD_ID)),
            Self::EmoneyInfo => sys(0x2200, 0x600, Some(RECORD_ID)),
            Self::DipSwitch => sys(0x2800, 0x200, None),
            Self::AimePayInfo => sys(0x2C00, 0x400, Some(RECORD_ID)),
            Self::PlayInfo => app(0x0000, 0x200),
            Self::LegacyFirmwareUpdateState => app(0x0200, 0x200),
            Self::AppErrorLogInfo => app(0x0400, 0x800),
            Self::AppDataJournalInfo => app(0x0C00, 0x400),
            Self::AimePlayLogInfo => app(0x1000, 0x400),
        }
    }

    /// Size of the record image in bytes.
    pub const fn size(self) -> usize {
        self.descriptor().size
    }

    /// Whether the record lives in `appfile.dat` (the file of the current game id) rather than `sysfile.dat`.
    pub const fn in_app_file(self) -> bool {
        matches!(self.descriptor().device, Device::Application)
    }

    /// The default image's placeholder dword (`0x5555AAAA` for system-file records except
    /// FirmwareUpdateStateInfo, `0x55555555` otherwise); the write replaces it with the CRC.
    pub const fn placeholder(self) -> u32 {
        match self {
            Self::FirmwareUpdateStateInfo => 0x5555_5555,
            _ if self.in_app_file() => 0x5555_5555,
            _ => 0x5555_AAAA,
        }
    }

    /// A zeroed image of the record carrying only the placeholder dword.
    pub fn blank_image(self) -> Vec<u8> {
        let mut image = vec![0; self.size()];
        image[..4].copy_from_slice(&self.placeholder().to_le_bytes());
        image
    }
}

/// Why a record could not be read or written (the backup library's code, `storage.md` 3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordError {
    /// `-23`: the CRC does not match.
    Crc,
    /// `-24`: the id does not match.
    Id,
    /// Any other library or device code (`-1..-7`, `-22`, ...).
    Io(i32),
    /// The context (or, for an application-file record, the application file) is not open.
    NotOpen,
}

impl RecordError {
    fn from_code(code: i32) -> Self {
        match code {
            -23 => Self::Crc,
            -24 => Self::Id,
            -28 | -3 => Self::NotOpen,
            other => Self::Io(other),
        }
    }
}

/// The record object's status after `load` / `store` (`0x14025AD90`, `0x14025B160`; `usbio.md` 6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordStatus {
    /// Success.
    Ok = 0,
    /// The context could not be created.
    ContextFailed = 1,
    /// The record could not be read (or no handle).
    ReadFailed = 2,
    /// The record (or its default image) could not be written.
    WriteFailed = 3,
}

fn stamp(image: &mut [u8], id: Option<u32>) {
    if let Some(id) = id {
        image[4..8].copy_from_slice(&id.to_le_bytes());
    }
    let crc = crc32fast::hash(&image[4..]);
    image[..4].copy_from_slice(&crc.to_le_bytes());
}

impl NvramContext {
    fn device(&self, device: Device) -> Option<&super::device::DeviceFile> {
        match device {
            Device::System => self.sys.as_ref(),
            Device::Application => self.app.as_ref(),
        }
    }

    fn range_ok(device: Device, address: u32, len: usize) -> bool {
        let size = u64::from(device.size());
        address.is_multiple_of(ALIGNMENT)
            && len.is_multiple_of(ALIGNMENT as usize)
            && len as u64 <= size
            && u64::from(address) + len as u64 <= size
    }

    /// Single-copy read plus validation (`0x1402CD320`): the device read, then CRC `-23`, then (when `id` is
    /// given) the id check `-24`, which runs afterwards and overrides `-23`.
    pub(crate) fn read_one(&self, device: Device, address: u32, buf: &mut [u8], id: Option<u32>) -> i32 {
        if self.sys.is_none() {
            return -28;
        }
        if !Self::range_ok(device, address, buf.len()) {
            return -22;
        }
        let Some(file) = self.device(device) else {
            return -3;
        };
        let code = file.read(address, buf);
        if code < 0 {
            return code;
        }
        let stored = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let mut result = if stored == crc32fast::hash(&buf[4..]) { 0 } else { -23 };
        if let Some(id) = id
            && u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) != id
        {
            result = -24;
        }
        result
    }

    /// Single-copy write (`0x1402CD4D0`): the id, then the CRC over `[4, len)` are stamped into the caller's
    /// buffer, then the device write.
    pub(crate) fn write_one(&self, device: Device, address: u32, buf: &mut [u8], id: Option<u32>) -> i32 {
        if self.sys.is_none() {
            return -28;
        }
        if !Self::range_ok(device, address, buf.len()) {
            return -22;
        }
        let Some(file) = self.device(device) else {
            return -3;
        };
        stamp(buf, id);
        file.write(address, buf)
    }

    /// Read-dup (`0x1402CD3B0`): the primary; on a record-level error (`<= -20`) the mirror, and a good mirror
    /// heals the primary. Device errors on the primary are returned without trying the mirror.
    fn read_dup(&self, record: Record, buf: &mut [u8]) -> i32 {
        let d = record.descriptor();
        let first = self.read_one(d.device, d.primary, buf, d.id);
        if first >= 0 {
            return 0;
        }
        if first > -20 {
            return first;
        }
        let second = self.read_one(d.device, d.mirror, buf, d.id);
        if second >= 0 {
            return self.write_one(d.device, d.primary, buf, d.id);
        }
        second
    }

    /// Write-dup (`0x1402CD580`): the mirror first (a device error aborts before the primary), then the primary.
    fn write_dup(&self, record: Record, buf: &mut [u8]) -> i32 {
        let d = record.descriptor();
        let first = self.write_one(d.device, d.mirror, buf, d.id);
        if (-19..=-1).contains(&first) {
            return first;
        }
        self.write_one(d.device, d.primary, buf, d.id)
    }

    /// The record's image read with read-dup; the image keeps its CRC and id header.
    pub fn read_record(&self, record: Record) -> Result<Vec<u8>, RecordError> {
        let mut image = vec![0; record.size()];
        match self.read_dup(record, &mut image) {
            0 => Ok(image),
            code => Err(RecordError::from_code(code)),
        }
    }

    /// Writes `image` (exactly [`Record::size`] bytes) with write-dup: id and CRC are stamped on a copy, the
    /// mirror is written first, then the primary.
    pub fn write_record(&self, record: Record, image: &[u8]) -> Result<(), RecordError> {
        if image.len() != record.size() {
            return Err(RecordError::Io(-22));
        }
        let mut copy = image.to_vec();
        match self.write_dup(record, &mut copy) {
            0 => Ok(()),
            code => Err(RecordError::from_code(code)),
        }
    }

    /// Record `load` (`0x14025AD90` / `0x14025AEF0`): the image when read-dup succeeds; when both copies fail
    /// the CRC or id check, `reset()` supplies the default image, which is written to both copies and returned.
    /// Any other read error, or a missing handle, is [`RecordStatus::ReadFailed`]; a failed default write is
    /// [`RecordStatus::WriteFailed`].
    pub fn load_record(&self, record: Record, reset: impl FnOnce() -> Vec<u8>) -> Result<Vec<u8>, RecordStatus> {
        match self.read_record(record) {
            Ok(image) => Ok(image),
            Err(RecordError::Crc | RecordError::Id) => {
                let mut image = reset();
                image.resize(record.size(), 0);
                match self.write_dup(record, &mut image) {
                    0 => Ok(image),
                    _ => Err(RecordStatus::WriteFailed),
                }
            }
            Err(_) => Err(RecordStatus::ReadFailed),
        }
    }

    /// Record `store` (`0x14025B160`): write-dup of the whole image.
    pub fn store_record(&self, record: Record, image: &[u8]) -> RecordStatus {
        match self.write_record(record, image) {
            Ok(()) => RecordStatus::Ok,
            Err(_) => RecordStatus::WriteFailed,
        }
    }

    /// Status to error code (`0x14025AEC0`): context failure 4105, a read or write failure 84, or 87 when the
    /// record's develop byte is set. `None` for [`RecordStatus::Ok`].
    pub fn status_error(status: RecordStatus, develop: bool) -> Option<(u16, u16)> {
        match status {
            RecordStatus::Ok => None,
            RecordStatus::ContextFailed => Some((4105, 0)),
            RecordStatus::ReadFailed | RecordStatus::WriteFailed => Some((if develop { 87 } else { 84 }, 0)),
        }
    }
}

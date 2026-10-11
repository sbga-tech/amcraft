//! The two file back ends of the backup library (`storage.md` 3.3): a fixed-size file addressed in 512-byte
//! units with synchronous positional I/O.
//!
//! Result codes are the original's (`storage.md` 3.3.4): `0` success, `-2` invalid argument, `-3` not
//! initialised, `-5` OS failure, `-6` short read, `-7` short write. The original opens the files with
//! write-through and no buffering; here every write is followed by `sync_data`, which keeps the same durability.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use backend::Fs;

/// Alignment of both devices (`0x200`).
pub const ALIGNMENT: u32 = 0x200;

/// One backup-library device (table `0x1406FD670`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    /// Device 0: `sysfile.dat`.
    System,
    /// Device 1: `appfile.dat` of the current game id.
    Application,
}

impl Device {
    /// Size of the device in bytes (`0x6000` / `0x800000`).
    pub const fn size(self) -> u32 {
        match self {
            Self::System => 0x6000,
            Self::Application => 0x80_0000,
        }
    }
}

/// An opened device file of exactly [`Device::size`] bytes.
#[derive(Debug)]
pub struct DeviceFile {
    file: File,
    size: u32,
}

impl DeviceFile {
    /// `amBackupFileInit` (`0x1402FDB20`): a missing file is created (its parent directory too, one level only),
    /// the file is opened read/write with share mode read|write and its size is forced to `size` (a short or new
    /// file is zero-extended, a longer one truncated). A directory at the path fails.
    pub fn open(host_fs: &dyn Fs, path: &Path, size: u32) -> io::Result<Self> {
        if !path.exists() {
            if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                match fs::create_dir(parent) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        }
        let file = host_fs.open_backup_file(path)?;
        if file.metadata()?.len() != u64::from(size) {
            file.set_len(u64::from(size))?;
        }
        Ok(Self { file, size })
    }

    fn check(&self, offset: u32, len: usize) -> bool {
        let aligned = offset.is_multiple_of(ALIGNMENT) && len.is_multiple_of(ALIGNMENT as usize);
        aligned && len != 0 && u64::from(offset) + len as u64 <= u64::from(self.size)
    }

    /// `ReadFile` at `offset` (`0x1402FDCE0`): `0`, `-2`, `-5` or `-6`.
    pub fn read(&self, offset: u32, buf: &mut [u8]) -> i32 {
        if !self.check(offset, buf.len()) {
            return -2;
        }
        let result = (&self.file).seek(SeekFrom::Start(u64::from(offset))).and_then(|_| (&self.file).read_exact(buf));
        match result {
            Ok(()) => 0,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => -6,
            Err(_) => -5,
        }
    }

    /// `WriteFile` at `offset` (`0x1402FDED0`): `0`, `-2`, `-5` or `-7`.
    pub fn write(&self, offset: u32, buf: &[u8]) -> i32 {
        if !self.check(offset, buf.len()) {
            return -2;
        }
        let result = (&self.file).seek(SeekFrom::Start(u64::from(offset))).and_then(|_| (&self.file).write_all(buf));
        match result {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WriteZero => return -7,
            Err(_) => return -5,
        }
        if self.file.sync_data().is_err() {
            return -5;
        }
        0
    }
}

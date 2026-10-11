//! The Windows file system: the default mapping plus the Win32 sharing modes, the system attribute and NTFS
//! junctions.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::fs::{Fs, NativeFs};

/// `FILE_FLAG_WRITE_THROUGH`.
const WRITE_THROUGH: u32 = 0x8000_0000;
/// `FILE_SHARE_READ | FILE_SHARE_WRITE`.
const SHARE_READ_WRITE: u32 = 3;
/// `FILE_ATTRIBUTE_SYSTEM`.
const ATTRIBUTE_SYSTEM: u32 = 0x4;

/// Cabinet paths are host paths (the mapping is the default [`NativeFs`]'s). The backup files open shared and
/// written through, the ICF files exclusively, and the APM log folder link is an NTFS junction made by the
/// `junction` crate: the mount-point reparse point of `FSCTL_SET_REPARSE_POINT` that the original writes by hand,
/// pointing at the cabinet-text target (the crate also fills the print name, which the original leaves empty).
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsFs(NativeFs);

impl Fs for WindowsFs {
    fn map(&self, cabinet_path: &str) -> PathBuf {
        self.0.map(cabinet_path)
    }

    fn open_backup_file(&self, path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).share_mode(SHARE_READ_WRITE).custom_flags(WRITE_THROUGH).open(path)
    }

    fn open_exclusive(&self, path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).share_mode(0).open(path)
    }

    fn is_system(&self, metadata: &Metadata) -> bool {
        metadata.file_attributes() & ATTRIBUTE_SYSTEM != 0
    }

    fn create_dir_link(&self, target: &Path, link: &Path) -> io::Result<()> {
        // The crate creates the directory and writes the mount-point reparse point; its substitute name is
        // `\??\` + the full path of the target.
        junction::create(target, link)
    }

    fn remove_dir_link(&self, link: &Path) -> io::Result<()> {
        fs::remove_dir(link)
    }
}

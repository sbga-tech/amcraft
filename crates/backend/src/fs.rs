//! The file-system interface: the cabinet path mapping and the operations that differ between operating systems.
//!
//! libam addresses files by cabinet paths (`Y:\...` the backup drive, `C:\Users\AppUser\...`, the AMFS mount
//! the registry names, `S:\`, `X:\`). Modules call [`Fs::map`] and then use `std::fs` on the result, so a
//! test points every drive at a temporary directory and the production backends map nothing. What `std::fs` cannot
//! express the same way on every operating system (opening with a sharing mode, the system attribute, directory
//! links) is an operation of this interface.
//!
//! [`NativeFs`] is the default implementation: `std::fs` on the host path, directory links through the `symlink`
//! crate.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Maps cabinet paths to host paths and performs the file operations that depend on the operating system.
pub trait Fs {
    /// The host path of `cabinet_path` (`X:\dir\file`, either slash accepted).
    fn map(&self, cabinet_path: &str) -> PathBuf;

    /// Opens an existing backup-library device file for reading and writing (`amBackupFileInit`, `storage.md`
    /// 3.3): shared with other readers and writers, and written through to the medium where the operating system
    /// has such a flag.
    fn open_backup_file(&self, path: &Path) -> io::Result<File>;

    /// Opens an existing file for reading, denying every other open while it is held (the ICF provider,
    /// `storage.md` 4.10). A file held by another process fails with the operating system's sharing violation
    /// (raw error 32 on Windows).
    fn open_exclusive(&self, path: &Path) -> io::Result<File>;

    /// Whether the entry carries the system attribute (`FILE_ATTRIBUTE_SYSTEM`: the system volume information and
    /// the recycle bin). An operating system without the attribute answers `false`.
    fn is_system(&self, metadata: &Metadata) -> bool;

    /// Makes `link` a directory link to the existing directory `target` (the APM log folder link,
    /// `storage.md` 5.6; the NTFS junction of `linkLogFolder` on Windows). `link` must not exist and its parent
    /// must.
    fn create_dir_link(&self, target: &Path, link: &Path) -> io::Result<()>;

    /// Removes a link made by [`Fs::create_dir_link`] without touching its target.
    fn remove_dir_link(&self, link: &Path) -> io::Result<()>;
}

/// The production mapping: the cabinet path is the host path. Files open plainly (no sharing modes), no entry has
/// the system attribute and a directory link is a symbolic link (`symlink` picks the right call of each operating
/// system).
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeFs;

impl Fs for NativeFs {
    fn map(&self, cabinet_path: &str) -> PathBuf {
        PathBuf::from(cabinet_path)
    }

    fn open_backup_file(&self, path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(path)
    }

    fn open_exclusive(&self, path: &Path) -> io::Result<File> {
        File::open(path)
    }

    fn is_system(&self, _metadata: &Metadata) -> bool {
        false
    }

    fn create_dir_link(&self, target: &Path, link: &Path) -> io::Result<()> {
        symlink::symlink_dir(target, link)
    }

    fn remove_dir_link(&self, link: &Path) -> io::Result<()> {
        symlink::remove_symlink_dir(link)
    }
}

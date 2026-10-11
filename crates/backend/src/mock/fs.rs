//! The simulated file system: every cabinet drive below one directory.

use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use crate::fs::{Fs, NativeFs};

/// Every drive letter becomes a directory below one root: `Y:\a\b` maps to `<root>/y/a/b`. A path that is already
/// an absolute host path (`/...`, such as a directory next to a test's executable) stays where it is, with
/// backslashes read as separators; any other relative path lands below the root. The file operations are the
/// default ones (plain opens, symbolic links, no system attribute).
#[derive(Debug, Clone)]
pub struct SimFs {
    root: PathBuf,
}

impl SimFs {
    /// Paths below `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Fs for SimFs {
    fn map(&self, cabinet_path: &str) -> PathBuf {
        if cabinet_path.starts_with('/') {
            return cabinet_path.split(['\\', '/']).filter(|part| !part.is_empty()).fold(
                PathBuf::from("/"),
                |mut out, part| {
                    out.push(part);
                    out
                },
            );
        }
        let mut parts = cabinet_path.split(['\\', '/']).filter(|part| !part.is_empty());
        let mut out = self.root.clone();
        match parts.next() {
            Some(first) if first.len() == 2 && first.ends_with(':') => out.push(first[..1].to_ascii_lowercase()),
            Some(first) => out.push(first),
            None => {}
        }
        out.extend(parts);
        out
    }

    fn open_backup_file(&self, path: &Path) -> io::Result<File> {
        NativeFs.open_backup_file(path)
    }

    fn open_exclusive(&self, path: &Path) -> io::Result<File> {
        NativeFs.open_exclusive(path)
    }

    fn is_system(&self, metadata: &Metadata) -> bool {
        NativeFs.is_system(metadata)
    }

    fn create_dir_link(&self, target: &Path, link: &Path) -> io::Result<()> {
        NativeFs.create_dir_link(target, link)
    }

    fn remove_dir_link(&self, link: &Path) -> io::Result<()> {
        NativeFs.remove_dir_link(link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letters_become_lower_case_directories() {
        let fs = SimFs::new("/r");
        assert_eq!(fs.map(r"Y:\sub\file.dat"), Path::new("/r/y/sub/file.dat"));
        assert_eq!(fs.map("C:/Users/AppUser/"), Path::new("/r/c/Users/AppUser"));
    }
}

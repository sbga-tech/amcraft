//! `linkLogFolder` (`0x140240290`, `spec/libam/storage.md` 5.6): `<main log folder>\<GAMEID>` becomes a directory
//! link to the sub-application's log folder, so the game writes into `<main>\<GAMEID>\` and the data lands in the
//! sub-application's own ABaaS log folder.
//!
//! The link itself is the file-system interface's ([`Fs::create_dir_link`]): on Windows an NTFS junction (the
//! mount-point reparse point that the original writes by hand, pointing at the cabinet-text target), elsewhere a
//! symbolic link, which has the same observable effect (the link path resolves to the sub-application folder).

use std::fs;
use std::io;
use std::path::Path;

use backend::Fs;

/// Why the link could not be made (every case is `am::Result` 6 for the caller, "start APM.").
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// A path is 260 characters or longer ("Error: path length").
    #[error("path length")]
    PathLength,
    /// The sub-application folder does not exist or is not a directory.
    #[error("folder does not exist")]
    NoTarget,
    /// An existing directory at the link path could not be removed.
    #[error("cannot remove the old link: {0}")]
    Remove(io::Error),
    /// The link folder could not be created or the link written.
    #[error("cannot create the link: {0}")]
    Create(io::Error),
}

/// The three paths of one link, as cabinet text (for the length checks and the junction target) and as host paths.
#[derive(Debug, Clone, Copy)]
pub struct LinkPaths<'a> {
    /// `L`: main log folder + game id, no trailing backslash.
    pub link_cabinet: &'a str,
    /// `S`: the sub-application log folder, with trailing backslash.
    pub target_cabinet: &'a str,
    /// Host path of `L`.
    pub link: &'a Path,
    /// Host path of `S`.
    pub target: &'a Path,
}

/// Creates the link, replacing an existing directory (or link) at its path.
pub fn link_log_folder(host_fs: &dyn Fs, paths: LinkPaths<'_>) -> Result<(), LinkError> {
    if paths.link_cabinet.encode_utf16().count() >= 260 || paths.target_cabinet.encode_utf16().count() >= 260 {
        return Err(LinkError::PathLength);
    }
    if !paths.target.is_dir() {
        return Err(LinkError::NoTarget);
    }
    remove_existing(host_fs, paths.link).map_err(LinkError::Remove)?;
    if let Some(parent) = paths.link.parent() {
        fs::create_dir_all(parent).map_err(LinkError::Create)?;
    }
    host_fs.create_dir_link(paths.target, paths.link).map_err(|error| {
        // A failure after the creation removes the new directory again.
        let _ = remove_existing(host_fs, paths.link);
        LinkError::Create(error)
    })
}

/// `RemoveDirectoryW`: removes an existing directory (empty) or link; nothing to do when absent.
fn remove_existing(host_fs: &dyn Fs, link: &Path) -> io::Result<()> {
    match fs::symlink_metadata(link) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(meta) if meta.file_type().is_symlink() => host_fs.remove_dir_link(link),
        Ok(_) => fs::remove_dir(link),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backend::mock::SimFs;

    fn link_paths<'a>(link: &'a Path, target: &'a Path) -> LinkPaths<'a> {
        LinkPaths { link_cabinet: r"Y:\MAIN\abaas\APP2", target_cabinet: r"Y:\APP2\abaas\", link, target }
    }

    #[test]
    fn link_resolves_to_the_target_and_is_replaced_by_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let host_fs = SimFs::new(dir.path());
        let first = dir.path().join("y/APP1/abaas");
        let second = dir.path().join("y/APP2/abaas");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("marker"), b"2").unwrap();
        let link = dir.path().join("y/MAIN/abaas/APP2");
        let paths = |target| link_paths(&link, target);
        link_log_folder(&host_fs, paths(&first)).unwrap();
        link_log_folder(&host_fs, paths(&second)).unwrap();
        assert_eq!(fs::read(link.join("marker")).unwrap(), b"2");
    }

    #[test]
    fn missing_target_and_long_paths_fail_without_touching_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let host_fs = SimFs::new(dir.path());
        let link = dir.path().join("link");
        let missing = dir.path().join("missing");
        let paths = LinkPaths { link_cabinet: r"Y:\a", target_cabinet: r"Y:\b\", link: &link, target: &missing };
        assert!(matches!(link_log_folder(&host_fs, paths), Err(LinkError::NoTarget)));
        let long = "Y:\\".to_owned() + &"x".repeat(260);
        let paths = LinkPaths { link_cabinet: &long, ..paths };
        assert!(matches!(link_log_folder(&host_fs, paths), Err(LinkError::PathLength)));
        assert!(!link.exists());
    }
}

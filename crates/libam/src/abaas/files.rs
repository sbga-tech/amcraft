//! The file-system suites of the ABaaS manager: the log-folder search, the segment reader and the delete step
//! (`spec/libam/storage.md` 6.9.9-6.9.11).

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Datelike, Local, Timelike};
use md5::{Digest, Md5};

/// Most files a polling request lists.
pub const MAX_FILES: usize = 250;
/// Longest relative path of a listed file, in characters.
const MAX_REL_PATH: usize = 200;
/// A full path of 258 characters or more is rejected.
const MAX_FULL_PATH: usize = 258;
/// Upload segment size.
pub const SEGMENT_SIZE: u64 = 65_536;

/// One listed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path relative to the log folder, backslash separated.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Local last-write time, `YYYYMMDDhhmmss`.
    pub mtime: String,
}

/// `YYYYMMDDhhmmss` of a local calendar time.
pub fn stamp<T: Datelike + Timelike>(time: &T) -> String {
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        time.year(),
        time.month(),
        time.day(),
        time.hour(),
        time.minute(),
        time.second()
    )
}

/// The local last-write time of a file as `YYYYMMDDhhmmss`.
pub fn file_stamp(time: SystemTime) -> String {
    stamp(&DateTime::<Local>::from(time))
}

/// `CryptBinaryToStringA(CRYPT_STRING_BASE64)`: 64-character lines, each ended by CRLF, the last one too.
pub fn base64_crlf(bytes: &[u8]) -> String {
    let text = STANDARD.encode(bytes);
    let mut out = String::with_capacity(text.len() + text.len() / 32 + 2);
    for line in text.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push_str("\r\n");
    }
    out
}

#[derive(Debug)]
struct Level {
    host: PathBuf,
    /// Cabinet text of the folder, ending in a backslash.
    cabinet: String,
    /// Path of the folder relative to the root, ending in a backslash (empty for the root).
    relative: String,
    /// Remaining entries, next one last: `(name, is_directory)`.
    entries: Vec<(String, bool)>,
}

fn list_folder(host: &Path) -> std::io::Result<Vec<(String, bool)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(host)? {
        let entry = entry?;
        let is_dir = entry.file_type()?.is_dir();
        match entry.file_name().into_string() {
            Ok(name) => entries.push((name, is_dir)),
            // A name that is not text cannot be listed; the original rejects such paths as invalid.
            Err(_) => entries.push((String::new(), is_dir)),
        }
    }
    // The volume enumerates in name order, case-insensitively.
    entries.sort_by_key(|(name, _)| name.to_lowercase());
    entries.reverse();
    Ok(entries)
}

/// `FileSearchSuite` (6.9.9): a depth-first walk of the log folder, one directory entry per [`Search::step`].
#[derive(Debug)]
pub struct Search {
    stack: Vec<Level>,
    found: Vec<FileEntry>,
    done: bool,
}

impl Search {
    /// Opens the root. `Err` carries the lib message of the failure (`cannot open <root>\*`).
    pub fn start(root_cabinet: &str, root_host: &Path) -> Result<Self, String> {
        let entries = list_folder(root_host).map_err(|_| format!("cannot open {root_cabinet}*"))?;
        let root =
            Level { host: root_host.to_path_buf(), cabinet: root_cabinet.to_owned(), relative: String::new(), entries };
        Ok(Self { stack: vec![root], found: Vec::new(), done: false })
    }

    /// Handles one directory entry; lib messages go to `messages`. Returns `true` once the walk is finished.
    pub fn step(&mut self, messages: &mut Vec<String>) -> bool {
        if self.done {
            return true;
        }
        let Some(level) = self.stack.last_mut() else {
            self.done = true;
            return true;
        };
        let Some((name, is_dir)) = level.entries.pop() else {
            self.stack.pop();
            self.done = self.stack.is_empty();
            return self.done;
        };
        let full = format!("{}{name}", level.cabinet);
        if name.is_empty() || full.chars().any(|c| c < ' ') {
            messages.push(format!("{full} includes invalid character."));
            return false;
        }
        if full.chars().count() >= MAX_FULL_PATH {
            messages.push(format!("{full} is too long."));
            return false;
        }
        let host = level.host.join(&name);
        let relative = format!("{}{name}", level.relative);
        if is_dir {
            match list_folder(&host) {
                Ok(entries) => {
                    let level =
                        Level { host, cabinet: format!("{full}\\"), relative: format!("{relative}\\"), entries };
                    self.stack.push(level);
                }
                Err(_) => messages.push(format!("{full} is unavailable.")),
            }
            return false;
        }
        if relative.chars().count() > MAX_REL_PATH {
            messages.push(format!("{full} is too long."));
            return false;
        }
        if let Ok(meta) = fs::metadata(&host) {
            let mtime = meta.modified().map(file_stamp).unwrap_or_default();
            self.found.push(FileEntry { path: relative, size: meta.len(), mtime });
        }
        if self.found.len() >= MAX_FILES {
            messages.push(format!("{MAX_FILES} files were detected."));
            self.done = true;
        }
        self.done
    }

    /// The files found.
    pub fn into_files(self) -> Vec<FileEntry> {
        self.found
    }
}

/// An upload file that passed the property and hash checks (`FileDataSuite` first segment, 6.9.10).
#[derive(Debug)]
pub struct UploadFile {
    /// Path relative to the log folder, as the server named it.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Local last-write time.
    pub mtime: String,
    /// Upper-case hex MD5 of the whole file.
    pub md5: String,
    /// `ceil(size / 65536)`.
    pub total_segments: u32,
    file: File,
}

/// Why a file could not be prepared for upload.
#[derive(Debug)]
pub enum UploadFail {
    /// The open or lookup failed: the instruction stays queued (the retry loop of the original).
    Keep(String),
    /// The file is unusable: the instruction is dropped.
    Drop(String),
}

impl UploadFile {
    /// Opens `host`, checks size, segment number and hashes it. `max_bytes` is `max_datasize << 20`.
    pub fn open(
        host: &Path,
        cabinet: &str,
        relative: &str,
        max_bytes: u64,
        next_segment: u32,
    ) -> Result<Self, UploadFail> {
        let mut file = File::open(host).map_err(|_| {
            if host.exists() {
                UploadFail::Keep(format!("cannot open {cabinet}"))
            } else {
                UploadFail::Keep(format!("cannot find {cabinet}"))
            }
        })?;
        let property = || UploadFail::Drop(format!("cannot get property from {cabinet}"));
        let meta = file.metadata().map_err(|_| property())?;
        let size = meta.len();
        if size == 0 || size > max_bytes {
            return Err(property());
        }
        let total_segments = u32::try_from(size.div_ceil(SEGMENT_SIZE)).map_err(|_| property())?;
        if total_segments < next_segment {
            return Err(property());
        }
        let mtime = meta.modified().map(file_stamp).map_err(|_| property())?;
        let mut hasher = Md5::new();
        let mut buffer = vec![0u8; SEGMENT_SIZE as usize];
        loop {
            let count =
                file.read(&mut buffer).map_err(|_| UploadFail::Drop(format!("cannot get hash from {cabinet}")))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        let md5 = hasher.finalize().iter().map(|byte| format!("{byte:02X}")).collect();
        Ok(Self { path: relative.to_owned(), size, mtime, md5, total_segments, file })
    }

    /// The bytes of the 1-based `segment`.
    pub fn read_segment(&mut self, segment: u32) -> std::io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(u64::from(segment.saturating_sub(1)) * SEGMENT_SIZE))?;
        let mut data = Vec::new();
        (&mut self.file).take(SEGMENT_SIZE).read_to_end(&mut data)?;
        Ok(data)
    }
}

/// `FileDeleteSuite` (6.9.11): the file goes only when its local last-write time equals `mtime`. The lib message
/// of a failed delete is returned.
pub fn delete_if_unchanged(host: &Path, cabinet: &str, mtime: &str) -> Option<String> {
    let current = fs::metadata(host).ok()?.modified().map(file_stamp).ok()?;
    if current != mtime {
        return None;
    }
    fs::remove_file(host)
        .err()
        .map(|error| format!("Cannot DeleteFile: {cabinet}{}", error.raw_os_error().unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_uses_64_column_crlf_lines() {
        let text = base64_crlf(&[0xAB; 100]);
        let lines: Vec<&str> = text.split("\r\n").collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].len(), 64);
        assert_eq!(lines[1].len(), 64);
        assert_eq!(lines[2].len(), 8);
        assert!(lines[3].is_empty());
        assert!(base64_crlf(&[]).is_empty());
    }

    #[test]
    fn search_walks_depth_first_in_name_order_and_limits_to_250() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("b/sub")).unwrap();
        fs::write(dir.path().join("a.log"), b"1").unwrap();
        fs::write(dir.path().join("b/sub/c.log"), b"22").unwrap();
        fs::write(dir.path().join("b/z.log"), b"333").unwrap();
        let mut search = Search::start(r"Y:\G\abaas\", dir.path()).unwrap();
        let mut messages = Vec::new();
        let mut steps = 0;
        while !search.step(&mut messages) {
            steps += 1;
        }
        assert!(steps >= 6);
        let files = search.into_files();
        let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.log", r"b\sub\c.log", r"b\z.log"]);
        assert_eq!(files[1].size, 2);
        assert_eq!(files[0].mtime.len(), 14);
        assert!(messages.is_empty());

        for index in 0..260 {
            fs::write(dir.path().join(format!("f{index:03}")), b"x").unwrap();
        }
        let mut search = Search::start(r"Y:\G\abaas\", dir.path()).unwrap();
        while !search.step(&mut messages) {}
        assert_eq!(search.into_files().len(), MAX_FILES);
        assert_eq!(messages, ["250 files were detected."]);
    }

    #[test]
    fn upload_file_hashes_and_reads_segments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        fs::write(&path, vec![7u8; 70_000]).unwrap();
        let mut file = UploadFile::open(&path, r"Y:\G\big.bin", "big.bin", 20 << 20, 2).ok().unwrap();
        assert_eq!(file.total_segments, 2);
        assert_eq!(file.md5.len(), 32);
        assert_eq!(file.read_segment(2).unwrap().len(), 70_000 - 65_536);
        assert!(matches!(UploadFile::open(&path, "c", "big.bin", 20 << 20, 3), Err(UploadFail::Drop(_))));
        assert!(matches!(UploadFile::open(&path, "c", "big.bin", 1000, 1), Err(UploadFail::Drop(_))));
        assert!(matches!(UploadFile::open(&dir.path().join("none"), "c", "none", 1000, 1), Err(UploadFail::Keep(_))));
    }

    #[test]
    fn delete_needs_the_exact_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.log");
        fs::write(&path, b"x").unwrap();
        let stamp = file_stamp(fs::metadata(&path).unwrap().modified().unwrap());
        assert_eq!(delete_if_unchanged(&path, "c", "19990101000000"), None);
        assert!(path.exists());
        assert_eq!(delete_if_unchanged(&path, "c", &stamp), None);
        assert!(!path.exists());
    }
}

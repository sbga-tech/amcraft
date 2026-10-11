//! The terminal info of the Manager (`spec/libam/payment.md` PayEmSvc 9.2 / 9.3): the certificate directory below
//! the AMFS mount, the terminal id and serial, the client certificate and its password.

use std::time::SystemTime;

use crate::registry_keys::MOUNT_KEY;
use backend::{Backend, Fs, KeyMode, RegValue};

/// Longest terminal id (31 wide characters including the terminator).
const ID_MAX: usize = 30;

/// `0x140270250`: `<AMFS>\emoney\` from the registry value `AMFS` of the mount key; `None` when the key or the value
/// cannot be read.
pub fn resolve_dir(backend: &mut Backend) -> Option<String> {
    let Ok(RegValue::Text(mount)) = backend.registry.read(MOUNT_KEY, KeyMode::Open, "AMFS") else { return None };
    let mut dir = mount;
    if !dir.is_empty() && !dir.ends_with('\\') {
        dir.push('\\');
    }
    dir.push_str("emoney\\");
    Some(dir)
}

/// The terminal info record (`ctx+0x...`, 9.2).
#[derive(Debug, Clone, Default)]
pub struct Terminal {
    /// The certificate directory, with a trailing backslash.
    pub dir: String,
    /// The terminal id (at most 30 characters).
    pub id: String,
    /// The terminal serial (the 11-character derived string).
    pub serial: String,
    /// The keychip serial block (`+0x4AA`): its 11-character derived string (`+0x24`), empty while the dongle's
    /// block is invalid.
    pub keychip: String,
    /// The de-obfuscated PKCS#12 password.
    pub password: String,
    /// The certificate path, empty when there is none.
    pub certificate: String,
    /// The certificate was accepted by the SDK (`+0x49D`).
    pub certificate_accepted: bool,
}

impl Terminal {
    /// `0x140270B20`: the directory exists or can be created.
    pub fn ensure_dir(&self, paths: &dyn Fs) -> bool {
        std::fs::create_dir_all(paths.map(&self.dir)).is_ok()
    }

    /// `0x140270430(serial, password, rescan, keychipBlock)` (state 3): `false` without a serial. With `rescan`
    /// the newest `*.p12` of the directory becomes the certificate and its stem the terminal id. The keychip block
    /// is copied whether it is valid or not; an invalid one leaves the string empty.
    pub fn set(
        &mut self,
        serial: Option<&str>,
        keychip: Option<&str>,
        password: &str,
        rescan: bool,
        paths: &dyn Fs,
    ) -> bool {
        let Some(serial) = serial else { return false };
        if rescan {
            self.scan(paths);
        }
        self.password = password.to_owned();
        self.serial = serial.to_owned();
        self.keychip = keychip.unwrap_or_default().to_owned();
        true
    }

    /// `0x140270C00`: the newest `*.p12` (greatest last-write time) of the directory; a stem of 31 or more
    /// characters leaves the id unchanged.
    fn scan(&mut self, paths: &dyn Fs) {
        let Ok(entries) = std::fs::read_dir(paths.map(&self.dir)) else { return };
        let mut newest: Option<(SystemTime, String)> = None;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.to_ascii_lowercase().ends_with(".p12") {
                continue;
            }
            let time = entry.metadata().and_then(|meta| meta.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            if newest.as_ref().is_none_or(|(best, _)| time > *best) {
                newest = Some((time, name));
            }
        }
        let Some((_, name)) = newest else { return };
        self.certificate = format!("{}{}", self.dir, name);
        if let Some((stem, _)) = name.rsplit_once('.')
            && stem.chars().count() <= ID_MAX
        {
            self.id = stem.to_owned();
        }
    }

    /// `0x1402707B0`: writes `<dir><id>.p12`; the id and the certificate path are then set (the caller sends the
    /// certificate to the SDK). `false` on a file error or an unusable id.
    pub fn store(&mut self, paths: &dyn Fs, id: &str, certificate: &[u8]) -> bool {
        let id: String = id.chars().take(ID_MAX).collect();
        if id.is_empty() || id.contains(['\\', '/', ':']) {
            return false;
        }
        let path = format!("{}{}.p12", self.dir, id);
        if std::fs::write(paths.map(&path), certificate).is_err() {
            return false;
        }
        self.id = id;
        self.certificate = path;
        self.certificate_accepted = false;
        true
    }

    /// `0x140270A30`: deletes the certificate file and forgets its path.
    pub fn delete(&mut self, paths: &dyn Fs) {
        if !self.certificate.is_empty() {
            let _ = std::fs::remove_file(paths.map(&self.certificate));
            self.certificate.clear();
        }
    }
}

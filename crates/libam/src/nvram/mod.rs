//! NVRAM: the two backup files and the record library on top of them (`spec/libam/storage.md` part 3,
//! `spec/libam/usbio.md` 6), and the `AppInfo` backup area manager.
//!
//! * [`NvramContext`]: `amSysFileInit` + `amBackupInit` (`sysfile.dat`, 24576 bytes, in the directory the registry
//!   value `AMFS` names) plus the application file `appfile.dat` (8 MiB, one per game id, below the registry value
//!   `APPDATA`). Both files are plain files forced to their exact size; records are CRC-32 framed with a mirror
//!   copy ([`records`]).
//! * [`AppInfoManager`]: the daemon-visible backup area (`storage.md` 3.7) with its journal. Its state word
//!   reaches 6 when the area is loaded; `allReady` needs exactly that (`core.main.allready`).
//!
//! The crate does not reproduce the original's shared-memory handle objects. A record is addressed by the
//! [`Record`] enum; the descriptor table (device, primary and mirror address, size, id check) is inside this
//! module.
//!
//! # Cross-module methods
//!
//! `NvramContext` lives in `Core::nvram`; hooks that persist records get `&NvramContext` (record I/O is
//! positional and needs no `&mut`).
//!
//! * [`NvramContext::acquire`] / [`NvramContext::release`]: the system-file handle (platform's StaticInfo record,
//!   `core.md` C.3 step 5). The file is open from `Interface::initialize` until the last release.
//! * [`NvramContext::init_records`]: `OperatingState::enter` step 3 (`0x14029C060`): opens `appfile.dat` of the
//!   configured game id. `Err` is posted as error 4105 by the flow.
//! * [`NvramContext::read_record`] / [`NvramContext::write_record`]: read-dup (primary, else mirror and a primary
//!   repair) and write-dup (CRC and id stamped on a copy, mirror first, then primary) of one [`Record`].
//! * [`NvramContext::load_record`]: record `load` (`0x14025AD90`): the image, or the module's `reset()` default
//!   written to both copies when both fail the CRC or id check.
//! * [`NvramContext::store_record`]: record `store` (`0x14025B160`).
//! * [`NvramContext::status_error`]: the status-to-error mapping (`0x14025AEC0`: 4105, else 84 or 87 with the
//!   develop byte).
//! * [`NvramContext::app_generation`]: bumps whenever `appfile.dat` is (re)opened, i.e. at `init_records` and at
//!   every game-id switch. Owners of application-file records (PlayInfo, AppErrorLogInfo, AimePlayLogInfo) reload
//!   them when it changes (`storage.md` 3.8 steps 7-8).
//! * [`AppInfoManager::restart`]: the module reset sweep (`0x1402544A0`, `storage.md` 3.7.9).
//! * `Core::switch_game_id` (in `port.rs`): the game-id switch (`0x140253940`, `storage.md` 3.8) used by the
//!   backup port and the APM `changeApp`.

mod app_info;
mod device;
mod port;
mod records;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use crate::registry_keys::MOUNT_KEY;
use backend::{Backend, KeyMode, RegValue};

pub use app_info::{AppInfoManager, SetupInputs};
pub use device::{ALIGNMENT, Device};
pub use port::{BackupHandle, BackupPort, GameId, NvramDevice};
pub use records::{Record, RecordError, RecordStatus};

use device::DeviceFile;

/// Size of `sysfile.dat` (`core.md` C.3 step 5).
pub const SYS_FILE_SIZE: u64 = 24_576;
/// File name of the system file inside the AMFS directory.
pub const SYS_FILE_NAME: &str = "sysfile.dat";
/// File name of the application file inside the game-id directory.
pub const APP_FILE_NAME: &str = "appfile.dat";
/// `AppInfo` state word once the area is loaded (`storage.md` 3.7, `core.main.allready`).
pub const APP_INFO_LOADED: u32 = 6;

/// Why a file of the NVRAM context could not be created.
#[derive(Debug, thiserror::Error)]
pub enum NvramError {
    /// The registry holds no AMFS mount path.
    #[error("no AMFS mount path")]
    NoMountPath,
    /// The registry holds no APPDATA path (`storage.md` 3.9: context creation failure).
    #[error("no APPDATA path")]
    NoAppData,
    /// The application file needs a valid game id (`Game ID is invalid.`).
    #[error("invalid game id")]
    NoGameId,
    /// The root and game id do not fit the 260-character path limit (`storage.md` 3.3.2 step 4).
    #[error("application file path too long")]
    PathTooLong,
    /// A device file could not be created or opened.
    #[error("device file unavailable: {0}")]
    File(#[from] std::io::Error),
}

/// The NVRAM context: the opened system file with its handle counter, and the application file of one game id.
#[derive(Debug, Default)]
pub struct NvramContext {
    handles: u32,
    path: Option<PathBuf>,
    sys: Option<DeviceFile>,
    app: Option<DeviceFile>,
    app_game_id: Option<GameId>,
    configured_game_id: Option<GameId>,
    generation: u32,
}

impl NvramContext {
    /// A context nobody has acquired.
    pub fn new() -> Self {
        Self::default()
    }

    /// The first acquisition initialises the context (creates or opens `sysfile.dat` of [`SYS_FILE_SIZE`] bytes
    /// in the AMFS directory); later ones only count. `core.md` C.3 step 5: the counter is bumped only when the
    /// initialisation succeeded.
    pub fn acquire(&mut self, backend: &mut Backend) -> Result<(), NvramError> {
        if self.handles == 0 {
            let (path, file) = Self::open_system_file(backend)?;
            self.path = Some(path);
            self.sys = Some(file);
        }
        self.handles += 1;
        Ok(())
    }

    /// Drops one handle; the last one closes the context (the application file goes with it).
    pub fn release(&mut self) {
        self.handles = self.handles.saturating_sub(1);
        if self.handles == 0 {
            self.path = None;
            self.sys = None;
            self.app = None;
        }
    }

    /// Whether the context is initialised.
    pub fn is_open(&self) -> bool {
        self.handles > 0
    }

    /// The host path of the system file while the context is open.
    pub fn system_file(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// `0x14029C060`: remembers the configured game id and opens the application file for it.
    pub fn init_records(&mut self, backend: &mut Backend, game_id: Option<GameId>) -> Result<(), NvramError> {
        self.configured_game_id = game_id;
        self.open_app_file(backend, game_id)
    }

    /// Closes the application file (the last holder released it) and opens the one of `game_id`
    /// (`amAppFileInitEx`, `storage.md` 3.3.2). A failure leaves the application file closed. Every successful
    /// open bumps [`Self::app_generation`].
    pub fn open_app_file(&mut self, backend: &mut Backend, game_id: Option<GameId>) -> Result<(), NvramError> {
        self.close_app_file();
        let id = game_id.ok_or(NvramError::NoGameId)?;
        let root = match backend.registry.read(MOUNT_KEY, KeyMode::CreateVolatile, "APPDATA") {
            Ok(RegValue::Text(text)) => text,
            _ => return Err(NvramError::NoAppData),
        };
        let mut cabinet_path = root;
        if !cabinet_path.is_empty() && !cabinet_path.ends_with('\\') {
            cabinet_path.push('\\');
        }
        if cabinet_path.len() + id.to_string().len() + 12 >= 260 {
            return Err(NvramError::PathTooLong);
        }
        cabinet_path.push_str(&format!("{id}\\{APP_FILE_NAME}"));
        let file = DeviceFile::open(backend.fs.as_ref(), &backend.fs.map(&cabinet_path), Device::Application.size())?;
        self.app = Some(file);
        self.app_game_id = Some(id);
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    /// Closes the application file (`amAppFileExit`).
    pub fn close_app_file(&mut self) {
        self.app = None;
        self.app_game_id = None;
    }

    /// Whether `appfile.dat` is open.
    pub fn app_file_open(&self) -> bool {
        self.app.is_some()
    }

    /// The game id whose `appfile.dat` is open.
    pub fn app_game_id(&self) -> Option<GameId> {
        self.app_game_id
    }

    /// The configured game id (the keychip's, `app+26214`) given to [`Self::init_records`].
    pub fn configured_game_id(&self) -> Option<GameId> {
        self.configured_game_id
    }

    /// Counter of application-file opens; see the module header.
    pub fn app_generation(&self) -> u32 {
        self.generation
    }

    fn open_system_file(backend: &mut Backend) -> Result<(PathBuf, DeviceFile), NvramError> {
        let mut mount = match backend.registry.read(MOUNT_KEY, KeyMode::CreateVolatile, "AMFS") {
            Ok(RegValue::Text(text)) => text,
            _ => return Err(NvramError::NoMountPath),
        };
        if !mount.is_empty() && !mount.ends_with('\\') {
            mount.push('\\');
        }
        mount.push_str(SYS_FILE_NAME);
        let path = backend.fs.map(&mount);
        let file = DeviceFile::open(backend.fs.as_ref(), &path, Device::System.size())?;
        Ok((path, file))
    }
}

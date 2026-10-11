//! The backup facade (`am::nvram`): backup devices and records ([`BackupPort`], [`GameId`], [`NvramDevice`]), and
//! libam's [`BackupHandle`] with the game-id switch.

use std::fmt::{self, Write as _};

use crate::core::Core;
use crate::result::{AmError, AmResult};
use crate::shared::Shared;

/// `am::GameId` (`0x140223120`): four upper-cased letters. The APM start request carries the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GameId([u16; 4]);

impl GameId {
    /// `GameId::parse` (`0x140223150`): the text must be exactly four wide characters (`wcsnlen(text, 5) == 4`)
    /// that are all `iswalpha`; they are stored upper-cased. `iswalpha` is approximated by Unicode alphabetics and
    /// `towupper` is applied for ASCII only, as in the CRT "C" locale (inferred).
    pub fn parse(text: &[u16; 5]) -> Option<Self> {
        let length = text.iter().position(|&unit| unit == 0).unwrap_or(text.len());
        if length != 4 {
            return None;
        }
        let mut id = [0u16; 4];
        for (stored, &unit) in id.iter_mut().zip(text) {
            if !char::from_u32(u32::from(unit)).is_some_and(char::is_alphabetic) {
                return None;
            }
            *stored = if (u16::from(b'a')..=u16::from(b'z')).contains(&unit) { unit - 0x20 } else { unit };
        }
        Some(Self(id))
    }

    /// The four upper-cased UTF-16 code units.
    pub fn units(&self) -> &[u16; 4] {
        &self.0
    }
}

impl fmt::Display for GameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .try_for_each(|&unit| f.write_char(char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER)))
    }
}

/// An NVRAM device of libam (the descriptor selected by `0x140217640`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NvramDevice {
    /// EEPROM.
    Eeprom = 0,
    /// Battery-backed SRAM.
    Sram = 1,
    /// File device.
    File = 2,
}

/// libam NVRAM operations used by the Backup controller.
///
/// Every method mirrors one callee recovered in `spec/controllers/backup_appimage.md` §4. The calls share the
/// singleton context `[0x1408A9C40]` and only work after [`Self::acquire_nvram`] succeeded.
pub trait BackupPort {
    /// `acquireNvram` (`0x140218AD0` / `0x140218B20`): `Err(code 2)` while the libam context is absent.
    fn acquire_nvram(&self) -> AmResult;

    /// `Device::size()` (`0x140223290`) of the descriptor selected by `0x140217640`.
    fn device_size(&self, device: NvramDevice) -> u64;

    /// `Device::alignment()` (`0x1402232A0`) of the descriptor selected by `0x140217640`.
    fn device_alignment(&self, device: NvramDevice) -> u32;

    /// libam set game id (`0x1402172C0` with a valid id).
    fn set_game_id(&mut self, game_id: &GameId);

    /// libam set default game id (`0x140217310`).
    fn set_default_game_id(&mut self);

    /// libam load of one record (`0x140217380`): resets the `AppInfo` list, adds the record
    /// (`size = buffer.len()`, the aligned slot size) and loads it into `buffer`. `Err` carries the libam code:
    /// 1 add rejected, 4 other game id, 5 CRC mismatch, 6 load failed.
    fn load_record(&mut self, device: NvramDevice, offset: u32, buffer: &mut [u8]) -> AmResult;

    /// libam add of one record for the next commit (`0x140217510`, `size = buffer.len()`). The libam list keeps
    /// the buffer until [`Self::commit_records`], so the implementation copies it. `Err(code 1)` when rejected.
    fn add_record(&mut self, device: NvramDevice, offset: u32, buffer: &[u8]) -> AmResult;

    /// libam commit of the added records (`0x1402175F0`); `Err(code 6)` when `AppInfo::save()` fails.
    fn commit_records(&mut self) -> AmResult;
}

/// `BackupPort` handle over the shared libam state.
#[derive(Debug)]
pub struct BackupHandle(pub(crate) Shared);

impl Core {
    /// The game-id switch (`0x140253940`, `storage.md` 3.8): `AppInfo` lets go of the application file, the file
    /// of `target` is opened (a failure is error 4105; every record owner notices the new
    /// [`crate::nvram::NvramContext::app_generation`] and reloads its application-file records), the id word is
    /// recomputed and `AppInfo` re-creates its context (journal reload).
    pub(crate) fn switch_game_id(&mut self, target: GameId) {
        self.nvram.close_app_file();
        self.app_info.set_game_id(&target);
        if self.nvram.open_app_file(&mut self.backend, Some(target)).is_err() {
            self.raise_error(4105, 0);
        }
        self.app_info.create_context(&self.nvram);
    }

    /// `set_default_game_id` target (`0x140217310`): the APM sub-game id when valid, else the configured one.
    pub(crate) fn default_game_id(&self) -> Option<GameId> {
        self.apm.sub_game_id().or_else(|| self.nvram.configured_game_id())
    }
}

impl BackupPort for BackupHandle {
    fn acquire_nvram(&self) -> AmResult {
        self.0.guard()
    }

    fn device_size(&self, device: NvramDevice) -> u64 {
        self.0.read(|core| if device == NvramDevice::File { core.app_info.file_size() } else { 0 })
    }

    fn device_alignment(&self, device: NvramDevice) -> u32 {
        self.0.read(|core| if device == NvramDevice::File { core.app_info.file_alignment() } else { 0 })
    }

    fn set_game_id(&mut self, game_id: &GameId) {
        if self.0.guard().is_ok() {
            self.0.with(|core| core.switch_game_id(*game_id));
        }
    }

    fn set_default_game_id(&mut self) {
        if self.0.guard().is_ok() {
            self.0.with(|core| {
                if let Some(target) = core.default_game_id() {
                    core.switch_game_id(target);
                }
            });
        }
    }

    fn load_record(&mut self, device: NvramDevice, offset: u32, buffer: &mut [u8]) -> AmResult {
        self.0.guard()?;
        let (status, loaded) = self.0.with(|core| {
            core.app_info.clear_list();
            let size = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
            if core.app_info.add_record(false, device as u32, offset, size, vec![0; buffer.len()]) != 0 {
                return (None, Vec::new());
            }
            let (status, mut buffers) = core.app_info.load(&core.nvram);
            (Some(status), buffers.pop().unwrap_or_default())
        });
        if loaded.len() == buffer.len() {
            buffer.copy_from_slice(&loaded);
        }
        match status {
            None => Err(AmError::code(AmError::INVALID_ARGUMENT)),
            Some(1) => Err(AmError::code(AmError::FAILED)),
            Some(2) => Err(AmError::code(AmError::UNKNOWN_TASK)),
            Some(3) => Err(AmError::code(4)),
            Some(_) => Ok(()),
        }
    }

    fn add_record(&mut self, device: NvramDevice, offset: u32, buffer: &[u8]) -> AmResult {
        self.0.guard()?;
        let size = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let rejected =
            self.0.with(|core| core.app_info.add_record(false, device as u32, offset, size, buffer.to_vec()));
        if rejected == 0 { Ok(()) } else { Err(AmError::code(AmError::INVALID_ARGUMENT)) }
    }

    fn commit_records(&mut self) -> AmResult {
        self.0.guard()?;
        let status = self.0.with(|core| core.app_info.save(&core.nvram));
        if status == 0 { Ok(()) } else { Err(AmError::code(AmError::FAILED)) }
    }
}

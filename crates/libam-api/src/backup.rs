//! `am::nvram`: backup devices and records.

use std::fmt::{self, Write as _};

use crate::result::AmResult;

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

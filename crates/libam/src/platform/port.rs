//! The platform facades and libam's handles: the facade tokens, the process hand-off, reboot, preload and the system
//! information reads (`spec/libam/platform.md` part A, B.1.1, C.2.10, C.4).
//!
//! * [`ProcessPort`]: the `process` facade: next process, clock ([`ProcessHandle`]).
//! * [`CommonPort`]: the `process` facade as Common uses it: player total, language, preload, reboot
//!   ([`CommonHandle`]).
//! * [`SystemInfoPort`]: board, dongle, video, storage and APM identity reads ([`SystemInfoHandle`]).

use crate::module::Cx;
use crate::nvram::GameId;
use crate::result::{AmError, AmResult};
use crate::shared::Shared;
use crate::time::DateTime;
use crate::util::wide_array;

/// libam "next process" code stored by `set_next_process` (`0x140217700`) and later tested against 3
/// (`0x140234270`, `0x140234AF0`). The wire-name ↔ code pairing is an inference; the numbers are direct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum NextProcessCode {
    /// Code 1: wire `Auto`, and the target of every wire value ≥ 4.
    Auto = 1,
    /// Code 2: wire `SegaBoot`.
    SegaBoot = 2,
    /// Code 3: wire `SegaBootError`.
    SegaBootError = 3,
    /// Code 4: wire `SystemTest`.
    SystemTest = 4,
}

/// libam and operating-system calls made by the process controller.
pub trait ProcessPort {
    /// `facade.process_token` (`0x140218C10`): the libam "process" facade handle. `Err` stands for a NULL
    /// token with `Result` code 2 ("amw is uninitialized"), i.e. the libam singleton is NULL.
    fn process_facade(&mut self) -> AmResult<()>;

    /// `libam.set_next_process` (`0x140217700`, target `0x140233050`): stores `code` in libam's next-process
    /// global (`0x140957F44`) iff libam's kill-guard byte (`app+0x61D8`) is set. Only called with a valid
    /// facade.
    fn set_next_process(&mut self, code: NextProcessCode);

    /// `Clock::tryGet` (`0x1400533C0`, constructor `0x140053280`): a machine-wide counter that only increases
    /// from machine start, `None` when there is none. The original reads `QueryPerformanceCounter`; the value is
    /// the boot clock, which the game's client library only compares for equality (`chusanApp` `0x5DC620`), so its
    /// unit does not matter.
    fn clock_ticks(&mut self) -> Option<i64>;

    /// `DateTime::now(local = 0)` (`0x14005D0F0`): microseconds since 0001-01-01, used when
    /// [`Self::clock_ticks`] fails.
    fn local_now(&mut self) -> DateTime;
}

/// libam calls made by the common controller.
pub trait CommonPort {
    /// `facade.process_token` (`0x140218C10`) as used by `updatePlayerTotal`. `Err` stands for a NULL token
    /// with `Result` code 2 ("amw is uninitialized"), i.e. the libam singleton is NULL.
    fn process_facade(&mut self) -> AmResult<()>;

    /// `libam.player_total` (`0x140217740`, chunk `0x140236110`): the qword at `app+0x20A68` while the state
    /// at `app+0x20E58` is 2 or 3, else 0. Only called with a valid process facade.
    fn player_total(&mut self) -> u64;

    /// `facade.get_language` (`0x140218440` → `0x140253D20`): libam's current language, the
    /// `wchar_t[16]` at `app+0xAE8`, NUL-terminated or filling the array. `Err` when the libam singleton is NULL.
    fn language(&mut self) -> AmResult<[u16; 16]>;

    /// `libam.set_language` (`0x140218310` → `0x140253210` → `am::ResourceSet::changeLanguage`
    /// `0x140284350`). `language` is the NUL-free value, already checked by the controller to fit
    /// the facade's `wcsnlen(str, 16) <= 15` requirement (otherwise libam answers `Result` code 1). `Err`
    /// covers a NULL singleton (code 2) and a failed change (code 6, "failed to change language"): libam
    /// refuses an empty value or a missing resource set, and treats the current value as success.
    fn set_language(&mut self, language: &[u16]) -> AmResult<()>;

    /// `facade.board_token` (`0x1402185D0`): the handle `am::platform::Manager::reboot` is called on.
    /// `Err` stands for a NULL token (libam singleton NULL).
    fn board_facade(&mut self) -> AmResult<()>;

    /// `am::platform::Manager::reboot` (`0x140217680` → `0x14024B8F0`, `this = app+0x6458`): enables
    /// `SeShutdownPrivilege` and, only if that worked, issues `ExitWindowsEx(EWX_REBOOT | EWX_FORCE, 0)`
    /// followed by a sleep of at least 20 s. Returns only when the machine did not reboot. Only called
    /// with a valid board facade.
    fn reboot(&mut self);

    /// `libam.preload` (`0x1402183D0` → `am::Manager::preload` `0x140253250`): hands `process_id` to
    /// `amWorkaroundPreloadDatasec(id, 0)` (`0x1402D38E0`). `Err` is `Result` code 6 ("failed to preload
    /// section") for a non-zero return, or code 2 for a NULL singleton.
    fn preload(&mut self, process_id: u32) -> AmResult<()>;
}

/// A serial as libam stores it: a `StandardSerialId` (`wchar_t[17]`) followed by a `ShortSerialId`
/// (`wchar_t[12]`), each NUL-terminated or filling its array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibamSerial {
    /// The standard part.
    pub standard: [u16; 17],
    /// The short part.
    pub short: [u16; 12],
}

/// One video output resolution: the first and second `u32` of a libam video descriptor (`+4`, `+8`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoResolution {
    /// First `u32` (libam descriptor `+4`, getter `0x14021F1C0`). Calling it the width is an inference.
    pub width: u32,
    /// Second `u32` (libam descriptor `+8`, getter `0x14021F1D0`). Calling it the height is an inference.
    pub height: u32,
}

/// libam queries made by the system-info controller. Every `*_facade` method is one of the "amw" facade
/// wrappers: it hands out a capability token, or fails (`Err`, `Result` code 2 "amw is uninitialized") when
/// libam's application singleton (`0x1408A9C40`) is NULL. The data methods read libam state directly and
/// are only called after the matching facade succeeded.
pub trait SystemInfoPort {
    /// `facade.board_token` (`0x1402185D0`).
    fn board_facade(&mut self) -> AmResult<()>;
    /// `facade.dongle_token` (`0x140218710`).
    fn dongle_facade(&mut self) -> AmResult<()>;
    /// `facade.video_token` (`0x140218DF0`).
    fn video_facade(&mut self) -> AmResult<()>;
    /// `facade.storage_token` (`0x140218E90`).
    fn storage_facade(&mut self) -> AmResult<()>;
    /// `facade.apm_token` (`0x140219610`).
    fn apm_facade(&mut self) -> AmResult<()>;

    /// Region optional (`0x1402176C0`, `app+0x66B0`): the raw value when valid.
    fn board_region(&mut self) -> Option<u32>;
    /// Board serial optional (`0x1402176A0`, `app+0x6608`): `wchar_t[17]` + `wchar_t[12]` when valid.
    fn board_serial(&mut self) -> Option<LibamSerial>;
    /// Keychip serial optional (`0x1402165F0`, `app+0x6672`) in the same layout.
    fn keychip_serial(&mut self) -> Option<LibamSerial>;
    /// Model type optional (`0x140216610`, `app+0x66AE`): the byte when valid.
    fn model_type(&mut self) -> Option<u8>;
    /// Develop byte (`0x1402165B0`, `app+0x6664`), unnormalised.
    fn develop(&mut self) -> u8;
    /// Dongle game id optional (`0x1402165D0`, `app+0x6666`): its string when valid. A `wchar_t[5]` (4 characters
    /// and the terminator) in the 12-byte optional `{valid u8, pad, wchar_t[5]@+2}`; the keychip serial optional
    /// follows at `app+0x6672`.
    fn dongle_game_id(&mut self) -> Option<[u16; 5]>;
    /// Apm sub-game id optional (`0x140215EE0` → `0x140240200`, `app+0x78C7C`): its string when valid. A
    /// `wchar_t[5]` in the same 12-byte optional layout: the reset `0x140223220` clears the valid byte, a qword at
    /// `+2` and a word at `+10`, the APM start function `0x14023FED0` copies the id in as a qword and a dword from
    /// `+0`, and the next member, an app-image property, starts at `+320`, 12 bytes after the optional at `+308`.
    fn apm_game_id(&mut self) -> Option<[u16; 5]>;
    /// Video descriptor `index` (`0x140218150`): the first and second `u32` (`0x14021F1C0`, `0x14021F1D0`,
    /// descriptor `+4` and `+8`). Descriptors live at `app+0x21D50` (index 1) and `app+0x21D60` (index 0).
    fn video_resolution(&mut self, index: u32) -> VideoResolution;
    /// Output flags of video descriptor `index` (`0x140218180`): `byte(desc+14) != 0 && byte(desc+15) != 0`.
    fn video_output_active(&mut self, index: u32) -> bool;
    /// Application root path on storage (`0x140217990`; variant B `0x14024C4B0` at `storage+0x338` when the
    /// apm game id is valid, else variant A `0x14024C480` at `storage+0x130`). `None` unless the storage state
    /// (`app+0x21D78`) is 2 or 3 and the path is non-empty. A `wchar_t[260]`, NUL-terminated or filling the array:
    /// the storage resets `0x14024C0F0`, `0x14024C210` and `0x14024C300` fill variant A (`storage+304`) and B
    /// (`storage+824`, also `0x14024C3C0`) with `wcscpy_s(.., 0x104, ..)`, and the two lie `0x208` bytes (260 units)
    /// apart.
    fn app_root_path(&mut self) -> Option<[u16; 260]>;
    /// RAM-disk path (`0x1402179E0` → `0x14024C4E0`, `storage+0x540`), with the same availability rule as
    /// [`Self::app_root_path`]. A `wchar_t[260]`: the same resets and the root-folder machine `0x14024C530` fill it
    /// (`storage+1344`) with `wcscpy_s(.., 0x104, ..)`, `0x208` bytes (260 units) behind variant B.
    fn ram_disk_path(&mut self) -> Option<[u16; 260]>;
}

/// The longest language name the facade accepts: `wcsnlen(str, 16) <= 15` (`error.md` 3).
const LANGUAGE_MAX_UNITS: usize = 15;

/// `SystemInfoPort` handle over the shared libam state.
#[derive(Debug)]
pub struct SystemInfoHandle(pub(crate) Shared);

/// `ProcessPort` handle over the shared libam state.
#[derive(Debug)]
pub struct ProcessHandle(pub(crate) Shared);

/// `CommonPort` handle over the shared libam state.
#[derive(Debug)]
pub struct CommonHandle(pub(crate) Shared);

/// A game id as the `wchar_t[5]` the facade returns: four characters and a NUL.
fn game_id_units(id: GameId) -> [u16; 5] {
    let mut units = [0u16; 5];
    units[..4].copy_from_slice(id.units());
    units
}

impl SystemInfoPort for SystemInfoHandle {
    fn board_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn dongle_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn video_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn storage_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn apm_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    /// The keychip region (`0x1402176C0`), 1 to 3.
    fn board_region(&mut self) -> Option<u32> {
        self.0.read(|core| core.dongle.region())
    }

    /// The platform manager's board block (`0x1402176A0`).
    fn board_serial(&mut self) -> Option<LibamSerial> {
        self.0.read(|core| core.platform.board_serial())
    }

    fn keychip_serial(&mut self) -> Option<LibamSerial> {
        self.0.read(|core| core.dongle.serial())
    }

    fn model_type(&mut self) -> Option<u8> {
        self.0.read(|core| core.dongle.model())
    }

    fn develop(&mut self) -> u8 {
        self.0.read(|core| u8::from(core.dongle.develop()))
    }

    fn dongle_game_id(&mut self) -> Option<[u16; 5]> {
        self.0.read(|core| core.dongle.game_id().map(game_id_units))
    }

    fn apm_game_id(&mut self) -> Option<[u16; 5]> {
        self.0.read(|core| core.apm.sub_game_id().map(game_id_units))
    }

    fn video_resolution(&mut self, index: u32) -> VideoResolution {
        self.0.read(|core| core.video.resolution(index))
    }

    /// `byte(desc+14) != 0 && byte(desc+15) != 0`: the flags are never set (C.2.10).
    fn video_output_active(&mut self, _index: u32) -> bool {
        false
    }

    fn app_root_path(&mut self) -> Option<[u16; 260]> {
        self.0.read(|core| core.storage.app_root_path(core.apm.sub_game_id().is_some()).map(wide_array::<260>))
    }

    fn ram_disk_path(&mut self) -> Option<[u16; 260]> {
        self.0.read(|core| core.storage.ram_disk().map(wide_array::<260>))
    }
}

impl ProcessPort for ProcessHandle {
    fn process_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    /// Stored only while the error manager is attached to the master context (the kill guard, A.3.1).
    fn set_next_process(&mut self, code: NextProcessCode) {
        self.0.with(|core| core.master.set_next_process(code as u32));
    }

    fn clock_ticks(&mut self) -> Option<i64> {
        self.0.read(|core| core.clock.performance_counter())
    }

    fn local_now(&mut self) -> DateTime {
        self.0.read(|core| core.clock.local_now())
    }
}

impl CommonPort for CommonHandle {
    fn process_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn player_total(&mut self) -> u64 {
        self.0.read(|core| core.sequence.player_total())
    }

    fn language(&mut self) -> AmResult<[u16; 16]> {
        self.0.guard()?;
        Ok(self.0.read(|core| core.error.language()))
    }

    /// `0x140218310`: a name longer than 15 units is refused with 1, a failed `changeLanguage` is 6.
    fn set_language(&mut self, language: &[u16]) -> AmResult<()> {
        self.0.guard()?;
        let length = language.iter().position(|&unit| unit == 0).unwrap_or(language.len());
        if length > LANGUAGE_MAX_UNITS {
            return Err(AmError::code(AmError::INVALID_ARGUMENT));
        }
        let changed = self.0.with(|core| {
            let mut cx = Cx {
                backend: &mut core.backend,
                clock: &*core.clock,
                events: &mut core.events,
                config: &core.config,
                nvram: &core.nvram,
            };
            core.error.change_language(&mut cx, &language[..length]) == 0
        });
        if changed { Ok(()) } else { Err(AmError::code(AmError::FAILED)) }
    }

    fn board_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    /// The platform reboot (`0x14024B8F0`): request, wait 20 s, latch 4105.
    fn reboot(&mut self) {
        self.0.with(|core| {
            let mut cx = Cx {
                backend: &mut core.backend,
                clock: &*core.clock,
                events: &mut core.events,
                config: &core.config,
                nvram: &core.nvram,
            };
            core.platform.reboot(&mut cx);
        });
    }

    /// `amWorkaroundPreloadDatasec(process_id, 0)`: `Result` 6 when the process cannot be preloaded.
    fn preload(&mut self, process_id: u32) -> AmResult<()> {
        self.0.guard()?;
        let preloaded = self.0.with(|core| {
            let outcome = core.backend.preload.preload(process_id);
            if outcome.is_err() {
                tracing::info!(process_id, "preloading the data sections failed");
            }
            outcome.is_ok()
        });
        if preloaded { Ok(()) } else { Err(AmError::code(AmError::FAILED)) }
    }
}

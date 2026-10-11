//! Board, dongle, video, storage and APM identity facades read by SystemInfo.

use crate::result::AmResult;

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

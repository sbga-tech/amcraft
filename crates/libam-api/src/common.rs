//! `process` facade as used by Common: player total, language, preload, reboot.

use crate::result::AmResult;

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

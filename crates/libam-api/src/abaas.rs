//! `am::abaas`: ABaaS log upload.

use crate::result::AmResult;

/// libam surface used by the ABaaS log controller (the `am::abaas::LogManager` facade).
///
/// The original reaches libam through the global instance `qword_1408A9C40`; every call below is a thin
/// wrapper over it.
pub trait LogPort {
    /// `amw::abaasHandle` (`0x140219570`): `Err` when libam is not initialised ("amw is uninitialized",
    /// code 2). The controller reports the `isReady()` precondition failure on `Err`.
    fn abaas_handle(&mut self) -> AmResult<()>;

    /// `LogManager::isAlive` (`0x1402150C0`): libabaas alive flag, `false` while the module is not ready.
    fn is_alive(&mut self) -> bool;

    /// `LogManager::fileCount` (`0x1402150E0`): pending upload records, `-1` when unavailable.
    fn file_count(&mut self) -> i32;

    /// `LogManager` upload-idle byte (`+0x7FB`, wrapper `0x140215100`): `true` when there is nothing to upload
    /// and the connection has not been down for 240 s or more.
    fn upload_idle(&mut self) -> bool;

    /// `amw::currentLogRoot` (`0x140215120`): the sub-application log folder while a game id is active
    /// (`LogManager+0x5F0`, getter `0x140237C00`), else the main log folder (`+0x3E8`, getter `0x140237BB0`);
    /// `None` when the folder does not exist. Both are `wchar_t[260]` buffers, NUL-terminated or filling the
    /// array: `0x140236FF0` (called by `LogManager::setup` `0x140237D40`) fills the main folder with
    /// `wcscpy_s(LM+1000, 0x104, ..)`, `LogManager::changeApp` `0x1402379F0` fills the sub-application folder with
    /// the same `0x104` bound, and the two buffers lie `0x208` bytes (260 units) apart.
    fn current_log_root(&mut self) -> Option<[u16; 260]>;

    /// `am::abaas::LogManager::sendErrorMessage` (`0x140215050` → `0x140237860`): fire-and-forget; libam drops
    /// messages it cannot convert or queue and reports nothing back. `message` excludes the terminator.
    fn send_error_message(&mut self, message: &[u16]);
}

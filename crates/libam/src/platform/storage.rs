//! The storage manager (`am::storage::Manager`, `spec/libam/platform.md` part C 4): the game's root folder
//! `Y:\<GAMEID>\`, the APM sub-game folder name and the RAM disk `C:\Users\AppUser\`, the
//! `FixAppRootFolderPath` event that announces path A to the daemon, and the app-data initializer (4.7) that the
//! daemon's configuration can never enable but another libam caller may.

use std::fs::ReadDir;

use libam_api::backup::GameId;
use libam_api::interface::LibamEvent;

use crate::module::{Cx, ErrorCell, StateWord};
use crate::util::{game_id, wide_to_string};
use backend::Fs;

/// Longest cabinet path the storage helpers accept (`wchar_t[260]`).
const PATH_CAPACITY: usize = 260;
/// The backup drive root (constant at `0x1406AF1C8`).
const ROOT: &str = "Y:\\";
/// The RAM-disk folder (constant at `0x1406AF1D0`).
const RAM_DISK: &str = "C:\\Users\\AppUser";

/// `append` (`0x140227EB0`): joins `id` below `base`; `None` when it would not fit 260 units.
fn append(base: &str, id: &str) -> Option<String> {
    let mut out = base.to_owned();
    let units = |text: &str| text.encode_utf16().count();
    if units(&out) + units(id) + 1 >= PATH_CAPACITY {
        return None;
    }
    if !out.is_empty() && !out.ends_with('\\') {
        if units(&out) + 1 >= PATH_CAPACITY {
            return None;
        }
        out.push('\\');
    }
    out.push_str(id);
    Some(out)
}

/// `ensureTrailingBackslash` (`0x140227F70`): the path unchanged when it is empty or ends in `\`.
fn with_trailing_backslash(mut path: String) -> String {
    if !path.is_empty() && !path.ends_with('\\') && path.encode_utf16().count() + 1 < PATH_CAPACITY {
        path.push('\\');
    }
    path
}

/// `create(path)` (`0x140284FB0`, `MakeSureDirectoryPathExists`): every missing component is created. Returns 0
/// when the directory exists afterwards, 1 on failure and 2 for an empty or over-long path.
fn create_folder(paths: &dyn Fs, path: &str) -> u32 {
    let units = path.encode_utf16().count();
    if units == 0 || units > PATH_CAPACITY - 1 {
        return 2;
    }
    let host = paths.map(path);
    if host.is_dir() {
        return 0;
    }
    match std::fs::create_dir_all(&host) {
        Ok(()) if host.is_dir() => 0,
        _ => 1,
    }
}

/// `0x140285150`: erases the directory `path` with everything in it. 0 when it is gone (or never existed), 2 for
/// an unusable argument, 1 when the removal failed.
fn erase_folder(paths: &dyn Fs, path: &str) -> u32 {
    let units = path.encode_utf16().count();
    if units == 0 || units > PATH_CAPACITY - 1 || (units == PATH_CAPACITY - 1 && !path.ends_with('\\')) {
        return 2;
    }
    let host = paths.map(path);
    if !host.is_dir() {
        return 0;
    }
    if std::fs::remove_dir_all(&host).is_err() {
        tracing::error!("storage: the application folder could not be erased");
        return 1;
    }
    u32::from(host.exists())
}

/// `0x1402983F0`: the folder name is a game id (four letters) written in upper case.
fn is_game_id_folder(name: &str) -> bool {
    game_id(name).is_some_and(|id| wide_to_string(id.units()) == name)
}

/// The app-data initializer's states (`EraseAppFolderState`, `CreateAppRootFolderState`, `EndState`).
#[derive(Debug)]
enum Job {
    /// Enumerating `Y:\*`; `None` until the first update opens the listing.
    Erase(Option<Box<ReadDir>>),
    /// Creating `Y:\<GAMEID>\`.
    Create,
    /// Finished, whether it worked or not (the result is reported nowhere).
    End,
}

/// What the running initializer needs besides the paths: the base folder and the title folder name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InitParams {
    /// The game id string (`gameId+2`).
    game_id: String,
    /// The keychip develop flag.
    develop: bool,
}

/// `am::storage::Manager`.
#[derive(Debug, Default)]
pub struct Storage {
    state: StateWord,
    /// Path A, `Y:\<GAMEID>\` (`+0x130`).
    path_a: String,
    /// Path B, `Y:\<APMID>\` (`+0x338`).
    path_b: String,
    /// The RAM-disk path (`+0x540`).
    ram_disk: String,
    /// The running app-data initializer (`+0x748`, `+0x750`).
    job: Option<Job>,
    /// The parameters last handed to the initializer.
    params: Option<InitParams>,
    /// `+0x7E8`: the storage config was taken.
    config_taken: bool,
    /// `+0x7E9`: initializer enable.
    initializer_enabled: bool,
    /// `+0x7EA`: initializer finished or skipped.
    initializer_done: bool,
}

impl Storage {
    /// The constructed manager (`0x14024C0F0`).
    pub fn new() -> Self {
        Self::default()
    }

    /// `0x14024C1F0`, from `CheckingCabinetTypeState::enter`: state 1. The listener is implicit: the
    /// `FixAppRootFolderPath` event is always delivered.
    pub fn enter(&mut self) {
        self.state = StateWord::INITIALIZED;
    }

    /// `0x14024C390`, from `OperatingState::enter`: takes the storage config once.
    pub fn configure(&mut self, cx: &Cx<'_>) {
        if !self.config_taken {
            self.config_taken = true;
            self.initializer_enabled = cx.config.storage.fix_app_root_folder;
        }
    }

    /// `0x14024C2D0(ready, gate, develop, gameId)`: state 1 runs `setup`, state 2 runs `execute`.
    ///
    /// `ready` is the dongle (and, in Operating, the sequence) readiness, `gate` the sequence manager's "title
    /// changed" latch.
    pub fn update(&mut self, cx: &mut Cx<'_>, ready: bool, gate: bool, develop: bool, game_id: Option<GameId>) {
        match self.state.0 {
            1 => self.setup(cx, ready, game_id),
            2 => self.execute(cx, ready, gate, develop, game_id),
            _ => {}
        }
    }

    /// `setup` (`0x14024C530`, 4.3): path A, the event, the RAM disk, state 2.
    fn setup(&mut self, cx: &mut Cx<'_>, ready: bool, game_id: Option<GameId>) {
        if !ready {
            return;
        }
        self.path_a.clear();
        if let Some(id) = game_id {
            let id = wide_to_string(id.units());
            self.path_a = append(ROOT, &id).unwrap_or_else(|| ROOT.to_owned());
        }
        self.path_a = with_trailing_backslash(std::mem::take(&mut self.path_a));
        if !self.path_a.is_empty() && create_folder(cx.backend.fs.as_ref(), &self.path_a) != 0 {
            tracing::error!(path = %self.path_a, "storage: the application root folder could not be created");
        }
        let announced = (!self.path_a.is_empty()).then(|| self.path_a.clone());
        cx.events.push(LibamEvent::FixAppRootFolderPath(announced));
        self.ram_disk = with_trailing_backslash(RAM_DISK.to_owned());
        // The original tests the first character of path A here, not of the RAM-disk path.
        if !self.path_a.is_empty() && create_folder(cx.backend.fs.as_ref(), &self.ram_disk) != 0 {
            tracing::error!(path = %self.ram_disk, "storage: the ram disk folder could not be created");
        }
        self.state = StateWord::READY;
    }

    /// `execute` (`0x14024C710`, 4.6): steps the initializer; without a job the initializer is started when the
    /// title changed and the caller enabled it, and storage is finished (3) as soon as no job runs.
    fn execute(&mut self, cx: &mut Cx<'_>, ready: bool, gate: bool, develop: bool, game_id: Option<GameId>) {
        if !ready {
            return;
        }
        if self.job.is_some() {
            self.step_job(cx);
            if matches!(self.job, Some(Job::End)) {
                self.job = None;
                self.state = StateWord::FAILED;
                self.initializer_done = true;
            }
            return;
        }
        if let Some(id) = game_id {
            self.params = Some(InitParams { game_id: wide_to_string(id.units()), develop });
        }
        if !self.initializer_done && self.initializer_enabled && gate {
            self.start_initializer();
        }
        if self.job.is_none() {
            self.state = StateWord::FAILED;
            self.initializer_done = true;
        }
    }

    /// `0x14024C830` and the initializer's `start`: accepted when path A and the parameters exist.
    fn start_initializer(&mut self) {
        if self.job.is_none() && !self.path_a.is_empty() && self.params.is_some() {
            self.job = Some(Job::Erase(None));
        }
    }

    /// One `update` of the initializer state machine (4.7).
    fn step_job(&mut self, cx: &mut Cx<'_>) {
        let Some(params) = self.params.clone() else {
            self.job = Some(Job::End);
            return;
        };
        let paths = cx.backend.fs.as_ref();
        self.job = Some(match self.job.take() {
            Some(Job::Erase(listing)) => erase_step(paths, &params, listing),
            Some(Job::Create) => create_step(paths, &params),
            Some(Job::End) | None => Job::End,
        });
    }

    /// Path B (`0x14024C3C0`, from the APM hand-over): only in state 2/3; the folder is not created.
    pub fn set_apm_game_id(&mut self, game_id: Option<GameId>) {
        if !self.state.is_done() {
            return;
        }
        self.path_b = match game_id {
            Some(id) => with_trailing_backslash(append(ROOT, &wide_to_string(id.units())).unwrap_or_default()),
            None => String::new(),
        };
    }

    /// Reset (`0x14024C300`, module restart): the job released, paths cleared, back to state 1; the config latch
    /// is kept.
    pub fn restart(&mut self) {
        self.job = None;
        self.path_a.clear();
        self.path_b.clear();
        self.ram_disk.clear();
        self.state = StateWord::INITIALIZED;
    }

    /// Exit (`0x14024C210`): everything back to the constructed state; no folder is removed.
    pub fn terminate(&mut self) {
        if self.state != StateWord::UNINITIALIZED {
            *self = Self::new();
        }
    }

    /// `0x14024C450`: states 2 and 3 are "ready".
    pub fn is_ready(&self) -> bool {
        self.state.is_done()
    }

    /// The error cell (`+4`, `0x14024C470`): never latched.
    pub fn error_cell(&self) -> ErrorCell {
        ErrorCell::EMPTY
    }

    /// Path A (`0x14024C480`): `None` outside state 2/3 or when empty.
    pub fn path_a(&self) -> Option<&str> {
        (self.state.is_done() && !self.path_a.is_empty()).then_some(self.path_a.as_str())
    }

    /// Path B (`0x14024C4B0`).
    pub fn path_b(&self) -> Option<&str> {
        (self.state.is_done() && !self.path_b.is_empty()).then_some(self.path_b.as_str())
    }

    /// The RAM-disk path (`0x14024C4E0`).
    pub fn ram_disk(&self) -> Option<&str> {
        (self.state.is_done() && !self.ram_disk.is_empty()).then_some(self.ram_disk.as_str())
    }

    /// `0x140217990`: the app root path libam reports: path B while an APM sub-game id is valid, else path A.
    pub fn app_root_path(&self, apm_game_id_valid: bool) -> Option<&str> {
        if apm_game_id_valid { self.path_b() } else { self.path_a() }
    }

    /// `0x14024C510`: whether the app-data initializer is running.
    pub fn initializer_busy(&self) -> bool {
        matches!(self.job, Some(Job::Erase(_) | Job::Create))
    }
}

/// `EraseAppFolderState::update` (`0x140298220`): one directory entry of `Y:\` per call. A retail keychip erases
/// every non-system file and folder, a developer keychip only the upper-case game id folders.
fn erase_step(paths: &dyn Fs, params: &InitParams, listing: Option<Box<ReadDir>>) -> Job {
    let mut listing = match listing {
        Some(listing) => listing,
        None => match std::fs::read_dir(paths.map(ROOT)) {
            Ok(listing) => Box::new(listing),
            Err(_) => return Job::End,
        },
    };
    let Some(entry) = listing.next() else {
        return Job::Create;
    };
    let Ok(entry) = entry else {
        return Job::End;
    };
    let Ok(meta) = entry.metadata() else {
        return Job::End;
    };
    if paths.is_system(&meta) {
        return Job::Erase(Some(listing));
    }
    let name = entry.file_name().to_string_lossy().into_owned();
    if meta.is_dir() {
        if !params.develop || is_game_id_folder(&name) {
            let folder = format!("{ROOT}{name}\\");
            if erase_folder(paths, &folder) != 0 {
                return Job::End;
            }
        }
    } else if !params.develop && std::fs::remove_file(entry.path()).is_err() {
        return Job::End;
    }
    Job::Erase(Some(listing))
}

/// `CreateAppRootFolderState::update` (`0x140298520`): a file in the way of `Y:\<GAMEID>` is deleted, then the
/// folder is created.
fn create_step(paths: &dyn Fs, params: &InitParams) -> Job {
    let folder = format!("{ROOT}{}", params.game_id);
    let host = paths.map(&folder);
    if host.is_file() && std::fs::remove_file(&host).is_err() {
        return Job::End;
    }
    let _ = create_folder(paths, &folder);
    Job::End
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_checks_capacity_and_separator() {
        assert_eq!(append("Y:\\", "ABCD").as_deref(), Some("Y:\\ABCD"));
        assert_eq!(append("Y:\\x", "ABCD").as_deref(), Some("Y:\\x\\ABCD"));
        assert_eq!(append("Y:\\", &"a".repeat(256)), None);
        assert_eq!(with_trailing_backslash("Y:\\ABCD".to_owned()), "Y:\\ABCD\\");
        assert_eq!(with_trailing_backslash(String::new()), "");
    }

    #[test]
    fn game_id_folders_are_four_upper_case_letters() {
        assert!(is_game_id_folder("SDAA"));
        assert!(!is_game_id_folder("sdaa"));
        assert!(!is_game_id_folder("SDA"));
        assert!(!is_game_id_folder("SDA1"));
    }
}

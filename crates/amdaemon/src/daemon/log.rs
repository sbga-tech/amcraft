//! The daemon's log: the file every `tracing` event is appended to, and the subscriber that writes it.
//!
//! [`LogPath`] (`App+0x20`, ctor `0x140083F90`) chooses the file: the module path plus `.log` by default, then, in
//! rising priority, `<app root>/<module>.log` (libam's `FixAppRootFolderPath`), `<ABaaS log root>/<module>.log`
//! and the `-l` path (`spec/controllers/lifecycle.md` §3.13, §M8.5). It is shared by the App, the hub and the
//! subscriber, so it is a cheap clonable handle; every event goes to the file chosen at that moment.
//!
//! [`subscriber`] is the daemon's `tracing` subscriber (`tracing-subscriber`'s formatter with local timestamps
//! from chrono). The original's log format, exception kinds and debugger output are not reproduced.

use std::fs::{File, OpenOptions};
use std::io::{self, Stderr, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::fmt::{self, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;

use crate::daemon::paths;

/// Priority assigned by the `-l` command-line option (`0x140083FC0`).
const PRIORITY_COMMAND_LINE: i32 = 3;
/// Priority assigned by the ABaaS log root (`0x1400841C0`).
const PRIORITY_ABAAS_ROOT: i32 = 2;
/// Priority assigned by the application root folder (`0x140084060`).
const PRIORITY_APP_ROOT: i32 = 1;

/// Extension appended to the module name (`0x1405FF8E8`, `0x1405FF918`).
const LOG_EXTENSION: &str = ".log";
/// File name used when the module name is unavailable (`0x1405FF8F8`).
const FALLBACK_FILE_NAME: &str = "amdaemon.log";

/// Filter applied when `RUST_LOG` is unset or invalid.
const DEFAULT_FILTER: &str = "info";

#[derive(Debug)]
struct State {
    priority: i32,
    path: String,
    module_path: String,
}

/// Shared handle to the log file path (`{i32 priority; wstring path}`).
#[derive(Debug, Clone)]
pub struct LogPath {
    state: Arc<Mutex<State>>,
}

impl LogPath {
    /// Constructor `0x140083F90`: priority 0 and the default path, the module path plus `.log` (empty when the
    /// module path is empty, `0x1400847F0`).
    pub fn new(module_path: &str) -> Self {
        let path = if module_path.is_empty() { String::new() } else { format!("{module_path}{LOG_EXTENSION}") };
        Self { state: Arc::new(Mutex::new(State { priority: 0, path, module_path: module_path.to_owned() })) }
    }

    /// Current priority (0 default, 1 app root, 2 ABaaS root, 3 `-l`).
    pub fn priority(&self) -> i32 {
        self.state().priority
    }

    /// Current log file path; empty means "write to stderr".
    pub fn path(&self) -> String {
        self.state().path.clone()
    }

    /// `-l` (`0x140083FC0`): the full path of `path` replaces the current one if the current priority is at
    /// most 3. Empty input or a failed `GetFullPathNameW` changes nothing.
    pub fn set_path(&self, path: &str) {
        if path.is_empty() {
            return;
        }
        self.assign(full_path_name(path), PRIORITY_COMMAND_LINE);
    }

    /// ABaaS log root (`0x1400841C0`): the log file is `<dir>\<module name>.log`, priority 2.
    pub fn set_abaas_root(&self, dir: &str) {
        self.set_in_directory(dir, PRIORITY_ABAAS_ROOT);
    }

    /// Application root folder (`0x140084060`): the log file is `<dir>\<module name>.log`, priority 1.
    pub fn set_app_root(&self, dir: &str) {
        self.set_in_directory(dir, PRIORITY_APP_ROOT);
    }

    fn set_in_directory(&self, dir: &str, priority: i32) {
        if dir.is_empty() {
            return;
        }
        let mut joined = dir.to_owned();
        let name = self.file_name();
        paths::join(&mut joined, &name);
        self.assign(full_path_name(&joined), priority);
    }

    /// `basename(module) + ".log"`, or `amdaemon.log` without a module name (`0x1400846E0`).
    fn file_name(&self) -> String {
        match paths::file_name(&self.state().module_path) {
            "" => FALLBACK_FILE_NAME.to_owned(),
            base => format!("{base}{LOG_EXTENSION}"),
        }
    }

    /// Applies `full` if it is non-empty and the current priority does not exceed `priority`.
    fn assign(&self, full: String, priority: i32) {
        let mut state = self.state();
        if !full.is_empty() && state.priority <= priority {
            state.priority = priority;
            state.path = full;
        }
    }

    /// The state is plain data that every writer leaves consistent, so a panic elsewhere does not invalidate it.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where one formatted event goes: the current log file, stderr while no path is known, or nowhere when the file
/// cannot be opened (the event is dropped, as the original's failed `wofstream` drops it).
#[derive(Debug)]
pub enum LogWriter {
    /// The log file, opened for appending.
    File(File),
    /// No log path is known.
    Stderr(Stderr),
    /// The log file could not be opened.
    Discard,
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.write(buf),
            Self::Stderr(stderr) => stderr.write(buf),
            Self::Discard => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::Stderr(stderr) => stderr.flush(),
            Self::Discard => Ok(()),
        }
    }
}

/// The formatter asks for a writer once per event, so a path change takes effect with the next event.
impl<'a> MakeWriter<'a> for LogPath {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        let path = self.path();
        if path.is_empty() {
            return LogWriter::Stderr(io::stderr());
        }
        OpenOptions::new().append(true).create(true).open(&path).map_or(LogWriter::Discard, LogWriter::File)
    }
}

/// The daemon's subscriber: every event the `RUST_LOG` filter lets through (default `info`) is appended to the
/// file `log` names and printed to stdout, which the console opened by `-w` or a developer dongle shows.
/// Timestamps are local time.
pub fn subscriber(log: LogPath) -> impl Subscriber + Send + Sync {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_timer(ChronoLocal::rfc_3339()).with_writer(log))
        .with(fmt::layer().with_timer(ChronoLocal::rfc_3339()).with_writer(io::stdout))
}

/// `GetFullPathNameW`; empty when the call fails.
fn full_path_name(path: &str) -> String {
    std::path::absolute(path).map(|full| full.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_path_beats_abaas_root_beats_app_root() {
        let log = LogPath::new(r"C:\amd\amdaemon.exe");

        log.set_app_root("/tmp/app_root");
        assert_eq!(log.priority(), 1);
        assert!(log.path().ends_with("amdaemon.exe.log") && log.path().contains("app_root"));

        log.set_abaas_root("/tmp/abaas_root");
        assert_eq!(log.priority(), 2);
        assert!(log.path().contains("abaas_root"));

        log.set_app_root("/tmp/app_root_again");
        assert_eq!(log.priority(), 2, "a lower priority never replaces a higher one");
        assert!(log.path().contains("abaas_root"));

        log.set_path("/tmp/cli.log");
        assert_eq!(log.priority(), 3);
        assert!(log.path().ends_with("cli.log"));

        log.set_abaas_root("/tmp/abaas_root_again");
        log.set_app_root("/tmp/app_root_again");
        assert!(log.path().ends_with("cli.log"), "the -l path is final");
    }

    #[test]
    fn equal_priority_replaces() {
        let log = LogPath::new("amdaemon.exe");
        log.set_abaas_root("/tmp/first");
        log.set_abaas_root("/tmp/second");
        assert!(log.path().contains("second"));
        log.set_path("/tmp/a.log");
        log.set_path("/tmp/b.log");
        assert!(log.path().ends_with("b.log"));
    }
}

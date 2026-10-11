//! The amdaemon 2024 process: `wWinMain`, the `App` lifecycle, the `arcade::Manager` event hub, command-line
//! arguments, the single-instance guard and the log path.
//!
//! Spec: `spec/controllers/lifecycle.md` §1-§5, §7 and §9. libam ([`libam::interface`]) and the IPC
//! server ([`app::ServerExchange`]) are ports. The daemon's own clock, window and console are internal
//! services gathered in [`app::SystemServices`]; [`launch`] manages the process's single-instance lock.

pub mod app;
pub mod arcade;
pub mod args;
pub mod backup;
pub mod clock;
pub mod console;
pub mod launch;
pub mod log;
pub mod registry;
pub mod window;

mod paths;
mod stdio;

use std::ffi::OsString;

use crate::daemon::app::{App, AppServices};
use crate::daemon::args::{ArgumentsParser, USAGE};
use crate::daemon::launch::{LaunchChecker, MUTEX_NAME};

/// `wWinMain` result when the single-instance lock cannot be acquired.
pub const EXIT_ALREADY_RUNNING: i32 = 10;

/// What `wWinMain` obtains from the process: the command line (`__wargv`), the module path
/// (`GetModuleFileNameW(NULL)`) and the single-instance lock name.
pub struct ProcessEnvironment {
    /// The full command line, `argv[0]` included.
    pub argv: Vec<OsString>,
    /// Path of the running executable.
    pub module_path: String,
    /// Base name passed to the cross-platform single-instance guard.
    pub instance_name: String,
}

impl std::fmt::Debug for ProcessEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessEnvironment")
            .field("argv", &self.argv)
            .field("module_path", &self.module_path)
            .field("instance_name", &self.instance_name)
            .finish_non_exhaustive()
    }
}

impl ProcessEnvironment {
    /// The environment of the current process; the module path is empty when it cannot be determined.
    pub fn current() -> Self {
        let module_path = std::env::current_exe().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default();
        Self { argv: std::env::args_os().collect(), module_path, instance_name: MUTEX_NAME.to_owned() }
    }
}

/// `wWinMain` (`0x140013360`).
///
/// 1. The launch guard: when the `AM Daemon` lock cannot be acquired the result is 10 and nothing else is built
///    (`build` is not called).
/// 2. The command line is parsed; a failure prints [`USAGE`] to stderr and **continues** with the default
///    parser.
/// 3. `build` supplies the OS, IPC and libam services and the controllers; the [`App`] is constructed, run and
///    destroyed (its shutdown sequence runs a second time here).
/// 4. The result is 0 whatever [`App::run`] returned.
///
/// The only other result, 1, is reported when `build` returns controllers that are not the 27 of the
/// registry in registry order (a construction bug that the original cannot have).
pub fn run_process(env: ProcessEnvironment, build: impl FnOnce() -> AppServices) -> i32 {
    let ProcessEnvironment { argv, module_path, instance_name } = env;
    let guard = LaunchChecker::new(&instance_name);
    if !guard.is_first() {
        return EXIT_ALREADY_RUNNING;
    }

    let mut parser = ArgumentsParser::new();
    if parser.parse(&argv, &module_path).is_err() {
        stdio::print_stderr(USAGE);
    }

    match App::new(build()) {
        Ok(mut app) => {
            let _ = app.run(&parser);
        }
        Err(error) => {
            stdio::print_stderr(&format!("{error}\n"));
            return 1;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use named_lock::NamedLock;

    use super::*;
    use crate::mock::{self, Fixture};

    fn environment(args: &[&str]) -> ProcessEnvironment {
        static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(0);
        let mut argv: Vec<OsString> = vec!["amdaemon.exe".into()];
        argv.extend(args.iter().map(OsString::from));
        ProcessEnvironment {
            argv,
            module_path: mock::MODULE_PATH.to_owned(),
            instance_name: format!(
                "AM Daemon test {} {}",
                std::process::id(),
                NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    #[test]
    fn second_instance_exits_with_10_before_anything_is_built() {
        let env = environment(&[]);
        let lock = NamedLock::create(&env.instance_name).expect("single-instance lock");
        let _owner = lock.try_lock().expect("first instance owns the lock");
        let built = Rc::new(RefCell::new(false));
        let flag = Rc::clone(&built);
        let code = run_process(env, move || {
            *flag.borrow_mut() = true;
            Fixture::new().into_services()
        });
        assert_eq!(code, EXIT_ALREADY_RUNNING);
        assert!(!*built.borrow(), "no App, platform or controller is constructed behind a failed guard");
    }

    #[test]
    fn first_instance_runs_the_app_and_returns_zero_even_when_run_fails() {
        let mut fixture = Fixture::new();
        fixture.window.creatable = false;
        let trace = fixture.trace.clone();
        let code = run_process(environment(&[]), move || fixture.into_services());
        assert_eq!(code, 0, "App::run returned 1 (window creation failed) but wWinMain still returns 0");
        let lines = mock::lines(&trace);
        assert_eq!(lines.iter().filter(|line| *line == "window.create").count(), 1);
        assert_eq!(
            lines.iter().filter(|line| *line == "am.finalize").count(),
            1,
            "the destructor still runs the shutdown sequence once after a failed initialisation"
        );
    }

    #[test]
    fn rejected_command_line_continues_with_defaults() {
        use crate::daemon::window::MessagePump;
        use libam::interface::LibamEvent;
        use libam::sequence::ProcessState;

        // 1 MHz clock advancing 5000 per read: at 66 Hz only the first of three turns is due, at 120 Hz all three.
        let mut fixture = Fixture::new();
        fixture.clock.frequency = 1_000_000;
        fixture.clock.now.set(1_000_000);
        fixture.clock.step = 5_000;
        fixture.interface.init = Some(Ok(vec![LibamEvent::ChangeState(ProcessState(3))]));
        fixture.window.pump = [MessagePump::Idle, MessagePump::Idle, MessagePump::Quit].into();
        let trace = fixture.trace.clone();
        let code = run_process(environment(&["-w", "maybe", "-f"]), move || fixture.into_services());
        assert_eq!(code, 0);
        let ticks = mock::lines(&trace).iter().filter(|line| *line == "am.execute").count();
        assert_eq!(ticks, 1, "a failed parse assigns nothing, so -f is not applied and the loop runs at 66 Hz");
    }
}

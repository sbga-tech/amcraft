//! The `amdaemon` executable (`wWinMain`, `0x140013360`): internal process services and external IPC and
//! device interfaces composed into the daemon, with the `tracing` subscriber that writes the log. The
//! cabinet daemon exists for Windows only; elsewhere the executable reports that and exits with status 1.

#[cfg(windows)]
fn main() {
    use amdaemon::daemon::app::{AppServices, SystemServices};
    use amdaemon::daemon::backup::IpcBackupMemory;
    use amdaemon::daemon::clock::StdClock;
    use amdaemon::daemon::console::ProcessConsole;
    use amdaemon::daemon::log::{self, LogPath};
    use amdaemon::daemon::window::HiddenWindow;
    use amdaemon::daemon::{ProcessEnvironment, run_process};
    use amdaemon::ipc::server::Server;
    use backend::windows::ipc::WindowsIpcPlatform;
    use libam::clock::SystemClock;

    let environment = ProcessEnvironment::current();
    let log = LogPath::new(&environment.module_path);
    // The only subscriber of the process; installing it cannot fail.
    let _ = tracing::subscriber::set_global_default(log::subscriber(log.clone()));
    let code = run_process(environment, || {
        let system = SystemServices {
            clock: Box::new(StdClock),
            window: Box::new(HiddenWindow::new()),
            console: Box::new(ProcessConsole::default()),
        };
        let ipc = WindowsIpcPlatform::new();
        let server = Server::new(Box::new(ipc.clone()));
        let libam = libam::Libam::new(backend::Backend::windows(), Box::new(SystemClock));
        let backup = Box::new(IpcBackupMemory::new(Box::new(ipc)));
        AppServices::with_libam(system, Box::new(server), &libam, backup, log)
    });
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("amdaemon runs on Windows only");
    std::process::exit(1);
}

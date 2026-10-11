//! The Windows implementation of the backend, one file per interface: the IPC objects shared with the game process,
//! and the operating-system services, devices and vendor DLLs of a cabinet.
//!
//! Every `unsafe` block of the workspace's external interfaces lives here, each with a `// SAFETY:` comment. Crates
//! are used wherever they behave like the original; the raw `windows-sys` calls that remain have no equivalent
//! crate. The IPC files are thin, literal ports of the Win32 calls of `amdipc` (`spec/ipc/transport.md` §2, §4, §8).
//!
//! * [`ipc`] holds [`WindowsIpcPlatform`](ipc::WindowsIpcPlatform), the factory of the IPC objects: the named
//!   mutexes ([`mutex`]), pipe endpoints ([`pipe`]), sections ([`section`]) and the memory protector
//!   ([`protector`]);
//! * the other files implement libam's interfaces and are assembled by [`Backend::windows`](crate::Backend::windows).
//!
//! # Threading of the IPC objects
//!
//! * **Single-threaded use.** A [`WindowsIpcPlatform`](ipc::WindowsIpcPlatform) and everything it creates (pipe
//!   endpoints, sections, mutexes) belongs to the thread that drives it; the concrete pipe endpoint type is `!Send`.
//! * **Mutex thread affinity.** A Win32 mutex is owned by the thread that acquired it, so `lock` / `try_lock` and the
//!   matching `unlock` must happen on the same thread.
//! * **Completion routines only run inside `PipeEndpoint::wait` / `PipeEndpoint::pump`.** `ReadFileEx` and
//!   `WriteFileEx` deliver their results as APCs, which Windows runs only during an alertable wait of the issuing
//!   thread. Nothing happens to an outstanding transfer between those calls. The disconnect that the original
//!   performs *inside* the completion routine is applied by the endpoint's next method call, before that method
//!   does anything else, so the observable sequence is the same.
//! * **Destroying an endpoint with a transfer in flight** cancels it and runs alertable waits (at most
//!   100 ms) until the completion routine has run. If it still has not run (the endpoint is dropped on another
//!   thread than the one that started the transfer), the transfer buffer and the request state are leaked
//!   instead of freed, because the kernel or a later APC may still reference them.
//!
//! The pipe and section code reports failures through return values; the IPC layer above logs them.

pub mod adapters;
pub mod fs;
pub mod hid;
pub mod hwm;
pub mod icmp;
pub mod ipc;
pub mod keychip;
pub mod mutex;
pub mod pipe;
pub mod preload;
pub mod protector;
pub mod registry;
pub mod section;
pub mod services;
pub mod system;
pub mod thinca;
pub mod video;

mod security;
mod util;

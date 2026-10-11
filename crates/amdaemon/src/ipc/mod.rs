//! amdaemon 2024 IPC protocol.
//!
//! Layers, bottom up (`spec/ipc/{transport,messenger_crypto,server}.md`):
//!
//! * [`mutex`] — the recursive-lock accounting of the named mutexes. The OS services the layer needs (named pipes,
//!   sections, mutexes, same-logon memory protection) are interfaces of the [`backend`] crate, implemented
//!   for Windows in `backend::windows` and in-process in `backend::mock::ipc` (the optional mock module).
//! * [`shared_memory`], [`serverio`], [`backup`] — size-stamped named sections, the ServerIO publisher and the
//!   protected Backup object.
//! * [`frame`], [`crypto`], [`messenger`] — 16-byte-header frames, the P-256/AES-GCM session and the
//!   Messenger state machine.
//! * [`relation`], [`header`], [`delta`], [`stream`], [`server`] — relation index allocation, the application
//!   message format, delta publication and the daemon-side [`server::Server`].
//!
//! Dead in the original binary and therefore not implemented: the AES-CBC record mode, and the
//! shared-memory Messenger channel (`mode == 1`). Both Messenger factories pass mode 0.

pub mod backup;
pub mod crypto;
pub mod delta;
pub mod frame;
pub mod header;
pub mod messenger;
pub mod mutex;
pub mod relation;
pub mod server;
pub mod serverio;
pub mod shared_memory;
pub mod stream;

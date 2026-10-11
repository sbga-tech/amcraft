//! Process blocks: `ServerData+0x38B8` (0x18) and `ClientData+0x1A28` (0x30).
//!
//! The request/response pair is shared by `ProcessController` (Kill) and `CommonController` (Language, Reboot,
//! Preload); see `spec/controllers/process_common_systeminfo.md` §1.2 and §4.

use num_enum::TryFromPrimitive;
use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

/// `ServerData.process`: the shared response, the Manager-running flag and the daemon's boot clock.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerProcess {
    /// Response shared by Process and Common (`checkResult` `0x14002AF30`).
    pub response: Response,
    /// `Manager.state == Running`, refreshed every tick (`0x1400427F8`).
    pub manager_running: bool,
    /// Performance-counter ticks taken by the first Process update; echoed by the client in
    /// `ClientProcess::server_boot_clock` before any request is accepted.
    pub boot_clock: i64,
}

/// `ClientData.process`: the shared request header, its per-task info bytes and the boot-clock echo.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientProcess<'b> {
    /// Request generation; a request is outstanding while it differs from the response generation.
    pub generation: u32,
    /// Raw [`ProcessTask`].
    pub task: u32,
    /// Per-task info union, decoded by the [`ClientProcessRef`] payload accessors.
    pub info: &'b [u8; 32],
    /// The client's copy of [`ServerProcess::boot_clock`].
    pub server_boot_clock: i64,
}

server_block!(process, process_mut: ServerProcess, ServerProcessRef, ServerProcessMut @ 0x38B8);
client_block!(process, process_mut: ClientProcess, ClientProcessRef, ClientProcessMut @ 0x1A28);

/// `ipc::process::Task` (name table `0x1405EEEF0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum ProcessTask {
    /// Terminate the daemon (Process).
    Kill = 0,
    /// Change the libam language (Common).
    Language = 1,
    /// Reboot the cabinet (Common).
    Reboot = 2,
    /// Preload a data section (Common).
    Preload = 3,
}

impl ProcessTask {
    /// Name from the task table (`0x1405EEEF0`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Kill => "Kill",
            Self::Language => "Language",
            Self::Reboot => "Reboot",
            Self::Preload => "Preload",
        }
    }
}

/// `NextProcess` of a Kill request (name table `0x1405EBF58`). Other values are accepted and mean `Auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum NextProcess {
    /// Let the platform decide.
    Auto = 0,
    /// SegaBoot.
    SegaBoot = 1,
    /// SegaBoot error screen.
    SegaBootError = 2,
    /// System test.
    SystemTest = 3,
}

impl NextProcess {
    /// Name from the next-process table (`0x1405EBF58`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::SegaBoot => "SegaBoot",
            Self::SegaBootError => "SegaBootError",
            Self::SystemTest => "SystemTest",
        }
    }
}

/// Kill payload.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct KillInfo {
    /// Raw [`NextProcess`].
    pub next_process: u32,
}

/// Language payload: the requested language tag, terminated within 16 units.
#[zero]
#[derive(Debug)]
pub struct LanguageInfo<'b> {
    /// Requested language tag.
    #[zero(capacity = 16)]
    pub language: &'b U16CStr,
}

/// Preload payload.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct PreloadInfo {
    /// Process id handed to libam.
    pub process_id: u32,
}

impl<'w> ClientProcessRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<ProcessTask> {
        ProcessTask::try_from(self.task()).ok()
    }

    /// Kill payload: the raw [`NextProcess`].
    pub fn next_process(&self) -> u32 {
        let (kill, _) = KillInfo::access_prefix(self.info()).expect("an integer payload accepts every bit pattern");
        kill.next_process()
    }

    /// Language payload: the requested tag, `None` when it is not terminated within its 16 units.
    pub fn requested_language(&self) -> Option<&'w U16CStr> {
        let info: &'w [u8; 32] = self.info();
        LanguageInfo::access_prefix(info).ok().map(|(payload, _)| payload.language())
    }

    /// Preload payload: the process id.
    pub fn process_id(&self) -> u32 {
        let (preload, _) =
            PreloadInfo::access_prefix(self.info()).expect("an integer payload accepts every bit pattern");
        preload.process_id()
    }
}

const _: () = {
    assert!(ServerProcess::SCHEMA_SIZE == 0x18);
    assert!(ClientProcess::SCHEMA_SIZE == 0x30);
};

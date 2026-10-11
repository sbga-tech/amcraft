//! APM blocks (`ServerData+0x9020`, `0xC` bytes; `ClientData+0x1A6C`, `0x18` bytes).
//!
//! Evidence: `spec/controllers/abaas_apm.md` §2.2 (claims `abaas_apm.apm.blocks`, `.sd.layout`, `.cd.layout`,
//! `.info.*`, `.task.enum`, `.listener.reason_map`).

use num_enum::TryFromPrimitive;
use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

/// Published `lastGameExitReason` (derived from `am::apm::State` by the listener `0x14001EF50`). The numeric
/// domain is exactly `0..=5`; the enumerator names are not present in the binary.
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ApmGameExitReason {
    /// No broadcast yet, or `am::apm::State` 0 / `>= 6`.
    Unset = 0,
    /// A sub-application is running (state 1, written on GameStart success).
    Running = 1,
    /// Exit outside test mode with `test = 0` (state 2).
    NormalExit = 2,
    /// Exit outside test mode with `test = 1` (state 5).
    TestFlagExit = 3,
    /// Exit while in test mode with `test = 0` (state 4).
    TestModeExit = 4,
    /// Exit while in test mode with `test = 1` (state 3).
    TestModeTestFlagExit = 5,
}

/// `ServerData.apm`: `ipc::apm::ServerResponse` plus the exit reason.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerApm {
    /// Response root `{generation, succeeded}` (`checkResult` `0x14001F6C0`).
    pub response: Response,
    /// Last published application state; [`ApmGameExitReason::Unset`] until the first broadcast. Written only by
    /// the application-state listener of the Apm controller.
    pub last_game_exit_reason: ApmGameExitReason,
}

/// `ClientData.apm`: `ipc::apm::ClientRequest`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientApm<'b> {
    /// Request generation.
    pub generation: u32,
    /// Raw [`ApmTask`] (`cd+6768`).
    pub task: u32,
    /// `ClientInfo` union, decoded by the [`ClientApmRef`] payload accessors.
    pub info: &'b [u8; 0x10],
}

server_block!(apm, apm_mut: ServerApm, ServerApmRef, ServerApmMut @ 0x9020);
client_block!(apm, apm_mut: ClientApm, ClientApmRef, ClientApmMut @ 0x1A6C);

/// `ipc::apm::Task` (name table `0x1405EEE20`); values `>= 2` are rejected at accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum ApmTask {
    /// Start a sub-application (`L"GameStart"` at `0x1405EEB30`).
    GameStart = 0,
    /// End the running sub-application (`L"GameExit"` at `0x1405EEB48`).
    GameExit = 1,
}

impl ApmTask {
    /// Name inserted into the request trace line (`0x14006D770`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::GameStart => "GameStart",
            Self::GameExit => "GameExit",
        }
    }
}

/// Info of [`ApmTask::GameStart`] (`0x10` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct GameStartInfo<'b> {
    /// `amdaemon::Version`: byte 0 = patch, byte 1 = minor, `u16` at 2 = major.
    pub app_version: u32,
    /// Game id: four alphabetic characters plus NUL.
    #[zero(capacity = 5)]
    pub game_id: &'b U16CStr,
    /// Start the sub-application with Aime.
    pub with_aime: bool,
}

/// Info of [`ApmTask::GameExit`]; the remaining bytes of the union are unused.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct GameExitInfo {
    /// Exit with the test flag (`movzx edx, byte [rdi+1A74h]` at `0x14001D82D`).
    pub test: bool,
}

impl<'w> ClientApmRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<ApmTask> {
        ApmTask::try_from(self.task()).ok()
    }

    /// Info of [`ApmTask::GameStart`], `None` when the game id is not terminated within its five units or the
    /// Aime flag is not a boolean.
    pub fn game_start_info(&self) -> Option<GameStartInfoRef<'w>> {
        let info: &'w [u8; 0x10] = self.info();
        GameStartInfo::access_prefix(info).ok().map(|(payload, _)| payload)
    }

    /// Info of [`ApmTask::GameExit`]: the test flag, `None` when the byte is not a boolean.
    pub fn game_exit_test(&self) -> Option<bool> {
        let info: &'w [u8; 0x10] = self.info();
        GameExitInfo::access_prefix(info).ok().map(|(payload, _)| payload.test())
    }
}

const _: () = {
    assert!(ServerApm::SCHEMA_SIZE == 0xC);
    assert!(ClientApm::SCHEMA_SIZE == 0x18);
    assert!(GameStartInfo::SCHEMA_SIZE == 0x10);
    assert!(GameExitInfo::SCHEMA_SIZE == 1);
};

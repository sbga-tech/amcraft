//! LAN install block (`ServerData+0x4BD8`, 0x8). There is no `ClientData` block.
//!
//! Evidence: `spec/controllers/network_install.md` §3.2, claims `network_install.laninstall.*`.

use zero_schema::zero;

use crate::schema::server_block;

/// `ServerData.lanInstall`: the sampled LAN-install mode, the server count and the exit request.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerLanInstall {
    /// libam LAN-install mode is 1 (client); sampled at the ready edge.
    pub client: bool,
    /// libam LAN-install mode is 2 (server); sampled at the ready edge.
    pub server: bool,
    /// Set by NeedToExit reason 1 (`*(u8*)(SD+0x4BDA)`); no clearing path exists.
    pub exit_needed: bool,
    /// libam server count; `-1` = unknown, mode 0, or LAN install failed to start.
    pub server_count: i32,
}

server_block!(lan_install, lan_install_mut: ServerLanInstall, ServerLanInstallRef, ServerLanInstallMut @ 0x4BD8);

const _: () = assert!(ServerLanInstall::SCHEMA_SIZE == 0x8);

//! Checked `zero-schema` views of the shared-memory blocks.
//!
//! Each controller's slice of `ServerData`, `ClientData` or the ServerIO body is its own `#[zero]` root schema
//! placed at a fixed offset of an [`image`]. Placement declarations generate checked accessors on the image:
//!
//! ```ignore
//! server_block!(process, process_mut: ServerProcess, ServerProcessRef, ServerProcessMut @ 0x38B8);
//! client_block!(process, process_mut: ClientProcess, ClientProcessRef, ClientProcessMut @ 0x1A28);
//! ```
//!
//! Every placement must also appear in its image's block map ([`SERVER_DATA_MAP`], [`CLIENT_DATA_MAP`],
//! [`SERVER_IO_MAP`]). Each map is checked at compile time to cover its image exactly once, apart from
//! alignment padding between blocks, which takes over the role of the original root structs: blocks can neither
//! overlap nor leave unexplained bytes.
//!
//! Layout rules: MSVC x64 natural layout, little-endian scalars, native (`little` on every supported target)
//! UTF-16 strings. Compiler padding replaces the explicit padding of the original structs.
//!
//! Strictness: daemon-written blocks use precise types (closed enums, `bool`, terminated strings) and start
//! from zero bytes. Client blocks only declare fields that accept every bit pattern; a request's per-task
//! payload is decoded with a strict payload schema (`access_prefix` over the request's info bytes), and a
//! payload that fails that proof fails the request.

pub mod image;
pub mod shared;
pub mod text;

pub mod abaas;
pub mod aime;
pub mod aime_pay;
pub mod allnet;
pub mod apm;
pub mod app_image;
pub mod backup;
pub mod board_io;
pub mod can;
pub mod common;
pub mod credit;
pub mod emoney;
pub mod error;
pub mod input;
pub mod jvs;
pub mod lan_install;
pub mod net_delivery;
pub mod network;
pub mod output;
pub mod process;
pub mod sequence;
pub mod system_info;
pub mod usb_device;
pub mod usb_io;

/// One block of an image: `(offset, size)`.
pub type BlockSpan = (usize, usize);

/// `ServerData` blocks in offset order (controller constructors in `App::App`, `0x140001000`).
pub const SERVER_DATA_MAP: &[BlockSpan] = &[
    (0x0000, allnet::ServerAccounting::SCHEMA_SIZE),
    (0x0058, sequence::ServerSequence::SCHEMA_SIZE),
    (0x0268, backup::ServerBackup::SCHEMA_SIZE),
    (0x0370, net_delivery::ServerNetDelivery::SCHEMA_SIZE),
    (0x03D0, aime::ServerAime::SCHEMA_SIZE),
    (0x1B88, credit::ServerCredit::SCHEMA_SIZE),
    (0x1C68, emoney::ServerEMoney::SCHEMA_SIZE),
    (0x3478, aime_pay::ServerAimePay::SCHEMA_SIZE),
    (0x38B8, process::ServerProcess::SCHEMA_SIZE),
    (0x38D0, network::ServerNetwork::SCHEMA_SIZE),
    (0x3D40, abaas::ServerAbaasLog::SCHEMA_SIZE),
    (0x3EE0, can::ServerCan::SCHEMA_SIZE),
    (0x478C, board_io::ServerBoardIo::SCHEMA_SIZE),
    (0x4794, usb_device::ServerUsbDevice::SCHEMA_SIZE),
    (0x47A4, allnet::ServerAir::SCHEMA_SIZE),
    (0x47FC, allnet::ServerWiFi::SCHEMA_SIZE),
    (0x4BD8, lan_install::ServerLanInstall::SCHEMA_SIZE),
    (0x4BE0, allnet::ServerAuth::SCHEMA_SIZE),
    (0x56B0, error::ServerError::SCHEMA_SIZE),
    (0x9020, apm::ServerApm::SCHEMA_SIZE),
    (0x902C, common::ServerCommon::SCHEMA_SIZE),
    (0x9050, system_info::ServerSystemInfo::SCHEMA_SIZE),
    (0x94EC, input::ServerInput::SCHEMA_SIZE),
    (0x10E7E, output::ServerOutput::SCHEMA_SIZE),
    (0x19110, app_image::ServerAppImage::SCHEMA_SIZE),
];

/// `ClientData` blocks in offset order.
pub const CLIENT_DATA_MAP: &[BlockSpan] = &[
    (0x0000, backup::ClientBackup::SCHEMA_SIZE),
    (0x0218, usb_io::ClientUsbIo::SCHEMA_SIZE),
    (0x0B78, jvs::ClientJvs::SCHEMA_SIZE),
    (0x0BB8, sequence::ClientSequence::SCHEMA_SIZE),
    (0x0BF0, aime::ClientAime::SCHEMA_SIZE),
    (0x0C40, credit::ClientCredit::SCHEMA_SIZE),
    (0x0C84, emoney::ClientEMoney::SCHEMA_SIZE),
    (0x0CC0, aime_pay::ClientAimePay::SCHEMA_SIZE),
    (0x0D00, allnet::ClientAccounting::SCHEMA_SIZE),
    (0x0E10, abaas::ClientAbaasLog::SCHEMA_SIZE),
    (0x161C, usb_device::ClientUsbDevice::SCHEMA_SIZE),
    (0x1628, can::ClientCan::SCHEMA_SIZE),
    (0x1808, network::ClientNetwork::SCHEMA_SIZE),
    (0x1810, net_delivery::ClientNetDelivery::SCHEMA_SIZE),
    (0x1A1C, board_io::ClientBoardIo::SCHEMA_SIZE),
    (0x1A28, process::ClientProcess::SCHEMA_SIZE),
    (0x1A58, error::ClientError::SCHEMA_SIZE),
    (0x1A64, allnet::ClientAir::SCHEMA_SIZE),
    (0x1A6C, apm::ClientApm::SCHEMA_SIZE),
];

/// ServerIO body blocks in offset order.
pub const SERVER_IO_MAP: &[BlockSpan] =
    &[(0x0000, usb_io::ServerUsbIo::SCHEMA_SIZE), (0x08A0, jvs::ServerJvs::SCHEMA_SIZE)];

/// Largest gap allowed between blocks: alignment padding of the original root (blocks align to at most 8).
const MAX_PADDING: usize = 7;

/// `map` is in offset order and covers `0..size` exactly once, apart from gaps of at most [`MAX_PADDING`] bytes.
const fn tiles(map: &[BlockSpan], size: usize) -> bool {
    let mut end = 0;
    let mut i = 0;
    while i < map.len() {
        let (offset, len) = map[i];
        if offset < end || offset - end > MAX_PADDING {
            return false;
        }
        end = offset + len;
        i += 1;
    }
    end <= size && size - end <= MAX_PADDING
}

/// `map` lists a block of `len` bytes at `offset`.
const fn lists(map: &[BlockSpan], offset: usize, len: usize) -> bool {
    let mut i = 0;
    while i < map.len() {
        if map[i].0 == offset && map[i].1 == len {
            return true;
        }
        i += 1;
    }
    false
}

const _: () = {
    assert!(tiles(SERVER_DATA_MAP, image::SERVER_DATA_SIZE), "ServerData blocks must tile the image");
    assert!(tiles(CLIENT_DATA_MAP, image::CLIENT_DATA_SIZE), "ClientData blocks must tile the image");
    assert!(tiles(SERVER_IO_MAP, image::SERVER_IO_BODY_SIZE), "ServerIO blocks must tile the body");
};

/// Places a block schema in an image and generates `$get` / `$get_mut` accessors on the image.
macro_rules! placed_block {
    ($image:ty, $map:expr, $get:ident, $get_mut:ident: $schema:ident, $view:ident, $view_mut:ident @ $offset:expr) => {
        const _: () = {
            assert!(($offset) % $schema::SCHEMA_ALIGN == 0, "a block offset must satisfy its schema alignment");
            assert!($crate::schema::lists($map, $offset, $schema::SCHEMA_SIZE), "a block must be in its image's map");
            assert!($schema::ZERO_IS_VALID, "every image starts zero-filled");
        };

        impl $image {
            #[doc = concat!("Checked read view of [`", stringify!($schema), "`] at `", stringify!($offset), "`.")]
            pub fn $get(&self) -> $view<'_> {
                $schema::access(self.span($offset, $schema::SCHEMA_SIZE)).expect(<$image>::BLOCK_INVARIANT)
            }

            #[doc = concat!("Checked write view of [`", stringify!($schema), "`] at `", stringify!($offset), "`.")]
            pub fn $get_mut(&mut self) -> $view_mut<'_> {
                $schema::access_mut(self.span_mut($offset, $schema::SCHEMA_SIZE)).expect(<$image>::BLOCK_INVARIANT)
            }
        }
    };
}
pub(crate) use placed_block;

/// Places a daemon-written block in [`image::ServerImage`] (`ServerData`).
macro_rules! server_block {
    ($get:ident, $get_mut:ident: $schema:ident, $view:ident, $view_mut:ident @ $offset:expr) => {
        $crate::schema::placed_block!(
            $crate::schema::image::ServerImage, $crate::schema::SERVER_DATA_MAP, $get, $get_mut: $schema, $view, $view_mut @ $offset
        );
    };
}
pub(crate) use server_block;

/// Places a game-written block in [`image::ClientImage`] (`ClientData`). The schema must accept every bit
/// pattern: integers and byte arrays only.
macro_rules! client_block {
    ($get:ident, $get_mut:ident: $schema:ident, $view:ident, $view_mut:ident @ $offset:expr) => {
        $crate::schema::placed_block!(
            $crate::schema::image::ClientImage, $crate::schema::CLIENT_DATA_MAP, $get, $get_mut: $schema, $view, $view_mut @ $offset
        );
    };
}
pub(crate) use client_block;

/// Places a daemon-written block in [`image::ServerIoImage`] (the ServerIO body).
macro_rules! server_io_block {
    ($get:ident, $get_mut:ident: $schema:ident, $view:ident, $view_mut:ident @ $offset:expr) => {
        $crate::schema::placed_block!(
            $crate::schema::image::ServerIoImage, $crate::schema::SERVER_IO_MAP, $get, $get_mut: $schema, $view, $view_mut @ $offset
        );
    };
}
pub(crate) use server_io_block;

#[cfg(test)]
mod tests {
    use super::image::ClientImage;

    /// Reads every `ClientData` block; a block with a field that rejects some bit pattern panics here.
    fn read_every_client_block(image: &ClientImage) {
        let _ = image.backup();
        let _ = image.usb_io();
        let _ = image.jvs();
        let _ = image.sequence();
        let _ = image.aime();
        let _ = image.credit();
        let _ = image.emoney();
        let _ = image.aime_pay();
        let _ = image.allnet_accounting();
        let _ = image.abaas_log();
        let _ = image.usb_device();
        let _ = image.can();
        let _ = image.network();
        let _ = image.net_delivery();
        let _ = image.board_io();
        let _ = image.process();
        let _ = image.error();
        let _ = image.allnet_air();
        let _ = image.apm();
    }

    /// The game may write any bytes into `ClientData`, and the daemon must still read every block: a malformed
    /// request fails through its payload decode, never through block access.
    #[test]
    fn every_client_block_reads_whatever_bytes_the_game_writes() {
        let mut image = ClientImage::new();
        for byte in [0x01u8, 0x02, 0x7F, 0x80, 0xFF] {
            image.bytes_mut().fill(byte);
            read_every_client_block(&image);
        }
        // xorshift64: varying bytes, so no field sees only one repeated value.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..16 {
            for byte in image.bytes_mut() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            read_every_client_block(&image);
        }
    }
}

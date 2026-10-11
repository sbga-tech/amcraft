//! Owned byte images of the three shared-memory roots.
//!
//! The daemon keeps `ServerData`, `ClientData` and the ServerIO body as aligned, zero-initialised bytes. The IPC
//! layer publishes and merges these bytes; controllers reach their own block through a checked `zero-schema`
//! capability placed at the block's offset (the `server_block!`, `client_block!` and `server_io_block!`
//! declarations in [`crate::schema`]).
//!
//! The daemon is the only writer of [`ServerImage`] and [`ServerIoImage`]: they start zeroed (every placed block
//! asserts `ZERO_IS_VALID`) and expose no raw mutable bytes, so their blocks stay type-valid and access cannot
//! fail. [`ClientImage`] receives arbitrary bytes from the game; its blocks therefore declare only fields that
//! accept every bit pattern (integers and byte arrays) and decode stricter payloads per request.

use core::fmt;

use zerocopy::IntoBytes;

/// Size of the `ServerData` root (`memset` in `Server::Impl::Impl`, `0x14006C6B0`).
pub const SERVER_DATA_SIZE: usize = 0x1A5E8;
/// Size of the `ClientData` root (`memset` in `0x14006C6B0`).
pub const CLIENT_DATA_SIZE: usize = 0x1A88;
/// Size of the ServerIO body published through the SVIO section (`Sender::Impl+8`, `0x140075DB0`).
pub const SERVER_IO_BODY_SIZE: usize = 0x1668;

macro_rules! image {
    ($(#[$doc:meta])* $name:ident, $size:expr, $invariant:literal) => {
        $(#[$doc])*
        pub struct $name {
            // `u64` storage gives the 8-byte alignment of the original roots without `unsafe`.
            words: Box<[u64]>,
        }

        impl $name {
            /// Size of the image in bytes.
            pub const SIZE: usize = $size;
            /// Why a placed block's checked access cannot fail.
            pub(crate) const BLOCK_INVARIANT: &str = $invariant;

            /// A zero-filled image.
            pub fn new() -> Self {
                Self { words: vec![0; $size / 8].into_boxed_slice() }
            }

            /// The image bytes.
            pub fn bytes(&self) -> &[u8] {
                self.words.as_bytes()
            }

            pub(crate) fn span(&self, offset: usize, len: usize) -> &[u8] {
                &self.bytes()[offset..offset + len]
            }

            pub(crate) fn span_mut(&mut self, offset: usize, len: usize) -> &mut [u8] {
                &mut self.words.as_mut_bytes()[offset..offset + len]
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name)).field("size", &Self::SIZE).finish_non_exhaustive()
            }
        }

        const _: () = assert!($size % 8 == 0);
    };
}

image!(
    /// `ServerData` (`0x1A5E8` bytes): written only by the daemon's controllers, published to the game.
    ServerImage,
    SERVER_DATA_SIZE,
    "daemon-written blocks start zeroed and change only through their checked capability"
);

image!(
    /// `ClientData` (`0x1A88` bytes): written by the game, merged by prefix, read by the controllers.
    ClientImage,
    CLIENT_DATA_SIZE,
    "client blocks declare only fields that accept every bit pattern"
);

image!(
    /// The ServerIO body (`0x1668` bytes): written by the JVS and USBIO controllers, published through the SVIO
    /// section.
    ServerIoImage,
    SERVER_IO_BODY_SIZE,
    "daemon-written blocks start zeroed and change only through their checked capability"
);

impl ClientImage {
    /// The image bytes, for the IPC prefix merge and for a game-side writer.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.words.as_mut_bytes()
    }
}

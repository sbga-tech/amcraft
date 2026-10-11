//! AppImage block (`ServerData+0x19110`, `0x14D8`, ends exactly at the `ServerData` image end).
//!
//! Evidence: `spec/controllers/backup_appimage.md` §5.2, §5.5. There is no client block. Field names are
//! inferred (the old documentation calls the block `ServerAppImageData`).

use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::server_block;

/// Capacity of [`ServerAppImage::option_infos`].
pub const MAX_OPTIONS: usize = 200;

/// One installed option image (`0x18` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct OptionImageInfo<'b> {
    /// Creation time of the image, `DateTime` microseconds (0 when invalid).
    pub creation_time: i64,
    /// Four-character option id (`wchar_t[5]`).
    #[zero(capacity = 5)]
    pub name: &'b U16CStr,
}

/// `ServerData.app_image`: the current app image, the option images and the option root directory.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAppImage<'b> {
    /// Version of the current app image, copied raw from libam: `major << 16 | minor << 8 | patch`
    /// (`amdaemon::Version::make` `0x140062E60`).
    pub current_version: u32,
    /// Creation time of the current app image, `DateTime` microseconds (0 when unavailable).
    pub creation_time: i64,
    /// **Unclamped** libam option count (at most 100 in practice).
    pub option_count: u32,
    /// Option images; only the first `min(option_count, 200)` entries are rewritten on a reload.
    pub option_infos: [OptionImageInfo<'b>; MAX_OPTIONS],
    /// Option root directory (`wchar_t[256]`).
    #[zero(capacity = 256)]
    pub option_root_path: &'b U16CStr,
}

server_block!(app_image, app_image_mut: ServerAppImage, ServerAppImageRef, ServerAppImageMut @ 0x19110);

const _: () = {
    assert!(OptionImageInfo::SCHEMA_SIZE == 0x18 && OptionImageInfo::SCHEMA_ALIGN == 8);
    assert!(ServerAppImage::SCHEMA_SIZE == 0x14D8);
};

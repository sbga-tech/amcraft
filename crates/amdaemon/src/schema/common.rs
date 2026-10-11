//! Common block (`ServerData+0x902C`, 0x24). Common's requests arrive in the shared process block
//! ([`crate::schema::process::ClientProcess`]).

use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::server_block;

/// `ServerData.common`: player total and current language.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerCommon<'b> {
    /// Low 32 bits of libam's player total (`updatePlayerTotal`).
    pub player_count: u32,
    /// libam language, at most 14 units (`wcsncpy_s(dst, 15, src, 14)`).
    #[zero(capacity = 15)]
    pub language: &'b U16CStr,
}

server_block!(common, common_mut: ServerCommon, ServerCommonRef, ServerCommonMut @ 0x902C);

const _: () = assert!(ServerCommon::SCHEMA_SIZE == 0x24);

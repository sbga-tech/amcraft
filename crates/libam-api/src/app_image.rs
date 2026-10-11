//! `am::appimage`: application and option images.

use crate::result::AmResult;
use crate::time::Tm;

/// Number of characters of an option id.
pub const OPTION_ID_LEN: usize = 4;

/// `am::appimage::Property` kind (`prop+0x08`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    /// Game application image.
    App = 0,
    /// Option image (carries a four-character id).
    Option = 1,
    /// Operating-system image.
    Os = 2,
}

/// The fields of libam's 632-byte `am::appimage::Property` that amdaemon reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppImageProperty {
    /// `+0x08` kind.
    pub kind: PropertyKind,
    /// `+0x0C` "initialised" byte set by the App/OS setters (ignored for options).
    pub initialized: bool,
    /// `+0x10` version dword of an App/OS image, `major << 16 | minor << 8 | patch`.
    pub version: u32,
    /// `+0x3C` option id (`wchar_t[5]`, NUL-terminated).
    pub option_id: [u16; 5],
    /// `+0x4C` creation timestamp.
    pub creation: Tm,
}

impl AppImageProperty {
    /// `Property::isValid` (`0x14021F9D0`): an option needs a four-character alphanumeric id, an App/OS image
    /// the initialised byte; both need a positive `tm_mday`. `iswalnum` is approximated by Unicode
    /// alphanumerics (inferred).
    pub fn is_valid(&self) -> bool {
        let identified = match self.kind {
            PropertyKind::Option => {
                let length = self.option_id.iter().position(|&unit| unit == 0).unwrap_or(self.option_id.len());
                length == OPTION_ID_LEN
                    && self.option_id[..OPTION_ID_LEN]
                        .iter()
                        .all(|&unit| char::from_u32(u32::from(unit)).is_some_and(char::is_alphanumeric))
            }
            PropertyKind::App | PropertyKind::Os => self.initialized,
        };
        identified && self.creation.mday > 0
    }
}

/// libam `am::appimage::Manager` accessors used by the AppImage controller.
pub trait AppImagePort {
    /// `acquireAppImage` (`0x140218A30`): `Err(code 2)` while the libam context `[0x1408A9C40]` is absent.
    fn acquire_app_image(&self) -> AmResult;

    /// `currentApp` (`0x140217070`): the APM-started app if one is active, otherwise the last app of the image
    /// list. `None` when there is none (libam result 4).
    fn current_app(&self) -> Option<AppImageProperty>;

    /// `optionCount(type = Option)` (`0x140216EA0`): `0` while an APM game id is set, else the list's option count.
    fn option_count(&self) -> u32;

    /// `optionProperty(type = Option, index)` (`0x140216F30`): `None` for an index past the count.
    fn option_property(&self, index: u32) -> Option<AppImageProperty>;

    /// `optionRoot` (`0x140217100`): the option root directory, `None` unless the image list is loaded (list state
    /// `ctx+55216 == 2`). A `wchar_t[260]` at `list+0x140` (`ctx+55520`), NUL-terminated or filling the array: the list
    /// loader `0x140244DC0` fills it with `wcscpy_s(list+320, 0x104, ..)` and the next member, the app array, starts
    /// `0x208` bytes (260 units) behind it at `list+840`.
    fn option_root(&self) -> Option<[u16; 260]>;
}

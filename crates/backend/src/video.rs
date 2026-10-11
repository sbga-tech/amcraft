//! The vendor video DLL `amVideo` (`spec/libam/platform.md` part C 2.7): the one call that applies the display
//! setting. The registry bookkeeping around it is libam's (`libam::platform::video`).

use crate::Unsupported;

/// The display mode of the request, as the vendor DLL numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// Mode 0: one monitor.
    Single,
    /// Mode 1: the same picture on every output.
    Clone,
    /// Mode 4: two monitors.
    Dual,
}

impl DisplayMode {
    /// The mode number of the vendor DLL request.
    pub const fn code(self) -> u32 {
        match self {
            Self::Single => 0,
            Self::Clone => 1,
            Self::Dual => 4,
        }
    }
}

/// The request handed to the vendor DLL (`0x1402F97F0`, command 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayRequest {
    /// The display mode.
    pub mode: DisplayMode,
    /// Widths of display 1 and 2 (low 16 bits of the descriptor values).
    pub width: [u16; 2],
    /// Heights of display 1 and 2.
    pub height: [u16; 2],
    /// Rotation codes of display 1 and 2: 0, 1 (90), 2 (180) or 3 (270).
    pub rotation: [u32; 2],
}

/// Why the vendor DLL could not be run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VideoError {
    /// The DLL, or its command entry point, does not exist (`GetLastError() == 2`).
    #[error("the display library is missing")]
    NotFound,
    /// The DLL could not be loaded or initialised.
    #[error("the display library failed")]
    Failed,
}

/// The vendor video DLL (`amVideo`).
pub trait VideoDriver {
    /// `LoadLibraryW(library)`, ordinal 1 `Init`, ordinal 3 `Command(ctx, request)` (command 2), ordinal 2 `Exit`,
    /// `FreeLibrary`. Returns the DLL's return code: 0 applied, 1 invalid argument, 4/5 unsupported, 8..=12 the
    /// output verdicts.
    fn apply(&mut self, library: &str, request: &DisplayRequest) -> Result<u32, VideoError>;
}

/// No vendor display library.
impl VideoDriver for Unsupported {
    fn apply(&mut self, _library: &str, _request: &DisplayRequest) -> Result<u32, VideoError> {
        Err(VideoError::NotFound)
    }
}

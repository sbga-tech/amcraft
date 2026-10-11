//! The vendor video DLL `amVideo` (`spec/libam/platform.md` C 2.7), bound with `libloading` by ordinal.

use std::ffi::c_void;

use libloading::os::windows::{Library, Symbol};

use crate::{DisplayRequest, VideoDriver, VideoError};

type VideoInit = unsafe extern "system" fn(*mut c_void) -> i32;
type VideoCommand = unsafe extern "system" fn(*mut c_void, *const c_void) -> i32;

/// The vendor video DLL, loaded for the one display call and freed again (`0x1402F9E90`, `0x1402F9730`).
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsVideoDriver;

/// The 28-byte request of command 2 (`0x1402F97F0`).
fn video_request(request: &DisplayRequest) -> [u8; 28] {
    let mut bytes = [0u8; 28];
    bytes[0..4].copy_from_slice(&2u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&request.mode.code().to_le_bytes());
    bytes[12..14].copy_from_slice(&request.width[0].to_le_bytes());
    bytes[14..16].copy_from_slice(&request.height[0].to_le_bytes());
    bytes[16..18].copy_from_slice(&request.width[1].to_le_bytes());
    bytes[18..20].copy_from_slice(&request.height[1].to_le_bytes());
    bytes[20..24].copy_from_slice(&request.rotation[0].to_le_bytes());
    bytes[24..28].copy_from_slice(&request.rotation[1].to_le_bytes());
    bytes
}

/// `ERROR_FILE_NOT_FOUND`. `libloading` keeps the code of a failed load private; its `Debug` is the plain number.
fn is_file_not_found(error: &libloading::Error) -> bool {
    match error {
        libloading::Error::LoadLibraryExW { source } => format!("{source:?}") == "2",
        _ => false,
    }
}

impl VideoDriver for WindowsVideoDriver {
    fn apply(&mut self, library: &str, request: &DisplayRequest) -> Result<u32, VideoError> {
        // SAFETY: loading runs the DLL's initialisation routines, as the original's `LoadLibraryW` does.
        let library = unsafe { Library::new(library) }
            .map_err(|error| if is_file_not_found(&error) { VideoError::NotFound } else { VideoError::Failed })?;
        // SAFETY: ordinals 1 to 3 are looked up in the module loaded above; they have the signatures of the vendor
        // interface (`Init(ctx)`, `Exit(ctx)`, `Command(ctx, request)`).
        let (init, exit, command) = unsafe {
            (
                library.get_ordinal::<VideoInit>(1),
                library.get_ordinal::<VideoInit>(2),
                library.get_ordinal::<VideoCommand>(3),
            )
        };
        match (init, exit, command) {
            (Ok(init), Ok(exit), Ok(command)) => run_command(&init, &exit, &command, request),
            (Ok(_), Ok(_), Err(_)) => Err(VideoError::NotFound),
            _ => Err(VideoError::Failed),
        }
    }
}

/// `Init`, `Command`, `Exit` on the context of the vendor interface: the zeroed 1 KiB buffer with its first dword
/// set to 1. `Exit` runs only after a successful `Init`.
fn run_command(
    init: &Symbol<VideoInit>,
    exit: &Symbol<VideoInit>,
    command: &Symbol<VideoCommand>,
    request: &DisplayRequest,
) -> Result<u32, VideoError> {
    let mut context = [0u32; 256];
    context[0] = 1;
    let context = context.as_mut_ptr().cast::<c_void>();
    let bytes = video_request(request);
    // SAFETY: the symbols come from a library that is still loaded; the context outlives the three calls and the
    // request is the 28 bytes command 2 reads.
    unsafe {
        if init(context) != 0 {
            return Err(VideoError::Failed);
        }
        let code = command(context, bytes.as_ptr().cast::<c_void>());
        exit(context);
        Ok(code as u32)
    }
}

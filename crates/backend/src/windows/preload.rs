//! `amWorkaroundPreloadDatasec` (`spec/libam/platform.md` A.5): the preloader walks the module image ranges with
//! `proc-maps` and reads them with `read-process-memory`. What stays raw `windows-sys` is `GetNativeSystemInfo`
//! for the architecture gate (std only knows the architecture libam was built for, not the machine's).

use proc_maps::get_process_maps;
use read_process_memory::{CopyAddress, ProcessHandle};
use windows_sys::Win32::System::SystemInformation::{GetNativeSystemInfo, SYSTEM_INFO};

use crate::{PreloadError, Preloader};

/// `PROCESSOR_ARCHITECTURE_AMD64`.
const ARCHITECTURE_AMD64: u16 = 9;
/// The page size of an AMD64 machine.
const PAGE_SIZE: usize = 4096;

/// `amWorkaroundPreloadDatasec(pid, 0)` (A.5).
pub struct WindowsPreloader;

impl Preloader for WindowsPreloader {
    fn preload(&mut self, process_id: u32) -> Result<(), PreloadError> {
        let process_id = if process_id == 0 { std::process::id() } else { process_id };
        let process = ProcessHandle::try_from(process_id).map_err(|_| PreloadError)?;
        page_in(process_id, &process)
    }
}

/// The processor architecture of the machine, not of this build (`GetNativeSystemInfo`).
fn native_architecture() -> u16 {
    let mut info = SYSTEM_INFO::default();
    // SAFETY: `info` is a live `SYSTEM_INFO` that the call fills.
    unsafe { GetNativeSystemInfo(&mut info) };
    // SAFETY: both members of the anonymous union describe the same leading dwords; the architecture is valid in
    // either view after the call.
    unsafe { info.Anonymous.Anonymous.wProcessorArchitecture }
}

/// The architecture gate (`0x1402D3560`, for a 64-bit libam) and the touch loop over every module.
///
/// `0x1402D37E0` reads one byte of every page of each module image that is committed and accessible. The module
/// list failing is not an error, like the original's `EnumProcessModulesEx`.
fn page_in(process_id: u32, process: &ProcessHandle) -> Result<(), PreloadError> {
    if native_architecture() != ARCHITECTURE_AMD64 {
        return Err(PreloadError);
    }
    let Ok(maps) = get_process_maps(process_id) else {
        return Ok(());
    };
    let mut byte = [0u8; 1];
    // Only the ranges that belong to a module image (`filename` is set for those). `PAGE_NOACCESS` is neither
    // readable nor executable; `PAGE_EXECUTE` alone is still touched.
    for range in maps.iter().filter(|range| range.filename().is_some() && (range.is_read() || range.is_exec())) {
        let end = range.start().saturating_add(range.size());
        for address in (range.start()..end).step_by(PAGE_SIZE) {
            // A failed read is ignored like the original's.
            let _ = process.copy_address(address, &mut byte);
        }
    }
    Ok(())
}

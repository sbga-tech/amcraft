//! [`Section`] on Win32 file mappings.

use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::{self, NonNull};

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, FALSE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
};

use crate::section::Section;
use crate::windows::security::{OBJECT_ACCESS_MASK, SecurityHelper};
use crate::windows::util::{last_error, own_or_null, wide};

/// A mapped view of a named section (`MapViewOfFile`, `FILE_MAP_WRITE`); unmapped and closed on drop.
///
/// The section object itself is never deleted: it lives while any process holds a handle.
#[derive(Debug)]
pub(super) struct WindowsSection {
    view: NonNull<u8>,
    len: usize,
    /// Held only so that the section object is closed after the view has been unmapped.
    _mapping: OwnedHandle,
}

impl WindowsSection {
    /// `createMemoryMap` primitive (`0x140077440`): opens the section `name` or creates it with `mapping_len`
    /// bytes and a current-user DACL, then maps `mapping_len` bytes. Also returns whether this call created it.
    pub(super) fn create(name: &str, mapping_len: usize) -> Option<(Self, bool)> {
        let security = SecurityHelper::new(OBJECT_ACCESS_MASK)?;
        let wide_name = wide(name);
        // SAFETY: `wide_name` is NUL-terminated and outlives the call; the result is NULL or a fresh handle.
        let existing = unsafe { own_or_null(OpenFileMappingW(FILE_MAP_WRITE, FALSE, wide_name.as_ptr())) };
        let (mapping, created) = match existing {
            Some(mapping) => (mapping, false),
            None => {
                let size = u64::try_from(mapping_len).ok()?;
                // SAFETY: page-file backed section (`INVALID_HANDLE_VALUE`); the attributes and `wide_name` are
                // valid for the call and `size` is passed as its high and low DWORD.
                let raw = unsafe {
                    CreateFileMappingW(
                        INVALID_HANDLE_VALUE,
                        security.attributes(),
                        PAGE_READWRITE,
                        (size >> 32) as u32,
                        size as u32,
                        wide_name.as_ptr(),
                    )
                };
                // Must directly follow the call: `ERROR_ALREADY_EXISTS` says that `raw` is an existing section.
                let created = last_error() != ERROR_ALREADY_EXISTS;
                // SAFETY: `raw` is NULL or a fresh handle.
                (unsafe { own_or_null(raw) }?, created)
            }
        };
        Some((Self::map(mapping, mapping_len)?, created))
    }

    /// `openCreatedMemoryMap` primitive (`0x1400779A0`): opens an existing section and maps all of it.
    pub(super) fn open(name: &str) -> Option<Self> {
        let wide_name = wide(name);
        // SAFETY: `wide_name` is NUL-terminated and outlives the call; the result is NULL or a fresh handle.
        let mapping = unsafe { own_or_null(OpenFileMappingW(FILE_MAP_WRITE, FALSE, wide_name.as_ptr())) }?;
        let mut section = Self::map(mapping, 0)?;
        section.len = section.region_len()?;
        Some(section)
    }

    /// Maps `len` bytes of `mapping` (`0` maps the whole section). `mapping` is closed again on failure.
    fn map(mapping: OwnedHandle, len: usize) -> Option<Self> {
        // SAFETY: `mapping` is an open section handle with write access; the call has no other preconditions.
        let view = unsafe { MapViewOfFile(mapping.as_raw_handle(), FILE_MAP_WRITE, 0, 0, len) };
        let view = NonNull::new(view.Value.cast::<u8>())?;
        Some(Self { view, len, _mapping: mapping })
    }

    /// Size of the memory region that starts at the view (`VirtualQuery`), rounded up to whole pages.
    fn region_len(&self) -> Option<usize> {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `self.view` is a mapped address and `info` is a writable buffer of the stated size.
        let written =
            unsafe { VirtualQuery(self.view.as_ptr().cast(), &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
        (written != 0).then_some(info.RegionSize)
    }

    /// Whether `offset..offset + len` lies inside the mapped bytes.
    fn contains(&self, offset: usize, len: usize) -> bool {
        offset.checked_add(len).is_some_and(|end| end <= self.len)
    }
}

impl Drop for WindowsSection {
    fn drop(&mut self) {
        // SAFETY: `self.view` is the base address returned by `MapViewOfFile` and is unmapped exactly once; the
        // mapping handle is closed afterwards when the field is dropped.
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.as_ptr().cast() }) };
    }
}

impl Section for WindowsSection {
    fn byte_len(&self) -> usize {
        self.len
    }

    fn read(&self, offset: usize, out: &mut [u8]) -> bool {
        if !self.contains(offset, out.len()) {
            return false;
        }
        // SAFETY: the source range lies inside the mapped view; `out` is a distinct, exclusively borrowed slice.
        // Other processes may write the view concurrently, so it is copied without forming a reference to it.
        unsafe { ptr::copy_nonoverlapping(self.view.as_ptr().add(offset), out.as_mut_ptr(), out.len()) };
        true
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> bool {
        if !self.contains(offset, data.len()) {
            return false;
        }
        // SAFETY: the destination range lies inside the mapped view; `data` is a distinct slice. See `read` for why
        // this is a raw copy.
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), self.view.as_ptr().add(offset), data.len()) };
        true
    }
}

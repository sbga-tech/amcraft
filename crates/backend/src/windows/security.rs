//! Current-user DACLs of the named objects (`SecurityHelper`, `spec/ipc/transport.md` §2.4).

use std::cell::UnsafeCell;
use std::fmt;
use std::os::windows::io::OwnedHandle;
use std::ptr;

use windows_sys::Win32::Foundation::{FALSE, TRUE};
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, AddAccessAllowedAce, GetLengthSid, InitializeAcl, InitializeSecurityDescriptor,
    LookupAccountNameW, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SID_NAME_USE,
    SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
};
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::Win32::System::WindowsProgramming::GetUserNameW;

use crate::windows::util::own_or_null;

/// DACL mask of named pipes and file mappings: `GENERIC_READ` plus `0x2` (`FILE_WRITE_DATA` / `SECTION_MAP_WRITE`).
pub(super) const OBJECT_ACCESS_MASK: u32 = 0x8000_0002;
/// DACL mask of the per-pipe I/O and connect events: `GENERIC_ALL`.
pub(super) const EVENT_ACCESS_MASK: u32 = 0x1000_0000;
/// `SECURITY_DESCRIPTOR_REVISION`.
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
/// Capacity of the `GetUserNameW` buffer, in UTF-16 units including the terminator.
const USER_NAME_CAPACITY: usize = 257;

/// Security attributes granting exactly one access mask to the current user (`SecurityHelper`, spec §2.4).
///
/// The SID, the ACL and the descriptor live in separate heap allocations, so the pointers stored inside the
/// descriptor and in the attributes stay valid when the helper itself is moved.
pub(super) struct SecurityHelper {
    sid: Vec<u32>,
    acl: Vec<u32>,
    descriptor: Box<UnsafeCell<SECURITY_DESCRIPTOR>>,
    attributes: SECURITY_ATTRIBUTES,
}

impl SecurityHelper {
    /// Builds the helper for `mask`; `None` when the user SID, the ACL or the descriptor cannot be built.
    pub(super) fn new(mask: u32) -> Option<Self> {
        let mut sid = current_user_sid()?;
        let sid_ptr: PSID = sid.as_mut_ptr().cast();
        // SAFETY: `sid_ptr` points to the valid SID filled in by `current_user_sid`.
        let acl_len = unsafe { GetLengthSid(sid_ptr) }.checked_add(16)?;
        let mut acl = vec![0u32; (acl_len as usize).div_ceil(4)];
        let acl_ptr: *mut ACL = acl.as_mut_ptr().cast();
        // SAFETY: `acl_ptr` addresses at least `acl_len` bytes of writable, DWORD-aligned storage and `sid_ptr` is
        // a valid SID that is not freed during the calls.
        let acl_ready = unsafe {
            InitializeAcl(acl_ptr, acl_len, ACL_REVISION) != 0
                && AddAccessAllowedAce(acl_ptr, ACL_REVISION, mask, sid_ptr) != 0
        };
        if !acl_ready {
            return None;
        }
        let descriptor = Box::new(UnsafeCell::new(SECURITY_DESCRIPTOR::default()));
        let descriptor_ptr: PSECURITY_DESCRIPTOR = descriptor.get().cast();
        // SAFETY: `descriptor_ptr` addresses a writable, zeroed `SECURITY_DESCRIPTOR`; the SID and the ACL are
        // heap allocations that outlive the descriptor because the helper owns all three.
        let descriptor_ready = unsafe {
            InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) != 0
                && SetSecurityDescriptorDacl(descriptor_ptr, TRUE, acl_ptr, FALSE) != 0
                && SetSecurityDescriptorOwner(descriptor_ptr, sid_ptr, FALSE) != 0
        };
        if !descriptor_ready {
            return None;
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor_ptr,
            bInheritHandle: FALSE,
        };
        Some(Self { sid, acl, descriptor, attributes })
    }

    /// The `SECURITY_ATTRIBUTES` to pass to `Create*` calls; valid for as long as `self` is alive.
    pub(super) fn attributes(&self) -> &SECURITY_ATTRIBUTES {
        &self.attributes
    }
}

impl fmt::Debug for SecurityHelper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecurityHelper")
            .field("sid", &self.sid)
            .field("acl", &self.acl)
            .field("descriptor", &self.descriptor)
            .field("security_descriptor", &self.attributes.lpSecurityDescriptor)
            .finish()
    }
}

/// SID of the current user, in DWORD-aligned storage (`GetUserNameW` + `LookupAccountNameW`, spec §2.4 step 1).
fn current_user_sid() -> Option<Vec<u32>> {
    let mut name = [0u16; USER_NAME_CAPACITY];
    let mut name_len = USER_NAME_CAPACITY as u32;
    // SAFETY: `name` provides `name_len` writable UTF-16 units.
    if unsafe { GetUserNameW(name.as_mut_ptr(), &mut name_len) } == 0 {
        return None;
    }
    let mut sid_len = 0u32;
    let mut domain_len = 0u32;
    let mut kind: SID_NAME_USE = 0;
    // SAFETY: size probe with NULL buffers of size zero; `name` is the NUL-terminated name written above. The call
    // is expected to fail with the required sizes in `sid_len` and `domain_len`.
    unsafe {
        LookupAccountNameW(
            ptr::null(),
            name.as_ptr(),
            ptr::null_mut(),
            &mut sid_len,
            ptr::null_mut(),
            &mut domain_len,
            &mut kind,
        )
    };
    if sid_len == 0 {
        return None;
    }
    let mut sid = vec![0u32; (sid_len as usize).div_ceil(4)];
    let mut domain = vec![0u16; domain_len as usize];
    // SAFETY: `sid` provides at least `sid_len` writable bytes and `domain` provides `domain_len` UTF-16 units, as
    // the probe requested; `name` is still NUL-terminated.
    let found = unsafe {
        LookupAccountNameW(
            ptr::null(),
            name.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    (found != 0).then_some(sid)
}

/// Unnamed manual-reset, initially non-signalled event with the attributes of `security`.
pub(super) fn create_event(security: &SecurityHelper) -> Option<OwnedHandle> {
    // SAFETY: the attributes are valid for the call and the name is NULL; the result is NULL or a fresh handle.
    unsafe { own_or_null(CreateEventW(security.attributes(), TRUE, FALSE, ptr::null())) }
}

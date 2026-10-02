// Who may open the relay pipe.
//
// Without an explicit security descriptor a named pipe gets the default DACL of
// the creating token, which Microsoft documents as: full control for SYSTEM,
// administrators and the creator owner, *read* access for Everyone and the
// anonymous account. Read access is not enough to talk to us (the relay opens the
// pipe read+write), but it lets any account on the machine open the pipe and sit
// in a connection slot, and it says nothing about who may create further
// instances. This module gives every instance a DACL with a single allow entry:
// the SID of the user running Coucou.
//
// What it does not do: it cannot stop another process of the *same* user. That is
// the trust boundary of the whole relay (such a process can already read the
// user's settings and Coucou's memory); coucou-hook's server check has the same
// edge. Administrators are not granted access either, but an administrator can
// still take ownership of the object, so this is not a defence against one.

use std::ffi::c_void;
use std::io;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

/// A security descriptor that grants all access to one SID and to nobody else.
pub struct OwnerOnly {
    descriptor: PSECURITY_DESCRIPTOR,
}

// SAFETY: the descriptor is a heap block that is written once, in `for_sid`, and
// only read afterwards (the kernel copies it when a pipe instance is created).
unsafe impl Send for OwnerOnly {}
unsafe impl Sync for OwnerOnly {}

impl OwnerOnly {
    /// `sid` is the textual form, `S-1-5-21-…`.
    pub fn for_sid(sid: &str) -> io::Result<Self> {
        // The SID comes from our own token, but it ends up inside an SDDL string:
        // refuse anything that is not shaped like one rather than trust it.
        if !sid.starts_with("S-1-")
            || !sid
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'-' || b == b'S')
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a SID"));
        }
        // D:P = protected DACL, one ACE: Allow, GENERIC_ALL, to this SID.
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})\0").encode_utf16().collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is NUL-terminated and outlives the call; `descriptor` is a
        // valid out-pointer. On success the block is ours and `Drop` frees it.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        Ok(OwnerOnly { descriptor })
    }

    /// Creates a pipe instance carrying this descriptor. Every instance has to
    /// come through here: one created without it would have the default DACL.
    pub fn create(&self, options: &ServerOptions, name: &str) -> io::Result<NamedPipeServer> {
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: `attrs` is a valid SECURITY_ATTRIBUTES whose descriptor stays
        // alive for the call (owned by `self`); the kernel copies it into the pipe.
        unsafe {
            options.create_with_security_attributes_raw(
                name,
                (&mut attrs as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
            )
        }
    }
}

impl Drop for OwnerOnly {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW,
        // which documents LocalFree as the release.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.descriptor.0)));
        }
    }
}

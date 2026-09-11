//! Secrets protected by Windows for the signed-in account: Credential Manager
//! entries, where several providers' own command-line tools keep their sign-in
//! tokens, and raw DPAPI blobs.
//!
//! Both rely on DPAPI, which ties the encryption to the current Windows
//! account on this machine. Nothing read through here was stored in plain text.

use std::ffi::c_void;

#[repr(C)]
struct CredentialW {
    flags: u32,
    type_: u32,
    target_name: *mut u16,
    comment: *mut u16,
    last_written: u64,
    credential_blob_size: u32,
    credential_blob: *mut u8,
    persist: u32,
    attribute_count: u32,
    attributes: *mut c_void,
    target_alias: *mut u16,
    user_name: *mut u16,
}

#[link(name = "Advapi32")]
extern "system" {
    fn CredReadW(
        target_name: *const u16,
        type_: u32,
        reserved_flags: u32,
        credential: *mut *mut CredentialW,
    ) -> i32;
    fn CredFree(buffer: *mut c_void);
}

/// Return the raw secret stored under a generic credential `target`, or `None`
/// when there is no such entry or it is empty.
///
/// Returns bytes rather than text because tools disagree on the encoding: the
/// Go keyring used by `gh` writes UTF-8, while Node-based tools such as the
/// Copilot CLI write UTF-16. Callers decode what they expect.
///
/// Deliberately silent on a missing entry. Several callers probe more than one
/// location in turn, and an absent entry is the ordinary case for all but one.
pub(super) fn read_generic(target: &str) -> Option<Vec<u8>> {
    const CRED_TYPE_GENERIC: u32 = 1;

    let target_wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut credential: *mut CredentialW = std::ptr::null_mut();
    let ok = unsafe { CredReadW(target_wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
    if ok == 0 || credential.is_null() {
        return None;
    }

    unsafe {
        let entry = &*credential;
        let bytes = if entry.credential_blob_size == 0 || entry.credential_blob.is_null() {
            None
        } else {
            Some(
                std::slice::from_raw_parts(entry.credential_blob, entry.credential_blob_size as usize)
                    .to_vec(),
            )
        };
        CredFree(credential as *mut c_void);
        bytes
    }
}

#[repr(C)]
struct CryptIntegerBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[link(name = "crypt32")]
extern "system" {
    fn CryptUnprotectData(
        data_in: *const CryptIntegerBlob,
        data_description: *mut *mut u16,
        optional_entropy: *const CryptIntegerBlob,
        reserved: *mut c_void,
        prompt_struct: *mut c_void,
        flags: u32,
        data_out: *mut CryptIntegerBlob,
    ) -> i32;
}

extern "system" {
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

/// Decrypt a DPAPI blob that was protected for the current Windows account
/// with no additional entropy — the form written by Chromium's OSCrypt and by
/// PowerShell's `ConvertFrom-SecureString`.
///
/// Fails for a blob made by another account or on another machine, which is
/// exactly the protection DPAPI provides. Silent on failure; callers log what
/// the failure means in their own context.
pub(super) fn dpapi_unprotect(data: &[u8]) -> Option<Vec<u8>> {
    let input = CryptIntegerBlob {
        cb_data: u32::try_from(data.len()).ok()?,
        pb_data: data.as_ptr() as *mut u8,
    };
    let mut output = CryptIntegerBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };

    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut output,
        )
    };
    if ok == 0 || output.pb_data.is_null() {
        return None;
    }

    Some(unsafe {
        let plain = std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec();
        LocalFree(output.pb_data as *mut c_void);
        plain
    })
}

/// Encrypt for the current account, as `ConvertFrom-SecureString` does. Only
/// tests need to produce blobs; the application only ever reads them.
#[cfg(test)]
pub(super) fn dpapi_protect(data: &[u8]) -> Option<Vec<u8>> {
    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            data_in: *const CryptIntegerBlob,
            description: *const u16,
            optional_entropy: *const CryptIntegerBlob,
            reserved: *mut c_void,
            prompt_struct: *mut c_void,
            flags: u32,
            data_out: *mut CryptIntegerBlob,
        ) -> i32;
    }

    let input = CryptIntegerBlob {
        cb_data: u32::try_from(data.len()).ok()?,
        pb_data: data.as_ptr() as *mut u8,
    };
    let mut output = CryptIntegerBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut output,
        )
    };
    if ok == 0 || output.pb_data.is_null() {
        return None;
    }
    Some(unsafe {
        let blob = std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec();
        LocalFree(output.pb_data as *mut c_void);
        blob
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trips_for_the_current_account() {
        let secret = b"not a real token";
        let blob = dpapi_protect(secret).expect("DPAPI protect");
        assert_ne!(blob.as_slice(), secret.as_slice(), "the blob must not hold the plain text");
        assert_eq!(dpapi_unprotect(&blob).as_deref(), Some(secret.as_slice()));
    }

    #[test]
    fn dpapi_rejects_data_it_did_not_produce() {
        assert!(dpapi_unprotect(b"").is_none());
        assert!(dpapi_unprotect(b"definitely not a DPAPI blob").is_none());
        let mut tampered = dpapi_protect(b"not a real token").unwrap();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        assert!(dpapi_unprotect(&tampered).is_none(), "a modified blob must not decrypt");
    }
}

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

/// Every DPAPI blob starts with a version of 1 followed by the DPAPI provider
/// GUID {df9d8cd0-1501-11d1-8c7a-00c04fc297eb}.
const DPAPI_BLOB_HEADER: [u8; 20] = [
    0x01, 0x00, 0x00, 0x00, 0xd0, 0x8c, 0x9d, 0xdf, 0x01, 0x15, 0xd1, 0x11, 0x8c, 0x7a, 0x00,
    0xc0, 0x4f, 0xc2, 0x97, 0xeb,
];

/// Why a value could not be used as `ConvertFrom-SecureString` output. The
/// reasons are logged, so none of them ever carries the value itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SecureStringError {
    NotHex,
    NotDpapi,
    Undecryptable,
    NotText,
}

impl SecureStringError {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::NotHex => {
                "it is not the output of ConvertFrom-SecureString; a plain-text value is never used"
            }
            Self::NotDpapi => "it is not DPAPI-protected data",
            Self::Undecryptable => {
                "it could not be decrypted; it must be created by this Windows account on this PC"
            }
            Self::NotText => "it did not decrypt to text",
        }
    }
}

/// Decrypt what PowerShell's `ConvertFrom-SecureString` writes when no key is
/// given: hexadecimal DPAPI data, protected for the current account with no
/// extra entropy, wrapping UTF-16 text.
pub(super) fn decrypt_secure_string(value: &str) -> Result<String, SecureStringError> {
    let blob = decode_hex(value.trim()).ok_or(SecureStringError::NotHex)?;
    if !blob.starts_with(&DPAPI_BLOB_HEADER) {
        return Err(SecureStringError::NotDpapi);
    }
    let plain = dpapi_unprotect(&blob).ok_or(SecureStringError::Undecryptable)?;
    if plain.len() % 2 != 0 {
        return Err(SecureStringError::NotText);
    }
    let units: Vec<u16> = plain
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).map_err(|_| SecureStringError::NotText)
}

/// Read an environment variable that must hold `ConvertFrom-SecureString`
/// output, returning the decrypted secret.
///
/// An unset or empty variable is simply absent. A value that is not protected
/// data — including a plain-text secret — is refused, and the log names the
/// variable and the reason, never the value.
pub(super) fn protected_environment_value(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    match decrypt_secure_string(value) {
        Ok(secret) => {
            let secret = secret.trim();
            if secret.is_empty() {
                log_once(name, format!("{name} ignored: it decrypted to an empty value"));
                None
            } else {
                Some(secret.to_string())
            }
        }
        Err(error) => {
            log_once(name, format!("{name} ignored: {}", error.reason()));
            None
        }
    }
}

/// Note, by name only, a variable this fork no longer reads, so a setup that
/// used the old name is told why it stopped working rather than failing
/// silently. The value is never read.
pub(super) fn note_retired_variable(retired: &str, replacement: &str) {
    if std::env::var_os(retired).is_some_and(|value| !value.is_empty()) {
        log_once(
            retired,
            format!("{retired} is no longer read; set {replacement} instead, as described in the README"),
        );
    }
}

/// Environment variables cannot change while the process runs, so there is
/// nothing new to say after the first poll; this keeps the diagnostic log from
/// repeating the same line every few minutes.
pub(super) fn log_once(key: &str, message: String) {
    static LOGGED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut logged = LOGGED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if logged.iter().any(|seen| seen == key) {
        return;
    }
    logged.push(key.to_string());
    drop(logged);
    crate::diagnose::log(message);
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.is_empty() || value.len() % 2 != 0 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

/// Produce a value exactly as `ConvertFrom-SecureString` would: UTF-16 text,
/// protected for this account, written as lowercase hexadecimal.
#[cfg(test)]
pub(super) fn convert_from_secure_string(text: &str) -> String {
    let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    dpapi_protect(&utf16)
        .expect("DPAPI protect")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_string_output_decrypts_to_the_original_text() {
        let protected = convert_from_secure_string("a=b; c%3A%3Ad");
        assert!(protected.starts_with("01000000d08c9ddf0115d1118c7a00c04fc297eb"));
        assert_eq!(decrypt_secure_string(&protected).as_deref(), Ok("a=b; c%3A%3Ad"));
        assert_eq!(
            decrypt_secure_string(&protected.to_uppercase()).as_deref(),
            Ok("a=b; c%3A%3Ad"),
            "hex case must not matter"
        );
    }

    #[test]
    fn anything_else_is_refused_with_a_reason() {
        assert_eq!(decrypt_secure_string("plain secret"), Err(SecureStringError::NotHex));
        assert_eq!(decrypt_secure_string("abc"), Err(SecureStringError::NotHex));
        assert_eq!(
            decrypt_secure_string("00112233445566778899aabbccddeeff00112233"),
            Err(SecureStringError::NotDpapi)
        );
        let forged = format!("01000000d08c9ddf0115d1118c7a00c04fc297eb{}", "00".repeat(40));
        assert_eq!(decrypt_secure_string(&forged), Err(SecureStringError::Undecryptable));
    }

    #[test]
    fn a_protected_variable_is_read_and_a_plain_one_refused() {
        // Unique names: tests run in parallel within one process.
        let protected = "CCUMPRO_TEST_PROTECTED_VARIABLE";
        let plain = "CCUMPRO_TEST_PLAIN_VARIABLE";
        std::env::set_var(protected, convert_from_secure_string("  the secret  "));
        std::env::set_var(plain, "the secret");
        assert_eq!(protected_environment_value(protected).as_deref(), Some("the secret"));
        assert_eq!(protected_environment_value(plain), None);
        assert_eq!(protected_environment_value("CCUMPRO_TEST_UNSET_VARIABLE"), None);
        std::env::remove_var(protected);
        std::env::remove_var(plain);
    }

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

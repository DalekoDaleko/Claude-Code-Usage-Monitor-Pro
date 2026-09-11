//! Reads the OAuth token that the Claude desktop app keeps for its bundled
//! Claude Code build.
//!
//! Machines that only ever ran Claude Code through the desktop app have no
//! `~/.claude/.credentials.json`, because that file is written by the
//! standalone CLI login flow. The desktop app is an Electron application and
//! stores its token cache with Chromium's OSCrypt scheme instead: an
//! AES-256-GCM key sits DPAPI-wrapped in `Local State`, and each encrypted
//! value is `"v10" || nonce || ciphertext || tag`.
//!
//! Everything here is read-only, runs as the signed-in user, and degrades to
//! `None` whenever the layout is not what we expect.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use crate::diagnose;

const TOKEN_CACHE_KEY: &str = "oauth:tokenCache";
const DPAPI_KEY_PREFIX: &[u8] = b"DPAPI";
const OS_CRYPT_PREFIX: &[u8] = b"v10";
const GCM_NONCE_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;
/// Desktop entries are keyed `"<ids>:<base url>:<scope> <scope> …"`, and the
/// app keeps several tokens with different scopes. The usage endpoint needs
/// both of these, as Claude Code's own token has; a token with the inference
/// scope alone is refused with 403.
const INFERENCE_SCOPE: &str = "user:inference";
const PROFILE_SCOPE: &str = "user:profile";
const BCRYPT_INIT_AUTH_MODE_INFO_VERSION: u32 = 1;

pub(super) struct DesktopToken {
    pub(super) access_token: String,
    pub(super) expires_at: Option<i64>,
}

/// The Microsoft Store build is the MSIX package `Claude_<publisher id>`.
const STORE_PACKAGE_PREFIX: &str = "Claude_";
/// Publisher id of the certificate Anthropic signs the Store package with.
const ANTHROPIC_PUBLISHER_ID: &str = "pzs8sxrjxfjjc";
/// Windows derives every publisher id as 13 characters of lowercase base32.
const PUBLISHER_ID_LEN: usize = 13;

/// Folders the Claude desktop app may keep its data in, most likely first.
///
/// The installer from claude.ai uses `%APPDATA%\Claude`. The Microsoft Store
/// build is an MSIX package, and Windows redirects that app's writes to AppData
/// into the package's own folder,
/// `%LOCALAPPDATA%\Packages\Claude_<publisher id>\LocalCache\Roaming\Claude`,
/// which the app itself still sees as `%APPDATA%\Claude`. Both layouts are the
/// same inside, and a machine can have both, for instance after switching from
/// one build to the other, so every one found is returned.
pub(super) fn data_directories() -> Vec<PathBuf> {
    data_directories_in(
        dirs::config_dir().as_deref(),
        dirs::data_local_dir().as_deref(),
    )
}

fn data_directories_in(roaming: Option<&Path>, local: Option<&Path>) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = roaming
        .map(|roaming| roaming.join("Claude"))
        .into_iter()
        .collect();
    let Some(packages) = local.map(|local| local.join("Packages")) else {
        return directories;
    };
    let mut names: Vec<String> = std::fs::read_dir(&packages)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_store_package(name))
        .collect();
    // Anthropic's own package first, then any other in a stable order.
    names.sort_by_key(|name| (!name.ends_with(ANTHROPIC_PUBLISHER_ID), name.clone()));
    directories.extend(names.into_iter().map(|name| {
        packages
            .join(name)
            .join("LocalCache")
            .join("Roaming")
            .join("Claude")
    }));
    directories
}

fn is_store_package(name: &str) -> bool {
    name.strip_prefix(STORE_PACKAGE_PREFIX)
        .is_some_and(|publisher_id| {
            publisher_id.len() == PUBLISHER_ID_LEN
                && publisher_id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

/// Whether a desktop-app path lies inside a Microsoft Store package folder.
pub(super) fn is_store_install(path: &Path) -> bool {
    let names: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    names
        .windows(2)
        .any(|pair| pair[0].eq_ignore_ascii_case("Packages") && is_store_package(&pair[1]))
}

/// Every place a desktop-app token cache may be, in [`data_directories`] order.
pub(super) fn config_paths() -> Vec<PathBuf> {
    data_directories()
        .into_iter()
        .map(|directory| directory.join("config.json"))
        .collect()
}

fn local_state_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("Local State")
}

pub(super) fn read_token(config_path: &Path) -> Option<DesktopToken> {
    let config = match std::fs::read_to_string(config_path) {
        Ok(config) => config,
        Err(error) => {
            // Both install layouts are checked, so one of them missing is normal.
            if diagnose::is_enabled() && error.kind() != std::io::ErrorKind::NotFound {
                diagnose::log_error(
                    &format!(
                        "unable to read Claude desktop config at {}",
                        config_path.display()
                    ),
                    error,
                );
            }
            return None;
        }
    };

    let cache = token_cache_value(&config)?;
    let key = os_crypt_key(&local_state_path(config_path))?;
    let plaintext = decrypt_os_crypt_value(&cache, &key)?;
    let plaintext = String::from_utf8(plaintext).ok()?;
    let token = select_token(&plaintext);
    if token.is_none() {
        diagnose::log("Claude desktop token cache held no usable inference token");
    }
    token
}

/// Signature over the encrypted cache rather than the file's mtime: the
/// desktop app rewrites `config.json` for unrelated state such as window
/// placement, and that must not read as a credential change.
pub(super) fn watch_signature(config_path: &Path) -> String {
    let key = format!("desktop:{}", config_path.display());
    match std::fs::read_to_string(config_path)
        .ok()
        .and_then(|config| token_cache_value(&config))
    {
        Some(cache) => format!("{key}|present|{}", fnv1a(cache.as_bytes())),
        None => format!("{key}|missing"),
    }
}

fn token_cache_value(config: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(config).ok()?;
    Some(json.get(TOKEN_CACHE_KEY)?.as_str()?.to_string())
}

/// Picks the freshest entry carrying both the inference and the profile scope,
/// which the usage endpoint needs. Failing that, the freshest with the
/// inference scope, then the freshest of any scope, so a future key layout
/// still resolves to something.
fn select_token(plaintext: &str) -> Option<DesktopToken> {
    let json: serde_json::Value = serde_json::from_str(plaintext).ok()?;
    let entries = json.as_object()?;

    let mut best: Option<((bool, bool, i64), DesktopToken)> = None;
    for (key, entry) in entries {
        let Some(access_token) = entry.get("token").and_then(|value| value.as_str()) else {
            continue;
        };
        if access_token.is_empty() {
            continue;
        }
        let expires_at = entry.get("expiresAt").and_then(|value| value.as_i64());
        let scopes = entry_scopes(key);
        let inference = scopes.contains(&INFERENCE_SCOPE);
        let rank = (
            inference && scopes.contains(&PROFILE_SCOPE),
            inference,
            expires_at.unwrap_or(i64::MIN),
        );
        if best
            .as_ref()
            .is_some_and(|(best_rank, _)| *best_rank >= rank)
        {
            continue;
        }
        best = Some((
            rank,
            DesktopToken {
                access_token: access_token.to_string(),
                expires_at,
            },
        ));
    }

    best.map(|(_, token)| token)
}

/// The scopes at the end of a cache key, `"<ids>:<base url>:<scope> <scope> …"`.
/// Every scope has the form `<kind>:<name>`, so the first one is the last two
/// `:`-separated parts of the first word, and the rest are whole words.
fn entry_scopes(key: &str) -> Vec<&str> {
    let mut words = key.split(' ');
    let first = words.next().unwrap_or_default();
    let first_scope = first
        .rmatch_indices(':')
        .nth(1)
        .map(|(index, _)| &first[index + 1..]);
    first_scope
        .into_iter()
        .chain(words)
        .filter(|scope| !scope.is_empty())
        .collect()
}

fn os_crypt_key(local_state_path: &Path) -> Option<Vec<u8>> {
    let local_state = std::fs::read_to_string(local_state_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&local_state).ok()?;
    let encoded = json.get("os_crypt")?.get("encrypted_key")?.as_str()?;
    let wrapped = base64_decode(encoded)?;
    let wrapped = wrapped.strip_prefix(DPAPI_KEY_PREFIX)?;
    dpapi_unprotect(wrapped)
}

fn decrypt_os_crypt_value(value: &str, key: &[u8]) -> Option<Vec<u8>> {
    let blob = base64_decode(value)?;
    let body = blob.strip_prefix(OS_CRYPT_PREFIX)?;
    if body.len() < GCM_NONCE_LEN + GCM_TAG_LEN {
        return None;
    }
    let (nonce, rest) = body.split_at(GCM_NONCE_LEN);
    let (ciphertext, tag) = rest.split_at(rest.len() - GCM_TAG_LEN);
    aes_gcm_decrypt(key, nonce, ciphertext, tag)
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let input = input.trim_end_matches('=');
    if input.len() % 4 == 1 {
        return None;
    }
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    let padding_mask = (1u32 << bits).saturating_sub(1);
    (buffer & padding_mask == 0).then_some(output)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[repr(C)]
struct AuthenticatedCipherModeInfo {
    cb_size: u32,
    dw_info_version: u32,
    pb_nonce: *mut u8,
    cb_nonce: u32,
    pb_auth_data: *mut u8,
    cb_auth_data: u32,
    pb_tag: *mut u8,
    cb_tag: u32,
    pb_mac_context: *mut u8,
    cb_mac_context: u32,
    cb_aad: u32,
    cb_data: u64,
    dw_flags: u32,
}

#[link(name = "bcrypt")]
extern "system" {
    fn BCryptOpenAlgorithmProvider(
        algorithm: *mut *mut c_void,
        id: *const u16,
        implementation: *const u16,
        flags: u32,
    ) -> i32;
    fn BCryptCloseAlgorithmProvider(algorithm: *mut c_void, flags: u32) -> i32;
    fn BCryptGetProperty(
        object: *mut c_void,
        property: *const u16,
        output: *mut u8,
        output_len: u32,
        result: *mut u32,
        flags: u32,
    ) -> i32;
    fn BCryptSetProperty(
        object: *mut c_void,
        property: *const u16,
        input: *const u8,
        input_len: u32,
        flags: u32,
    ) -> i32;
    fn BCryptGenerateSymmetricKey(
        algorithm: *mut c_void,
        key: *mut *mut c_void,
        key_object: *mut u8,
        key_object_len: u32,
        secret: *const u8,
        secret_len: u32,
        flags: u32,
    ) -> i32;
    fn BCryptDestroyKey(key: *mut c_void) -> i32;
    fn BCryptDecrypt(
        key: *mut c_void,
        input: *const u8,
        input_len: u32,
        padding_info: *const c_void,
        iv: *mut u8,
        iv_len: u32,
        output: *mut u8,
        output_len: u32,
        result: *mut u32,
        flags: u32,
    ) -> i32;
}

fn dpapi_unprotect(data: &[u8]) -> Option<Vec<u8>> {
    let key = super::windows_credentials::dpapi_unprotect(data);
    if key.is_none() {
        diagnose::log("unable to unwrap the Claude desktop OSCrypt key with DPAPI");
    }
    key
}

fn aes_gcm_decrypt(key: &[u8], nonce: &[u8], ciphertext: &[u8], tag: &[u8]) -> Option<Vec<u8>> {
    let algorithm_id = wide("AES");
    let mut algorithm: *mut c_void = std::ptr::null_mut();
    if unsafe {
        BCryptOpenAlgorithmProvider(&mut algorithm, algorithm_id.as_ptr(), std::ptr::null(), 0)
    } != 0
    {
        diagnose::log("unable to open the AES provider for the Claude desktop token cache");
        return None;
    }

    let plaintext = with_gcm_key(algorithm, key, |key_handle| {
        decrypt_with_key(key_handle, nonce, ciphertext, tag)
    });

    unsafe { BCryptCloseAlgorithmProvider(algorithm, 0) };
    plaintext
}

fn with_gcm_key(
    algorithm: *mut c_void,
    key: &[u8],
    decrypt: impl FnOnce(*mut c_void) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let chaining_property = wide("ChainingMode");
    let chaining_gcm = wide("ChainingModeGCM");
    if unsafe {
        BCryptSetProperty(
            algorithm,
            chaining_property.as_ptr(),
            chaining_gcm.as_ptr() as *const u8,
            u32::try_from(std::mem::size_of_val(chaining_gcm.as_slice())).ok()?,
            0,
        )
    } != 0
    {
        diagnose::log("unable to select GCM chaining for the Claude desktop token cache");
        return None;
    }

    let object_length_property = wide("ObjectLength");
    let mut object_length = 0u32;
    let mut written = 0u32;
    if unsafe {
        BCryptGetProperty(
            algorithm,
            object_length_property.as_ptr(),
            &mut object_length as *mut u32 as *mut u8,
            u32::try_from(std::mem::size_of::<u32>()).ok()?,
            &mut written,
            0,
        )
    } != 0
    {
        return None;
    }

    // The key object buffer must outlive the key handle it backs.
    let mut key_object = vec![0u8; object_length as usize];
    let mut key_handle: *mut c_void = std::ptr::null_mut();
    if unsafe {
        BCryptGenerateSymmetricKey(
            algorithm,
            &mut key_handle,
            key_object.as_mut_ptr(),
            object_length,
            key.as_ptr(),
            u32::try_from(key.len()).ok()?,
            0,
        )
    } != 0
    {
        diagnose::log("unable to import the Claude desktop OSCrypt key");
        return None;
    }

    let plaintext = decrypt(key_handle);
    unsafe { BCryptDestroyKey(key_handle) };
    drop(key_object);
    plaintext
}

fn decrypt_with_key(
    key_handle: *mut c_void,
    nonce: &[u8],
    ciphertext: &[u8],
    tag: &[u8],
) -> Option<Vec<u8>> {
    let mut nonce = nonce.to_vec();
    let mut tag = tag.to_vec();
    let mode_info = AuthenticatedCipherModeInfo {
        cb_size: u32::try_from(std::mem::size_of::<AuthenticatedCipherModeInfo>()).ok()?,
        dw_info_version: BCRYPT_INIT_AUTH_MODE_INFO_VERSION,
        pb_nonce: nonce.as_mut_ptr(),
        cb_nonce: u32::try_from(nonce.len()).ok()?,
        pb_auth_data: std::ptr::null_mut(),
        cb_auth_data: 0,
        pb_tag: tag.as_mut_ptr(),
        cb_tag: u32::try_from(tag.len()).ok()?,
        pb_mac_context: std::ptr::null_mut(),
        cb_mac_context: 0,
        cb_aad: 0,
        cb_data: 0,
        dw_flags: 0,
    };

    let mut plaintext = vec![0u8; ciphertext.len()];
    let mut written = 0u32;
    let status = unsafe {
        BCryptDecrypt(
            key_handle,
            ciphertext.as_ptr(),
            u32::try_from(ciphertext.len()).ok()?,
            &mode_info as *const AuthenticatedCipherModeInfo as *const c_void,
            std::ptr::null_mut(),
            0,
            plaintext.as_mut_ptr(),
            u32::try_from(plaintext.len()).ok()?,
            &mut written,
            0,
        )
    };

    if status != 0 {
        diagnose::log("Claude desktop token cache failed AES-GCM authentication");
        return None;
    }

    plaintext.truncate(written as usize);
    Some(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_freshest_inference_scoped_token() {
        let plaintext = r#"{
            "install:user:https://api.anthropic.com:user:profile": {
                "token": "profile-only",
                "expiresAt": 9000000000000
            },
            "install:user:https://api.anthropic.com:user:inference user:profile": {
                "token": "current",
                "expiresAt": 1818644595762
            },
            "install:old:https://api.anthropic.com:user:inference": {
                "token": "stale",
                "expiresAt": 1518644595762
            }
        }"#;

        let token = select_token(plaintext).expect("an inference token should be selected");
        assert_eq!(token.access_token, "current");
        assert_eq!(token.expires_at, Some(1818644595762));
    }

    #[test]
    fn ignores_entries_without_a_usable_token() {
        assert!(select_token(r#"{"install:user:scope": {"expiresAt": 1}}"#).is_none());
        assert!(select_token(r#"{"install:user:scope": {"token": ""}}"#).is_none());
        assert!(select_token("not json").is_none());
    }

    #[test]
    fn reads_the_token_cache_out_of_a_desktop_config() {
        let config = r#"{"locale": "en-US", "oauth:tokenCache": "djEwYWJj"}"#;
        assert_eq!(token_cache_value(config).as_deref(), Some("djEwYWJj"));
        assert!(token_cache_value(r#"{"locale": "en-US"}"#).is_none());
    }

    #[test]
    fn rejects_blobs_that_are_not_os_crypt_v10() {
        // Valid base64, but the version prefix is not "v10".
        assert!(decrypt_os_crypt_value("bm90LXYxMC1kYXRh", &[0u8; 32]).is_none());
        // Right prefix, too short to hold a nonce and a tag.
        assert!(decrypt_os_crypt_value("djEwc2hvcnQ", &[0u8; 32]).is_none());
        assert!(decrypt_os_crypt_value("!!!", &[0u8; 32]).is_none());
    }

    #[test]
    fn decodes_standard_base64_with_and_without_padding() {
        assert_eq!(base64_decode("djEw").unwrap(), b"v10");
        assert_eq!(base64_decode("YWJjZA==").unwrap(), b"abcd");
        assert!(base64_decode("a").is_none());
        assert!(base64_decode("a-b_").is_none());
    }

    /// Ignored by default: this one proves the real DPAPI + AES-GCM path
    /// against whatever the Claude desktop app has on the current machine,
    /// whether it was installed from claude.ai or from the Microsoft Store.
    /// Run it with `cargo test -- --ignored` while signed in to the app.
    #[test]
    #[ignore = "requires a signed-in Claude desktop app on this machine"]
    fn reads_a_token_from_the_installed_desktop_app() {
        let token = config_paths()
            .iter()
            .filter(|path| path.is_file())
            .find_map(|path| read_token(path))
            .expect("the desktop app should expose a token");
        // Never print the token: check its shape only.
        assert!(token.access_token.starts_with("sk-ant-"));
        assert!(token.expires_at.unwrap_or_default() > 0);
    }

    /// The layout the desktop app actually keeps: several long-lived tokens,
    /// and the one expiring last has only the inference scope, which the
    /// usage endpoint refuses.
    #[test]
    fn prefers_a_token_the_usage_endpoint_accepts_over_a_later_one() {
        let plaintext = r#"{
            "org:account:https://api.anthropic.com:user:inference": {
                "token": "inference-only", "expiresAt": 1820129204645
            },
            "org:account:https://api.anthropic.com:user:inference user:office": {
                "token": "office", "expiresAt": 1819522154267
            },
            "org:account:https://api.anthropic.com:user:inference user:file_upload user:profile": {
                "token": "usage-capable", "expiresAt": 1819578528760, "refreshToken": "r"
            },
            "org2:account:https://api.anthropic.com:user:inference user:file_upload user:profile": {
                "token": "usage-capable-older", "expiresAt": 1819523090835
            }
        }"#;
        let token = select_token(plaintext).expect("a token should be selected");
        assert_eq!(token.access_token, "usage-capable");
    }

    #[test]
    fn falls_back_to_an_inference_token_when_none_has_the_profile_scope() {
        let plaintext = r#"{
            "org:account:https://api.anthropic.com:user:profile": {
                "token": "profile-only", "expiresAt": 9000000000000
            },
            "org:account:https://api.anthropic.com:user:inference": {
                "token": "inference", "expiresAt": 1
            }
        }"#;
        assert_eq!(select_token(plaintext).unwrap().access_token, "inference");
    }

    #[test]
    fn scopes_are_read_exactly_from_the_end_of_a_cache_key() {
        assert_eq!(
            entry_scopes("org:account:https://api.anthropic.com:user:inference user:profile"),
            ["user:inference", "user:profile"]
        );
        assert_eq!(
            entry_scopes("install:user:https://api.anthropic.com:user:inference"),
            ["user:inference"]
        );
        // A look-alike is not the scope it resembles.
        assert!(!entry_scopes(
            "org:account:https://api.anthropic.com:user:inference_v2 user:profiles"
        )
        .iter()
        .any(|scope| *scope == INFERENCE_SCOPE || *scope == PROFILE_SCOPE));
        assert!(entry_scopes("").is_empty());
    }

    #[test]
    fn store_install_paths_are_recognised() {
        assert!(is_store_install(Path::new(
            r"C:\Users\me\AppData\Local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Roaming\Claude\config.json"
        )));
        assert!(is_store_install(Path::new(
            r"C:\Users\me\AppData\Local\packages\Claude_0123456789abc\LocalCache\Roaming\Claude\config.json"
        )));
        assert!(!is_store_install(Path::new(
            r"C:\Users\me\AppData\Roaming\Claude\config.json"
        )));
        assert!(!is_store_install(Path::new(
            r"C:\Users\me\AppData\Local\Packages\ClaudeHelper_pzs8sxrjxfjjc\config.json"
        )));
    }

    #[test]
    fn finds_both_the_installer_and_the_store_data_folders() {
        let root = std::env::temp_dir().join(format!(
            "ccum-pro-claude-desktop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let roaming = root.join("Roaming");
        let local = root.join("Local");
        for name in [
            "Claude_pzs8sxrjxfjjc",
            "Claude_0123456789abc",
            // Not the Store package: wrong prefix, id length or characters.
            "ClaudeHelper_pzs8sxrjxfjjc",
            "Claude_short",
            "Claude_PZS8SXRJXFJJC",
            "Other_pzs8sxrjxfjjc",
        ] {
            std::fs::create_dir_all(local.join("Packages").join(name)).unwrap();
        }
        // A well-formed name that is a file, not a package folder.
        std::fs::write(local.join("Packages").join("Claude_f1le000000000"), b"").unwrap();

        let store = |name: &str| {
            local
                .join("Packages")
                .join(name)
                .join("LocalCache")
                .join("Roaming")
                .join("Claude")
        };
        assert_eq!(
            data_directories_in(Some(&roaming), Some(&local)),
            vec![
                roaming.join("Claude"),
                store("Claude_pzs8sxrjxfjjc"),
                store("Claude_0123456789abc"),
            ]
        );
        assert_eq!(
            data_directories_in(None, Some(&root.join("no-such-folder"))),
            Vec::<PathBuf>::new()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn watch_signature_reports_missing_config() {
        let signature = watch_signature(Path::new("C:/nonexistent/Claude/config.json"));
        assert!(signature.ends_with("|missing"), "{signature}");
    }
}

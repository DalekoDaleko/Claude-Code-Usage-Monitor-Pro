use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::windows_credentials;
use super::{build_agent, PollError, SignInReport};
use crate::diagnose;
use crate::models::{UsageData, UsageSection};

const DASHBOARD_URL_PREFIX: &str = "https://opencode.ai/workspace/";
const DASHBOARD_URL_SUFFIX: &str = "/go";
const DASHBOARD_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
/// The workspace ID is part of the dashboard URL, not a secret, so it is
/// read as plain text.
const WORKSPACE_ID_ENV: &str = "CLAUDECODEUSAGE_OPENCODE_GO_WORKSPACE_ID";
/// The session cookie is a secret. It must hold the output of
/// `ConvertFrom-SecureString`; a plain-text cookie is refused, never used.
const AUTH_COOKIE_ENV: &str = "CLAUDECODEUSAGE_OPENCODE_GO_AUTH_COOKIE";
/// The upstream names, the second of which held the cookie in plain text.
const RETIRED_WORKSPACE_ID_ENV: &str = "OPENCODE_GO_WORKSPACE_ID";
const RETIRED_AUTH_COOKIE_ENV: &str = "OPENCODE_GO_AUTH_COOKIE";
/// A path to this monitor's own config file, read in the encrypted format.
const CONFIG_FILE_ENV: &str = "CLAUDECODEUSAGE_OPENCODE_GO_CONFIG_FILE";
const RETIRED_CONFIG_FILE_ENV: &str = "OPENCODE_GO_CONFIG_FILE";

/// Whose format a config file is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigFormat {
    /// This monitor's own file. The cookie must be DPAPI-protected, in
    /// `encryptedAuthCookie`; a plain-text `authCookie` is refused.
    Own,
    /// A file written by another tool (opencode-bar, opencode-quota), read in
    /// that tool's own plain-text format so an existing sign-in can be reused.
    OtherTool,
}

/// This monitor's own config file.
#[derive(Deserialize)]
struct OwnDashboardConfig {
    #[serde(alias = "workspaceId", alias = "workspaceID")]
    workspace_id: String,
    #[serde(default, alias = "encryptedAuthCookie")]
    encrypted_auth_cookie: Option<String>,
    /// Read only to recognise a plain-text cookie and explain why it is refused.
    #[serde(default, alias = "authCookie", alias = "cookie")]
    auth_cookie: Option<String>,
}

/// The format written by the other tools whose sign-in can be reused.
#[derive(Deserialize)]
struct DashboardConfig {
    #[serde(alias = "workspaceId", alias = "workspaceID")]
    workspace_id: String,
    #[serde(alias = "authCookie", alias = "cookie")]
    auth_cookie: String,
}

struct DashboardCredentials {
    workspace_id: String,
    auth_cookie: String,
    source: CredentialOrigin,
}

/// Where dashboard credentials were read from. Displays as before this was
/// typed, so log lines and the credential-watch signature are unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CredentialOrigin {
    /// The workspace-id and auth-cookie environment variables.
    Environment,
    File(PathBuf),
}

impl std::fmt::Display for CredentialOrigin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Environment => formatter.write_str("environment"),
            Self::File(path) => write!(formatter, "{}", path.display()),
        }
    }
}

/// Where the dashboard credentials come from. A session cookie records no
/// expiry that can be read here, so none is reported.
pub(super) fn sign_in_report() -> Option<SignInReport> {
    let credentials = read_dashboard_credentials()?;
    Some(match credentials.source {
        CredentialOrigin::Environment => {
            SignInReport::new("Environment variable").detail(AUTH_COOKIE_ENV)
        }
        CredentialOrigin::File(path) => {
            SignInReport::new("Config file").detail(path.display().to_string())
        }
    })
}

#[derive(Clone, Debug, PartialEq)]
struct UsageWindow {
    usage_percent: f64,
    reset_in_sec: i64,
}

#[derive(Debug, Default, PartialEq)]
struct DashboardUsage {
    rolling: Option<UsageWindow>,
    weekly: Option<UsageWindow>,
    monthly: Option<UsageWindow>,
}

pub(super) fn poll_opencode() -> Result<UsageData, PollError> {
    let credentials = read_dashboard_credentials().ok_or_else(|| {
        diagnose::log("OpenCode usage poll failed: no dashboard credentials found");
        PollError::NoCredentials
    })?;
    poll_dashboard(&credentials)
}

pub(super) fn credential_watch_snapshot(_all_sources: bool) -> Vec<String> {
    vec![credential_watch_signature()]
}

fn poll_dashboard(credentials: &DashboardCredentials) -> Result<UsageData, PollError> {
    let usage = fetch_dashboard_usage(credentials).inspect_err(|error| {
        diagnose::log(format!(
            "OpenCode dashboard poll failed via {}: {error:?}",
            credentials.source
        ));
    })?;

    if usage.rolling.is_none() && usage.weekly.is_none() && usage.monthly.is_none() {
        diagnose::log(format!(
            "OpenCode dashboard returned no usage windows from {}",
            credentials.source
        ));
        return Err(PollError::RequestFailed);
    }

    let now = SystemTime::now();
    let session = usage
        .rolling
        .as_ref()
        .map(|window| section_from_window(window, now))
        .unwrap_or_default();
    let (weekly, weekly_label) = select_long_window(&usage, now);

    Ok(UsageData {
        session,
        weekly,
        weekly_label,
        // The monthly window is kept available to themes alongside the
        // auto-selected `weekly` slot (which prefers the more constrained
        // of the two windows, as before).
        monthly: usage
            .monthly
            .as_ref()
            .map(|window| section_from_window(window, now)),
        credits: None,
        stale: false,
    })
}

fn select_long_window(usage: &DashboardUsage, now: SystemTime) -> (UsageSection, Option<String>) {
    match (&usage.weekly, &usage.monthly) {
        (Some(weekly), Some(monthly)) if monthly.usage_percent > weekly.usage_percent => {
            (section_from_window(monthly, now), Some("30d".to_string()))
        }
        (Some(weekly), _) => (section_from_window(weekly, now), Some("7d".to_string())),
        (None, Some(monthly)) => (section_from_window(monthly, now), Some("30d".to_string())),
        (None, None) => (UsageSection::default(), None),
    }
}

fn section_from_window(window: &UsageWindow, now: SystemTime) -> UsageSection {
    UsageSection {
        percentage: window.usage_percent.clamp(0.0, 100.0),
        resets_at: now.checked_add(Duration::from_secs(window.reset_in_sec.max(0) as u64)),
    }
}

fn read_dashboard_credentials() -> Option<DashboardCredentials> {
    windows_credentials::note_retired_variable(RETIRED_WORKSPACE_ID_ENV, WORKSPACE_ID_ENV);
    windows_credentials::note_retired_variable(RETIRED_AUTH_COOKIE_ENV, AUTH_COOKIE_ENV);
    if let (Some(workspace_id), Some(auth_cookie)) = (
        non_empty_environment(WORKSPACE_ID_ENV),
        windows_credentials::protected_environment_value(AUTH_COOKIE_ENV),
    ) {
        if valid_workspace_id(&workspace_id) && valid_cookie(&auth_cookie) {
            return Some(DashboardCredentials {
                workspace_id,
                auth_cookie,
                source: CredentialOrigin::Environment,
            });
        }
    }

    dashboard_config_paths()
        .into_iter()
        .find_map(|(path, format)| read_dashboard_config(&path, format))
}

fn read_dashboard_config(path: &Path, format: ConfigFormat) -> Option<DashboardCredentials> {
    let content = std::fs::read_to_string(path).ok()?;
    // Windows PowerShell 5.1 writes UTF-8 files with a byte-order mark, which
    // strict JSON rejects; the file would otherwise be skipped without a word.
    let content = content.trim_start_matches('\u{feff}');
    let (workspace_id, auth_cookie) = match format {
        ConfigFormat::Own => {
            let config: OwnDashboardConfig = serde_json::from_str(content).ok()?;
            let cookie = own_config_cookie(path, &config)?;
            (config.workspace_id, cookie)
        }
        ConfigFormat::OtherTool => {
            let config: DashboardConfig = serde_json::from_str(content).ok()?;
            (config.workspace_id, config.auth_cookie)
        }
    };
    let workspace_id = workspace_id.trim().to_string();
    let auth_cookie = auth_cookie.trim().to_string();
    if !valid_workspace_id(&workspace_id) || !valid_cookie(&auth_cookie) {
        return None;
    }
    Some(DashboardCredentials {
        workspace_id,
        auth_cookie,
        source: CredentialOrigin::File(path.to_path_buf()),
    })
}

/// The cookie from this monitor's own config file, decrypted. A plain-text
/// cookie is refused; the log names the file and the reason, never the value.
fn own_config_cookie(path: &Path, config: &OwnDashboardConfig) -> Option<String> {
    let file = path.display().to_string();
    let present = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    match present(&config.encrypted_auth_cookie) {
        Some(protected) => match windows_credentials::decrypt_secure_string(&protected) {
            Ok(cookie) => Some(cookie),
            Err(error) => {
                windows_credentials::log_once(
                    &file,
                    format!("{file}: encryptedAuthCookie ignored: {}", error.reason()),
                );
                None
            }
        },
        None => {
            if present(&config.auth_cookie).is_some() {
                windows_credentials::log_once(
                    &file,
                    format!(
                        "{file}: authCookie holds a plain-text cookie, which is never used; store \
                         the output of ConvertFrom-SecureString in encryptedAuthCookie instead"
                    ),
                );
            }
            None
        }
    }
}

fn fetch_dashboard_usage(credentials: &DashboardCredentials) -> Result<DashboardUsage, PollError> {
    let url = format!(
        "{DASHBOARD_URL_PREFIX}{}{DASHBOARD_URL_SUFFIX}",
        credentials.workspace_id
    );
    let cookie = if credentials
        .auth_cookie
        .split(';')
        .any(|part| part.trim_start().starts_with("auth="))
    {
        credentials.auth_cookie.clone()
    } else {
        format!("auth={}", credentials.auth_cookie)
    };

    let mut response = match build_agent()?
        .get(&url)
        .header("Accept", "text/html,application/xhtml+xml")
        .header("Cookie", &cookie)
        .header("User-Agent", DASHBOARD_USER_AGENT)
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(401 | 403)) => return Err(PollError::AuthRequired),
        Err(error) => {
            diagnose::log_error("OpenCode Go dashboard request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let html = response.body_mut().read_to_string().map_err(|error| {
        diagnose::log_error("OpenCode Go dashboard response is not UTF-8", error);
        PollError::RequestFailed
    })?;
    Ok(parse_dashboard_html(&html))
}

fn parse_dashboard_html(html: &str) -> DashboardUsage {
    let normalized = html
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .replace("\\\"", "\"")
        .replace("\\u0022", "\"");
    DashboardUsage {
        rolling: parse_window("rollingUsage", &normalized),
        weekly: parse_window("weeklyUsage", &normalized),
        monthly: parse_window("monthlyUsage", &normalized),
    }
}

fn parse_window(field_name: &str, text: &str) -> Option<UsageWindow> {
    text.match_indices(field_name)
        .find_map(|(index, _)| parse_window_value(field_value_at(text, field_name, index)?))
}

fn parse_window_value(mut value: &str) -> Option<UsageWindow> {
    if let Some(rest) = value.strip_prefix("$R[") {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        value = rest.get(digits..)?.strip_prefix(']')?.trim_start();
        value = value.strip_prefix('=')?.trim_start();
    }
    let body = value.strip_prefix('{')?.split_once('}')?.0;
    Some(UsageWindow {
        usage_percent: numeric_field(body, "usagePercent")?,
        reset_in_sec: numeric_field(body, "resetInSec")?.max(0.0) as i64,
    })
}

fn field_value<'a>(text: &'a str, field_name: &str) -> Option<&'a str> {
    text.match_indices(field_name)
        .find_map(|(index, _)| field_value_at(text, field_name, index))
}

fn field_value_at<'a>(text: &'a str, field_name: &str, index: usize) -> Option<&'a str> {
    let preceding = text[..index].bytes().next_back();
    if preceding.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        return None;
    }

    let mut remainder = &text[index + field_name.len()..];
    if remainder.starts_with(['\'', '"']) {
        remainder = &remainder[1..];
    }
    remainder
        .trim_start()
        .strip_prefix(':')
        .map(str::trim_start)
}

fn numeric_field(text: &str, field_name: &str) -> Option<f64> {
    let mut value = field_value(text, field_name)?;
    if value.starts_with(['\'', '"']) {
        value = &value[1..];
    }

    let bytes = value.as_bytes();
    let mut end = usize::from(bytes.first() == Some(&b'-'));
    let integer_start = end;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == integer_start {
        return None;
    }
    if bytes.get(end) == Some(&b'.') {
        let fraction_start = end + 1;
        end = fraction_start;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end == fraction_start {
            return None;
        }
    }
    value[..end].parse().ok()
}

/// Config files to try, in order, each with the format it is written in.
fn dashboard_config_paths() -> Vec<(PathBuf, ConfigFormat)> {
    windows_credentials::note_retired_variable(RETIRED_CONFIG_FILE_ENV, CONFIG_FILE_ENV);
    let mut paths = Vec::new();
    if let Some(path) = non_empty_environment(CONFIG_FILE_ENV).map(PathBuf::from) {
        paths.push((path, ConfigFormat::Own));
    }
    if let Some(app_data) = non_empty_environment("APPDATA").map(PathBuf::from) {
        paths.push((app_data.join("opencode-go").join("config.json"), ConfigFormat::Own));
    }
    let mut other_tool = |base: PathBuf| {
        for tool in ["opencode-bar", "opencode-quota"] {
            paths.push((base.join(tool).join("opencode-go.json"), ConfigFormat::OtherTool));
        }
    };
    if let Some(config_home) = non_empty_environment("XDG_CONFIG_HOME").map(PathBuf::from) {
        other_tool(config_home);
    }
    if let Some(home) = dirs::home_dir() {
        other_tool(home.join(".config"));
    }
    paths
}

fn non_empty_environment(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn valid_workspace_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_cookie(value: &str) -> bool {
    !value.is_empty() && !value.bytes().any(|byte| matches!(byte, b'\r' | b'\n'))
}

fn credential_watch_signature() -> String {
    let mut parts = Vec::new();
    match read_dashboard_credentials() {
        Some(credentials) => {
            let mut hasher = DefaultHasher::new();
            credentials.workspace_id.hash(&mut hasher);
            credentials.auth_cookie.hash(&mut hasher);
            parts.push(format!(
                "dashboard|present|{}|{}|{:x}|{}",
                credentials.workspace_id.len(),
                credentials.auth_cookie.len(),
                hasher.finish(),
                credentials.source
            ));
        }
        None => parts.push("dashboard|missing".to_string()),
    }
    for (path, _) in dashboard_config_paths() {
        parts.push(path_signature("config", &path));
    }
    parts.join(";;")
}

fn path_signature(kind: &str, path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs())
                .unwrap_or(0);
            format!(
                "{kind}:{}|present|{}|{modified}",
                path.display(),
                metadata.len()
            )
        }
        Err(_) => format!("{kind}:{}|missing", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `content` to a unique file under the temp directory.
    fn config_file(label: &str, content: &str) -> PathBuf {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("ccum-pro-opencode-{label}-{unique}.json"));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn the_monitors_own_file_reads_an_encrypted_cookie() {
        let protected = windows_credentials::convert_from_secure_string("Fe26.2**sealed");
        let path = config_file(
            "encrypted",
            &format!(r#"{{"workspaceId":"wrk_01OWN","encryptedAuthCookie":"{protected}"}}"#),
        );
        let credentials = read_dashboard_config(&path, ConfigFormat::Own).expect("decrypted");
        assert_eq!(credentials.workspace_id, "wrk_01OWN");
        assert_eq!(credentials.auth_cookie, "Fe26.2**sealed");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_file_saved_with_a_byte_order_mark_is_still_read() {
        let protected = windows_credentials::convert_from_secure_string("Fe26.2**sealed");
        let path = config_file(
            "bom",
            &format!("\u{feff}{{\"workspaceId\":\"wrk_01OWN\",\"encryptedAuthCookie\":\"{protected}\"}}"),
        );
        assert!(read_dashboard_config(&path, ConfigFormat::Own).is_some());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn the_monitors_own_file_refuses_a_plain_text_cookie() {
        let plain = config_file("plain", r#"{"workspaceId":"wrk_01OWN","authCookie":"Fe26.2**sealed"}"#);
        assert!(read_dashboard_config(&plain, ConfigFormat::Own).is_none());
        // Plain text placed in the encrypted field is refused too.
        let mislabelled = config_file(
            "mislabelled",
            r#"{"workspaceId":"wrk_01OWN","encryptedAuthCookie":"Fe26.2**sealed"}"#,
        );
        assert!(read_dashboard_config(&mislabelled, ConfigFormat::Own).is_none());
        std::fs::remove_file(plain).ok();
        std::fs::remove_file(mislabelled).ok();
    }

    #[test]
    fn another_tools_file_is_read_in_that_tools_format() {
        let path = config_file("other", r#"{"workspaceId":"wrk_01BAR","authCookie":"Fe26.2**bar"}"#);
        let credentials = read_dashboard_config(&path, ConfigFormat::OtherTool).expect("read as-is");
        assert_eq!(credentials.auth_cookie, "Fe26.2**bar");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_file_named_by_the_config_variable_uses_the_encrypted_format() {
        // The only test touching this variable, so parallel tests cannot race.
        std::env::set_var(CONFIG_FILE_ENV, r"C:\custom\opencode.json");
        let paths = dashboard_config_paths();
        std::env::remove_var(CONFIG_FILE_ENV);
        assert_eq!(paths[0], (PathBuf::from(r"C:\custom\opencode.json"), ConfigFormat::Own));
        assert!(paths
            .iter()
            .filter(|(path, _)| path.to_string_lossy().contains("opencode-bar")
                || path.to_string_lossy().contains("opencode-quota"))
            .all(|(_, format)| *format == ConfigFormat::OtherTool));
    }

    #[test]
    fn the_session_cookie_is_read_only_when_it_is_protected() {
        // The only test touching these variables, so parallel tests cannot race.
        std::env::set_var(WORKSPACE_ID_ENV, "wrk_01TESTWORKSPACE");
        std::env::set_var(
            AUTH_COOKIE_ENV,
            windows_credentials::convert_from_secure_string("Fe26.2**sealed-cookie"),
        );
        let credentials = read_dashboard_credentials().expect("protected cookie is used");
        assert_eq!(credentials.source, CredentialOrigin::Environment);
        assert_eq!(credentials.source.to_string(), "environment");
        assert_eq!(credentials.workspace_id, "wrk_01TESTWORKSPACE");
        assert_eq!(credentials.auth_cookie, "Fe26.2**sealed-cookie");

        std::env::set_var(AUTH_COOKIE_ENV, "Fe26.2**sealed-cookie");
        assert!(
            read_dashboard_credentials()
                .is_none_or(|credentials| credentials.source != CredentialOrigin::Environment),
            "a plain-text cookie in the variable must never be used"
        );

        std::env::remove_var(WORKSPACE_ID_ENV);
        std::env::remove_var(AUTH_COOKIE_ENV);
    }

    #[test]
    fn dashboard_parser_accepts_serialized_and_html_escaped_windows() {
        let html = r#"rollingUsage:{usagePercent:12.5,resetInSec:300},&quot;weeklyUsage&quot;:{&quot;usagePercent&quot;:&quot;45&quot;,&quot;resetInSec&quot;:7200},monthlyUsage:$R[7]={usagePercent:60,resetInSec:9000}"#;
        let usage = parse_dashboard_html(html);
        assert_eq!(usage.rolling.unwrap().usage_percent, 12.5);
        assert_eq!(usage.weekly.unwrap().reset_in_sec, 7_200);
        assert_eq!(usage.monthly.unwrap().usage_percent, 60.0);
    }

    #[test]
    fn dashboard_parser_rejects_lookalike_and_malformed_fields() {
        let html = r#"notrollingUsage:{usagePercent:1,resetInSec:2},rollingUsage:null,rollingUsage:{usagePercent:7,resetInSec:8},weeklyUsage:$R[x]={usagePercent:3,resetInSec:4},monthlyUsage:{usagePercent:.5,resetInSec:6}"#;
        let usage = parse_dashboard_html(html);
        assert_eq!(
            usage.rolling,
            Some(UsageWindow {
                usage_percent: 7.0,
                reset_in_sec: 8,
            })
        );
        assert!(usage.weekly.is_none());
        assert!(usage.monthly.is_none());
    }

    #[test]
    fn most_constrained_long_window_is_selected() {
        let usage = DashboardUsage {
            weekly: Some(UsageWindow {
                usage_percent: 40.0,
                reset_in_sec: 60,
            }),
            monthly: Some(UsageWindow {
                usage_percent: 70.0,
                reset_in_sec: 120,
            }),
            ..Default::default()
        };
        let (section, label) = select_long_window(&usage, UNIX_EPOCH);
        assert_eq!(section.percentage, 70.0);
        assert_eq!(label.as_deref(), Some("30d"));
    }

    #[test]
    fn dashboard_identifiers_and_cookie_headers_reject_request_injection() {
        assert!(valid_workspace_id("wrk_01-test"));
        assert!(!valid_workspace_id("../other"));
        assert!(valid_cookie("auth=abc; theme=dark"));
        assert!(!valid_cookie("auth=abc\r\nX-Test: injected"));
    }
}

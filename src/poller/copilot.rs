//! GitHub Copilot premium-request quota.
//!
//! Reads `api.github.com/copilot_internal/user`, the endpoint VS Code itself
//! queries for its Copilot quota indicator (it is the `entitlementUrl` in VS
//! Code's `product.json`). It is not a documented public API, so every field
//! is treated as optional and an unexpected shape is reported rather than
//! guessed at. The request is read-only and does not consume any quota.
//!
//! The usage percentage follows VS Code's own derivation in
//! `chatEntitlementService.ts` so the widget and the editor agree.
//!
//! Every token source is protected by DPAPI for the signed-in Windows account:
//!
//! 1. `CLAUDECODEUSAGE_COPILOT_GITHUB_TOKEN_DPAPI`, if set, holding the output
//!    of PowerShell's `ConvertFrom-SecureString`. A plain-text token placed
//!    there is refused, never used: an environment variable is stored
//!    unencrypted and handed to every child process.
//! 2. The Copilot CLI's own sign-in, from Windows Credential Manager.
//! 3. The GitHub CLI's sign-in, from Windows Credential Manager.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

use super::windows_credentials;
use super::{build_agent, parse_iso8601, PollError, SignInReport};
use crate::diagnose;
use crate::models::{UsageData, UsageSection};

const COPILOT_USER_URL: &str = "https://api.github.com/copilot_internal/user";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Optional token override. Must hold DPAPI-protected data, never a token.
const DPAPI_TOKEN_ENV: &str = "CLAUDECODEUSAGE_COPILOT_GITHUB_TOKEN_DPAPI";
/// The GitHub CLI keeps the active account's token under this target.
const GH_CLI_TARGET: &str = "gh:github.com:";
/// The Copilot CLI names its entry `<host>:<login>.copilot-cli`, with the
/// signed-in account recorded in `~/.copilot/config.json`.
const COPILOT_CLI_TARGET_SUFFIX: &str = ".copilot-cli";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenSource {
    Environment,
    CopilotCli,
    GitHubCli,
}

impl TokenSource {
    /// In order of preference: an explicit override first, then each tool's
    /// own sign-in.
    const ORDER: [Self; 3] = [Self::Environment, Self::CopilotCli, Self::GitHubCli];

    fn describe(self) -> &'static str {
        match self {
            Self::Environment => DPAPI_TOKEN_ENV,
            Self::CopilotCli => "the Copilot CLI sign-in",
            Self::GitHubCli => "the GitHub CLI sign-in",
        }
    }

    /// Read this source's token, only when it is actually needed.
    fn read(self) -> Option<String> {
        match self {
            Self::Environment => environment_token(),
            Self::CopilotCli => read_token(&copilot_cli_target()?),
            Self::GitHubCli => read_token(GH_CLI_TARGET),
        }
    }
}

/// The first source holding a token, which is the one tried first. Whether
/// GitHub accepts it is only known by asking, which this does not do, and
/// GitHub's tokens record no expiry, so none is reported.
pub(super) fn sign_in_report() -> Option<SignInReport> {
    let source = TokenSource::ORDER
        .into_iter()
        .find(|source| source.read().is_some())?;
    Some(match source {
        TokenSource::Environment => {
            SignInReport::new("Environment variable").detail(DPAPI_TOKEN_ENV)
        }
        TokenSource::CopilotCli => SignInReport::new("Copilot CLI sign-in"),
        TokenSource::GitHubCli => SignInReport::new("GitHub CLI sign-in"),
    })
}

pub(super) fn poll_copilot() -> Result<UsageData, PollError> {
    let mut last_error = None;
    for source in TokenSource::ORDER {
        let Some(token) = source.read() else {
            continue;
        };
        match fetch_copilot_usage(&token) {
            Ok(data) => {
                diagnose::log(format!("Copilot quota read using {}", source.describe()));
                return Ok(data);
            }
            // A token GitHub rejects may simply belong to a source the user
            // has since signed out of; the next source can still be valid.
            Err(PollError::AuthRequired) => {
                diagnose::log(format!(
                    "GitHub rejected the Copilot token from {}; trying the next source",
                    source.describe()
                ));
                last_error = Some(PollError::AuthRequired);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        diagnose::log("Copilot usage poll failed: no Copilot token found in any source");
        PollError::NoCredentials
    }))
}

/// Fingerprints of every place a token can come from, so polling resumes by
/// itself once the user signs in again. Only a length and a hash of each secret
/// are kept — never the secret.
pub(super) fn credential_watch_snapshot(_all_sources: bool) -> Vec<String> {
    let mut snapshot = vec![config_signature()];
    for (source, target) in credential_targets() {
        snapshot.push(match windows_credentials::read_generic(&target) {
            Some(bytes) => {
                let mut hasher = DefaultHasher::new();
                bytes.hash(&mut hasher);
                format!("{source:?}|present|{}|{}", bytes.len(), hasher.finish())
            }
            None => format!("{source:?}|missing"),
        });
    }
    snapshot
}

/// Credential Manager entries watched for a new sign-in. The environment
/// variable is not among them: a running process never sees it change.
fn credential_targets() -> Vec<(TokenSource, String)> {
    let mut targets = Vec::with_capacity(2);
    if let Some(target) = copilot_cli_target() {
        targets.push((TokenSource::CopilotCli, target));
    }
    targets.push((TokenSource::GitHubCli, GH_CLI_TARGET.to_string()));
    targets
}

fn copilot_cli_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".copilot").join("config.json"))
}

fn copilot_cli_target() -> Option<String> {
    let config = std::fs::read_to_string(copilot_cli_config_path()?).ok()?;
    copilot_cli_target_from_config(&config)
}

#[derive(Deserialize)]
struct CopilotCliConfig {
    #[serde(rename = "lastLoggedInUser")]
    last_logged_in_user: Option<CopilotCliUser>,
}

#[derive(Deserialize)]
struct CopilotCliUser {
    host: Option<String>,
    login: Option<String>,
}

fn copilot_cli_target_from_config(config: &str) -> Option<String> {
    // The Copilot CLI writes this file with `//` comment lines at the top, which
    // strict JSON rejects. Without stripping them the whole source is skipped
    // silently and the GitHub CLI's token is used instead.
    let user = serde_json::from_str::<CopilotCliConfig>(&strip_json_comments(config))
        .ok()?
        .last_logged_in_user?;
    let host = user.host.filter(|host| !host.is_empty())?;
    let login = user.login.filter(|login| !login.is_empty())?;
    Some(format!("{host}:{login}{COPILOT_CLI_TARGET_SUFFIX}"))
}

/// Remove `//` and `/* */` comments, leaving anything inside a string literal
/// alone — the file's own values contain `https://`.
fn strip_json_comments(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            output.push(c);
            match c {
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        output.push(escaped);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                output.push(c);
            }
            ('/', Some('/')) => {
                // Keep the newline so line numbers in any parse error still match.
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        output.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = '\0';
                for skipped in chars.by_ref() {
                    if previous == '*' && skipped == '/' {
                        break;
                    }
                    previous = skipped;
                }
            }
            _ => output.push(c),
        }
    }
    output
}

fn config_signature() -> String {
    let Some(path) = copilot_cli_config_path() else {
        return "config|missing".into();
    };
    match std::fs::metadata(&path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => {
            let stamp = modified
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            format!("config|{stamp}")
        }
        Err(_) => "config|missing".into(),
    }
}

/// The override token, if the variable is set and decrypts cleanly. Anything
/// else is logged — without the value — and skipped, so the Credential Manager
/// sources still get their turn.
fn environment_token() -> Option<String> {
    let value = std::env::var(DPAPI_TOKEN_ENV).ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    match decode_protected_token(value) {
        Ok(token) => Some(token),
        Err(reason) => {
            diagnose::log(format!("{DPAPI_TOKEN_ENV} ignored: {reason}"));
            None
        }
    }
}

/// Decrypt the hexadecimal DPAPI blob that `ConvertFrom-SecureString` writes
/// when no key is given. It protects UTF-16 text for the current account with
/// no extra entropy.
///
/// The reason returned on failure is written to the log, so it must never
/// contain the value itself.
fn decode_protected_token(value: &str) -> Result<String, String> {
    let text = match windows_credentials::decrypt_secure_string(value) {
        Ok(text) => text,
        Err(windows_credentials::SecureStringError::NotHex) => {
            // GitHub's token prefixes: OAuth, classic PAT, user-to-server,
            // server-to-server, refresh, and fine-grained PAT. Naming the
            // prefix tells the user which kind of token they pasted; it is the
            // same for every token of that kind, so it reveals nothing secret.
            let prefix = ["gho_", "ghp_", "ghu_", "ghs_", "ghr_", "github_pat_"]
                .into_iter()
                .find(|prefix| value.starts_with(prefix));
            return Err(match prefix {
                Some(prefix) => format!(
                    "it holds a plain-text {prefix}… token, which is never used; store the \
                     output of ConvertFrom-SecureString instead"
                ),
                None => windows_credentials::SecureStringError::NotHex.reason().to_string(),
            });
        }
        Err(error) => return Err(error.reason().to_string()),
    };
    decode_token(text.as_bytes()).ok_or_else(|| "it did not decrypt to a token".to_string())
}

fn read_token(target: &str) -> Option<String> {
    decode_token(&windows_credentials::read_generic(target)?)
}

/// Decode a stored token, accepting UTF-8 (`gh`) or UTF-16LE (Copilot CLI).
///
/// Tokens are ASCII, so UTF-16LE shows up as a zero in every odd byte. Anything
/// that does not look like a bare token is refused rather than sent to GitHub.
fn decode_token(bytes: &[u8]) -> Option<String> {
    let utf16 = bytes.len() >= 2
        && bytes.len() % 2 == 0
        && bytes.iter().skip(1).step_by(2).all(|byte| *byte == 0);
    let text = if utf16 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units).ok()?
    } else {
        String::from_utf8(bytes.to_vec()).ok()?
    };
    let token = text.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    let plausible = !token.is_empty()
        && token.len() <= 512
        && token.chars().all(|c| c.is_ascii_graphic());
    plausible.then(|| token.to_string())
}

fn fetch_copilot_usage(token: &str) -> Result<UsageData, PollError> {
    let mut response = match build_agent()?
        .get(COPILOT_USER_URL)
        .header("Authorization", &format!("token {token}"))
        .header("Accept", "application/json")
        .header("User-Agent", USER_AGENT)
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(code @ (401 | 403 | 404))) => {
            // 404 is what an account with no Copilot access receives.
            diagnose::log(format!("Copilot quota request returned HTTP {code}"));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Copilot quota request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let body: CopilotUserResponse = response.body_mut().read_json().map_err(|error| {
        diagnose::log_error("unable to parse the Copilot quota response", error);
        PollError::RequestFailed
    })?;
    copilot_usage_from_response(&body).ok_or_else(|| {
        diagnose::log("Copilot quota response had no premium_interactions snapshot");
        PollError::RequestFailed
    })
}

#[derive(Deserialize, Default)]
struct CopilotUserResponse {
    quota_reset_date_utc: Option<String>,
    quota_reset_date: Option<String>,
    limited_user_reset_date: Option<String>,
    quota_snapshots: Option<QuotaSnapshots>,
}

#[derive(Deserialize, Default)]
struct QuotaSnapshots {
    premium_interactions: Option<QuotaSnapshot>,
}

/// Numeric fields are kept as raw JSON: VS Code coerces them with `Number()`,
/// which implies they are not always sent as JSON numbers.
#[derive(Deserialize, Default)]
struct QuotaSnapshot {
    entitlement: Option<Value>,
    quota_remaining: Option<Value>,
    percent_remaining: Option<Value>,
    unlimited: Option<bool>,
    quota_reset_at: Option<Value>,
}

fn copilot_usage_from_response(response: &CopilotUserResponse) -> Option<UsageData> {
    let snapshot = response.quota_snapshots.as_ref()?.premium_interactions.as_ref()?;
    Some(UsageData {
        session: UsageSection {
            percentage: premium_percent_used(snapshot),
            resets_at: reset_time(response, snapshot),
        },
        // Copilot meters a single monthly quota. The second row is reserved
        // and always drawn empty.
        weekly: UsageSection {
            percentage: 0.0,
            resets_at: None,
        },
        weekly_label: None,
        monthly: None,
        credits: None,
        stale: false,
    })
}

/// Share of the premium-request allowance used, as VS Code derives it.
///
/// `entitlement − quota_remaining` is preferred because the two share a basis;
/// VS Code notes that `percent_remaining` can disagree with actual usage under
/// token-based billing. A plan with no premium ceiling, or no premium allowance
/// at all, reports nothing worth a percentage and is shown as 0%.
fn premium_percent_used(snapshot: &QuotaSnapshot) -> f64 {
    if snapshot.unlimited == Some(true) {
        return 0.0;
    }
    let percent_remaining = number(&snapshot.percent_remaining).map(|value| value.clamp(0.0, 100.0));
    let used = match number(&snapshot.entitlement).filter(|total| *total > 0.0) {
        Some(total) => {
            let used = match number(&snapshot.quota_remaining) {
                Some(remaining) => total - remaining,
                None => match percent_remaining {
                    Some(percent) => total * (100.0 - percent) / 100.0,
                    None => return 0.0,
                },
            };
            used.max(0.0) / total * 100.0
        }
        // An entitlement of 0 is the free tier's "no premium requests".
        None if number(&snapshot.entitlement) == Some(0.0) => return 0.0,
        None => match percent_remaining {
            Some(percent) => 100.0 - percent,
            None => return 0.0,
        },
    };
    used.clamp(0.0, 100.0)
}

/// When the allowance next resets: the snapshot's own time first, then the
/// account-level dates, in the same order VS Code checks them.
fn reset_time(response: &CopilotUserResponse, snapshot: &QuotaSnapshot) -> Option<SystemTime> {
    snapshot_reset_time(&snapshot.quota_reset_at)
        .or_else(|| parse_iso8601(response.quota_reset_date_utc.as_deref()))
        .or_else(|| date_only(response.quota_reset_date.as_deref()))
        .or_else(|| date_only(response.limited_user_reset_date.as_deref()))
}

/// `quota_reset_at` arrives as `0` when unset, which VS Code treats as absent.
fn snapshot_reset_time(value: &Option<Value>) -> Option<SystemTime> {
    match value.as_ref()? {
        Value::String(text) if !text.is_empty() => parse_iso8601(Some(text)),
        Value::Number(number) => {
            let raw = number.as_f64().filter(|raw| raw.is_finite() && *raw > 0.0)?;
            // JavaScript timestamps are milliseconds; anything this large
            // cannot be seconds for a plausible reset date.
            let seconds = if raw > 1e11 { raw / 1000.0 } else { raw };
            UNIX_EPOCH.checked_add(Duration::from_secs_f64(seconds))
        }
        _ => None,
    }
}

/// Parse a bare `YYYY-MM-DD` as midnight UTC.
fn date_only(value: Option<&str>) -> Option<SystemTime> {
    let value = value?.trim();
    if value.len() == 10 {
        parse_iso8601(Some(&format!("{value}T00:00:00Z")))
    } else {
        parse_iso8601(Some(value))
    }
}

/// Accept a JSON number or a numeric string, as long as it is finite and not
/// negative.
fn number(value: &Option<Value>) -> Option<f64> {
    let parsed = match value.as_ref()? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    (parsed.is_finite() && parsed >= 0.0).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> CopilotUserResponse {
        serde_json::from_str(json).expect("valid test JSON")
    }

    fn unix(time: SystemTime) -> u64 {
        time.duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    /// Shape captured from a real Copilot Pro account.
    const PRO_ACCOUNT: &str = r#"{
        "copilot_plan": "individual_pro",
        "quota_reset_date": "2026-10-01",
        "quota_reset_date_utc": "2026-10-01T00:00:00.000Z",
        "token_based_billing": true,
        "quota_snapshots": {
            "chat": { "entitlement": 0, "remaining": 0, "percent_remaining": 100, "unlimited": true },
            "completions": { "entitlement": 0, "remaining": 0, "percent_remaining": 100, "unlimited": true },
            "premium_interactions": {
                "overage_count": 0, "overage_permitted": false, "percent_remaining": 100.0,
                "quota_id": "premium_interactions", "quota_remaining": 7000.0, "unlimited": false,
                "has_quota": true, "quota_reset_at": 0, "token_based_billing": true,
                "credits_used": 0, "remaining": 7000, "entitlement": 7000
            }
        }
    }"#;

    #[test]
    fn a_fresh_pro_allowance_reads_as_unused_until_the_first_of_the_month() {
        let usage = copilot_usage_from_response(&parse(PRO_ACCOUNT)).unwrap();
        assert_eq!(usage.session.percentage, 0.0);
        // 2026-10-01T00:00:00Z
        assert_eq!(unix(usage.session.resets_at.unwrap()), 1_790_812_800);
    }

    #[test]
    fn the_reserved_second_row_is_always_empty() {
        let usage = copilot_usage_from_response(&parse(PRO_ACCOUNT)).unwrap();
        assert_eq!(usage.weekly.percentage, 0.0);
        assert!(usage.weekly.resets_at.is_none());
    }

    #[test]
    fn usage_follows_the_remaining_count_rather_than_the_percentage() {
        // Under token-based billing the two can disagree; the count wins.
        let response = parse(
            r#"{"quota_snapshots":{"premium_interactions":
                {"entitlement":7000,"quota_remaining":5250,"percent_remaining":90,"unlimited":false}}}"#,
        );
        let usage = copilot_usage_from_response(&response).unwrap();
        assert_eq!(usage.session.percentage, 25.0);
    }

    #[test]
    fn the_percentage_is_used_when_no_remaining_count_is_sent() {
        let response = parse(
            r#"{"quota_snapshots":{"premium_interactions":
                {"entitlement":300,"percent_remaining":40,"unlimited":false}}}"#,
        );
        assert_eq!(copilot_usage_from_response(&response).unwrap().session.percentage, 60.0);
    }

    #[test]
    fn numeric_strings_are_accepted() {
        let response = parse(
            r#"{"quota_snapshots":{"premium_interactions":
                {"entitlement":"300","quota_remaining":"150","unlimited":false}}}"#,
        );
        assert_eq!(copilot_usage_from_response(&response).unwrap().session.percentage, 50.0);
    }

    #[test]
    fn overspending_and_nonsense_stay_within_the_gauge() {
        let over = parse(
            r#"{"quota_snapshots":{"premium_interactions":
                {"entitlement":300,"quota_remaining":0,"unlimited":false}}}"#,
        );
        assert_eq!(copilot_usage_from_response(&over).unwrap().session.percentage, 100.0);
        let refunded = parse(
            r#"{"quota_snapshots":{"premium_interactions":
                {"entitlement":300,"quota_remaining":400,"unlimited":false}}}"#,
        );
        assert_eq!(copilot_usage_from_response(&refunded).unwrap().session.percentage, 0.0);
    }

    #[test]
    fn plans_without_a_premium_ceiling_show_an_empty_gauge() {
        let unlimited = parse(
            r#"{"quota_snapshots":{"premium_interactions":{"entitlement":0,"unlimited":true}}}"#,
        );
        assert_eq!(copilot_usage_from_response(&unlimited).unwrap().session.percentage, 0.0);
        let free_tier = parse(
            r#"{"quota_snapshots":{"premium_interactions":
                {"entitlement":0,"percent_remaining":0,"unlimited":false}}}"#,
        );
        assert_eq!(copilot_usage_from_response(&free_tier).unwrap().session.percentage, 0.0);
    }

    #[test]
    fn a_response_without_premium_quota_is_reported_not_guessed() {
        assert!(copilot_usage_from_response(&parse(r#"{"quota_snapshots":{}}"#)).is_none());
        assert!(copilot_usage_from_response(&parse(r#"{}"#)).is_none());
    }

    #[test]
    fn reset_time_prefers_the_snapshot_then_utc_then_the_bare_date() {
        let snapshot_first = parse(
            r#"{"quota_reset_date_utc":"2026-10-01T00:00:00.000Z",
                "quota_snapshots":{"premium_interactions":
                {"entitlement":1,"quota_remaining":1,"quota_reset_at":"2026-09-20T12:00:00Z"}}}"#,
        );
        assert_eq!(
            unix(copilot_usage_from_response(&snapshot_first).unwrap().session.resets_at.unwrap()),
            1_789_905_600
        );

        let date_only_fallback = parse(
            r#"{"quota_reset_date":"2026-10-01",
                "quota_snapshots":{"premium_interactions":{"entitlement":1,"quota_remaining":1,"quota_reset_at":0}}}"#,
        );
        assert_eq!(
            unix(copilot_usage_from_response(&date_only_fallback).unwrap().session.resets_at.unwrap()),
            1_790_812_800,
            "quota_reset_at of 0 means unset, so the bare date applies"
        );
    }

    #[test]
    fn a_millisecond_reset_timestamp_is_understood() {
        let response = parse(
            r#"{"quota_snapshots":{"premium_interactions":
                {"entitlement":1,"quota_remaining":1,"quota_reset_at":1790812800000}}}"#,
        );
        assert_eq!(
            unix(copilot_usage_from_response(&response).unwrap().session.resets_at.unwrap()),
            1_790_812_800
        );
    }

    #[test]
    fn tokens_decode_from_either_keyring_encoding() {
        let token = "gho_abcdefghijklmnopqrstuvwxyz0123456789";
        assert_eq!(decode_token(token.as_bytes()).as_deref(), Some(token));
        let utf16: Vec<u8> = token.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_token(&utf16).as_deref(), Some(token));
    }

    #[test]
    fn a_stored_value_that_is_not_a_bare_token_is_refused() {
        assert!(decode_token(b"").is_none());
        assert!(decode_token(b"   ").is_none());
        assert!(decode_token(b"two words").is_none());
        assert!(decode_token(&[0xff, 0xfe, 0xfd]).is_none());
        // Trailing whitespace or a terminating NUL is tolerated.
        assert_eq!(decode_token(b"gho_x\n").as_deref(), Some("gho_x"));
    }

    #[test]
    fn a_convert_from_secure_string_value_decrypts_to_the_token() {
        let token = "gho_abcdefghijklmnopqrstuvwxyz0123456789";
        let protected = windows_credentials::convert_from_secure_string(token);
        assert!(protected.starts_with("01000000d08c9ddf0115d1118c7a00c04fc297eb"));
        assert_eq!(decode_protected_token(&protected).as_deref(), Ok(token));
        assert_eq!(
            decode_protected_token(&protected.to_uppercase()).as_deref(),
            Ok(token),
            "hex case must not matter"
        );
    }

    #[test]
    fn a_plain_text_token_in_the_variable_is_refused_without_logging_it() {
        for (token, prefix, secret) in [
            ("gho_abcdefghijklmnopqrstuvwxyz0123456789", "gho_", "abcdefghijklmnop"),
            ("github_pat_11ABCDEFG_hijklmnopqrstuvwxyz", "github_pat_", "11ABCDEFG"),
        ] {
            let error = decode_protected_token(token).unwrap_err();
            assert!(error.contains("plain-text"), "{error}");
            assert!(error.contains(prefix), "the token kind helps the user: {error}");
            assert!(!error.contains(secret), "the reason is logged, so it must not leak: {error}");
        }
    }

    #[test]
    fn values_that_are_not_protected_tokens_are_refused() {
        assert!(decode_protected_token("not hex at all").is_err());
        assert!(decode_protected_token("abc").is_err(), "odd length");
        let not_dpapi =
            decode_protected_token("00112233445566778899aabbccddeeff00112233").unwrap_err();
        assert!(not_dpapi.contains("not DPAPI"), "{not_dpapi}");
        let mut forged = String::from("01000000d08c9ddf0115d1118c7a00c04fc297eb");
        forged.push_str(&"00".repeat(40));
        let undecryptable = decode_protected_token(&forged).unwrap_err();
        assert!(undecryptable.contains("could not be decrypted"), "{undecryptable}");
    }

    #[test]
    fn the_copilot_cli_entry_is_named_after_the_signed_in_account() {
        // The header comments are exactly what the Copilot CLI writes.
        let config = "// User settings belong in settings.json.\n\
            // This file is managed automatically.\n\
            {\"firstLaunchAt\":\"2026-01-01\",\n\
            \"lastLoggedInUser\":{\"host\":\"https://github.com\",\"login\":\"octocat\"},\n\
            \"loggedInUsers\":[{\"host\":\"https://github.com\",\"login\":\"octocat\"}]}";
        assert_eq!(
            copilot_cli_target_from_config(config).as_deref(),
            Some("https://github.com:octocat.copilot-cli")
        );
        assert!(copilot_cli_target_from_config(r#"{"loggedInUsers":[]}"#).is_none());
        assert!(copilot_cli_target_from_config("not json").is_none());
    }

    #[test]
    fn comment_stripping_leaves_string_contents_alone() {
        let source = "/* block */ {\"url\": \"https://x//y\", // trailing\n\"q\": \"a \\\" // b\"}";
        let parsed: Value = serde_json::from_str(&strip_json_comments(source)).unwrap();
        assert_eq!(parsed["url"], "https://x//y");
        assert_eq!(parsed["q"], "a \" // b", "an escaped quote must not end the string");
    }
}

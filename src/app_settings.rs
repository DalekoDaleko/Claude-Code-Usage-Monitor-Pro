//! Shared, atomically persisted state used by the widget and studio processes.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

use crate::models::{AppUsageData, CodexCreditsState};
use crate::providers::{ProviderId, ProviderSet};

pub const POLL_1_MIN_SECONDS: u32 = 60;
pub const POLL_5_MIN_SECONDS: u32 = 300;
pub const POLL_15_MIN_SECONDS: u32 = 900;
pub const POLL_1_HOUR_SECONDS: u32 = 3_600;
pub const POLL_1_MIN: u32 = POLL_1_MIN_SECONDS * 1_000;
pub const POLL_5_MIN: u32 = POLL_5_MIN_SECONDS * 1_000;
pub const POLL_15_MIN: u32 = POLL_15_MIN_SECONDS * 1_000;
pub const POLL_1_HOUR: u32 = POLL_1_HOUR_SECONDS * 1_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettingsFile {
    #[serde(default, skip_serializing)]
    pub tray_offset: i32,
    #[serde(default, skip_serializing)]
    pub taskbar_index: usize,
    /// True only when the settings file still contains the pre-theme placement
    /// fields. While this remains true, ordinary settings saves preserve those
    /// fields so only the startup migration can consume them.
    #[serde(skip)]
    pub legacy_placement_pending: bool,
    #[serde(default = "default_true", skip_serializing)]
    pub widget_visible: bool,
    /// True only while the pre-theme `widget_visible` value still needs to be
    /// transferred to the main root's Render expression.
    #[serde(skip)]
    pub legacy_visibility_pending: bool,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_check_unix: Option<u64>,
    #[serde(default = "default_true")]
    show_claude_code: bool,
    #[serde(default)]
    show_codex: bool,
    #[serde(default)]
    show_antigravity: bool,
    #[serde(default)]
    show_opencode: bool,
    #[serde(default)]
    show_cursor: bool,
    #[serde(default)]
    show_copilot: bool,
    #[serde(default = "default_true")]
    pub custom_theme_enabled: bool,
    /// Show what is left of each allowance instead of what has been spent, so
    /// the widget counts down towards a limit rather than up from zero.
    #[serde(default)]
    pub usage_countdown: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_theme_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_width: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_height: Option<f32>,
    /// Whether the monitor may start the provider CLI (`claude -p .`) to renew
    /// an expired token. That is a real API call, so it spends a little of the
    /// quota this tool reports. With it off the monitor stays passive and waits
    /// for the user's own CLI session to refresh the credentials file.
    #[serde(default = "default_true")]
    pub active_token_refresh: bool,
    /// Where the user dragged the widget while it was floating clear of a full
    /// taskbar. Absent until the widget is dragged, in which case it snaps to
    /// the right edge of the screen above the taskbar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub float_x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub float_y: Option<i32>,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            tray_offset: 0,
            taskbar_index: 0,
            legacy_placement_pending: false,
            widget_visible: true,
            legacy_visibility_pending: false,
            poll_interval_ms: default_poll_interval(),
            language: None,
            last_update_check_unix: None,
            show_claude_code: true,
            show_codex: false,
            show_antigravity: false,
            show_opencode: false,
            show_cursor: false,
            show_copilot: false,
            custom_theme_enabled: true,
            usage_countdown: false,
            active_theme_path: None,
            dashboard_width: None,
            dashboard_height: None,
            active_token_refresh: true,
            float_x: None,
            float_y: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegacyPlacement {
    pub tray_offset: i32,
    pub taskbar_index: usize,
}

impl SettingsFile {
    pub fn normalize(&mut self) {
        if !matches!(
            self.poll_interval_ms,
            POLL_1_MIN | POLL_5_MIN | POLL_15_MIN | POLL_1_HOUR
        ) {
            self.poll_interval_ms = default_poll_interval();
        }
        if self.enabled_providers().is_empty() {
            self.set_enabled_providers(ProviderSet::default());
        }
        // The widget and Theme Studio are now one system. Keep accepting this
        // legacy setting so older settings files migrate cleanly.
        self.custom_theme_enabled = true;
        self.dashboard_width = valid_dashboard_dimension(self.dashboard_width);
        self.dashboard_height = valid_dashboard_dimension(self.dashboard_height);
    }

    pub fn legacy_placement(&self) -> Option<LegacyPlacement> {
        self.legacy_placement_pending.then_some(LegacyPlacement {
            tray_offset: self.tray_offset,
            taskbar_index: self.taskbar_index,
        })
    }

    pub fn consume_legacy_placement(&mut self) -> Option<LegacyPlacement> {
        let placement = self.legacy_placement()?;
        self.legacy_placement_pending = false;
        self.tray_offset = 0;
        self.taskbar_index = 0;
        Some(placement)
    }

    pub fn legacy_widget_visibility(&self) -> Option<bool> {
        self.legacy_visibility_pending
            .then_some(self.widget_visible)
    }

    pub fn consume_legacy_widget_visibility(&mut self) -> Option<bool> {
        let visible = self.legacy_widget_visibility()?;
        self.legacy_visibility_pending = false;
        self.widget_visible = true;
        Some(visible)
    }

    pub fn enabled_providers(&self) -> ProviderSet {
        ProviderSet::from_enabled(
            ProviderId::ALL
                .into_iter()
                .filter(|provider| self.provider_enabled(*provider)),
        )
    }

    pub fn provider_enabled(&self, provider: ProviderId) -> bool {
        match provider {
            ProviderId::Claude => self.show_claude_code,
            ProviderId::Codex => self.show_codex,
            ProviderId::Antigravity => self.show_antigravity,
            ProviderId::OpenCode => self.show_opencode,
            ProviderId::Cursor => self.show_cursor,
            ProviderId::Copilot => self.show_copilot,
        }
    }

    pub fn set_provider_enabled(&mut self, provider: ProviderId, enabled: bool) {
        match provider {
            ProviderId::Claude => self.show_claude_code = enabled,
            ProviderId::Codex => self.show_codex = enabled,
            ProviderId::Antigravity => self.show_antigravity = enabled,
            ProviderId::OpenCode => self.show_opencode = enabled,
            ProviderId::Cursor => self.show_cursor = enabled,
            ProviderId::Copilot => self.show_copilot = enabled,
        }
    }

    pub fn set_enabled_providers(&mut self, providers: ProviderSet) {
        for provider in ProviderId::ALL {
            self.set_provider_enabled(provider, providers.contains(provider));
        }
    }

    pub fn toggle_provider(&mut self, provider: ProviderId) -> bool {
        let mut providers = self.enabled_providers();
        if !providers.toggle(provider) {
            return false;
        }
        self.set_enabled_providers(providers);
        true
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageCache {
    pub updated_unix: u64,
    pub poll_ok: bool,
    pub data: AppUsageData,
}

/// Folder under `%APPDATA%` holding settings, themes, context menus and the
/// usage cache.
///
/// Deliberately distinct from the folder the original project uses, so this
/// fork and the original can be installed side by side without overwriting
/// each other's settings.
pub const APP_DATA_FOLDER: &str = "ClaudeCodeUsageMonitorPro";

/// The original project's folder, read once to seed this fork's own.
const UPSTREAM_APP_DATA_FOLDER: &str = "ClaudeCodeUsageMonitor";

/// This fork's settings folder; themes, assets and context menus live inside
/// it. Every path the application writes its own state to derives from here.
pub fn app_data_directory() -> PathBuf {
    appdata_root().join(APP_DATA_FOLDER)
}

#[cfg(not(test))]
fn appdata_root() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Tests must never read or write the settings, themes or context menus of
/// whoever runs them, so a test build keeps its own stand-in for `%APPDATA%`:
/// one folder per test process under the temp directory.
#[cfg(test)]
fn appdata_root() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let parent = std::env::temp_dir().join("ccum-pro-test-appdata");
        remove_stale_test_roots(&parent);
        let root = parent.join(std::process::id().to_string());
        // A reused process id must not inherit an earlier run's files.
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("the test APPDATA folder must be creatable");
        root
    })
    .clone()
}

/// Clear folders left by earlier test runs. Another test process may be using
/// its folder right now, so only one untouched for an hour counts as left over.
#[cfg(test)]
fn remove_stale_test_roots(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(60 * 60));
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Seed this fork's settings folder from the original project's, once.
///
/// Someone switching from the original would otherwise start from defaults and
/// lose their themes, widget position and provider selection. The original
/// folder is only ever read — never moved or deleted — so the original
/// application keeps working exactly as before.
///
/// Does nothing once this fork has a folder of its own, so a later change to
/// the original's settings never reaches back into this one.
pub fn migrate_from_original_settings() {
    let destination = app_data_directory();
    // Keyed on the settings file rather than the folder. The folder gets
    // created incidentally — `ensure_starter_theme` writes into `themes/` —
    // which would otherwise silently skip the copy and leave the user on
    // defaults. `settings.json` is written only once this fork has settings of
    // its own worth keeping.
    if settings_path().exists() {
        return;
    }
    let source = appdata_root().join(UPSTREAM_APP_DATA_FOLDER);
    if !source.is_dir() {
        return;
    }

    match copy_directory(&source, &destination) {
        Ok(count) => {
            crate::diagnose::log(format!(
                "copied {count} settings file(s) from {} into {}",
                source.display(),
                destination.display()
            ));
            repoint_active_theme(&source, &destination);
        }
        Err(error) => {
            crate::diagnose::log(format!(
                "unable to copy settings from {}: {error}; starting from defaults",
                source.display()
            ));
        }
    }
}

/// Rewrite the copied `active_theme_path` to the matching file in this fork's
/// own folder.
///
/// The setting holds an absolute path. Left alone it would still resolve — the
/// original folder is intact — so this fork would silently read *and write* the
/// original application's theme file, editing a theme belonging to the other
/// install.
fn repoint_active_theme(source: &Path, destination: &Path) {
    let settings_file = destination.join("settings.json");
    let Ok(contents) = std::fs::read_to_string(&settings_file) else {
        return;
    };
    let Ok(mut settings) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return;
    };
    let Some(active) = settings.get("active_theme_path").and_then(|v| v.as_str()) else {
        return;
    };

    // Only rewrite a path that actually lives in the original folder; a theme
    // the user stored elsewhere should keep pointing where they put it.
    let Ok(relative) = Path::new(active).strip_prefix(source) else {
        return;
    };
    let moved = destination.join(relative);
    settings["active_theme_path"] = serde_json::Value::String(moved.to_string_lossy().into_owned());

    if let Ok(serialized) = serde_json::to_string_pretty(&settings) {
        if std::fs::write(&settings_file, serialized).is_ok() {
            crate::diagnose::log(format!("active theme now read from {}", moved.display()));
        }
    }
}

/// Recursively copy `source` into `destination`, returning the file count.
fn copy_directory(source: &Path, destination: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(destination)?;
    let mut copied = 0;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copied += copy_directory(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
            copied += 1;
        }
    }
    Ok(copied)
}

pub fn settings_path() -> PathBuf {
    app_data_directory().join("settings.json")
}
pub fn usage_cache_path() -> PathBuf {
    app_data_directory().join("usage-cache.json")
}

pub fn load_settings() -> SettingsFile {
    let mut settings = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|content| decode_settings(&content))
        .unwrap_or_default();
    settings.normalize();
    settings
}

pub fn save_settings(settings: &SettingsFile) -> Result<(), String> {
    let mut normalized = settings.clone();
    normalized.normalize();
    write_json_atomic(&settings_path(), &settings_json(&normalized))
}

fn decode_settings(content: &str) -> Option<SettingsFile> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    let legacy_placement_pending = value.as_object().is_some_and(|object| {
        object.contains_key("tray_offset") || object.contains_key("taskbar_index")
    });
    let legacy_visibility_pending = value
        .as_object()
        .is_some_and(|object| object.contains_key("widget_visible"));
    let mut settings: SettingsFile = serde_json::from_value(value).ok()?;
    settings.legacy_placement_pending = legacy_placement_pending;
    settings.legacy_visibility_pending = legacy_visibility_pending;
    Some(settings)
}

fn settings_json(settings: &SettingsFile) -> serde_json::Value {
    let mut value = serde_json::to_value(settings).unwrap_or_default();
    if settings.legacy_placement_pending {
        if let Some(object) = value.as_object_mut() {
            object.insert("tray_offset".into(), settings.tray_offset.into());
            object.insert("taskbar_index".into(), settings.taskbar_index.into());
        }
    }
    if settings.legacy_visibility_pending {
        if let Some(object) = value.as_object_mut() {
            object.insert("widget_visible".into(), settings.widget_visible.into());
        }
    }
    value
}

pub fn codex_credits_path() -> PathBuf {
    app_data_directory().join("codex-credits.json")
}

pub fn load_codex_credits() -> Option<CodexCreditsState> {
    read_json(&codex_credits_path())
}

pub fn save_codex_credits(state: &CodexCreditsState) -> Result<(), String> {
    write_json_atomic(&codex_credits_path(), state)
}

pub fn load_usage_cache() -> Option<UsageCache> {
    read_json(&usage_cache_path())
}

pub fn save_usage_cache(data: &AppUsageData, poll_ok: bool) -> Result<(), String> {
    write_json_atomic(
        &usage_cache_path(),
        &UsageCache {
            updated_unix: now_unix(),
            poll_ok,
            data: data.clone(),
        },
    )
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path.parent().ok_or("Invalid settings path")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let json = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&temporary).map_err(|error| error.to_string())?;
        file.write_all(&json).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
    }
    let source = wide_path(&temporary);
    let destination = wide_path(path);
    let moved = unsafe {
        MoveFileExW(
            PCWSTR::from_raw(source.as_ptr()),
            PCWSTR::from_raw(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return Err("Unable to replace the settings file".into());
    }
    Ok(())
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn default_poll_interval() -> u32 {
    POLL_15_MIN
}
fn default_true() -> bool {
    true
}
fn valid_dashboard_dimension(value: Option<f32>) -> Option<f32> {
    value.filter(|value| value.is_finite() && (64.0..=16_384.0).contains(value))
}
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Running the tests once wrote built-in menus and themes into the real
    /// `%APPDATA%\ClaudeCodeUsageMonitorPro` of whoever ran them.
    #[test]
    fn tests_never_touch_the_real_application_folder() {
        let root = app_data_directory();
        let sandbox = std::env::temp_dir().join("ccum-pro-test-appdata");
        assert!(root.starts_with(&sandbox), "{}", root.display());
        if let Some(real) = std::env::var_os("APPDATA") {
            assert!(!root.starts_with(PathBuf::from(real)), "{}", root.display());
        }
        for folder in [
            settings_path(),
            usage_cache_path(),
            codex_credits_path(),
            crate::theme_engine::themes_directory(),
            crate::theme_engine::assets_directory(),
            crate::context_menu::context_menus_directory(),
        ] {
            assert!(folder.starts_with(&root), "{}", folder.display());
        }
    }

    /// A unique scratch directory, removed when the test finishes.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("ccum-pro-{label}-{unique}"));
            std::fs::create_dir_all(&path).expect("scratch directory");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn copying_settings_preserves_nested_files() {
        let scratch = Scratch::new("copy");
        let source = scratch.0.join("from");
        let destination = scratch.0.join("to");
        std::fs::create_dir_all(source.join("themes/assets")).unwrap();
        std::fs::write(source.join("settings.json"), "{}").unwrap();
        std::fs::write(source.join("themes/classic.json"), "{}").unwrap();
        std::fs::write(source.join("themes/assets/icon.png"), [1u8, 2, 3]).unwrap();

        assert_eq!(copy_directory(&source, &destination).unwrap(), 3);
        assert!(destination.join("settings.json").is_file());
        assert!(destination.join("themes/classic.json").is_file());
        assert_eq!(
            std::fs::read(destination.join("themes/assets/icon.png")).unwrap(),
            [1u8, 2, 3],
            "nested binary assets must survive the copy"
        );
    }

    #[test]
    fn migrated_theme_path_points_into_the_new_folder() {
        let scratch = Scratch::new("repoint");
        let source = scratch.0.join("Original");
        let destination = scratch.0.join("Pro");
        std::fs::create_dir_all(&destination).unwrap();
        let original_theme = source.join("themes").join("classic.json");
        std::fs::write(
            destination.join("settings.json"),
            serde_json::json!({ "active_theme_path": original_theme.to_string_lossy() }).to_string(),
        )
        .unwrap();

        repoint_active_theme(&source, &destination);

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(destination.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            written["active_theme_path"].as_str().unwrap(),
            destination.join("themes").join("classic.json").to_string_lossy(),
            "the fork must not read or write the original install's theme file"
        );
    }

    #[test]
    fn a_theme_stored_outside_the_original_folder_is_left_alone() {
        let scratch = Scratch::new("external");
        let source = scratch.0.join("Original");
        let destination = scratch.0.join("Pro");
        std::fs::create_dir_all(&destination).unwrap();
        let elsewhere = "D:\\my-themes\\custom.json";
        std::fs::write(
            destination.join("settings.json"),
            serde_json::json!({ "active_theme_path": elsewhere }).to_string(),
        )
        .unwrap();

        repoint_active_theme(&source, &destination);

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(destination.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(written["active_theme_path"].as_str().unwrap(), elsewhere);
    }

    #[test]
    fn an_incidental_themes_folder_does_not_block_the_copy() {
        // `ensure_starter_theme` and the test suite both create `themes/`
        // before any settings exist. Keying the migration on the folder made
        // that silently skip the copy and strand the user on defaults.
        let scratch = Scratch::new("incidental");
        let source = scratch.0.join("Original");
        let destination = scratch.0.join("Pro");
        std::fs::create_dir_all(source.join("themes")).unwrap();
        std::fs::write(source.join("settings.json"), r#"{"poll_interval_ms":300000}"#).unwrap();
        std::fs::create_dir_all(destination.join("themes")).unwrap();

        assert!(
            !destination.join("settings.json").exists(),
            "the marker the migration keys on must still be absent"
        );
        assert_eq!(copy_directory(&source, &destination).unwrap(), 1);
        assert!(destination.join("settings.json").is_file());
    }

    #[test]
    fn the_fork_uses_its_own_folder() {
        assert_ne!(
            APP_DATA_FOLDER, UPSTREAM_APP_DATA_FOLDER,
            "sharing a settings folder lets the two installs overwrite each other"
        );
    }

    #[test]
    fn settings_never_disable_every_provider() {
        let mut settings = SettingsFile {
            show_claude_code: false,
            show_codex: false,
            show_antigravity: false,
            ..Default::default()
        };
        settings.normalize();
        assert_eq!(settings.enabled_providers(), ProviderSet::default());
    }

    #[test]
    fn provider_selection_keeps_the_existing_settings_keys() {
        let mut settings = SettingsFile::default();
        settings.set_enabled_providers(ProviderSet::from_enabled([
            ProviderId::Codex,
            ProviderId::Antigravity,
            ProviderId::OpenCode,
            ProviderId::Cursor,
        ]));

        let json = settings_json(&settings);
        assert_eq!(json["show_claude_code"], false);
        assert_eq!(json["show_codex"], true);
        assert_eq!(json["show_antigravity"], true);
        assert_eq!(json["show_opencode"], true);
        assert_eq!(json["show_cursor"], true);

        let decoded = decode_settings(&json.to_string()).unwrap();
        assert_eq!(decoded.enabled_providers(), settings.enabled_providers());
    }

    #[test]
    fn provider_toggle_keeps_the_last_provider_enabled() {
        let mut settings = SettingsFile::default();
        assert!(!settings.toggle_provider(ProviderId::Claude));
        assert_eq!(settings.enabled_providers(), ProviderSet::default());
    }

    #[test]
    fn usage_direction_defaults_to_counting_up_and_round_trips() {
        let settings = SettingsFile::default();
        assert!(!settings.usage_countdown);
        assert_eq!(settings_json(&settings)["usage_countdown"], false);

        let counting_up = decode_settings(r#"{"poll_interval_ms":900000}"#).unwrap();
        assert!(!counting_up.usage_countdown);

        let counting_down = decode_settings(r#"{"usage_countdown":true}"#).unwrap();
        assert!(counting_down.usage_countdown);
        assert_eq!(settings_json(&counting_down)["usage_countdown"], true);
    }

    #[test]
    fn settings_always_use_the_theme_widget() {
        let mut settings = SettingsFile {
            custom_theme_enabled: false,
            ..Default::default()
        };
        settings.normalize();
        assert!(settings.custom_theme_enabled);
    }

    #[test]
    fn legacy_widget_visibility_is_preserved_until_migration_consumes_it() {
        let mut settings = decode_settings(r#"{"widget_visible":false}"#).unwrap();
        assert_eq!(settings.legacy_widget_visibility(), Some(false));
        assert_eq!(settings_json(&settings)["widget_visible"], false);

        assert_eq!(settings.consume_legacy_widget_visibility(), Some(false));
        assert_eq!(settings.legacy_widget_visibility(), None);
        assert!(settings_json(&settings).get("widget_visible").is_none());
    }

    #[test]
    fn legacy_placement_is_preserved_until_the_migration_consumes_it() {
        let mut settings = decode_settings(
            r#"{
                "tray_offset": 144,
                "taskbar_index": 2,
                "poll_interval_ms": 60000,
                "show_claude_code": true
            }"#,
        )
        .unwrap();

        assert_eq!(
            settings.legacy_placement(),
            Some(LegacyPlacement {
                tray_offset: 144,
                taskbar_index: 2,
            })
        );
        let pending = settings_json(&settings);
        assert_eq!(pending["tray_offset"], 144);
        assert_eq!(pending["taskbar_index"], 2);

        settings.consume_legacy_placement();
        let migrated = settings_json(&settings);
        assert!(migrated.get("tray_offset").is_none());
        assert!(migrated.get("taskbar_index").is_none());
        assert_eq!(migrated["poll_interval_ms"], 60000);
    }

    #[test]
    fn modern_settings_do_not_request_legacy_migration() {
        let settings = decode_settings(
            r#"{
                "poll_interval_ms": 900000,
                "active_theme_path": "migrated-theme.json"
            }"#,
        )
        .unwrap();
        assert_eq!(settings.legacy_placement(), None);
        assert_eq!(settings.legacy_widget_visibility(), None);
    }

    #[test]
    fn dashboard_dimensions_are_preserved_and_validated() {
        let settings = decode_settings(
            r#"{
                "dashboard_width": 1280.5,
                "dashboard_height": 760.0
            }"#,
        )
        .unwrap();
        assert_eq!(settings.dashboard_width, Some(1280.5));
        assert_eq!(settings.dashboard_height, Some(760.0));

        let mut invalid = SettingsFile {
            dashboard_width: Some(0.0),
            dashboard_height: Some(20_000.0),
            ..Default::default()
        };
        invalid.normalize();
        assert_eq!(invalid.dashboard_width, None);
        assert_eq!(invalid.dashboard_height, None);
    }
}

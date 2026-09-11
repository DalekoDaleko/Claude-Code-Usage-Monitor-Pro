# Claude Code Usage Monitor Pro

![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A lightweight, open-source Windows taskbar widget for monitoring Claude Code usage limits and reset times. It can also display usage for Codex, Google Antigravity, OpenCode Go, and Cursor.

> **This is an unofficial fork.** The original **Claude Code Usage Monitor** is created and
> maintained by **Craig Constable** ([Code Zeno Pty Ltd](https://codezeno.com.au)) at
> [CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor),
> and all credit for the application belongs to them. This fork only adds the changes listed
> in [Changes in this fork](#changes-in-this-fork). It is not affiliated with, endorsed by, or
> supported by Code Zeno Pty Ltd — please report issues with this fork here, not upstream.

![Claude Code Usage Monitor Pro running in the Windows taskbar](.github/animation.gif)

## Features

- Displays current usage and time remaining until each limit resets
- Counts usage up from zero or down from the full allowance, whichever you prefer
- Supports Claude Code, Codex, Google Antigravity, OpenCode Go, and Cursor
- Lives in the Windows taskbar with quick controls in the system tray
- Supports multiple monitors and Windows startup
- Includes configurable refresh intervals, providers, languages, and updates
- Provides built-in themes and a visual Theme Studio for custom layouts
- Collects no analytics or telemetry

## Requirements

- Windows 10 or Windows 11
- At least one supported provider installed and signed in

Claude Code credentials can be detected from the CLI, Claude desktop app, or WSL. Other providers are optional and can be enabled independently from the dashboard.

## Installation

Install the latest release with WinGet:

```powershell
winget install DalekoDaleko.ClaudeCodeUsageMonitorPro
```

or download `claude-code-usage-monitor-pro.exe` from
[GitHub Releases](https://github.com/DalekoDaleko/Claude-Code-Usage-Monitor-Pro/releases).

You can also [build it from source](#build-from-source).

## Usage

Start the monitor:

```powershell
claude-code-usage-monitor-pro
```

Open the settings dashboard directly:

```powershell
claude-code-usage-monitor-pro --dashboard
```

Use the dashboard to select providers, change the refresh interval, choose a display, enable startup, or customize the widget. **Settings > Display > Usage direction** switches the default theme and other themes that support this setting between showing what has been used and what is left, with Used as the default. Selecting Remaining makes a fresh limit read 100% and drain as you work.

Theme authors can opt in with `.display` bindings, including `{claude.session.display:usage_line}` and `{claude.session.display:usage_badge}`. Existing `.percentage`, `.remaining`, and unsuffixed usage summaries keep their meaning; warning thresholds should continue to use `.percentage`.

In the default theme, left-click a provider tray icon to show or hide the widget and right-click it to open the menu.

## Provider setup

| Provider | Setup |
| --- | --- |
| Claude Code | Sign in with the Claude Code CLI or desktop app. Windows and WSL credentials are detected automatically. |
| Codex | Install and sign in to the Codex CLI, then enable Codex in **Providers**. |
| Google Antigravity | Sign in to Antigravity, then enable it in **Providers**. |
| OpenCode Go | Connect an OpenCode Go account, configure the credentials described below, then enable OpenCode in **Providers**. |
| Cursor | Sign in to Cursor, then enable it in **Providers**. The local session is detected automatically. |

For OpenCode Go, set `OPENCODE_GO_WORKSPACE_ID` and `OPENCODE_GO_AUTH_COOKIE`, or create `%APPDATA%\opencode-go\config.json`:

```json
{
  "workspaceId": "wrk_01...",
  "authCookie": "your-opencode-auth-cookie"
}
```

The workspace ID is part of the OpenCode Go workspace URL. The auth cookie comes from an authenticated `opencode.ai` browser session. Set `OPENCODE_GO_CONFIG_FILE` to use a different config path.

For Cursor, `CURSOR_SESSION_TOKEN` can override the automatically detected local session.

## Data and privacy

The monitor reads local sign-in credentials for enabled providers and sends usage requests directly to their official services. It has no backend service, collects no telemetry, and does not upload credentials or project files.

Credentials are read without modifying the provider files that contain them. OpenCode Go credentials saved in a JSON configuration file are plain text and should be protected like a browser session cookie.

## Troubleshooting

Run diagnostics with:

```powershell
claude-code-usage-monitor-pro --diagnose
```

The diagnostic log is written to `%TEMP%\claude-code-usage-monitor-pro.log`. Application settings are stored in `%APPDATA%\ClaudeCodeUsageMonitorPro\settings.json`, separate from the original application's folder so both can be installed side by side. On first run, settings are copied from `%APPDATA%\ClaudeCodeUsageMonitor` if that folder exists; the original is left untouched.

## Build from source

Install [Rust](https://www.rust-lang.org/tools/install) 1.95 or later, then run:

```powershell
cargo build --release
```

The executable will be created at `target\release\claude-code-usage-monitor-pro.exe`.

## Changes in this fork

Based on upstream **v2.10.23**. Every change below is additive; no existing feature was removed
except where noted under Security.

### Taskbar placement

Upstream anchors the widget to the left edge of the system tray and grows leftwards, treating the
whole taskbar as free space. On a full taskbar the widget therefore drew on top of the running
application buttons and the "..." overflow button, making them unreachable.

- Added `src/taskbar_layout.rs`, which measures the real Windows 11 taskbar through UI Automation.
  Elements are classified by `AutomationId` (`StartButton`, `Appid:`/`Window:` buttons,
  `OverflowButton`, `SystemTrayIcon`), so the genuine free gap is known rather than assumed. The
  legacy `ReBarWindow32` window reports a fixed span on Windows 11 and cannot distinguish a full
  taskbar from an empty one.
- When the gap is too small, the widget now floats just above the taskbar instead of overlapping
  it, snapped by default to the right edge of the screen.
- The floating widget can be dragged anywhere on screen, and its position persists across restarts
  (`float_x`/`float_y` in `settings.json`). A 4 px threshold distinguishes a drag from a click, so
  click actions still work.
- A floating widget paints an opaque backdrop sampled from the taskbar's own colour, since it no
  longer has the taskbar behind its transparent pixels. The colour is re-sampled shortly after a
  theme or accent-colour change, once the shell has repainted.
- Measurements are cached briefly and invalidated on display, DPI, and setting changes, so
  UI Automation is not queried on every window move.

### Window behaviour fixes

- `make_popup` now clears `GWLP_HWNDPARENT`. `SetParent(NULL)` leaves the former host as the
  window's *owner*, and an owned window's z-order is pinned to its owner's, so requesting
  `HWND_TOPMOST` succeeded while never actually setting `WS_EX_TOPMOST`. This affected any
  surface that had been taskbar-hosted, not only the new floating mode.
- Floating surfaces pin their z-order in `WM_WINDOWPOSCHANGING`, so activating another window can
  no longer push the widget behind it.
- `apply_custom_theme` re-applies placement after resetting the surface, instead of leaving it
  stranded at `HWND_NOTOPMOST` until an unrelated event triggered a reposition.

### Token refresh

- Added an **Enable active token refresh** setting (Dashboard, General). Upstream renews an expired
  token by starting the provider's own CLI (`claude -p .`, `codex exec .`). That is a real API call,
  so refreshing consumes a little of the quota the tool exists to report.
- With the setting off, the monitor stays passive: it reports the expired token and waits for the
  user's own CLI session to refresh the credentials file, at which point the existing credential
  watcher resumes polling automatically.
- The setting defaults to **on**, so upgrading changes nothing until you choose otherwise.

### Stale readings

- When a poll fails while figures are still on screen, the widget now renders in greyscale rather
  than continuing to look live. Upstream showed a single tray balloon and then left the last known
  numbers on screen indefinitely with no visual indication they had stopped updating.

### Security

- **Removed the portable self-update mechanism.** Upstream downloaded a replacement executable over
  HTTPS and swapped the running binary with no code-signature or hash verification. WinGet installs
  still update in place, because WinGet verifies its own packages; portable builds are pointed at the
  Releases page. The `--apply-update` command-line mode is now rejected rather than honoured, so it
  can no longer be used as an arbitrary file-overwrite primitive.
- **Fixed the GitHub link in the dashboard.** It called `Context::open_url`, which is a silent no-op
  because `eframe` is built with `default-features = false`. It now opens the system browser through
  `ShellExecuteW`, restricted to `http`/`https` so a user-editable theme or context menu cannot use
  it to launch a local executable or a registered protocol handler.

## Credits

The original **Claude Code Usage Monitor** is the work of **Craig Constable**
([Code Zeno Pty Ltd](https://codezeno.com.au)) and its contributors. This fork builds on their
code, which remains the overwhelming majority of this repository, and is redistributed under the
same MIT licence with the original copyright notice intact.

## License

Licensed under the [MIT License](LICENSE).

- Copyright (c) 2026 Vitaliy Titov (updates to UI and self-update logic)
- Copyright (c) 2025 Craig Constable (original author)

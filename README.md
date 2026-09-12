# Claude Code Usage Monitor Pro

![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A lightweight, open-source Windows taskbar widget for monitoring Claude Code usage limits and reset times. It can also display usage for Codex, Google Antigravity, OpenCode Go, Cursor, and GitHub Copilot.

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
- Supports Claude Code, Codex, Google Antigravity, OpenCode Go, Cursor, and GitHub Copilot
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
| Claude Code | Sign in with the Claude Code CLI or the Claude desktop app, installed from claude.ai or the Microsoft Store. Windows and WSL credentials are detected automatically. |
| Codex | Install and sign in to the Codex CLI, then enable Codex in **Providers**. |
| Google Antigravity | Sign in to Antigravity, then enable it in **Providers**. |
| OpenCode Go | Connect an OpenCode Go account, configure the credentials described below, then enable OpenCode in **Providers**. |
| Cursor | Sign in to Cursor, then enable it in **Providers**. The local session is detected automatically. |
| GitHub Copilot | Sign in with the GitHub Copilot CLI, or run `gh auth login`, then enable GitHub Copilot in **Providers**. The widget shows the premium requests used this month. To use a separate token, see below. |

For OpenCode Go, set `CLAUDECODEUSAGE_OPENCODE_GO_WORKSPACE_ID` to the workspace ID and
`CLAUDECODEUSAGE_OPENCODE_GO_AUTH_COOKIE` to the **encrypted** session cookie (see
[Secrets in environment variables](#secrets-in-environment-variables)), or create
`%APPDATA%\opencode-go\config.json`, in which the cookie is stored **encrypted** as
`encryptedAuthCookie`:

```json
{
  "workspaceId": "wrk_01...",
  "encryptedAuthCookie": "01000000d08c9ddf0115d1118c7a00c04fc297eb..."
}
```

This creates that file, prompting for the cookie without echoing it:

```powershell
$cookie = Read-Host -AsSecureString 'OpenCode auth cookie' | ConvertFrom-SecureString
New-Item -ItemType Directory -Force "$env:APPDATA\opencode-go" | Out-Null
[ordered]@{ workspaceId = 'wrk_01...'; encryptedAuthCookie = $cookie } |
    ConvertTo-Json | Set-Content "$env:APPDATA\opencode-go\config.json" -Encoding utf8
```

A plain-text `authCookie` in this file is refused and never used; the diagnostic log names the file
and the reason, never the value.

The workspace ID is part of the OpenCode Go workspace URL. It is not a secret, so its variable holds
plain text:

```powershell
[Environment]::SetEnvironmentVariable('CLAUDECODEUSAGE_OPENCODE_GO_WORKSPACE_ID', 'wrk_01...', 'User')
```

The auth cookie comes from an authenticated `opencode.ai` browser session. Set
`CLAUDECODEUSAGE_OPENCODE_GO_CONFIG_FILE` to keep this config file somewhere else; a file named there
uses the same encrypted format.

If you already use opencode-bar or opencode-quota, the
monitor also reuses the sign-in from their `opencode-go.json` in `~/.config/opencode-bar` or
`~/.config/opencode-quota`. Those files belong to those tools and store the cookie in their own
plain-text format, which the monitor reads as-is.

For Cursor, an **encrypted** `CLAUDECODEUSAGE_CURSOR_SESSION_TOKEN` can override the automatically
detected local session.

For GitHub Copilot, the token is read from the GitHub Copilot CLI's sign-in, or failing that the
GitHub CLI's, both kept encrypted in Windows Credential Manager. To give the monitor a token of its
own instead, store it **encrypted** in `CLAUDECODEUSAGE_COPILOT_GITHUB_TOKEN_DPAPI`. If a plain-text
token is placed there, the log names only its kind, such as `gho_…`.

### Secrets in environment variables

A variable that carries a secret must hold the output of PowerShell's `ConvertFrom-SecureString`,
never the secret itself:

| Variable | Holds |
| --- | --- |
| `CLAUDECODEUSAGE_OPENCODE_GO_AUTH_COOKIE` | OpenCode Go session cookie |
| `CLAUDECODEUSAGE_CURSOR_SESSION_TOKEN` | Cursor session token |
| `CLAUDECODEUSAGE_COPILOT_GITHUB_TOKEN_DPAPI` | GitHub token for Copilot |

This command stores one, changing the variable name as needed. It prompts for the secret without
echoing it and never places it on a command line:

```powershell
[Environment]::SetEnvironmentVariable(
    'CLAUDECODEUSAGE_CURSOR_SESSION_TOKEN',
    (Read-Host -AsSecureString 'Secret' | ConvertFrom-SecureString),
    'User')
```

Restart the monitor afterwards; a running program never sees a changed environment variable.
`ConvertFrom-SecureString` encrypts with DPAPI for your Windows account, so the value only decrypts
for that account on that PC, and a copy that leaks elsewhere is useless. It does not protect against
programs running as you, which is equally true of the provider sign-ins the monitor reads. A value
that is not encrypted this way is refused and never used; the diagnostic log names the variable and
the reason, never the value.

The upstream names `OPENCODE_GO_WORKSPACE_ID`, `OPENCODE_GO_AUTH_COOKIE`, `OPENCODE_GO_CONFIG_FILE`
and `CURSOR_SESSION_TOKEN` are no longer read. If one is still set, the diagnostic log says which variable replaces it.

## Data and privacy

The monitor reads local sign-in credentials for enabled providers and sends usage requests directly to their official services. It has no backend service, collects no telemetry, and does not upload credentials or project files.

Credentials are read without modifying the provider files that contain them. Secrets that this monitor itself reads from environment variables or from its own OpenCode Go configuration file must be encrypted with DPAPI; see [Secrets in environment variables](#secrets-in-environment-variables). Sign-ins reused from other tools are read in whatever form those tools store them.

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

After changing dependencies, regenerate the third-party licence notices, which are embedded in the
executable; a test fails until they match `Cargo.lock` again:

```powershell
python tools/third_party_notices.py
```

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
- The widget now floats just above the taskbar by default, snapped to the right edge of the screen,
  rather than being hosted inside it. Windows sets no space aside in the taskbar for another
  gadget, so whatever sits there covers something else. **Settings > Display > Dock in taskbar when
  possible**, also in the tray menu under **Settings**, puts it back in the taskbar; it then still
  floats whenever the gap is too small, which is what measuring the taskbar is for.
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
- A valid Claude token from any source is used before anything is refreshed. Upstream stopped at the
  first sign-in it found: an expired `~/.claude/.credentials.json` hid a current token kept by the
  Claude desktop app, and with refresh on it would spend quota on `claude -p .` instead of using it.
- The Claude desktop app's token is also found when the app was installed from the **Microsoft
  Store**. Windows keeps a Store app's AppData in its package folder
  (`%LOCALAPPDATA%\Packages\Claude_<publisher id>\LocalCache\Roaming\Claude`), where upstream never
  looked, so Store users had to keep a separate Claude Code CLI login fresh.
- Of the several tokens the desktop app keeps, the one chosen now carries both the `user:inference`
  and `user:profile` scopes. Upstream picked the latest-expiring inference token, which can be one
  without `user:profile`; the usage endpoint refuses that with 403.
- WSL is left alone unless it can help, and never on the UI thread. Upstream started `wsl.exe` to
  list Linux distros whenever no Windows-side Claude token was usable, probed each one with a
  five-second timeout, and did so from the window procedure while waiting for a new sign-in. Since
  this widget is parented into Explorer's taskbar, Explorer waits on that thread, so the taskbar
  froze and could crash. The monitor now checks the credential sources on a worker thread, reads the
  registry first and skips WSL entirely when no distro is installed, looks only at distros that are
  already running, and ignores the appliances belonging to Docker Desktop, Rancher Desktop and
  Podman, which no one signs in to and which took the full timeout to answer.

### Stale readings

- When a poll fails while figures are still on screen, the widget now renders in greyscale rather
  than continuing to look live. Upstream showed a single tray balloon and then left the last known
  numbers on screen indefinitely with no visual indication they had stopped updating.

### GitHub Copilot provider

- Shows GitHub Copilot's monthly premium-request allowance: one bar for the share used and the time
  until it resets. The second row is reserved and always drawn empty.
- Reads `api.github.com/copilot_internal/user`, the endpoint VS Code queries for its own Copilot
  quota display (the `entitlementUrl` in VS Code's `product.json`). It is not a documented public
  API, so every field is treated as optional. The percentage follows VS Code's own derivation, so the
  widget agrees with the editor. Reading it does not consume any quota.
- Every token source is DPAPI-protected: an optional `CLAUDECODEUSAGE_COPILOT_GITHUB_TOKEN_DPAPI`
  variable holding `ConvertFrom-SecureString` output, then the GitHub Copilot CLI's sign-in, then the
  GitHub CLI's, both from Windows Credential Manager. A plain-text token in the variable is refused.
  The token is sent only to `api.github.com`. Both CLI sign-ins carry broad repository permissions;
  the monitor only makes one read-only request with them. If GitHub rejects one source's token, the
  next source is tried.
- The Credential Manager reader and the DPAPI decryption, previously private to the Antigravity and
  Claude desktop readers, are shared so all three decode through the same tested code. The Copilot
  CLI's settings file carries `//` comments, which strict JSON rejects; they are stripped before
  parsing, or that sign-in would be skipped.
- Fixed `usage_line` formatting keeping its own list of provider names, which made any provider
  missing from that list render as `--`. It now resolves providers through the registry.

### Security

- **Encrypted secrets in environment variables.** The OpenCode Go session cookie and the Cursor
  session token, which upstream read from environment variables in plain text, must now be
  DPAPI-encrypted with `ConvertFrom-SecureString`, as the Copilot token is; a plain-text value is
  refused and never used. The variables gained a `CLAUDECODEUSAGE_` prefix
  (`OPENCODE_GO_WORKSPACE_ID`, `OPENCODE_GO_AUTH_COOKIE` and `CURSOR_SESSION_TOKEN` become
  `CLAUDECODEUSAGE_OPENCODE_GO_WORKSPACE_ID`, `CLAUDECODEUSAGE_OPENCODE_GO_AUTH_COOKIE` and
  `CLAUDECODEUSAGE_CURSOR_SESSION_TOKEN`). The old names are no longer read, and if one is set the
  log names its replacement. The workspace ID is not a secret and stays plain text. All three
  providers decrypt through one shared, tested routine.
- **Encrypted OpenCode Go config file.** In the monitor's own `%APPDATA%\opencode-go\config.json`,
  and any file named by `CLAUDECODEUSAGE_OPENCODE_GO_CONFIG_FILE` (formerly
  `OPENCODE_GO_CONFIG_FILE`), the cookie moved from a plain-text `authCookie` to a DPAPI-encrypted
  `encryptedAuthCookie`; a plain `authCookie` there is refused. Files written by opencode-bar and
  opencode-quota are still read in those tools' own format. A UTF-8 byte-order mark, which Windows
  PowerShell 5.1 writes, no longer causes the file to be skipped silently.
- **Removed the portable self-update mechanism.** Upstream downloaded a replacement executable over
  HTTPS and swapped the running binary with no code-signature or hash verification. WinGet installs
  still update in place, because WinGet verifies its own packages; portable builds are pointed at the
  Releases page. The `--apply-update` command-line mode is now rejected rather than honoured, so it
  can no longer be used as an arbitrary file-overwrite primitive.
- **Fixed the GitHub link in the dashboard.** It called `Context::open_url`, which is a silent no-op
  because `eframe` is built with `default-features = false`. It now opens the system browser through
  `ShellExecuteW`, restricted to `http`/`https` so a user-editable theme or context menu cannot use
  it to launch a local executable or a registered protocol handler.
- **Self-contained executable.** The C runtime is linked into the exe instead of being loaded from
  `VCRUNTIME140.dll`, which belongs to the Visual C++ Redistributable rather than to Windows. On a PC
  without that redistributable, the upstream build cannot start at all (`STATUS_DLL_NOT_FOUND`,
  `0xC0000135`); this build needs only DLLs that ship with Windows 10 and 11.
- **Third-party licence notices.** The MIT and Apache-2.0 licences of the crates this app is built
  from ask for their notices to accompany binary copies, and neither upstream's repository nor its
  release shipped any. [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt) now lists every crate
  compiled into the exe and the embedded Ubuntu font and Lucide icon subsets, with their licence
  texts. It is generated by `tools/third_party_notices.py`, embedded in the executable, and shown
  from the About page, so a WinGet install, which consists of the exe alone, carries it too.
- **Complete translations.** Every string the app shows is now in all 14 language files. This fork's
  additions (active token refresh, the About page, sign-in sources, GitHub Copilot) had fallen back to
  English everywhere, and about a hundred upstream Theme Studio strings per language were still
  English placeholders. Only product names, X, Y and URL stay in English.
- **About page in the dashboard.** Shows the version and copyright notices, and for each enabled
  provider where its sign-in comes from (for example the Claude desktop app from the Microsoft Store,
  an environment variable, or a CLI sign-in) and when its token expires, when the credential records
  that. Everything is read from the PC without contacting the providers, and tokens are never shown.
- **The dashboard draws with Direct3D 12.** Upstream used OpenGL, which depends on the graphics
  vendor's driver; in a virtual machine or a Remote Desktop session without GPU support Windows offers
  only OpenGL 1.1, and the dashboard failed with "egui_glow requires opengl 2.0+". Direct3D 12 works
  on any Windows 10 or 11 PC, through WARP, Windows' software renderer, when there is no GPU driver.
  Only the Direct3D 12 backend is compiled in, with Windows' own FXC shader compiler; wgpu's default
  of loading `dxcompiler.dll` from the PATH, and its environment-variable overrides for the compiler
  and runtime, are not used.
- **Cursor's state database is no longer copied.** When Cursor held a lock on
  `%APPDATA%\Cursor\User\globalStorage\state.vscdb`, upstream copied the whole database, which holds
  everything Cursor stores, to `%TEMP%`, read the token from the copy and deleted it, ignoring a
  failed delete. The database is now always read in place: normally with SQLite's shared lock, and
  while Cursor holds a write lock, with SQLite's `immutable` option, which takes no lock and writes
  nothing.

## Credits

The original **Claude Code Usage Monitor** is the work of **Craig Constable**
([Code Zeno Pty Ltd](https://codezeno.com.au)) and its contributors. This fork builds on their
code, which remains the overwhelming majority of this repository, and is redistributed under the
same MIT licence with the original copyright notice intact.

## License

Licensed under the [MIT License](LICENSE).

- Copyright (c) 2026 Vitaliy Titov (updates to UI and self-update logic)
- Copyright (c) 2025 Craig Constable (original author)

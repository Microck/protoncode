<p align="center">
  <img src="assets/protoncode-icon.png" width="100" alt="protoncode logo">
</p>

<p align="center">
  <strong>protoncode</strong> — proton mail otp notifications for windows and linux
</p>

<p align="center">
  masked by default · tray-first · built in rust
</p>

<p align="center">
  <a href="https://github.com/Microck/protoncode/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-mit-000000?style=flat-square" alt="license badge"></a>
  <a href="https://github.com/Microck/protoncode/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/Microck/protoncode/ci.yml?branch=main&style=flat-square&label=ci&color=000000" alt="ci badge"></a>
  <a href="https://github.com/Microck/protoncode/releases"><img src="https://img.shields.io/github/v/release/Microck/protoncode?display_name=tag&style=flat-square&label=release&color=000000" alt="release badge"></a>
  <a href="https://github.com/Microck/protoncode/actions/workflows/release-please.yml"><img src="https://img.shields.io/badge/versioning-release--please-000000?style=flat-square" alt="versioning badge"></a>
</p>

<p align="center">
  <img src="assets/protoncode-visual.png" width="800" alt="protoncode visual">
</p>

## overview

protoncode is a desktop tray app that watches a live proton mail session and surfaces otp emails as masked notifications near the bottom-right corner of the screen. it is built for people who need quick code access without opening mail every time or leaking codes during screen sharing.

the app embeds a webview that maintains a proton mail login session. it polls for new emails, detects otp codes using context-aware pattern matching, and displays a masked overlay notification. the code stays hidden until you explicitly reveal or copy it.

## features

- **tray-first design** — lives in the system tray with an optional autostart launch path that starts hidden
- **masked otp overlay** — codes are hidden by default (`****`) with reveal and copy actions
- **context-aware detection** — identifies otp codes by looking for verification keywords near digit sequences (4–8 digits), rejecting dates and random numbers
- **proton-style dark ui** — status window and notification card follow proton mail's dark theme
- **proton mail session monitoring** — embedded webview maintains a live session, no bridge required
- **windows autostart** — per-user autostart via `hkcu\software\microsoft\windows\currentversion\run`
- **cross-platform packaging** — `.exe` for windows, `.deb`/`.rpm`/`.tar.gz` for linux
- **credential store integration** — session metadata stored through the platform credential store via the `keyring` crate
- **github release automation** — release-please manages versioning, ci builds and publishes artifacts

## install

### requirements

| platform | requirements |
|----------|-------------|
| windows  | windows 10+ with webview2 runtime |
| linux    | gtk3, webkitgtk, and ayatana appindicator runtime packages |

you also need a proton mail account. the app uses the proton mail web interface directly — proton mail bridge is not required.

### from github releases

download the latest release for your platform:

- **windows** — download `protoncode.exe` and run it
- **linux** — download the `.deb`, `.rpm`, or `.tar.gz` and install with your package manager

after launching, enable **launch on windows sign-in** from the status window to set up autostart.

### from source

```bash
git clone https://github.com/Microck/protoncode.git
cd protoncode
cargo run
```

for a cross-compiled windows release from linux:

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

## usage

1. launch `protoncode`
2. sign into proton mail in the embedded login window
3. leave the app running in the tray
4. when an otp email arrives, a masked notification appears
5. click to reveal the code or copy it directly

the notification shows the code as `****` by default — you must explicitly reveal it. this prevents accidental exposure during screen sharing or when someone is looking at your screen.

## configuration

protoncode stores its configuration at the os-specific config directory under `protoncode/config.json`.

| setting | default | description |
|---------|---------|-------------|
| `poll_interval_seconds` | `8` | how often to check for new emails |
| `notification_duration_seconds` | `8` | how long the notification stays visible |
| `launch_on_startup` | `false` | whether to launch on system startup |
| `start_minimized_to_tray` | `true` | whether to start hidden in the tray |
| `copy_button_enabled` | `true` | show a copy button on the notification |
| `proton_mail_url` | `https://mail.proton.me/u/0/inbox` | the proton mail url to monitor |
| `user_data_dir` | `{config_dir}/protoncode/webview-data` | webview session data directory |

the first time protoncode runs, it creates a default config file automatically.

## architecture

```
src/
├── main.rs            # entry point, logging init, platform dispatch
├── lib.rs             # crate root
├── app.rs             # core app logic, session management, notification dispatch
├── config.rs          # appconfig with json persistence
├── desktop_app.rs     # gui: tray icon, webview, status window, notification card
├── models.rs          # data types: mailsessionstate, otpcandidateemail, otpnotification
├── otp.rs             # otp detection: regex matching with context-aware filtering
├── secrets.rs         # credential store wrapper via keyring
└── autostart.rs       # platform-specific autostart registration
```

**otp detection flow:**

1. webview polls the proton mail inbox at the configured interval
2. new emails are parsed into `OtpCandidateEmail` structs
3. the otp detector (`otp.rs`) normalizes text and searches for 4–8 digit sequences
4. it validates candidates against context keywords (e.g. "verification", "2fa", "otp")
5. date-like patterns are rejected to avoid false positives
6. matched codes are wrapped in `OtpNotification` with masked display

## development

```bash
# run tests (otp detection, config parsing)
cargo test

# check compilation
cargo check

# check windows target from linux
cargo check --target x86_64-pc-windows-msvc
```

## release flow

1. ci runs on every push and pull request
2. [release-please](https://github.com/googleapis/release-please) manages version bumps from [conventional commits](https://www.conventionalcommits.org/)
3. release workflows build platform-specific artifacts:
   - `protoncode.exe` on windows
   - `.deb`, `.rpm`, `.tar.gz` on linux

## limitations

- the proton integration uses the proton mail web session, not proton mail bridge
- the monitor is best-effort and may break if proton changes its web ui or session behavior
- session metadata is stored through the platform credential store, while the live web session stays in the webview profile directory
- only windows and linux are currently supported

## license

[mit](license)

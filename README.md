<p align="center">
  <img src="assets/protoncode-icon.png" width="100" alt="protoncode logo">
</p>

<p align="center">
  proton mail otp notifications for windows, masked by default and built for tray-first use.
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

protoncode is a windows tray app that watches a live proton mail session and surfaces otp emails as masked notifications near the bottom-right corner of the screen. it is built for people who need quick code access without opening mail every time or leaking codes during screen sharing.

last reviewed: 2026-03-13

## features

- tray-first windows app with a hidden-on-autostart launch path
- masked otp overlay with reveal and copy actions
- proton-style dark ui for the status window and notification card
- proton mail session monitoring through an embedded webview2 window
- per-user windows autostart via `hkcu\software\microsoft\windows\currentversion\run`
- embedded app icon for the tray and windows executable
- github release automation for windows `.exe` bundles

## install

### requirements

- windows 10 or newer
- webview2 runtime
- a proton mail account that works with the current web-session approach

### from source

```bash
cargo run --target x86_64-pc-windows-msvc
```

### from github releases

- download the latest windows zip from releases
- extract it
- run `protoncode.exe`
- enable `launch on windows sign-in` from the status window if you want autostart

## usage

1. launch `protoncode.exe`
2. sign into proton mail in the embedded login window
3. leave the app in the tray
4. wait for otp mail to arrive
5. use the masked overlay to reveal or copy the code only when needed

## configuration

the app stores local state under the os config directory in `protoncode/`.

key settings:

- `poll_interval_seconds`
- `notification_duration_seconds`
- `copy_button_enabled`
- `launch_on_startup`

when autostart is enabled, protoncode writes a `protoncode` entry to the current-user windows run key and starts with `--autostart`, which forces a hidden-to-tray launch.

## visuals

- logo asset: [assets/protoncode-icon.png](assets/protoncode-icon.png)
- release icon resource: [assets/protoncode.ico](assets/protoncode.ico)
- screenshot placeholder: status window on the left, masked six-digit overlay on the bottom-right, proton login window hidden in tray mode

## development

```bash
cargo test
cargo check --target x86_64-pc-windows-msvc
```

for a local cross-compiled windows release build from linux:

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

## release flow

- ci runs on pushes and pull requests
- release-please manages version bumps and github releases from conventional commits
- a release workflow builds `protoncode.exe` on windows and uploads a zip asset to the github release

## notes

- the proton integration uses a web-session approach, not proton mail bridge
- that means the monitor is best-effort and may break if proton changes its web ui or session behavior
- session metadata is stored through the platform credential store, while the live web session stays in the webview2 profile directory

<p align="center">
  <img src="assets/protoncode-icon.png" width="100" alt="protoncode logo">
</p>

<p align="center">
  proton mail otp notifications for windows and linux, masked by default and built for tray-first use.
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

protoncode is a desktop tray app for windows and linux that watches a live proton mail session and surfaces otp emails as masked notifications near the bottom-right corner of the screen. it is built for people who need quick code access without opening mail every time or leaking codes during screen sharing.

## features

- tray-first desktop app with a hidden-on-autostart launch path
- masked otp overlay with reveal and copy actions
- proton-style dark ui for the status window and notification card
- proton mail session monitoring through embedded webviews
- per-user windows autostart via `hkcu\software\microsoft\windows\currentversion\run`
- linux packages for debian/ubuntu, fedora/rhel, and generic tarball installs
- embedded app icon for the tray and windows executable
- github release automation for windows `.exe` and linux package bundles

## install

### requirements

- windows 10 or newer
- modern linux desktop with gtk3, webkitgtk, and ayatana appindicator runtime packages
- webview2 runtime
- a proton mail account that works with the current web-session approach

### from source

```bash
cargo run
```

### from github releases

- download the latest windows `.exe` from releases
- or download the latest linux `.deb`, `.rpm`, or `.tar.gz` from releases
- run `protoncode.exe`
- on linux packages, install with your distro package manager and launch `protoncode`
- enable `launch on windows sign-in` from the status window if you want autostart

## usage

1. launch `protoncode`
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

when autostart is enabled on windows, protoncode writes a `protoncode` entry to the current-user windows run key and starts with `--autostart`, which forces a hidden-to-tray launch.

## development

```bash
cargo test
cargo check
cargo check --target x86_64-pc-windows-msvc
```

for a local cross-compiled windows release build from linux:

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

## release flow

- ci runs on pushes and pull requests
- release-please manages version bumps and github releases from conventional commits
- release workflows build `protoncode.exe` on windows and `.deb`, `.rpm`, and `.tar.gz` assets on linux

## notes

- the proton integration uses a web-session approach, not proton mail bridge
- that means the monitor is best-effort and may break if proton changes its web ui or session behavior
- session metadata is stored through the platform credential store, while the live web session stays in the webview profile directory

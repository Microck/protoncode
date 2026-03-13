#![cfg(windows)]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use arboard::Clipboard;
use serde::{Deserialize, Serialize};
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::{Window, WindowBuilder};
use time::OffsetDateTime;
use tracing::{error, info, warn};
use tray_icon::TrayIconBuilder;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use wry::http::Request;
use wry::{WebContext, WebView, WebViewBuilder};

use crate::app::AppState;
use crate::autostart;
use crate::config::AppConfig;
use crate::models::{MailSessionState, OtpCandidateEmail, OtpNotification};
use crate::secrets::SecretStore;

const SETTINGS_HTML_TITLE: &str = "protoncode status";
const PROTON_LOGIN_TITLE: &str = "proton mail login";
const OVERLAY_WIDTH: f64 = 360.0;
const OVERLAY_HEIGHT: f64 = 172.0;

pub fn run() -> Result<()> {
    let state = Arc::new(Mutex::new(AppState::load()?));
    reconcile_launch_on_startup(&state)?;
    let secrets = SecretStore::new();
    let launched_from_autostart = autostart::has_autostart_flag(std::env::args_os());

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let windows = Windows::build(&event_loop, proxy.clone(), state.clone())?;
    let tray = AppTray::build()?;

    if let Some(marker) = secrets.load_session_marker()? {
        info!(marker, "restored prior session marker");
        update_session(
            state.clone(),
            &windows.settings,
            MailSessionState::Restoring,
        )?;
    }

    if launched_from_autostart
        || state
            .lock()
            .map_err(|_| anyhow!("app state poisoned"))?
            .config
            .start_minimized_to_tray
    {
        windows.proton_window.set_visible(false);
    } else {
        windows.proton_window.set_visible(true);
    }

    event_loop.run(move |event, elwt, control_flow| {
        *control_flow = ControlFlow::Wait;

        drain_tray_events(&tray, &windows, &state, &secrets, &proxy, control_flow);

        match event {
            Event::NewEvents(StartCause::Init) => {
                if let Ok(state) = lock_state(&state) {
                    let _ = refresh_settings(&windows.settings, &state);
                }
            }
            Event::UserEvent(user_event) => {
                if let Err(error) = handle_user_event(user_event, &windows, &state, &secrets) {
                    error!(?error, "user event handling failed");
                }
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if window_id == windows.proton_window.id() {
                    windows.proton_window.set_visible(false);
                } else if window_id == windows.settings_window.id() {
                    windows.settings_window.set_visible(false);
                } else if window_id == windows.overlay_window.id() {
                    windows.overlay_window.set_visible(false);
                }
            }
            Event::LoopDestroyed => {
                let _ = elwt;
            }
            _ => {}
        }
    });

    #[allow(unreachable_code)]
    Ok(())
}

fn handle_user_event(
    event: UserEvent,
    windows: &Windows,
    state: &Arc<Mutex<AppState>>,
    secrets: &SecretStore,
) -> Result<()> {
    match event {
        UserEvent::ProtonSnapshot(snapshot) => {
            handle_proton_snapshot(snapshot, windows, state, secrets)?;
        }
        UserEvent::OverlayAction(action) => match action.action.as_str() {
            "dismiss" => {
                windows.overlay_window.set_visible(false);
                let mut state = lock_state(state)?;
                state.clear_notification();
            }
            "copy" => {
                let code = {
                    let state = lock_state(state)?;
                    state.latest_notification_code().map(str::to_owned)
                };
                if let Some(code) = code {
                    let mut clipboard = Clipboard::new().context("failed to access clipboard")?;
                    clipboard
                        .set_text(code)
                        .context("failed to copy OTP code")?;
                }
            }
            _ => {}
        },
        UserEvent::SettingsAction(action) => match action.kind.as_str() {
            "save_config" => {
                let mut state = lock_state(state)?;
                if let Some(interval) = action.poll_interval_seconds {
                    state.config.poll_interval_seconds = interval.clamp(5, 30);
                }
                if let Some(duration) = action.notification_duration_seconds {
                    state.config.notification_duration_seconds = duration.clamp(5, 15);
                }
                if let Some(copy_button_enabled) = action.copy_button_enabled {
                    state.config.copy_button_enabled = copy_button_enabled;
                }
                let launch_on_startup = action
                    .launch_on_startup
                    .unwrap_or(state.config.launch_on_startup);
                autostart::sync_launch_on_startup(launch_on_startup)?;
                state.config.launch_on_startup = launch_on_startup;
                state.save_config()?;
                refresh_settings(&windows.settings, &state)?;
            }
            "login_window" => {
                windows.proton_window.set_visible(true);
                windows.proton_window.set_focus();
            }
            "hide_status" => {
                windows.settings_window.set_visible(false);
            }
            _ => {}
        },
        UserEvent::DismissOverlay => {
            windows.overlay_window.set_visible(false);
            let mut state = lock_state(state)?;
            state.clear_notification();
        }
        UserEvent::SetSession(session_state) => {
            update_session(state.clone(), &windows.settings, session_state)?;
        }
    }

    Ok(())
}

fn handle_proton_snapshot(
    snapshot: ProtonSnapshot,
    windows: &Windows,
    state: &Arc<Mutex<AppState>>,
    secrets: &SecretStore,
) -> Result<()> {
    let session_state = infer_session_state(&snapshot);
    update_session(state.clone(), &windows.settings, session_state)?;

    if session_state == MailSessionState::Authenticated {
        secrets.save_session_marker("authenticated")?;
    } else if matches!(
        session_state,
        MailSessionState::Unauthenticated | MailSessionState::Expired
    ) && !windows.proton_window.is_visible()
    {
        windows.proton_window.set_visible(true);
        windows.proton_window.set_focus();
    }

    let mut state_guard = lock_state(state)?;
    if state_guard.session_state == MailSessionState::Paused {
        return Ok(());
    }

    let candidate = OtpCandidateEmail {
        message_id: fingerprint_snapshot(&snapshot),
        sender: None,
        subject: Some(snapshot.title.clone()),
        received_at: OffsetDateTime::now_utc(),
        body_text: snapshot.text,
    };

    if let Some(notification) = state_guard.register_candidate(&candidate) {
        show_overlay(
            &windows.overlay_window,
            &windows.overlay,
            &notification,
            state_guard.config.copy_button_enabled,
        )?;
    }

    refresh_settings(&windows.settings, &state_guard)?;
    Ok(())
}

fn update_session(
    state: Arc<Mutex<AppState>>,
    settings: &WebView,
    session_state: MailSessionState,
) -> Result<()> {
    let mut state = lock_state(&state)?;
    state.set_session_state(session_state);
    refresh_settings(settings, &state)?;
    Ok(())
}

fn drain_tray_events(
    tray: &AppTray,
    windows: &Windows,
    state: &Arc<Mutex<AppState>>,
    secrets: &SecretStore,
    proxy: &EventLoopProxy<UserEvent>,
    control_flow: &mut ControlFlow,
) {
    while let Ok(event) = MenuEvent::receiver().try_recv() {
        let id = event.id;
        if id == tray.open_status.id() {
            windows.settings_window.set_visible(true);
            windows.settings_window.set_focus();
            if let Ok(state) = lock_state(state) {
                let _ = refresh_settings(&windows.settings, &state);
            }
        } else if id == tray.open_login.id() {
            windows.proton_window.set_visible(true);
            windows.proton_window.set_focus();
        } else if id == tray.pause_resume.id() {
            if let Ok(mut state) = lock_state(state) {
                let next = if state.session_state == MailSessionState::Paused {
                    MailSessionState::Authenticated
                } else {
                    MailSessionState::Paused
                };
                state.set_session_state(next);
                let _ = refresh_settings(&windows.settings, &state);
            }
        } else if id == tray.clear_session.id() {
            if let Err(error) = secrets.clear_session_marker() {
                warn!(?error, "failed to clear session marker");
            }
            let _ = proxy.send_event(UserEvent::SetSession(MailSessionState::Unauthenticated));
            windows.proton_window.set_visible(true);
        } else if id == tray.quit.id() {
            *control_flow = ControlFlow::Exit;
        }
    }
}

fn refresh_settings(settings_view: &WebView, state: &impl StateView) -> Result<()> {
    let snapshot = SettingsSnapshot::from_state(state);
    let payload =
        serde_json::to_string(&snapshot).context("failed to serialize settings snapshot")?;
    settings_view
        .evaluate_script(&format!("window.__PROTON2FA_STATUS.render({payload});"))
        .context("failed to refresh settings window")
}

fn reconcile_launch_on_startup(state: &Arc<Mutex<AppState>>) -> Result<()> {
    let autostart_enabled = autostart::is_enabled()?;
    let mut state = lock_state(state)?;
    if state.config.launch_on_startup != autostart_enabled {
        state.config.launch_on_startup = autostart_enabled;
        state.save_config()?;
    }
    Ok(())
}

fn show_overlay(
    window: &Window,
    overlay_view: &WebView,
    notification: &OtpNotification,
    copy_enabled: bool,
) -> Result<()> {
    position_overlay(window)?;
    let payload = serde_json::to_string(&OverlayPayload::from_notification(
        notification,
        copy_enabled,
    ))
    .context("failed to serialize overlay payload")?;
    overlay_view
        .evaluate_script(&format!("window.__PROTON2FA_OVERLAY.show({payload});"))
        .context("failed to render overlay notification")?;
    window.set_visible(true);
    window.set_focus();
    Ok(())
}

fn position_overlay(window: &Window) -> Result<()> {
    let monitor = window
        .current_monitor()
        .context("missing current monitor for overlay")?;
    let scale_factor = monitor.scale_factor();
    let size = monitor.size().to_logical::<f64>(scale_factor);
    let x = size.width - OVERLAY_WIDTH - 24.0;
    let y = size.height - OVERLAY_HEIGHT - 64.0;
    window.set_outer_position(LogicalPosition::new(x.max(0.0), y.max(0.0)));
    Ok(())
}

fn infer_session_state(snapshot: &ProtonSnapshot) -> MailSessionState {
    let url = snapshot.url.as_str();
    let text = snapshot.text.to_lowercase();

    if url.contains("account.proton.me") || text.contains("sign in") || text.contains("log in") {
        MailSessionState::Unauthenticated
    } else if url.contains("mail.proton.me") {
        MailSessionState::Authenticated
    } else {
        MailSessionState::Restoring
    }
}

fn fingerprint_snapshot(snapshot: &ProtonSnapshot) -> String {
    let mut hasher = DefaultHasher::new();
    snapshot.url.hash(&mut hasher);
    snapshot.title.hash(&mut hasher);
    snapshot.text.hash(&mut hasher);
    format!("snapshot-{:016x}", hasher.finish())
}

fn lock_state(state: &Arc<Mutex<AppState>>) -> Result<std::sync::MutexGuard<'_, AppState>> {
    state.lock().map_err(|_| anyhow!("app state poisoned"))
}

trait StateView {
    fn config(&self) -> &AppConfig;
    fn session_state(&self) -> MailSessionState;
    fn current_notification(&self) -> Option<&OtpNotification>;
}

impl StateView for AppState {
    fn config(&self) -> &AppConfig {
        &self.config
    }

    fn session_state(&self) -> MailSessionState {
        self.session_state
    }

    fn current_notification(&self) -> Option<&OtpNotification> {
        self.current_notification.as_ref()
    }
}

impl StateView for std::sync::MutexGuard<'_, AppState> {
    fn config(&self) -> &AppConfig {
        &self.config
    }

    fn session_state(&self) -> MailSessionState {
        self.session_state
    }

    fn current_notification(&self) -> Option<&OtpNotification> {
        self.current_notification.as_ref()
    }
}

struct Windows {
    overlay_window: Window,
    overlay: WebView,
    settings_window: Window,
    settings: WebView,
    proton_window: Window,
    _proton: WebView,
}

impl Windows {
    fn build(
        event_loop: &EventLoop<UserEvent>,
        proxy: EventLoopProxy<UserEvent>,
        state: Arc<Mutex<AppState>>,
    ) -> Result<Self> {
        let config = {
            let state = lock_state(&state)?;
            state.config.clone()
        };

        let overlay_window = WindowBuilder::new()
            .with_title("protoncode overlay")
            .with_visible(false)
            .with_decorations(false)
            .with_always_on_top(true)
            .with_skip_taskbar(true)
            .with_transparent(true)
            .with_inner_size(LogicalSize::new(OVERLAY_WIDTH, OVERLAY_HEIGHT))
            .build(event_loop)
            .context("failed to build overlay window")?;

        let overlay_proxy = proxy.clone();
        let overlay =
            WebViewBuilder::new()
                .with_html(overlay_html())
                .with_ipc_handler(move |payload: Request<String>| {
                    let parsed = serde_json::from_str::<OverlayAction>(payload.body())
                        .unwrap_or_else(|_| OverlayAction {
                            action: "dismiss".to_owned(),
                        });
                    let _ = overlay_proxy.send_event(UserEvent::OverlayAction(parsed));
                })
                .build(&overlay_window)
                .context("failed to build overlay webview")?;

        let settings_window = WindowBuilder::new()
            .with_title(SETTINGS_HTML_TITLE)
            .with_visible(true)
            .with_inner_size(LogicalSize::new(440.0, 520.0))
            .build(event_loop)
            .context("failed to build settings window")?;

        let settings_proxy = proxy.clone();
        let settings = WebViewBuilder::new()
            .with_html(settings_html())
            .with_ipc_handler(move |payload: Request<String>| {
                if let Ok(parsed) = serde_json::from_str::<SettingsAction>(payload.body()) {
                    let _ = settings_proxy.send_event(UserEvent::SettingsAction(parsed));
                }
            })
            .build(&settings_window)
            .context("failed to build settings webview")?;

        let proton_window = WindowBuilder::new()
            .with_title(PROTON_LOGIN_TITLE)
            .with_visible(true)
            .with_inner_size(LogicalSize::new(1120.0, 820.0))
            .build(event_loop)
            .context("failed to build Proton login window")?;

        let proton_proxy = proxy;
        let monitor_script = proton_monitor_script(config.poll_interval_seconds);
        let mut web_context = WebContext::new(Some(config.user_data_dir.clone()));
        let proton = WebViewBuilder::new_with_web_context(&mut web_context)
            .with_url(&config.proton_mail_url)
            .with_initialization_script(&monitor_script)
            .with_ipc_handler(move |payload: Request<String>| {
                match serde_json::from_str::<ProtonIpc>(payload.body()) {
                    Ok(ProtonIpc::Snapshot(snapshot)) => {
                        let _ = proton_proxy.send_event(UserEvent::ProtonSnapshot(snapshot));
                    }
                    Ok(ProtonIpc::DismissOverlay) => {
                        let _ = proton_proxy.send_event(UserEvent::DismissOverlay);
                    }
                    Err(error) => {
                        let _ =
                            proton_proxy.send_event(UserEvent::SetSession(MailSessionState::Error));
                        warn!(?error, "failed to parse Proton ipc payload");
                    }
                }
            })
            .build(&proton_window)
            .context("failed to build Proton webview")?;

        Ok(Self {
            overlay_window,
            overlay,
            settings_window,
            settings,
            proton_window,
            _proton: proton,
        })
    }
}

struct AppTray {
    _icon: tray_icon::TrayIcon,
    open_status: MenuItem,
    open_login: MenuItem,
    pause_resume: MenuItem,
    clear_session: MenuItem,
    quit: MenuItem,
}

impl AppTray {
    fn build() -> Result<Self> {
        let menu = Menu::new();
        let open_status = MenuItem::new("Open status", true, None);
        let open_login = MenuItem::new("Open Proton login", true, None);
        let pause_resume = MenuItem::new("Pause or resume monitoring", true, None);
        let clear_session = MenuItem::new("Clear saved session state", true, None);
        let quit = MenuItem::new("Quit", true, None);

        menu.append(&open_status)
            .context("failed to append status menu item")?;
        menu.append(&open_login)
            .context("failed to append login menu item")?;
        menu.append(&pause_resume)
            .context("failed to append pause menu item")?;
        menu.append(&clear_session)
            .context("failed to append clear-session menu item")?;
        menu.append(&quit)
            .context("failed to append quit menu item")?;

        let icon = app_icon().context("failed to create tray icon")?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("protoncode")
            .with_icon(icon)
            .build()
            .context("failed to build tray icon")?;

        Ok(Self {
            _icon: tray,
            open_status,
            open_login,
            pause_resume,
            clear_session,
            quit,
        })
    }
}

#[derive(Debug)]
enum UserEvent {
    ProtonSnapshot(ProtonSnapshot),
    OverlayAction(OverlayAction),
    SettingsAction(SettingsAction),
    DismissOverlay,
    SetSession(MailSessionState),
}

#[derive(Debug, Clone, Deserialize)]
struct OverlayAction {
    action: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SettingsAction {
    kind: String,
    poll_interval_seconds: Option<u64>,
    notification_duration_seconds: Option<u64>,
    copy_button_enabled: Option<bool>,
    launch_on_startup: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProtonSnapshot {
    url: String,
    title: String,
    text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProtonIpc {
    Snapshot(ProtonSnapshot),
    DismissOverlay,
}

#[derive(Debug, Clone, Serialize)]
struct OverlayPayload {
    source_label: String,
    masked_code: String,
    raw_code: String,
    received_at_label: String,
    duration_ms: u64,
    copy_enabled: bool,
}

impl OverlayPayload {
    fn from_notification(notification: &OtpNotification, copy_enabled: bool) -> Self {
        let received_at_label = notification
            .received_at
            .format(&time::format_description::parse("[hour]:[minute]").expect("valid time format"))
            .unwrap_or_else(|_| "now".to_owned());

        Self {
            source_label: notification.source_label.clone(),
            masked_code: notification.masked_code.clone(),
            raw_code: notification.raw_code.clone(),
            received_at_label,
            duration_ms: ((notification.expires_at - notification.received_at)
                .whole_milliseconds()
                .max(0)) as u64,
            copy_enabled,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SettingsSnapshot {
    session_state: &'static str,
    autostart_status: &'static str,
    poll_interval_seconds: u64,
    notification_duration_seconds: u64,
    copy_button_enabled: bool,
    launch_on_startup: bool,
    last_masked_code: Option<String>,
}

impl SettingsSnapshot {
    fn from_state(state: &impl StateView) -> Self {
        Self {
            session_state: match state.session_state() {
                MailSessionState::Unauthenticated => "Unauthenticated",
                MailSessionState::Restoring => "Restoring session",
                MailSessionState::Authenticated => "Authenticated",
                MailSessionState::Expired => "Expired",
                MailSessionState::Error => "Error",
                MailSessionState::Paused => "Paused",
            },
            autostart_status: if state.config().launch_on_startup {
                "Enabled"
            } else {
                "Disabled"
            },
            poll_interval_seconds: state.config().poll_interval_seconds,
            notification_duration_seconds: state.config().notification_duration_seconds,
            copy_button_enabled: state.config().copy_button_enabled,
            launch_on_startup: state.config().launch_on_startup,
            last_masked_code: state
                .current_notification()
                .map(|notification| notification.masked_code.clone()),
        }
    }
}

fn proton_monitor_script(poll_interval_seconds: u64) -> String {
    format!(
        r#"
(() => {{
  const sendSnapshot = () => {{
    const payload = {{
      kind: "snapshot",
      url: window.location.href,
      title: document.title || "Proton Mail",
      text: (document.body?.innerText || "").slice(0, 12000)
    }};
    window.ipc.postMessage(JSON.stringify(payload));
  }};

  const schedule = Math.max({poll_interval_seconds}, 5) * 1000;
  window.addEventListener("load", () => setTimeout(sendSnapshot, 1500));
  document.addEventListener("visibilitychange", sendSnapshot);
  new MutationObserver(() => {{
    window.clearTimeout(window.__protoncodeMutationTimer);
    window.__protoncodeMutationTimer = window.setTimeout(sendSnapshot, 800);
  }}).observe(document.documentElement, {{ childList: true, subtree: true, characterData: true }});
  window.setInterval(sendSnapshot, schedule);
  sendSnapshot();
}})();
"#
    )
}

fn overlay_html() -> String {
    r#"
<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <style>
      :root {
        color-scheme: dark;
        --bg: #111827;
        --panel: rgba(17, 24, 39, 0.96);
        --panel-border: rgba(255, 255, 255, 0.08);
        --text: #f8fafc;
        --muted: #94a3b8;
        --accent: #6d4aff;
        --accent-soft: rgba(109, 74, 255, 0.16);
      }
      html, body {
        margin: 0;
        width: 100%;
        height: 100%;
        background: transparent;
        overflow: hidden;
        font-family: "Segoe UI", "Inter", sans-serif;
      }
      body {
        display: flex;
        align-items: center;
        justify-content: center;
        padding: 12px;
      }
      .card {
        width: 100%;
        height: 100%;
        box-sizing: border-box;
        padding: 18px;
        border-radius: 22px;
        border: 1px solid var(--panel-border);
        background:
          radial-gradient(circle at top left, rgba(0, 174, 255, 0.18), transparent 35%),
          radial-gradient(circle at bottom right, rgba(109, 74, 255, 0.24), transparent 42%),
          var(--panel);
        color: var(--text);
        box-shadow: 0 22px 60px rgba(15, 23, 42, 0.45);
        display: none;
      }
      .eyebrow {
        font-size: 12px;
        letter-spacing: 0.12em;
        text-transform: uppercase;
        color: var(--muted);
      }
      .source {
        margin-top: 10px;
        font-size: 16px;
        font-weight: 600;
      }
      .code-row {
        margin-top: 14px;
        display: flex;
        align-items: center;
        gap: 8px;
      }
      .code {
        flex: 1;
        padding: 14px 16px;
        border-radius: 16px;
        background: var(--accent-soft);
        font-size: 30px;
        font-weight: 700;
        letter-spacing: 0.18em;
      }
      button {
        border: 0;
        border-radius: 14px;
        padding: 11px 14px;
        background: rgba(255, 255, 255, 0.09);
        color: var(--text);
        font-size: 14px;
        cursor: pointer;
      }
      .meta {
        margin-top: 14px;
        display: flex;
        justify-content: space-between;
        color: var(--muted);
        font-size: 12px;
      }
      .actions {
        margin-top: 14px;
        display: flex;
        justify-content: flex-end;
        gap: 8px;
      }
      .copy {
        background: var(--accent);
      }
    </style>
  </head>
  <body>
    <div class="card" id="card">
      <div class="eyebrow">Proton-style OTP alert</div>
      <div class="source" id="source">Waiting for code</div>
      <div class="code-row">
        <div class="code" id="code">******</div>
        <button id="toggle" type="button" aria-label="Reveal code">Eye</button>
      </div>
      <div class="meta">
        <span>Masked by default</span>
        <span id="received-at">now</span>
      </div>
      <div class="actions">
        <button id="dismiss" type="button">Dismiss</button>
        <button class="copy" id="copy" type="button">Copy</button>
      </div>
    </div>
    <script>
      const state = {
        maskedCode: "******",
        rawCode: "",
        revealed: false,
        hideTimer: null,
      };

      const card = document.getElementById("card");
      const code = document.getElementById("code");
      const source = document.getElementById("source");
      const receivedAt = document.getElementById("received-at");
      const copy = document.getElementById("copy");

      document.getElementById("toggle").addEventListener("click", () => {
        state.revealed = !state.revealed;
        renderCode();
      });

      document.getElementById("dismiss").addEventListener("click", () => {
        window.ipc.postMessage(JSON.stringify({ action: "dismiss" }));
        hide();
      });

      copy.addEventListener("click", () => {
        window.ipc.postMessage(JSON.stringify({ action: "copy" }));
      });

      function renderCode() {
        code.textContent = state.revealed ? state.rawCode : state.maskedCode;
      }

      function hide() {
        card.style.display = "none";
        state.revealed = false;
      }

      window.__PROTON2FA_OVERLAY = {
        show(payload) {
          source.textContent = payload.source_label;
          state.maskedCode = payload.masked_code;
          state.rawCode = payload.raw_code;
          receivedAt.textContent = payload.received_at_label;
          copy.style.display = payload.copy_enabled ? "inline-flex" : "none";
          state.revealed = false;
          renderCode();
          card.style.display = "block";
          window.clearTimeout(state.hideTimer);
          state.hideTimer = window.setTimeout(() => {
            window.ipc.postMessage(JSON.stringify({ action: "dismiss" }));
            hide();
          }, payload.duration_ms);
        }
      };
    </script>
  </body>
</html>
"#
    .to_owned()
}

fn settings_html() -> String {
    r#"
<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <style>
      :root {
        color-scheme: dark;
        --bg: #0f172a;
        --panel: #111827;
        --text: #f8fafc;
        --muted: #94a3b8;
        --accent: #6d4aff;
        --accent-soft: rgba(109, 74, 255, 0.18);
      }
      html, body {
        margin: 0;
        background:
          radial-gradient(circle at top left, rgba(0, 174, 255, 0.14), transparent 30%),
          linear-gradient(160deg, #0b1220 0%, #121a2f 100%);
        color: var(--text);
        font-family: "Segoe UI", "Inter", sans-serif;
      }
      body { padding: 28px; }
      .card {
        max-width: 560px;
        margin: 0 auto;
        padding: 24px;
        border-radius: 22px;
        background: rgba(17, 24, 39, 0.92);
        box-shadow: 0 24px 70px rgba(15, 23, 42, 0.45);
      }
      h1 { margin: 0 0 6px; font-size: 28px; }
      p { color: var(--muted); margin-top: 0; }
      .status {
        margin: 18px 0 24px;
        padding: 16px;
        border-radius: 18px;
        background: var(--accent-soft);
      }
      label {
        display: block;
        margin: 14px 0 6px;
        color: var(--muted);
        font-size: 13px;
      }
      input[type="number"] {
        width: 100%;
        box-sizing: border-box;
        padding: 12px 14px;
        border-radius: 14px;
        border: 1px solid rgba(255,255,255,0.08);
        background: rgba(15, 23, 42, 0.75);
        color: var(--text);
      }
      .toggle-row {
        margin-top: 12px;
        display: flex;
        align-items: center;
        gap: 12px;
      }
      .actions {
        margin-top: 24px;
        display: flex;
        gap: 10px;
      }
      button {
        border: 0;
        border-radius: 14px;
        padding: 12px 16px;
        background: rgba(255,255,255,0.08);
        color: var(--text);
        cursor: pointer;
      }
      .primary { background: var(--accent); }
    </style>
  </head>
  <body>
    <div class="card">
      <h1>protoncode</h1>
      <p>background watcher for masked otp overlays.</p>
      <div class="status">
        <div><strong>Session:</strong> <span id="session-state">Restoring session</span></div>
        <div><strong>Autostart:</strong> <span id="autostart-status">Disabled</span></div>
        <div><strong>Last code:</strong> <span id="last-code">None yet</span></div>
      </div>

      <label for="poll-interval">Polling interval in seconds</label>
      <input id="poll-interval" type="number" min="5" max="30" />

      <label for="notification-duration">Notification duration in seconds</label>
      <input id="notification-duration" type="number" min="5" max="15" />

      <div class="toggle-row">
        <input id="copy-button-enabled" type="checkbox" />
        <label for="copy-button-enabled" style="margin: 0; color: var(--text);">Enable copy button on overlays</label>
      </div>

      <div class="toggle-row">
        <input id="launch-on-startup" type="checkbox" />
        <label for="launch-on-startup" style="margin: 0; color: var(--text);">Launch on Windows sign-in</label>
      </div>

      <div class="actions">
        <button class="primary" id="save">Save settings</button>
        <button id="login">Open Proton login</button>
        <button id="close">Hide</button>
      </div>
    </div>
    <script>
      const elements = {
        sessionState: document.getElementById("session-state"),
        autostartStatus: document.getElementById("autostart-status"),
        lastCode: document.getElementById("last-code"),
        pollInterval: document.getElementById("poll-interval"),
        notificationDuration: document.getElementById("notification-duration"),
        copyEnabled: document.getElementById("copy-button-enabled"),
        launchOnStartup: document.getElementById("launch-on-startup")
      };

      document.getElementById("save").addEventListener("click", () => {
        window.ipc.postMessage(JSON.stringify({
          kind: "save_config",
          poll_interval_seconds: Number(elements.pollInterval.value),
          notification_duration_seconds: Number(elements.notificationDuration.value),
          copy_button_enabled: elements.copyEnabled.checked,
          launch_on_startup: elements.launchOnStartup.checked
        }));
      });

      document.getElementById("login").addEventListener("click", () => {
        window.ipc.postMessage(JSON.stringify({ kind: "login_window" }));
      });

      document.getElementById("close").addEventListener("click", () => {
        window.ipc.postMessage(JSON.stringify({ kind: "hide_status" }));
      });

      window.__PROTON2FA_STATUS = {
        render(payload) {
          elements.sessionState.textContent = payload.session_state;
          elements.autostartStatus.textContent = payload.autostart_status;
          elements.lastCode.textContent = payload.last_masked_code || "None yet";
          elements.pollInterval.value = payload.poll_interval_seconds;
          elements.notificationDuration.value = payload.notification_duration_seconds;
          elements.copyEnabled.checked = payload.copy_button_enabled;
          elements.launchOnStartup.checked = payload.launch_on_startup;
        }
      };
    </script>
  </body>
</html>
"#
    .to_owned()
}

fn app_icon() -> Result<tray_icon::Icon> {
    let icon = image::load_from_memory(include_bytes!("../assets/protoncode-icon.png"))
        .expect("embedded app icon must be a valid png")
        .into_rgba8();
    let (width, height) = icon.dimensions();
    tray_icon::Icon::from_rgba(icon.into_raw(), width, height)
        .context("failed to build icon from embedded png")
}

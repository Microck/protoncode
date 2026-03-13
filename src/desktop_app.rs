use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use arboard::Clipboard;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::window::{Icon as TaoIcon, Window, WindowBuilder};
use time::OffsetDateTime;
use tracing::{error, info, warn};
use tray_icon::TrayIconBuilder;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use wry::http::header::CONTENT_TYPE;
use wry::http::{Request, Response, StatusCode};
use wry::{WebContext, WebView, WebViewBuilder};

#[cfg(target_os = "linux")]
use tao::platform::unix::{EventLoopBuilderExtUnix, WindowBuilderExtUnix, WindowExtUnix};
#[cfg(windows)]
use tao::platform::windows::WindowBuilderExtWindows;
#[cfg(target_os = "linux")]
use wry::WebViewBuilderExtUnix;

use crate::app::AppState;
use crate::autostart;
use crate::config::AppConfig;
use crate::models::{MailSessionState, OtpCandidateEmail, OtpNotification};
use crate::secrets::SecretStore;

const APP_NAME: &str = "ProtonCode";
const SETTINGS_HTML_TITLE: &str = "ProtonCode";
const PROTON_LOGIN_TITLE: &str = "Proton Mail - ProtonCode";
const OVERLAY_WINDOW_TITLE: &str = "ProtonCode Notification";
const OVERLAY_WIDTH: f64 = 420.0;
const OVERLAY_HEIGHT: f64 = 236.0;
const APP_PROTOCOL: &str = "protoncode";
const OVERLAY_PAGE_URL: &str = "protoncode://app/overlay.html";
const SETTINGS_PAGE_URL: &str = "protoncode://app/settings.html";

pub fn run() -> Result<()> {
    let state = Arc::new(Mutex::new(AppState::load()?));
    reconcile_launch_on_startup(&state)?;
    let secrets = SecretStore::new();
    let launched_from_autostart = autostart::has_autostart_flag(std::env::args_os());

    let mut event_loop_builder = EventLoopBuilder::<UserEvent>::with_user_event();
    #[cfg(target_os = "linux")]
    event_loop_builder.with_app_id("dev.micr.protoncode");
    let event_loop = event_loop_builder.build();
    let proxy = event_loop.create_proxy();
    install_menu_event_handler(proxy.clone());

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

        match event {
            Event::NewEvents(StartCause::Init) => {
                if let Ok(state) = lock_state(&state) {
                    let _ = refresh_settings(&windows.settings, &state);
                }
            }
            Event::UserEvent(UserEvent::TrayMenu(menu_id)) => {
                handle_tray_event(
                    &menu_id,
                    &tray,
                    &windows,
                    &state,
                    &secrets,
                    &proxy,
                    control_flow,
                );
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
            "test_notification" => {
                let (notification, copy_enabled) = {
                    let mut state = lock_state(state)?;
                    let notification = OtpNotification::new(
                        "Test Notification".to_owned(),
                        "258630".to_owned(),
                        OffsetDateTime::now_utc(),
                        state.config.notification_duration_seconds,
                    );
                    let copy_enabled = state.config.copy_button_enabled;
                    state.current_notification = Some(notification.clone());
                    refresh_settings(&windows.settings, &state)?;
                    (notification, copy_enabled)
                };

                show_overlay(
                    &windows.overlay_window,
                    &windows.overlay,
                    &notification,
                    copy_enabled,
                )?;
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
        UserEvent::TrayMenu(_) => {}
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

fn handle_tray_event(
    menu_id: &MenuId,
    tray: &AppTray,
    windows: &Windows,
    state: &Arc<Mutex<AppState>>,
    secrets: &SecretStore,
    proxy: &EventLoopProxy<UserEvent>,
    control_flow: &mut ControlFlow,
) {
    if menu_id == tray.open_status.id() {
        windows.settings_window.set_visible(true);
        windows.settings_window.set_focus();
        if let Ok(state) = lock_state(state) {
            let _ = refresh_settings(&windows.settings, &state);
        }
    } else if menu_id == tray.open_login.id() {
        windows.proton_window.set_visible(true);
        windows.proton_window.set_focus();
    } else if menu_id == tray.pause_resume.id() {
        if let Ok(mut state) = lock_state(state) {
            let next = if state.session_state == MailSessionState::Paused {
                MailSessionState::Authenticated
            } else {
                MailSessionState::Paused
            };
            state.set_session_state(next);
            let _ = refresh_settings(&windows.settings, &state);
        }
    } else if menu_id == tray.clear_session.id() {
        if let Err(error) = secrets.clear_session_marker() {
            warn!(?error, "failed to clear session marker");
        }
        let _ = proxy.send_event(UserEvent::SetSession(MailSessionState::Unauthenticated));
        windows.proton_window.set_visible(true);
    } else if menu_id == tray.quit.id() {
        *control_flow = ControlFlow::Exit;
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
        .or_else(|| window.available_monitors().next())
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

fn install_menu_event_handler(proxy: EventLoopProxy<UserEvent>) {
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(UserEvent::TrayMenu(event.id().clone()));
    }));
}

fn build_platform_webview<'a>(
    builder: WebViewBuilder<'a>,
    window: &'a Window,
) -> wry::Result<WebView> {
    #[cfg(target_os = "linux")]
    {
        let vbox = window
            .default_vbox()
            .expect("tao linux windows should provide a default GTK box");
        builder.build_gtk(vbox)
    }

    #[cfg(not(target_os = "linux"))]
    {
        builder.build(window)
    }
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
        let app_window_icon =
            native_window_icon().context("failed to create native window icon")?;

        let overlay_window = WindowBuilder::new()
            .with_title(OVERLAY_WINDOW_TITLE)
            .with_visible(false)
            .with_decorations(false)
            .with_always_on_top(true)
            .with_skip_taskbar(true)
            .with_transparent(true)
            .with_window_icon(Some(app_window_icon.clone()))
            .with_inner_size(LogicalSize::new(OVERLAY_WIDTH, OVERLAY_HEIGHT))
            .build(event_loop)
            .context("failed to build overlay window")?;

        let overlay_proxy = proxy.clone();
        let overlay = build_platform_webview(
            WebViewBuilder::new()
                .with_transparent(true)
                .with_custom_protocol(APP_PROTOCOL.into(), |_webview_id, request| {
                    app_protocol_response(request)
                })
                .with_url(OVERLAY_PAGE_URL)
                .with_ipc_handler(move |payload: Request<String>| {
                    let parsed = serde_json::from_str::<OverlayAction>(payload.body())
                        .unwrap_or_else(|_| OverlayAction {
                            action: "dismiss".to_owned(),
                        });
                    let _ = overlay_proxy.send_event(UserEvent::OverlayAction(parsed));
                }),
            &overlay_window,
        )
        .context("failed to build overlay webview")?;

        let settings_window = WindowBuilder::new()
            .with_title(SETTINGS_HTML_TITLE)
            .with_visible(true)
            .with_window_icon(Some(app_window_icon.clone()))
            .with_inner_size(LogicalSize::new(540.0, 640.0))
            .build(event_loop)
            .context("failed to build settings window")?;

        let settings_proxy = proxy.clone();
        let settings = build_platform_webview(
            WebViewBuilder::new()
                .with_custom_protocol(APP_PROTOCOL.into(), |_webview_id, request| {
                    app_protocol_response(request)
                })
                .with_url(SETTINGS_PAGE_URL)
                .with_ipc_handler(move |payload: Request<String>| {
                    if let Ok(parsed) = serde_json::from_str::<SettingsAction>(payload.body()) {
                        let _ = settings_proxy.send_event(UserEvent::SettingsAction(parsed));
                    }
                }),
            &settings_window,
        )
        .context("failed to build settings webview")?;

        let proton_window = WindowBuilder::new()
            .with_title(PROTON_LOGIN_TITLE)
            .with_visible(true)
            .with_window_icon(Some(app_window_icon))
            .with_inner_size(LogicalSize::new(1120.0, 820.0))
            .build(event_loop)
            .context("failed to build Proton login window")?;

        let proton_proxy = proxy;
        let monitor_script = proton_monitor_script(config.poll_interval_seconds);
        let mut web_context = WebContext::new(Some(config.user_data_dir.clone()));
        let proton = build_platform_webview(
            WebViewBuilder::new_with_web_context(&mut web_context)
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
                            let _ = proton_proxy
                                .send_event(UserEvent::SetSession(MailSessionState::Error));
                            warn!(?error, "failed to parse Proton ipc payload");
                        }
                    }
                }),
            &proton_window,
        )
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
        let open_status = MenuItem::new("Open ProtonCode", true, None);
        let open_login = MenuItem::new("Open Proton Mail", true, None);
        let pause_resume = MenuItem::new("Pause or Resume Monitoring", true, None);
        let clear_session = MenuItem::new("Clear Saved Session", true, None);
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
            .with_tooltip(APP_NAME)
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
    TrayMenu(MenuId),
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
                MailSessionState::Unauthenticated => "Sign-in required",
                MailSessionState::Restoring => "Restoring session",
                MailSessionState::Authenticated => "Monitoring active",
                MailSessionState::Expired => "Session expired",
                MailSessionState::Error => "Attention needed",
                MailSessionState::Paused => "Monitoring paused",
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
    let app_icon_url = app_icon_data_url();
    let mut html = String::from(
        r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <style>
"#,
    );
    html.push_str(embedded_font_face_css());
    html.push_str(
        r#"
      :root {
        color-scheme: dark;
        --bg: #081221;
        --panel: rgba(10, 21, 41, 0.98);
        --panel-strong: rgba(12, 24, 46, 0.98);
        --panel-border: rgba(129, 154, 196, 0.2);
        --panel-outline: rgba(255, 255, 255, 0.05);
        --text: #f8fbff;
        --muted: #9aaac5;
        --accent-strong: #6d4aff;
        --accent-soft: rgba(109, 74, 255, 0.12);
        --shadow: 0 22px 44px rgba(2, 8, 20, 0.42);
        --font: "Arizona Sans Local", "Ubuntu Local", "Segoe UI", sans-serif;
        --font-display: "Arizona Flare Local", "Arizona Sans Local", "Ubuntu Local", "Segoe UI", sans-serif;
      }
      * {
        box-sizing: border-box;
      }
      html, body {
        margin: 0;
        width: 100%;
        height: 100%;
        background: transparent;
        overflow: hidden;
        font-family: var(--font);
      }
      body {
        padding: 16px;
      }
      .shell {
        width: 100%;
        height: 100%;
        display: flex;
        align-items: stretch;
        justify-content: stretch;
      }
      .card {
        position: relative;
        width: 100%;
        min-height: 100%;
        padding: 18px 18px 16px;
        border-radius: 24px;
        border: 1px solid var(--panel-border);
        background: var(--panel);
        color: var(--text);
        box-shadow: var(--shadow);
        overflow: hidden;
        display: none;
      }
      .card::before {
        content: "";
        position: absolute;
        inset: 1px;
        border-radius: 23px;
        border: 1px solid var(--panel-outline);
        pointer-events: none;
      }
      .brand-row {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 12px;
      }
      .brand {
        display: flex;
        align-items: center;
        gap: 12px;
      }
      .brand-mark {
        width: 20px;
        height: 20px;
        border-radius: 7px;
        display: block;
      }
      .brand-label {
        font-size: 11px;
        letter-spacing: 0.14em;
        text-transform: uppercase;
        color: var(--muted);
      }
      .time-pill {
        padding: 6px 10px;
        border-radius: 999px;
        border: 1px solid rgba(129, 154, 196, 0.18);
        background: rgba(255, 255, 255, 0.04);
        color: var(--muted);
        font-size: 12px;
      }
      .headline {
        margin-top: 14px;
        font-size: 14px;
        font-family: var(--font);
        font-weight: 450;
        color: #cbd8ec;
      }
      .source {
        margin-top: 6px;
        font-size: 18px;
        font-weight: 500;
        line-height: 1.35;
      }
      .code-row {
        margin-top: 16px;
        display: flex;
        align-items: center;
        gap: 10px;
      }
      .code {
        flex: 1;
        padding: 14px 16px;
        border-radius: 18px;
        border: 1px solid rgba(109, 74, 255, 0.22);
        background: var(--panel-strong);
        font-size: 30px;
        font-weight: 500;
        letter-spacing: 0.2em;
        text-align: center;
      }
      button {
        appearance: none;
        border: 0;
        border-radius: 16px;
        padding: 12px 14px;
        min-width: 88px;
        background: rgba(255, 255, 255, 0.07);
        color: var(--text);
        font-family: var(--font);
        font-size: 14px;
        font-weight: 500;
        cursor: pointer;
      }
      button:hover {
        background: rgba(255, 255, 255, 0.11);
      }
      .toggle {
        min-width: 92px;
      }
      .meta {
        margin-top: 12px;
        display: flex;
        justify-content: space-between;
        gap: 12px;
        color: var(--muted);
        font-size: 12px;
      }
      .actions {
        margin-top: 16px;
        display: flex;
        justify-content: flex-end;
        gap: 10px;
      }
      .ghost {
        background: rgba(255, 255, 255, 0.06);
      }
      .primary {
        background: var(--accent-strong);
        border: 1px solid rgba(134, 112, 255, 0.32);
      }
    </style>
  </head>
  <body>
    <div class="shell">
      <div class="card" id="card">
        <div class="brand-row">
          <div class="brand">
            <img class="brand-mark" src="" alt="ProtonCode icon" id="brand-mark" />
            <div class="brand-label">ProtonCode Notification</div>
          </div>
          <div class="time-pill" id="received-at">Now</div>
        </div>
        <div class="headline">New one-time passcode detected</div>
        <div class="source" id="source">Waiting for the next code</div>
        <div class="code-row">
          <div class="code" id="code">******</div>
          <button class="toggle" id="toggle" type="button">Reveal</button>
        </div>
        <div class="meta">
          <span>Codes stay masked until you reveal them.</span>
          <span>Copied only on request.</span>
        </div>
        <div class="actions">
          <button class="ghost" id="dismiss" type="button">Dismiss</button>
          <button class="primary" id="copy" type="button">Copy Code</button>
        </div>
      </div>
    </div>
    <script>
      const state = {
        maskedCode: "******",
        rawCode: "",
        revealed: false,
        hideTimer: null,
      };

      document.getElementById("brand-mark").src = "__APP_ICON__";

      const card = document.getElementById("card");
      const code = document.getElementById("code");
      const source = document.getElementById("source");
      const receivedAt = document.getElementById("received-at");
      const copy = document.getElementById("copy");
      const toggle = document.getElementById("toggle");

      toggle.addEventListener("click", () => {
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
        const isRevealed = state.revealed;
        code.textContent = isRevealed ? state.rawCode : state.maskedCode;
        toggle.textContent = isRevealed ? "Hide" : "Reveal";
      }

      function hide() {
        card.style.display = "none";
        state.revealed = false;
        renderCode();
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
"#,
    );

    html.replace("__APP_ICON__", &app_icon_url)
}

fn settings_html() -> String {
    let app_icon_url = app_icon_data_url();
    let mut html = String::from(
        r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <style>
"#,
    );
    html.push_str(embedded_font_face_css());
    html.push_str(
        r#"
      :root {
        color-scheme: dark;
        --bg: #081221;
        --panel: rgba(10, 21, 41, 0.98);
        --panel-border: rgba(129, 154, 196, 0.18);
        --panel-outline: rgba(255, 255, 255, 0.05);
        --surface: rgba(12, 24, 46, 0.88);
        --surface-strong: rgba(14, 28, 52, 0.96);
        --text: #f8fbff;
        --muted: #9aaac5;
        --accent-strong: #6d4aff;
        --field-border: rgba(129, 154, 196, 0.18);
        --font: "Arizona Sans Local", "Ubuntu Local", "Segoe UI", sans-serif;
        --font-display: "Arizona Flare Local", "Arizona Sans Local", "Ubuntu Local", "Segoe UI", sans-serif;
      }
      * {
        box-sizing: border-box;
      }
      html, body {
        margin: 0;
        min-height: 100%;
        color: var(--text);
        font-family: var(--font);
        background: var(--bg);
      }
      body {
        padding: 30px;
      }
      .app {
        max-width: 620px;
        margin: 0 auto;
        padding: 28px;
        border-radius: 28px;
        border: 1px solid var(--panel-border);
        background: var(--panel);
        box-shadow: 0 28px 80px rgba(2, 10, 25, 0.42);
        position: relative;
        overflow: hidden;
      }
      .app::before {
        content: "";
        position: absolute;
        inset: 1px;
        border-radius: 27px;
        border: 1px solid var(--panel-outline);
        pointer-events: none;
      }
      .hero {
        display: flex;
        align-items: flex-start;
        justify-content: space-between;
        gap: 18px;
      }
      .brand {
        display: flex;
        align-items: center;
        gap: 14px;
      }
      .brand-mark {
        width: 44px;
        height: 44px;
        border-radius: 14px;
        display: block;
      }
      .eyebrow {
        margin: 0 0 6px;
        font-size: 11px;
        letter-spacing: 0.16em;
        text-transform: uppercase;
        color: var(--muted);
      }
      h1 {
        margin: 0;
        font-size: 32px;
        font-family: var(--font-display);
        font-weight: 450;
        letter-spacing: -0.02em;
      }
      .lede {
        margin: 10px 0 0;
        max-width: 420px;
        color: #c8d5e7;
        line-height: 1.55;
      }
      .hero-badge {
        padding: 10px 14px;
        border-radius: 16px;
        background: rgba(255, 255, 255, 0.04);
        border: 1px solid rgba(129, 154, 196, 0.14);
        color: #c8d5e7;
        font-size: 12px;
        text-align: right;
      }
      .status-grid {
        margin-top: 24px;
        display: grid;
        grid-template-columns: repeat(3, minmax(0, 1fr));
        gap: 12px;
      }
      .status-card {
        padding: 16px;
        border-radius: 20px;
        background: var(--surface);
        border: 1px solid rgba(129, 154, 196, 0.14);
      }
      .status-card span {
        display: block;
      }
      .status-label {
        margin-bottom: 8px;
        color: var(--muted);
        font-size: 12px;
        letter-spacing: 0.08em;
        text-transform: uppercase;
      }
      .status-value {
        color: var(--text);
        font-size: 15px;
        line-height: 1.45;
      }
      .section {
        margin-top: 26px;
        padding: 20px;
        border-radius: 22px;
        background: var(--surface-strong);
        border: 1px solid rgba(129, 154, 196, 0.14);
      }
      .section-title {
        margin: 0 0 6px;
        font-size: 18px;
        font-family: var(--font);
        font-weight: 500;
      }
      .section-copy {
        margin: 0 0 18px;
        color: var(--muted);
        line-height: 1.5;
      }
      .field-grid {
        display: grid;
        grid-template-columns: repeat(2, minmax(0, 1fr));
        gap: 14px;
      }
      label.field-label {
        display: block;
        margin: 0 0 8px;
        color: #d7e3f8;
        font-size: 13px;
      }
      .field-hint {
        display: block;
        margin-top: 6px;
        color: var(--muted);
        font-size: 12px;
      }
      input[type="number"] {
        width: 100%;
        padding: 13px 14px;
        border-radius: 16px;
        border: 1px solid var(--field-border);
        background: rgba(6, 18, 38, 0.88);
        color: var(--text);
        font-family: var(--font);
        font-size: 15px;
      }
      .toggle-list {
        margin-top: 18px;
        display: grid;
        gap: 12px;
      }
      .toggle-row {
        display: flex;
        align-items: flex-start;
        gap: 12px;
        padding: 14px 16px;
        border-radius: 18px;
        background: rgba(255, 255, 255, 0.04);
        border: 1px solid rgba(143, 189, 255, 0.08);
      }
      .toggle-row input {
        margin: 3px 0 0;
        inline-size: 16px;
        block-size: 16px;
        accent-color: var(--accent-strong);
      }
      .toggle-copy {
        display: block;
        color: var(--muted);
        font-size: 12px;
        line-height: 1.5;
      }
      .actions {
        margin-top: 24px;
        display: flex;
        flex-wrap: wrap;
        gap: 10px;
      }
      button {
        appearance: none;
        border: 0;
        border-radius: 16px;
        padding: 13px 18px;
        background: rgba(255, 255, 255, 0.08);
        color: var(--text);
        font-family: var(--font);
        font-size: 14px;
        font-weight: 500;
        cursor: pointer;
      }
      button:hover {
        background: rgba(255, 255, 255, 0.12);
      }
      .primary {
        background: var(--accent-strong);
        border: 1px solid rgba(134, 112, 255, 0.32);
      }
      @media (max-width: 640px) {
        body {
          padding: 18px;
        }
        .app {
          padding: 22px;
        }
        .hero,
        .status-grid,
        .field-grid {
          grid-template-columns: 1fr;
          display: grid;
        }
        .hero {
          gap: 16px;
        }
        .hero-badge {
          text-align: left;
        }
      }
    </style>
  </head>
  <body>
    <div class="app">
      <div class="hero">
        <div>
          <div class="brand">
            <img class="brand-mark" src="" alt="ProtonCode icon" id="brand-mark" />
            <div>
              <p class="eyebrow">Desktop Control Center</p>
              <h1>ProtonCode</h1>
            </div>
          </div>
          <p class="lede">Monitor Proton Mail for one-time passcodes with a calmer, polished desktop experience. Notifications stay masked until you choose to reveal or copy them.</p>
        </div>
        <div class="hero-badge">Secure desktop notifications<br />for Proton Mail sign-in codes</div>
      </div>

      <div class="status-grid">
        <div class="status-card">
          <span class="status-label">Session</span>
          <span class="status-value" id="session-state">Restoring session</span>
        </div>
        <div class="status-card">
          <span class="status-label">Launch at Sign-In</span>
          <span class="status-value" id="autostart-status">Disabled</span>
        </div>
        <div class="status-card">
          <span class="status-label">Last Masked Code</span>
          <span class="status-value" id="last-code">No code received yet</span>
        </div>
      </div>

      <div class="section">
        <h2 class="section-title">Monitoring Preferences</h2>
        <p class="section-copy">Adjust how frequently ProtonCode checks for updates and how long notification cards remain visible.</p>

        <div class="field-grid">
          <div>
            <label class="field-label" for="poll-interval">Monitoring Interval</label>
            <input id="poll-interval" type="number" min="5" max="30" />
            <span class="field-hint">Accepted range: 5 to 30 seconds.</span>
          </div>
          <div>
            <label class="field-label" for="notification-duration">Notification Duration</label>
            <input id="notification-duration" type="number" min="5" max="15" />
            <span class="field-hint">Accepted range: 5 to 15 seconds.</span>
          </div>
        </div>

        <div class="toggle-list">
          <label class="toggle-row" for="copy-button-enabled">
            <input id="copy-button-enabled" type="checkbox" />
            <span>
              <strong>Allow Copy from Notifications</strong>
              <span class="toggle-copy">Show the copy action on notification cards when you want a faster clipboard workflow.</span>
            </span>
          </label>

          <label class="toggle-row" for="launch-on-startup">
            <input id="launch-on-startup" type="checkbox" />
            <span>
              <strong>Launch with Windows</strong>
              <span class="toggle-copy">Start ProtonCode automatically after Windows sign-in and keep it ready in the tray.</span>
            </span>
          </label>
        </div>

        <div class="actions">
          <button class="primary" id="save">Save Changes</button>
          <button id="test-notification">Test Notification</button>
          <button id="login">Open Proton Mail</button>
          <button id="close">Hide Window</button>
        </div>
      </div>
    </div>
    <script>
      document.getElementById("brand-mark").src = "__APP_ICON__";

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

      document.getElementById("test-notification").addEventListener("click", () => {
        window.ipc.postMessage(JSON.stringify({ kind: "test_notification" }));
      });

      document.getElementById("close").addEventListener("click", () => {
        window.ipc.postMessage(JSON.stringify({ kind: "hide_status" }));
      });

      window.__PROTON2FA_STATUS = {
        render(payload) {
          elements.sessionState.textContent = payload.session_state;
          elements.autostartStatus.textContent = payload.autostart_status;
          elements.lastCode.textContent = payload.last_masked_code || "No code received yet";
          elements.pollInterval.value = payload.poll_interval_seconds;
          elements.notificationDuration.value = payload.notification_duration_seconds;
          elements.copyEnabled.checked = payload.copy_button_enabled;
          elements.launchOnStartup.checked = payload.launch_on_startup;
        }
      };
    </script>
  </body>
</html>
"#,
    );

    html.replace("__APP_ICON__", &app_icon_url)
}

fn embedded_font_face_css() -> &'static str {
    r#"
      @font-face {
        font-family: "Arizona Sans Local";
        src: url("protoncode://app/fonts/arizona-sans.ttf") format("truetype");
        font-style: normal;
        font-weight: 100 700;
        font-display: swap;
      }
      @font-face {
        font-family: "Arizona Flare Local";
        src: url("protoncode://app/fonts/arizona-flare.ttf") format("truetype");
        font-style: normal;
        font-weight: 100 700;
        font-display: swap;
      }
      @font-face {
        font-family: "Ubuntu Local";
        src: url("protoncode://app/fonts/ubuntu-r.ttf") format("truetype");
        font-style: normal;
        font-weight: 400;
      }
      @font-face {
        font-family: "Ubuntu Local";
        src: url("protoncode://app/fonts/ubuntu-m.ttf") format("truetype");
        font-style: normal;
        font-weight: 500 700;
      }
"#
}

fn app_icon_data_url() -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("../assets/protoncode-icon.png"))
    )
}

fn app_protocol_response(request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let path = request.uri().path();

    match path {
        "/overlay.html" => html_response(overlay_html()),
        "/settings.html" => html_response(settings_html()),
        "/fonts/arizona-sans.ttf" => asset_response(
            "font/ttf",
            include_bytes!("../assets/ABCArizonaSansVariable-Trial.ttf"),
        ),
        "/fonts/arizona-flare.ttf" => asset_response(
            "font/ttf",
            include_bytes!("../assets/ABCArizonaFlareVariable-Trial.ttf"),
        ),
        "/fonts/ubuntu-r.ttf" => {
            asset_response("font/ttf", include_bytes!("../assets/ubuntu-r.ttf"))
        }
        "/fonts/ubuntu-m.ttf" => {
            asset_response("font/ttf", include_bytes!("../assets/ubuntu-m.ttf"))
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Cow::Borrowed(&b"Not Found"[..]))
            .expect("valid 404 response"),
    }
}

fn html_response(html: String) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .header(CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Cow::Owned(html.into_bytes()))
        .expect("valid html response")
}

fn asset_response(
    content_type: &'static str,
    bytes: &'static [u8],
) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .header(CONTENT_TYPE, content_type)
        .body(Cow::Borrowed(bytes))
        .expect("valid asset response")
}

fn app_icon() -> Result<tray_icon::Icon> {
    let icon = image::load_from_memory(include_bytes!("../assets/protoncode-icon.png"))
        .expect("embedded app icon must be a valid png")
        .into_rgba8();
    let (width, height) = icon.dimensions();
    tray_icon::Icon::from_rgba(icon.into_raw(), width, height)
        .context("failed to build icon from embedded png")
}

fn native_window_icon() -> Result<TaoIcon> {
    let icon = image::load_from_memory(include_bytes!("../assets/protoncode-icon.png"))
        .expect("embedded app icon must be a valid png")
        .into_rgba8();
    let (width, height) = icon.dimensions();
    TaoIcon::from_rgba(icon.into_raw(), width, height).context("failed to build native window icon")
}

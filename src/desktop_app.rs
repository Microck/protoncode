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
const OVERLAY_HEIGHT: f64 = 222.0;
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
                let poll_interval = state.config.poll_interval_seconds;
                let notification_duration = state.config.notification_duration_seconds;
                let copy_enabled = state.config.copy_button_enabled;
                let launch_on_startup = state.config.launch_on_startup;
                state.push_debug_log(format!(
                    "Config saved: interval={}s, duration={}s, copy={}, autostart={}",
                    poll_interval, notification_duration, copy_enabled, launch_on_startup
                ));
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
                    state.push_debug_log("Debug notification triggered");
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

    let was_seen = state_guard.has_seen_message(&candidate.message_id);
    if let Some(notification) = state_guard.register_candidate(&candidate) {
        state_guard.push_debug_log(format!(
            "OTP matched from {} -> {}",
            notification.source_label, notification.masked_code
        ));
        show_overlay(
            &windows.overlay_window,
            &windows.overlay,
            &notification,
            state_guard.config.copy_button_enabled,
        )?;
    } else if !was_seen && snapshot_has_otp_signal(&candidate.body_text) {
        state_guard.push_debug_log(format!(
            "Snapshot captured but no OTP matched: {}",
            truncate_debug_title(&snapshot.title)
        ));
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
    if state.set_session_state(session_state) {
        state.push_debug_log(format!("Session -> {}", session_state_label(session_state)));
    }
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

    if url.contains("mail.proton.me") {
        MailSessionState::Authenticated
    } else if url.contains("account.proton.me")
        || url.contains("/login")
        || text.contains("sign in to proton")
        || text.contains("log in to proton")
    {
        MailSessionState::Unauthenticated
    } else {
        MailSessionState::Restoring
    }
}

fn session_state_label(state: MailSessionState) -> &'static str {
    match state {
        MailSessionState::Unauthenticated => "Sign-in required",
        MailSessionState::Restoring => "Restoring session",
        MailSessionState::Authenticated => "Monitoring active",
        MailSessionState::Expired => "Session expired",
        MailSessionState::Error => "Attention needed",
        MailSessionState::Paused => "Monitoring paused",
    }
}

fn snapshot_has_otp_signal(text: &str) -> bool {
    let lowered = text.to_lowercase();
    let has_context = [
        "code",
        "verification",
        "2fa",
        "two-factor",
        "two factor",
        "otp",
        "security",
        "passcode",
    ]
    .iter()
    .any(|term| lowered.contains(term));
    let has_digits = text
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .take(4)
        .count()
        >= 4;
    has_context || has_digits
}

fn truncate_debug_title(title: &str) -> String {
    const LIMIT: usize = 72;
    let trimmed = title.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_owned();
    }

    let shortened: String = trimmed.chars().take(LIMIT - 1).collect();
    format!("{shortened}…")
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
    fn debug_logs(&self) -> Vec<String>;
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

    fn debug_logs(&self) -> Vec<String> {
        AppState::debug_logs(self)
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

    fn debug_logs(&self) -> Vec<String> {
        AppState::debug_logs(&*self)
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
            .with_resizable(false)
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
            .with_resizable(false)
            .with_window_icon(Some(app_window_icon.clone()))
            .with_inner_size(LogicalSize::new(900.0, 720.0))
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
    debug_logs: Vec<String>,
}

impl SettingsSnapshot {
    fn from_state(state: &impl StateView) -> Self {
        Self {
            session_state: session_state_label(state.session_state()),
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
            debug_logs: state.debug_logs(),
        }
    }
}

fn proton_monitor_script(poll_interval_seconds: u64) -> String {
    format!(
        r#"
(() => {{
  const OTP_TERMS = /(code|verification|2fa|two-factor|two factor|one-time|one time|otp|security|passcode|sign in|signin)/i;

  const normalizeBlock = (value) => value
    .replace(/\u00a0/g, " ")
    .replace(/\r/g, "\n")
    .replace(/[ \t]+/g, " ")
    .replace(/\n{{3,}}/g, "\n\n")
    .trim();

  const isVisible = (element) => {{
    if (!element || typeof element.getClientRects !== "function") {{
      return false;
    }}
    return element.getClientRects().length > 0;
  }};

  const collectRelevantText = () => {{
    const selectors = [
      '[data-testid*="message"]',
      '[data-testid*="conversation"]',
      '[data-testid*="item"]',
      '[role="main"] article',
      '[role="main"] section',
      'main article',
      'main section',
      '[role="main"]',
      'main'
    ];

    const blocks = [];
    const seen = new Set();
    const maybeAddBlock = (rawText) => {{
      const normalized = normalizeBlock(rawText || "");
      if (normalized.length < 12 || seen.has(normalized)) {{
        return;
      }}
      if (!OTP_TERMS.test(normalized) && !/\d{{4,8}}/.test(normalized)) {{
        return;
      }}
      seen.add(normalized);
      blocks.push(normalized);
    }};

    for (const selector of selectors) {{
      const nodes = document.querySelectorAll(selector);
      for (const node of nodes) {{
        if (!isVisible(node)) {{
          continue;
        }}
        maybeAddBlock(node.innerText || node.textContent || "");
        if (blocks.length >= 8) {{
          return blocks.join("\n\n");
        }}
      }}
    }}

    const bodyText = normalizeBlock(document.body?.innerText || "");
    if (blocks.length) {{
      return blocks.join("\n\n").slice(0, 40000);
    }}
    return bodyText.slice(0, 40000);
  }};

  const collectSnapshotTitle = (snapshotText) => {{
    const firstUsefulLine = snapshotText
      .split(/\n+/)
      .map((line) => normalizeBlock(line))
      .find((line) => line.length >= 6);
    return firstUsefulLine || document.title || "Proton Mail";
  }};

  const sendSnapshot = () => {{
    const snapshotText = collectRelevantText();
    const payload = {{
      kind: "snapshot",
      url: window.location.href,
      title: collectSnapshotTitle(snapshotText),
      text: snapshotText
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
    let app_version = format!("v{}", env!("CARGO_PKG_VERSION"));
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
        --bg: #181818;
        --panel: rgba(15, 17, 21, 0.98);
        --surface: rgba(255, 255, 255, 0.03);
        --border: #343434;
        --text: #e2e8f0;
        --muted: #64748b;
        --accent: #8b5cf6;
        --font: "Segoe UI", "Ubuntu Local", sans-serif;
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
        padding: 14px;
      }
      .shell {
        width: 100%;
        height: 100%;
        display: flex;
        align-items: stretch;
        justify-content: stretch;
      }
      .card {
        width: 100%;
        min-height: 100%;
        padding: 16px 16px 14px;
        border-radius: 22px;
        border: 1px solid var(--border);
        background: var(--panel);
        color: var(--text);
        display: none;
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
        width: 18px;
        height: 18px;
        border-radius: 6px;
        display: block;
      }
      .brand-label {
        color: #ffffff;
        font-size: 16px;
        font-weight: 400;
        letter-spacing: -0.02em;
      }
      .version {
        color: var(--muted);
        font-size: 10px;
        font-weight: 500;
        letter-spacing: 0.22em;
        text-transform: uppercase;
        border-bottom: 1px solid var(--border);
        padding-bottom: 4px;
      }
      .headline {
        margin-top: 12px;
        color: var(--muted);
        font-size: 13px;
      }
      .source {
        margin-top: 4px;
        color: #ffffff;
        font-size: 16px;
        font-weight: 400;
        line-height: 1.35;
      }
      .code-row {
        margin-top: 14px;
        display: flex;
        align-items: center;
        gap: 10px;
      }
      .code {
        flex: 1;
        padding: 13px 16px;
        border-radius: 16px;
        border: 1px solid var(--border);
        background: var(--surface);
        color: #ffffff;
        font-size: 28px;
        font-weight: 400;
        letter-spacing: 0.2em;
        text-align: center;
      }
      button {
        appearance: none;
        border: 1px solid transparent;
        border-radius: 999px;
        padding: 10px 16px;
        min-width: 88px;
        background: transparent;
        color: var(--muted);
        font-family: var(--font);
        font-size: 12px;
        font-weight: 400;
        cursor: pointer;
      }
      button:hover {
        color: #ffffff;
      }
      .toggle {
        min-width: 84px;
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
        margin-top: 14px;
        display: flex;
        align-items: center;
        gap: 10px;
      }
      .primary {
        padding: 10px 18px;
        border-color: rgba(255, 255, 255, 0.1);
        background: rgba(255, 255, 255, 0.05);
        color: #ffffff;
      }
      .primary:hover {
        background: rgba(255, 255, 255, 0.1);
      }
      .spacer {
        flex: 1;
      }
    </style>
  </head>
  <body>
    <div class="shell">
      <div class="card" id="card">
        <div class="brand-row">
          <div class="brand">
            <img class="brand-mark" src="" alt="ProtonCode icon" id="brand-mark" />
            <div class="brand-label">ProtonCode</div>
          </div>
          <div class="version">__APP_VERSION__</div>
        </div>
        <div class="headline" id="received-at">New notification</div>
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
          <button id="dismiss" type="button">Dismiss</button>
          <div class="spacer"></div>
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
        .replace("__APP_VERSION__", &app_version)
}

fn settings_html() -> String {
    let app_icon_url = app_icon_data_url();
    let app_version = format!("v{}", env!("CARGO_PKG_VERSION"));
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
        --bg: #181818;
        --border: #343434;
        --muted: #64748b;
        --text: #e2e8f0;
        --accent: #8b5cf6;
        --font: "Segoe UI", "Ubuntu Local", sans-serif;
      }
      * {
        box-sizing: border-box;
      }
      html,
      body {
        margin: 0;
        min-height: 100%;
        background: var(--bg);
        color: var(--text);
        font-family: var(--font);
        -webkit-font-smoothing: antialiased;
      }
      body {
        min-height: 100vh;
        display: flex;
        align-items: center;
        justify-content: center;
        padding: 48px;
      }
      ::selection {
        background: rgba(139, 92, 246, 0.3);
      }
      [hidden] {
        display: none !important;
      }
      main {
        width: 100%;
        max-width: 760px;
        display: flex;
        flex-direction: column;
        gap: 72px;
      }
      header {
        display: flex;
        align-items: flex-end;
        justify-content: space-between;
        gap: 24px;
      }
      h1 {
        margin: 0;
        display: flex;
        align-items: center;
        gap: 12px;
        color: #ffffff;
        font-size: 24px;
        font-weight: 400;
        letter-spacing: -0.02em;
      }
      .brand-mark {
        width: 22px;
        height: 22px;
        display: block;
        border-radius: 7px;
      }
      .session-state {
        margin: 6px 0 0;
        color: var(--muted);
        font-size: 14px;
      }
      .version {
        color: var(--muted);
        font-size: 10px;
        font-weight: 500;
        letter-spacing: 0.26em;
        text-transform: uppercase;
        border-bottom: 1px solid var(--border);
        padding-bottom: 4px;
        white-space: nowrap;
      }
      .settings-grid {
        display: grid;
        grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
        column-gap: 80px;
        row-gap: 64px;
      }
      .number-group {
        display: flex;
        flex-direction: column;
        gap: 6px;
      }
      .number-label {
        color: var(--muted);
        font-size: 11px;
        letter-spacing: 0.16em;
        text-transform: uppercase;
      }
      .number-input-row {
        display: inline-flex;
        align-items: flex-end;
        gap: 10px;
      }
      input[type="number"] {
        width: 58px;
        padding: 0 0 6px;
        border: 0;
        border-bottom: 1px solid rgba(255, 255, 255, 0.16);
        outline: none;
        background: transparent;
        color: #ffffff;
        font-family: var(--font);
        font-size: 48px;
        font-weight: 300;
        line-height: 1;
        transition: border-color 0.2s ease;
        appearance: textfield;
      }
      input[type="number"]::-webkit-inner-spin-button,
      input[type="number"]::-webkit-outer-spin-button {
        -webkit-appearance: none;
        margin: 0;
      }
      .number-group:hover input[type="number"] {
        border-bottom-color: rgba(255, 255, 255, 0.28);
      }
      input[type="number"]:focus {
        border-bottom-color: var(--accent);
      }
      .number-unit {
        padding-bottom: 10px;
        color: var(--muted);
        font-size: 14px;
        font-weight: 300;
      }
      .right-column {
        display: flex;
        flex-direction: column;
        gap: 32px;
        padding-top: 4px;
      }
      .toggle-row {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 20px;
        cursor: pointer;
        padding: 6px 0;
      }
      .toggle-label {
        color: var(--muted);
        font-size: 14px;
        transition: color 0.2s ease;
      }
      .toggle-row:hover .toggle-label {
        color: #ffffff;
      }
      .toggle-input {
        position: absolute;
        opacity: 0;
        pointer-events: none;
      }
      .toggle-switch {
        width: 32px;
        height: 18px;
        flex-shrink: 0;
        position: relative;
        border-radius: 999px;
        background: #2b2b2b;
        box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.06);
        transition: background 0.2s ease;
      }
      .toggle-switch::after {
        content: "";
        position: absolute;
        top: 3px;
        left: 3px;
        width: 12px;
        height: 12px;
        border-radius: 50%;
        background: #c1cad7;
        transition: all 0.2s ease;
      }
      .toggle-input:checked + .toggle-switch {
        background: var(--accent);
      }
      .toggle-input:checked + .toggle-switch::after {
        left: 17px;
        background: #ffffff;
      }
      .last-code {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 20px;
      }
      .last-code-label {
        color: var(--muted);
        font-size: 14px;
      }
      .last-code-value {
        color: var(--accent);
        font-size: 14px;
        font-weight: 500;
        letter-spacing: 0.22em;
        text-transform: uppercase;
        text-align: right;
      }
      .last-code-value.empty {
        color: var(--muted);
        font-weight: 400;
        letter-spacing: 0;
        text-transform: none;
      }
      footer {
        display: flex;
        align-items: center;
        gap: 16px;
        padding-top: 40px;
        border-top: 1px solid rgba(30, 33, 40, 0.5);
        flex-wrap: wrap;
      }
      button {
        appearance: none;
        border: 0;
        background: transparent;
        color: var(--muted);
        font-family: var(--font);
        font-size: 12px;
        cursor: pointer;
        transition: color 0.2s ease, background 0.2s ease, border-color 0.2s ease;
      }
      button:hover {
        color: #ffffff;
      }
      .primary-action {
        padding: 10px 20px;
        border-radius: 999px;
        border: 1px solid rgba(255, 255, 255, 0.1);
        background: rgba(255, 255, 255, 0.05);
        color: #ffffff;
      }
      .primary-action:hover {
        background: rgba(255, 255, 255, 0.1);
      }
      .spacer {
        flex: 1;
      }
      .hide-button {
        display: inline-flex;
        align-items: center;
        gap: 8px;
      }
      .hide-glyph {
        font-size: 14px;
        line-height: 1;
      }
      .debug-panel {
        display: grid;
        gap: 10px;
        padding-top: 8px;
        border-top: 1px solid rgba(30, 33, 40, 0.5);
      }
      .debug-header {
        color: var(--muted);
        font-size: 11px;
        font-weight: 500;
        letter-spacing: 0.16em;
        text-transform: uppercase;
      }
      .debug-list {
        display: grid;
        gap: 8px;
      }
      .debug-entry {
        padding: 10px 12px;
        border-radius: 14px;
        border: 1px solid var(--border);
        background: rgba(255, 255, 255, 0.03);
        color: #b7c4d8;
        font-family: "SFMono-Regular", "Consolas", "Liberation Mono", monospace;
        font-size: 12px;
        line-height: 1.45;
        white-space: pre-wrap;
        word-break: break-word;
      }
      .debug-empty {
        color: var(--muted);
        font-size: 12px;
      }
      @media (max-width: 720px) {
        body {
          padding: 28px;
        }
        main {
          gap: 56px;
        }
        header,
        .settings-grid,
        footer {
          display: flex;
          flex-direction: column;
          align-items: flex-start;
        }
        .settings-grid {
          gap: 40px;
        }
        .right-column {
          width: 100%;
          gap: 24px;
        }
        .toggle-row,
        .last-code {
          width: 100%;
        }
        .spacer {
          display: none;
        }
      }
    </style>
  </head>
  <body>
    <main>
      <header>
        <div>
          <h1>
            <img class="brand-mark" src="" alt="ProtonCode icon" id="brand-mark" />
            <span>ProtonCode</span>
          </h1>
          <p class="session-state" id="session-state">Monitoring active</p>
        </div>
        <div class="version">__APP_VERSION__</div>
      </header>

      <section class="settings-grid">
        <div class="left-column">
          <div class="number-group">
            <label class="number-label" for="poll-interval">Interval</label>
            <div class="number-input-row">
              <input id="poll-interval" type="number" min="5" max="30" />
              <span class="number-unit">seconds</span>
            </div>
          </div>

          <div class="number-group" style="margin-top: 36px;">
            <label class="number-label" for="notification-duration">Duration</label>
            <div class="number-input-row">
              <input id="notification-duration" type="number" min="5" max="15" />
              <span class="number-unit">seconds</span>
            </div>
          </div>
        </div>

        <div class="right-column">
          <label class="toggle-row" for="copy-button-enabled">
            <span class="toggle-label">Allow Copy</span>
            <span>
              <input class="toggle-input" id="copy-button-enabled" type="checkbox" />
              <span class="toggle-switch"></span>
            </span>
          </label>

          <label class="toggle-row" for="launch-on-startup">
            <span class="toggle-label">Launch at Sign-In</span>
            <span>
              <input class="toggle-input" id="launch-on-startup" type="checkbox" />
              <span class="toggle-switch"></span>
            </span>
          </label>

          <div class="last-code">
            <span class="last-code-label">Last Code</span>
            <span class="last-code-value empty" id="last-code">No code received yet</span>
          </div>
        </div>
      </section>

      <footer>
        <button class="primary-action" id="save" type="button">Save Changes</button>
        <button id="login" type="button">Open Proton Mail</button>
        <div class="spacer"></div>
        <button class="hide-button" id="close" type="button">
          <span class="hide-glyph">−</span>
          <span>Hide Window</span>
        </button>
      </footer>

      <section class="debug-panel" id="debug-panel" hidden>
        <div class="debug-header">Debug Logs</div>
        <div class="debug-list" id="debug-list"></div>
      </section>
    </main>
    <script>
      document.getElementById("brand-mark").src = "__APP_ICON__";

      const debugState = {
        visible: false,
        logs: []
      };

      const elements = {
        sessionState: document.getElementById("session-state"),
        lastCode: document.getElementById("last-code"),
        pollInterval: document.getElementById("poll-interval"),
        notificationDuration: document.getElementById("notification-duration"),
        copyEnabled: document.getElementById("copy-button-enabled"),
        launchOnStartup: document.getElementById("launch-on-startup"),
        debugPanel: document.getElementById("debug-panel"),
        debugList: document.getElementById("debug-list")
      };

      function renderDebugPanel() {
        elements.debugPanel.hidden = !debugState.visible;
        elements.debugList.replaceChildren();

        if (!debugState.visible) {
          return;
        }

        if (!debugState.logs.length) {
          const emptyState = document.createElement("div");
          emptyState.className = "debug-empty";
          emptyState.textContent = "No debug entries yet.";
          elements.debugList.appendChild(emptyState);
          return;
        }

        for (const entry of debugState.logs) {
          const item = document.createElement("div");
          item.className = "debug-entry";
          item.textContent = entry;
          elements.debugList.appendChild(item);
        }
      }

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

      window.addEventListener("keydown", (event) => {
        if (event.defaultPrevented || event.repeat) {
          return;
        }

        if (event.ctrlKey || event.metaKey || event.altKey) {
          return;
        }

        if (event.code === "KeyP") {
          event.preventDefault();
          window.ipc.postMessage(JSON.stringify({ kind: "test_notification" }));
          return;
        }

        if (event.code === "KeyL") {
          event.preventDefault();
          debugState.visible = !debugState.visible;
          renderDebugPanel();
        }
      });

      window.__PROTON2FA_STATUS = {
        render(payload) {
          const hasCode = Boolean(payload.last_masked_code);
          elements.sessionState.textContent = payload.session_state;
          elements.lastCode.textContent = payload.last_masked_code || "No code received yet";
          elements.lastCode.classList.toggle("empty", !hasCode);
          elements.pollInterval.value = payload.poll_interval_seconds;
          elements.notificationDuration.value = payload.notification_duration_seconds;
          elements.copyEnabled.checked = payload.copy_button_enabled;
          elements.launchOnStartup.checked = payload.launch_on_startup;
          debugState.logs = payload.debug_logs || [];
          renderDebugPanel();
        }
      };
    </script>
  </body>
</html>
"#,
    );

    html.replace("__APP_ICON__", &app_icon_url)
        .replace("__APP_VERSION__", &app_version)
}

#[cfg(test)]
mod desktop_tests {
    use super::{MailSessionState, ProtonSnapshot, infer_session_state};

    #[test]
    fn mail_page_is_not_marked_signed_out_when_message_mentions_sign_in() {
        let snapshot = ProtonSnapshot {
            url: "https://mail.proton.me/u/0/inbox".to_owned(),
            title: "Inbox | Proton Mail".to_owned(),
            text: "Your verification code is 123456. Use it to sign in.".to_owned(),
        };

        assert_eq!(
            infer_session_state(&snapshot),
            MailSessionState::Authenticated
        );
    }

    #[test]
    fn account_login_page_is_marked_unauthenticated() {
        let snapshot = ProtonSnapshot {
            url: "https://account.proton.me/login".to_owned(),
            title: "Proton Login".to_owned(),
            text: "Sign in to Proton".to_owned(),
        };

        assert_eq!(
            infer_session_state(&snapshot),
            MailSessionState::Unauthenticated
        );
    }
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

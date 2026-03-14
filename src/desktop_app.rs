use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
const OVERLAY_HEIGHT: f64 = 312.0;
const APP_PROTOCOL: &str = "protoncode";
const OVERLAY_PAGE_URL: &str = "protoncode://app/overlay.html";
const SETTINGS_PAGE_URL: &str = "protoncode://app/settings.html";
#[cfg(windows)]
const WEBVIEW2_DEFAULT_BACKGROUND_COLOR: &str = "WEBVIEW2_DEFAULT_BACKGROUND_COLOR";
const WEBVIEW2_OVERLAY_BACKGROUND: &str = "00000000";
const WEBVIEW2_APP_BACKGROUND: &str = "FF181818";
const STARTUP_PROTON_DEFER_MS: u64 = 650;

pub fn run() -> Result<()> {
    let state = Arc::new(Mutex::new(AppState::load()?));
    reconcile_launch_on_startup(&state)?;
    let secrets = SecretStore::new();
    let launched_from_autostart = autostart::has_autostart_flag(std::env::args_os());
    let show_proton_on_start = !launched_from_autostart
        && !state
            .lock()
            .map_err(|_| anyhow!("app state poisoned"))?
            .config
            .start_minimized_to_tray;

    let mut event_loop_builder = EventLoopBuilder::<UserEvent>::with_user_event();
    #[cfg(target_os = "linux")]
    event_loop_builder.with_app_id("dev.micr.protoncode");
    let event_loop = event_loop_builder.build();
    let proxy = event_loop.create_proxy();
    install_menu_event_handler(proxy.clone());

    let mut windows = Windows::build(&event_loop, proxy.clone(), state.clone())?;
    let tray = AppTray::build()?;

    if let Some(marker) = secrets.load_session_marker()? {
        info!(marker, "restored prior session marker");
        update_session(
            state.clone(),
            &windows.settings,
            MailSessionState::Restoring,
        )?;
    }

    event_loop.run(move |event, elwt, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                if let Ok(state) = lock_state(&state) {
                    let _ = refresh_settings(&windows.settings, &state);
                }
                let deferred_proxy = proxy.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(STARTUP_PROTON_DEFER_MS));
                    let _ = deferred_proxy.send_event(UserEvent::EnsureProtonWebview {
                        show_window: show_proton_on_start,
                    });
                });
            }
            Event::UserEvent(UserEvent::TrayMenu(menu_id)) => {
                handle_tray_event(
                    &menu_id,
                    &tray,
                    &mut windows,
                    &state,
                    &secrets,
                    &proxy,
                    control_flow,
                );
            }
            Event::UserEvent(user_event) => {
                if let Err(error) =
                    handle_user_event(user_event, &mut windows, &state, &secrets, &proxy)
                {
                    push_debug_log_and_refresh(
                        &state,
                        &windows.settings,
                        format!("User event failed: {error:#}"),
                    );
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
    windows: &mut Windows,
    state: &Arc<Mutex<AppState>>,
    secrets: &SecretStore,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<()> {
    match event {
        UserEvent::ProtonSnapshot(snapshot) => {
            handle_proton_snapshot(snapshot, windows, state, secrets, proxy)?;
        }
        UserEvent::OverlayAction(action) => match action.action.as_str() {
            "dismiss" => {
                windows.overlay_window.set_visible(false);
                push_debug_log_and_refresh(state, &windows.settings, "Overlay dismissed");
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
                    push_debug_log_and_refresh(state, &windows.settings, "Overlay code copied");
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

                show_overlay(windows, proxy, state, &notification, copy_enabled)?;
            }
            "login_window" => {
                windows.ensure_proton_ready(proxy, state)?;
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
            push_debug_log_and_refresh(state, &windows.settings, "Overlay auto-dismissed");
            let mut state = lock_state(state)?;
            state.clear_notification();
        }
        UserEvent::SetSession(session_state) => {
            update_session(state.clone(), &windows.settings, session_state)?;
        }
        UserEvent::EnsureProtonWebview { show_window } => {
            windows.ensure_proton_ready(proxy, state)?;
            if show_window {
                windows.proton_window.set_visible(true);
                windows.proton_window.set_focus();
            }
        }
        UserEvent::TrayMenu(_) => {}
    }

    Ok(())
}

fn handle_proton_snapshot(
    snapshot: ProtonSnapshot,
    windows: &mut Windows,
    state: &Arc<Mutex<AppState>>,
    secrets: &SecretStore,
    proxy: &EventLoopProxy<UserEvent>,
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

    let mut notification_to_show = None;
    let monitored_mailbox = snapshot
        .mailbox_label
        .clone()
        .unwrap_or_else(|| "All Mail".to_owned());

    {
        let mut state_guard = lock_state(state)?;
        if let Some(debug_log) = snapshot.debug_log.as_deref() {
            state_guard.push_debug_log(debug_log);
        }

        if state_guard.session_state == MailSessionState::Paused {
            refresh_settings(&windows.settings, &state_guard)?;
            return Ok(());
        }

        if session_state == MailSessionState::Authenticated && !snapshot.all_mail_ready {
            refresh_settings(&windows.settings, &state_guard)?;
            return Ok(());
        }

        let copy_enabled = state_guard.config.copy_button_enabled;
        for candidate in &snapshot.candidates {
            if let Some(notification) = state_guard.register_candidate(candidate) {
                state_guard.push_debug_log(format!(
                    "OTP matched in {}: {}",
                    monitored_mailbox, notification.masked_code
                ));
                notification_to_show = Some((notification, copy_enabled));
                break;
            }
        }

        refresh_settings(&windows.settings, &state_guard)?;
    }

    if let Some((notification, copy_enabled)) = notification_to_show {
        show_overlay(windows, proxy, state, &notification, copy_enabled)?;
    }

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
    windows: &mut Windows,
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
        if let Err(error) = windows.ensure_proton_ready(proxy, state) {
            warn!(?error, "failed to initialize Proton Mail window");
            return;
        }
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
        if let Err(error) = windows.ensure_proton_ready(proxy, state) {
            warn!(?error, "failed to initialize Proton Mail window");
            return;
        }
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
    windows: &mut Windows,
    proxy: &EventLoopProxy<UserEvent>,
    state: &Arc<Mutex<AppState>>,
    notification: &OtpNotification,
    copy_enabled: bool,
) -> Result<()> {
    windows.ensure_overlay_ready(proxy, state)?;
    let overlay_view = windows
        .overlay
        .as_ref()
        .context("overlay webview should be initialized")?;
    position_overlay(&windows.overlay_window)?;
    let payload = serde_json::to_string(&OverlayPayload::from_notification(
        notification,
        copy_enabled,
    ))
    .context("failed to serialize overlay payload")?;
    if let Err(error) =
        overlay_view.evaluate_script(&format!("window.__PROTON2FA_OVERLAY.show({payload});"))
    {
        push_debug_log_and_refresh(
            state,
            &windows.settings,
            format!("Overlay render failed: {error:#}"),
        );
        return Err(error).context("failed to render overlay notification");
    }
    windows.overlay_window.set_visible(true);
    windows.overlay_window.set_focus();
    push_debug_log_and_refresh(
        state,
        &windows.settings,
        format!("Overlay rendered for {}", notification.source_label),
    );
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
    let text = snapshot.page_text.to_lowercase();

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

fn lock_state(state: &Arc<Mutex<AppState>>) -> Result<std::sync::MutexGuard<'_, AppState>> {
    state.lock().map_err(|_| anyhow!("app state poisoned"))
}

fn push_debug_log(state: &Arc<Mutex<AppState>>, message: impl Into<String>) {
    if let Ok(mut state) = state.lock() {
        state.push_debug_log(message.into());
    }
}

fn push_debug_log_and_refresh(
    state: &Arc<Mutex<AppState>>,
    settings_view: &WebView,
    message: impl Into<String>,
) {
    if let Ok(mut state) = state.lock() {
        state.push_debug_log(message.into());
        let _ = refresh_settings(settings_view, &state);
    }
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

fn build_overlay_webview(window: &Window, proxy: EventLoopProxy<UserEvent>) -> Result<WebView> {
    with_windows_webview_background(WEBVIEW2_OVERLAY_BACKGROUND, || {
        let webview = build_platform_webview(
            WebViewBuilder::new()
                .with_transparent(true)
                .with_initialization_script(
                    r#"
                      document.documentElement.style.background = "transparent";
                      document.body.style.background = "transparent";
                    "#,
                )
                .with_custom_protocol(APP_PROTOCOL.into(), |_webview_id, request| {
                    app_protocol_response(request)
                })
                .with_url(OVERLAY_PAGE_URL)
                .with_ipc_handler(move |payload: Request<String>| {
                    let parsed = serde_json::from_str::<OverlayAction>(payload.body())
                        .unwrap_or_else(|_| OverlayAction {
                            action: "dismiss".to_owned(),
                        });
                    let _ = proxy.send_event(UserEvent::OverlayAction(parsed));
                }),
            window,
        )
        .context("failed to build overlay webview")?;
        webview
            .set_background_color((0, 0, 0, 0))
            .context("failed to set overlay background color")?;
        Ok(webview)
    })
}

fn build_proton_webview(
    window: &Window,
    proxy: EventLoopProxy<UserEvent>,
    config: &AppConfig,
) -> Result<WebView> {
    let monitor_script = proton_monitor_script(config.poll_interval_seconds);
    let mut web_context = WebContext::new(Some(config.user_data_dir.clone()));
    with_windows_webview_background(WEBVIEW2_APP_BACKGROUND, || {
        build_platform_webview(
            WebViewBuilder::new_with_web_context(&mut web_context)
                .with_background_color((24, 24, 24, 255))
                .with_url(&config.proton_mail_url)
                .with_initialization_script(&monitor_script)
                .with_ipc_handler(move |payload: Request<String>| {
                    match serde_json::from_str::<ProtonIpc>(payload.body()) {
                        Ok(ProtonIpc::Snapshot(snapshot)) => {
                            let _ = proxy.send_event(UserEvent::ProtonSnapshot(snapshot));
                        }
                        Ok(ProtonIpc::DismissOverlay) => {
                            let _ = proxy.send_event(UserEvent::DismissOverlay);
                        }
                        Err(error) => {
                            let _ =
                                proxy.send_event(UserEvent::SetSession(MailSessionState::Error));
                            warn!(?error, "failed to parse Proton ipc payload");
                        }
                    }
                }),
            window,
        )
        .context("failed to build Proton webview")
    })
}

#[cfg(windows)]
fn with_windows_webview_background<T>(color: &str, build: impl FnOnce() -> Result<T>) -> Result<T> {
    let previous = std::env::var_os(WEBVIEW2_DEFAULT_BACKGROUND_COLOR);
    unsafe {
        std::env::set_var(WEBVIEW2_DEFAULT_BACKGROUND_COLOR, color);
    }
    let result = build();
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var(WEBVIEW2_DEFAULT_BACKGROUND_COLOR, previous);
        } else {
            std::env::remove_var(WEBVIEW2_DEFAULT_BACKGROUND_COLOR);
        }
    }
    result
}

#[cfg(not(windows))]
fn with_windows_webview_background<T>(
    _color: &str,
    build: impl FnOnce() -> Result<T>,
) -> Result<T> {
    build()
}

trait StateView {
    fn config(&self) -> &AppConfig;
    fn session_state(&self) -> MailSessionState;
    fn last_notification(&self) -> Option<&OtpNotification>;
    fn debug_logs(&self) -> Vec<String>;
}

impl StateView for AppState {
    fn config(&self) -> &AppConfig {
        &self.config
    }

    fn session_state(&self) -> MailSessionState {
        self.session_state
    }

    fn last_notification(&self) -> Option<&OtpNotification> {
        AppState::last_notification(self)
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

    fn last_notification(&self) -> Option<&OtpNotification> {
        AppState::last_notification(&*self)
    }

    fn debug_logs(&self) -> Vec<String> {
        AppState::debug_logs(&*self)
    }
}

struct Windows {
    overlay_window: Window,
    overlay: Option<WebView>,
    settings_window: Window,
    settings: WebView,
    proton_window: Window,
    proton: Option<WebView>,
}

impl Windows {
    fn build(
        event_loop: &EventLoop<UserEvent>,
        proxy: EventLoopProxy<UserEvent>,
        state: Arc<Mutex<AppState>>,
    ) -> Result<Self> {
        let app_window_icon =
            native_window_icon().context("failed to create native window icon")?;

        #[allow(unused_mut)]
        let mut overlay_window_builder = WindowBuilder::new()
            .with_title(OVERLAY_WINDOW_TITLE)
            .with_visible(false)
            .with_decorations(false)
            .with_always_on_top(true)
            .with_skip_taskbar(true)
            .with_transparent(true)
            .with_resizable(false)
            .with_window_icon(Some(app_window_icon.clone()))
            .with_inner_size(LogicalSize::new(OVERLAY_WIDTH, OVERLAY_HEIGHT));

        #[cfg(windows)]
        {
            overlay_window_builder = overlay_window_builder.with_no_redirection_bitmap(true);
        }

        let overlay_window = overlay_window_builder
            .build(event_loop)
            .context("failed to build overlay window")?;

        #[cfg(windows)]
        push_debug_log(
            &state,
            format!(
                "Overlay window created; webview deferred until first notification. no_redirection_bitmap=true, background {}",
                WEBVIEW2_OVERLAY_BACKGROUND
            ),
        );
        #[cfg(not(windows))]
        push_debug_log(
            &state,
            "Overlay window created; webview deferred until first notification",
        );

        let settings_window = WindowBuilder::new()
            .with_title(SETTINGS_HTML_TITLE)
            .with_visible(true)
            .with_resizable(false)
            .with_window_icon(Some(app_window_icon.clone()))
            .with_inner_size(LogicalSize::new(900.0, 720.0))
            .build(event_loop)
            .context("failed to build settings window")?;

        let settings_proxy = proxy.clone();
        let settings = with_windows_webview_background(WEBVIEW2_APP_BACKGROUND, || {
            build_platform_webview(
                WebViewBuilder::new()
                    .with_background_color((24, 24, 24, 255))
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
            .context("failed to build settings webview")
        })?;

        let proton_window = WindowBuilder::new()
            .with_title(PROTON_LOGIN_TITLE)
            .with_visible(false)
            .with_window_icon(Some(app_window_icon))
            .with_inner_size(LogicalSize::new(1120.0, 820.0))
            .build(event_loop)
            .context("failed to build Proton login window")?;

        Ok(Self {
            overlay_window,
            overlay: None,
            settings_window,
            settings,
            proton_window,
            proton: None,
        })
    }

    fn ensure_overlay_ready(
        &mut self,
        proxy: &EventLoopProxy<UserEvent>,
        state: &Arc<Mutex<AppState>>,
    ) -> Result<()> {
        if self.overlay.is_none() {
            let overlay = build_overlay_webview(&self.overlay_window, proxy.clone())
                .context("failed to lazily build overlay webview")?;
            self.overlay = Some(overlay);
            #[cfg(windows)]
            push_debug_log(
                state,
                format!(
                    "Overlay webview initialized lazily with no_redirection_bitmap=true and background {}",
                    WEBVIEW2_OVERLAY_BACKGROUND
                ),
            );
            #[cfg(not(windows))]
            push_debug_log(state, "Overlay webview initialized lazily");
        }
        Ok(())
    }

    fn ensure_proton_ready(
        &mut self,
        proxy: &EventLoopProxy<UserEvent>,
        state: &Arc<Mutex<AppState>>,
    ) -> Result<()> {
        if self.proton.is_some() {
            return Ok(());
        }

        let config = {
            let state = lock_state(state)?;
            state.config.clone()
        };
        let proton = build_proton_webview(&self.proton_window, proxy.clone(), &config)
            .context("failed to build Proton webview")?;
        self.proton = Some(proton);
        Ok(())
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
    EnsureProtonWebview { show_window: bool },
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
    page_text: String,
    #[serde(default)]
    mailbox_label: Option<String>,
    #[serde(default)]
    all_mail_ready: bool,
    #[serde(default)]
    debug_log: Option<String>,
    #[serde(default)]
    candidates: Vec<OtpCandidateEmail>,
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
                .last_notification()
                .map(|notification| notification.masked_code.clone()),
            debug_logs: state.debug_logs(),
        }
    }
}

fn proton_monitor_script(poll_interval_seconds: u64) -> String {
    let schedule_ms = (poll_interval_seconds.max(5) * 1000).to_string();
    r#"
(() => {
  const OTP_TERMS = /(code|verification|2fa|two-factor|two factor|one-time|one time|otp|security|passcode|sign in|signin)/i;
  const DIGIT_SEQUENCE = /\b((?:[0-9][\s-]?){3,7}[0-9])\b/;
  const ALL_MAIL_LABEL = "all mail";
  const MORE_LABEL = "more";
  const MAX_ROW_SCAN = 20;
  const CLICK_THROTTLE_MS = 2500;
  const OPEN_MESSAGE_TIMEOUT_MS = 5000;
  const FALLBACK_ATTEMPT_LIMIT = 64;

  const state = {
    pendingOpenedId: null,
    pendingOpenedBaseline: null,
    pendingOpenedRequestedAt: 0,
    fallbackAttempted: [],
    fallbackAttemptedSet: new Set(),
    lastAllMailClickAt: 0,
    lastMoreClickAt: 0,
    lastDebugLog: "",
    pendingDebugLog: null,
  };

  const normalizeBlock = (value) => (value || "")
    .replace(/\u00a0/g, " ")
    .replace(/\r/g, "\n")
    .replace(/[ \t]+/g, " ")
    .replace(/\n{3,}/g, "\n\n")
    .trim();

  const linesOf = (value) => normalizeBlock(value)
    .split(/\n+/)
    .map((line) => line.trim())
    .filter(Boolean);

  const uniqueLines = (lines) => {
    const seen = new Set();
    return lines.filter((line) => {
      const key = line.toLowerCase();
      if (seen.has(key)) {
        return false;
      }
      seen.add(key);
      return true;
    });
  };

  const normalizeMatchText = (value) => uniqueLines(linesOf(value)).join(" ").toLowerCase();
  const textOf = (node) => normalizeBlock(node?.innerText || node?.textContent || "");
  const nowIso = () => new Date().toISOString();

  const isVisible = (element) => {
    if (!element || typeof element.getClientRects !== "function") {
      return false;
    }
    return element.getClientRects().length > 0;
  };

  const queueDebugLog = (message) => {
    if (!message || state.lastDebugLog === message) {
      return;
    }
    state.pendingDebugLog = message;
    state.lastDebugLog = message;
  };

  const drainDebugLog = () => {
    const message = state.pendingDebugLog;
    state.pendingDebugLog = null;
    return message;
  };

  const collectVisibleNodes = (selectors, limit = Number.POSITIVE_INFINITY) => {
    const nodes = [];
    const seen = new Set();
    for (const selector of selectors) {
      for (const node of document.querySelectorAll(selector)) {
        if (!isVisible(node) || seen.has(node)) {
          continue;
        }
        seen.add(node);
        nodes.push(node);
        if (nodes.length >= limit) {
          return nodes;
        }
      }
    }
    return nodes;
  };

  const clickableTarget = (node) => node?.closest?.('button, a, [role="button"], [role="link"], [tabindex]') || node;
  const clickElement = (node) => {
    const target = clickableTarget(node);
    if (!target || typeof target.click !== "function") {
      return false;
    }
    target.click();
    return true;
  };

  const fingerprint = (value) => {
    let hash = 2166136261;
    for (const char of value) {
      hash ^= char.charCodeAt(0);
      hash = Math.imul(hash, 16777619);
    }
    return (hash >>> 0).toString(16).padStart(8, "0");
  };

  const findMailboxButton = (label) => {
    const selectors = [
      '[data-testid*="navigation"] button',
      '[data-testid*="navigation"] a',
      '[data-testid*="sidebar"] button',
      '[data-testid*="sidebar"] a',
      '[role="navigation"] button',
      '[role="navigation"] a',
      'nav button',
      'nav a'
    ];
    const nodes = collectVisibleNodes(selectors, 96);
    const exactMatch = nodes.find((node) => {
      const text = normalizeMatchText(node.innerText || node.textContent || "");
      return text === label || text.startsWith(`${label} `) || text.endsWith(` ${label}`);
    });
    if (exactMatch) {
      return exactMatch;
    }
    return nodes.find((node) => normalizeMatchText(node.innerText || node.textContent || "").includes(label)) || null;
  };

  const currentMailboxLabel = () => {
    if (window.location.href.toLowerCase().includes("all-mail")) {
      return ALL_MAIL_LABEL;
    }
    const selectors = [
      '[aria-current="page"]',
      '[aria-selected="true"]',
      '[data-testid*="sidebar"] [aria-checked="true"]',
      'nav [aria-current="page"]',
      'nav [aria-selected="true"]'
    ];
    const active = collectVisibleNodes(selectors, 10);
    for (const node of active) {
      const text = normalizeMatchText(node.innerText || node.textContent || "");
      if (text) {
        return text;
      }
    }
    return null;
  };

  const ensureAllMailView = () => {
    if (!window.location.href.includes("mail.proton.me")) {
      return { ready: false, label: null };
    }

    const currentLabel = currentMailboxLabel();
    if (currentLabel && currentLabel.includes(ALL_MAIL_LABEL)) {
      return { ready: true, label: currentLabel };
    }

    const now = Date.now();
    const allMailButton = findMailboxButton(ALL_MAIL_LABEL);
    if (allMailButton) {
      if (now - state.lastAllMailClickAt > CLICK_THROTTLE_MS && clickElement(allMailButton)) {
        state.lastAllMailClickAt = now;
        queueDebugLog("Navigating Proton monitor to All Mail");
      }
      return { ready: false, label: currentLabel };
    }

    const moreButton = findMailboxButton(MORE_LABEL);
    if (moreButton) {
      if (now - state.lastMoreClickAt > CLICK_THROTTLE_MS && clickElement(moreButton)) {
        state.lastMoreClickAt = now;
        queueDebugLog("Expanding Proton mailbox list");
      }
      return { ready: false, label: currentLabel };
    }

    queueDebugLog("All Mail mailbox not found in Proton navigation");
    return { ready: false, label: currentLabel };
  };

  const extractRowMetadata = (text) => {
    const lines = uniqueLines(linesOf(text)).filter((line) => line.length >= 2);
    const sender = lines[0] || null;
    const subject = lines[1] || lines[0] || "Proton Mail";
    const snippet = lines.slice(2).join(" ");
    return { sender, subject, snippet, lineCount: lines.length };
  };

  const buildRowCandidate = (node, mailboxLabel) => {
    const text = textOf(node);
    if (text.length < 12) {
      return null;
    }

    const metadata = extractRowMetadata(text);
    if (metadata.lineCount > 8 || text.length > 420) {
      return null;
    }

    const hasCode = DIGIT_SEQUENCE.test(text);
    if (!hasCode && !OTP_TERMS.test(text)) {
      return null;
    }

    const stableKey = [
      mailboxLabel || "",
      node.getAttribute?.("data-testid") || "",
      node.getAttribute?.("data-id") || "",
      node.id || "",
      metadata.sender || "",
      metadata.subject || "",
      metadata.snippet || ""
    ].join("|") || text;

    return {
      node,
      fullText: text,
      hasCode,
      candidate: {
        message_id: `all-mail-${fingerprint(stableKey)}`,
        sender: metadata.sender,
        subject: metadata.subject,
        received_at: nowIso(),
        body_text: text
      }
    };
  };

  const collectRowCandidates = (mailboxLabel) => {
    const selectors = [
      '[data-testid*="conversation"]',
      '[data-testid*="item-row"]',
      '[data-testid*="message-row"]',
      '[data-testid*="item"]',
      '[role="main"] [role="row"]',
      'main [role="row"]',
      '[role="main"] article',
      'main article'
    ];

    const rows = collectVisibleNodes(selectors, MAX_ROW_SCAN * 4);
    const seenIds = new Set();
    const candidates = [];

    for (const row of rows) {
      const candidate = buildRowCandidate(row, mailboxLabel);
      if (!candidate || seenIds.has(candidate.candidate.message_id)) {
        continue;
      }
      seenIds.add(candidate.candidate.message_id);
      candidates.push(candidate);
      if (candidates.length >= MAX_ROW_SCAN) {
        break;
      }
    }

    return candidates;
  };

  const collectOpenedMessageText = () => {
    const selectors = [
      '[data-testid*="message-body"]',
      '[data-testid*="message-content"]',
      '[data-testid*="conversation"] [data-testid*="message"]',
      '[role="main"] article',
      '[role="main"] section',
      'main article',
      'main section',
      '[role="main"]',
      'main'
    ];

    const blocks = [];
    const seen = new Set();
    for (const selector of selectors) {
      for (const node of document.querySelectorAll(selector)) {
        if (!isVisible(node)) {
          continue;
        }
        const text = textOf(node);
        if (text.length < 24 || seen.has(text)) {
          continue;
        }
        seen.add(text);
        blocks.push(text);
        if (blocks.length >= 6) {
          return blocks.join("\n\n").slice(0, 40000);
        }
      }
    }

    return blocks.join("\n\n").slice(0, 40000);
  };

  const rememberFallbackAttempt = (messageId) => {
    if (state.fallbackAttemptedSet.has(messageId)) {
      return;
    }
    state.fallbackAttempted.push(messageId);
    state.fallbackAttemptedSet.add(messageId);
    while (state.fallbackAttempted.length > FALLBACK_ATTEMPT_LIMIT) {
      const staleId = state.fallbackAttempted.shift();
      state.fallbackAttemptedSet.delete(staleId);
    }
  };

  const clearPendingOpen = () => {
    state.pendingOpenedId = null;
    state.pendingOpenedBaseline = null;
    state.pendingOpenedRequestedAt = 0;
  };

  const maybeOpenFallbackCandidate = (rowCandidates) => {
    if (state.pendingOpenedId) {
      return false;
    }

    const fallback = rowCandidates.find((candidate) =>
      !candidate.hasCode && !state.fallbackAttemptedSet.has(candidate.candidate.message_id)
    );
    if (!fallback) {
      return false;
    }

    rememberFallbackAttempt(fallback.candidate.message_id);
    state.pendingOpenedId = fallback.candidate.message_id;
    state.pendingOpenedBaseline = fallback.fullText;
    state.pendingOpenedRequestedAt = Date.now();

    if (clickElement(fallback.node)) {
      queueDebugLog("Opening All Mail message for OTP fallback");
      return true;
    }

    clearPendingOpen();
    queueDebugLog("Failed to open All Mail message for OTP fallback");
    return false;
  };

  const collectOpenedMessageCandidate = () => {
    if (!state.pendingOpenedId) {
      return null;
    }

    const openedText = collectOpenedMessageText();
    if (!openedText) {
      if (Date.now() - state.pendingOpenedRequestedAt > OPEN_MESSAGE_TIMEOUT_MS) {
        queueDebugLog("Opened All Mail message did not expose readable content");
        clearPendingOpen();
      }
      return null;
    }

    if (openedText === state.pendingOpenedBaseline && Date.now() - state.pendingOpenedRequestedAt < OPEN_MESSAGE_TIMEOUT_MS) {
      return null;
    }

    const candidate = {
      message_id: state.pendingOpenedId,
      sender: null,
      subject: null,
      received_at: nowIso(),
      body_text: openedText
    };
    clearPendingOpen();
    queueDebugLog("Parsed opened All Mail message for OTP fallback");
    return candidate;
  };

  const collectPageText = () => normalizeBlock(document.body?.innerText || "").slice(0, 40000);

  const sendSnapshot = () => {
    const isMailPage = window.location.href.includes("mail.proton.me");
    const mailboxState = isMailPage ? ensureAllMailView() : { ready: false, label: null };
    const candidates = [];

    if (isMailPage && mailboxState.ready) {
      const rowCandidates = collectRowCandidates(mailboxState.label);
      if (rowCandidates.length > 0) {
        queueDebugLog(`All Mail scan found ${rowCandidates.length} OTP-like rows`);
      }

      const openedCandidate = collectOpenedMessageCandidate();
      if (openedCandidate) {
        candidates.push(openedCandidate);
      }

      const directCandidates = rowCandidates
        .filter((candidate) => candidate.hasCode)
        .map((candidate) => candidate.candidate)
        .slice(0, 5);
      candidates.push(...directCandidates);

      if (!openedCandidate && directCandidates.length === 0) {
        maybeOpenFallbackCandidate(rowCandidates);
      }
    }

    const payload = {
      kind: "snapshot",
      url: window.location.href,
      page_text: collectPageText(),
      mailbox_label: mailboxState.label || null,
      all_mail_ready: Boolean(mailboxState.ready),
      debug_log: drainDebugLog(),
      candidates
    };

    window.ipc.postMessage(JSON.stringify(payload));
  };

  const schedule = __SCHEDULE_MS__;
  window.addEventListener("load", () => setTimeout(sendSnapshot, 1500));
  document.addEventListener("visibilitychange", sendSnapshot);
  new MutationObserver(() => {
    window.clearTimeout(window.__protoncodeMutationTimer);
    window.__protoncodeMutationTimer = window.setTimeout(sendSnapshot, 800);
  }).observe(document.documentElement, { childList: true, subtree: true, characterData: true });
  window.setInterval(sendSnapshot, schedule);
  sendSnapshot();
})();
"#
    .replace("__SCHEDULE_MS__", &schedule_ms)
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
        --bg: #181818;
        --panel: rgba(24, 24, 24, 0.94);
        --surface: rgba(255, 255, 255, 0.04);
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
        background: rgba(0, 0, 0, 0) !important;
        overflow: hidden;
        font-family: var(--font);
        overscroll-behavior: none;
      }
      .shell {
        width: 100%;
        height: 100%;
        display: flex;
        align-items: stretch;
        justify-content: stretch;
        padding: 12px;
        background: transparent;
      }
      .card {
        width: 100%;
        height: 100%;
        padding: 18px;
        border-radius: 22px;
        background: var(--panel);
        color: var(--text);
        box-shadow: 0 24px 60px rgba(0, 0, 0, 0.42);
        display: flex;
        flex-direction: column;
        gap: 12px;
        opacity: 0;
        transform: translateY(12px) scale(0.985);
        overflow: hidden;
      }
      .card.is-visible {
        animation: overlay-enter 180ms cubic-bezier(0.22, 1, 0.36, 1) both;
      }
      @keyframes overlay-enter {
        from {
          opacity: 0;
          transform: translateY(12px) scale(0.985);
        }
        to {
          opacity: 1;
          transform: translateY(0) scale(1);
        }
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
      .dismiss {
        width: 32px;
        height: 32px;
        min-width: 32px;
      }
      .headline {
        color: var(--muted);
        font-size: 13px;
        letter-spacing: 0.02em;
      }
      .source {
        color: #ffffff;
        font-size: 16px;
        font-weight: 400;
        line-height: 1.35;
        display: -webkit-box;
        overflow: hidden;
        text-overflow: ellipsis;
        -webkit-box-orient: vertical;
        -webkit-line-clamp: 3;
        min-height: 2.7em;
        overflow-wrap: anywhere;
      }
      .code-row {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto;
        align-items: stretch;
        gap: 10px;
        min-height: 56px;
      }
      .code {
        min-width: 0;
        padding: 0 16px;
        border-radius: 16px;
        background: rgba(255, 255, 255, 0.06);
        color: #ffffff;
        display: flex;
        align-items: center;
        justify-content: center;
        font-size: 28px;
        font-weight: 400;
        letter-spacing: 0.2em;
        text-align: center;
        white-space: nowrap;
        overflow: hidden;
        text-overflow: ellipsis;
      }
      .code-actions {
        display: flex;
        align-items: center;
        gap: 8px;
      }
      button {
        appearance: none;
        border: 0;
        border-radius: 999px;
        padding: 10px 14px;
        min-width: 0;
        background: rgba(255, 255, 255, 0.05);
        color: var(--muted);
        font-family: var(--font);
        font-size: 12px;
        font-weight: 400;
        cursor: pointer;
        transition: color 0.2s ease, background 0.2s ease;
      }
      button:hover {
        color: #ffffff;
        background: rgba(139, 92, 246, 0.16);
      }
      .icon-button {
        width: 44px;
        height: 44px;
        display: inline-flex;
        align-items: center;
        justify-content: center;
        padding: 0;
        flex-shrink: 0;
      }
      .icon-button svg {
        width: 17px;
        height: 17px;
        stroke: currentColor;
        stroke-width: 1.8;
        fill: none;
        stroke-linecap: round;
        stroke-linejoin: round;
      }
      .meta {
        display: flex;
        align-items: center;
        gap: 8px;
        color: var(--muted);
        font-size: 12px;
        line-height: 1.4;
        min-height: 18px;
      }
      .meta strong {
        color: #ffffff;
        font-weight: 500;
      }
      [hidden] {
        display: none !important;
      }
    </style>
  </head>
  <body>
    <div class="shell">
      <div class="card" id="card" hidden>
        <div class="brand-row">
          <div class="brand">
            <img class="brand-mark" src="" alt="ProtonCode icon" id="brand-mark" />
            <div class="brand-label">ProtonCode</div>
          </div>
          <button class="dismiss icon-button" id="dismiss" type="button" aria-label="Dismiss notification" title="Dismiss">
            <svg viewBox="0 0 24 24" aria-hidden="true">
              <path d="M6 6L18 18"></path>
              <path d="M18 6L6 18"></path>
            </svg>
          </button>
        </div>
        <div class="headline" id="received-at">Received now</div>
        <div class="source" id="source">Waiting for the next code</div>
        <div class="code-row">
          <div class="code" id="code">******</div>
          <div class="code-actions">
            <button class="icon-button" id="toggle" type="button" aria-label="Reveal code" title="Reveal code"></button>
            <button class="icon-button" id="copy" type="button" aria-label="Copy code" title="Copy code">
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <rect x="9" y="9" width="10" height="10" rx="2"></rect>
                <path d="M7 15H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h7a2 2 0 0 1 2 2v1"></path>
              </svg>
            </button>
          </div>
        </div>
        <div class="meta" id="meta">
          <span>Masked by default. Copy stays manual.</span>
        </div>
      </div>
    </div>
    <script>
      const state = {
        maskedCode: "******",
        rawCode: "",
        revealed: false,
        hideTimer: null,
        hideStartedAt: 0,
        remainingMs: 0,
        hovering: false,
      };
      const eyeIcon = `
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M2 12s3.5-6 10-6 10 6 10 6-3.5 6-10 6S2 12 2 12Z"></path>
          <circle cx="12" cy="12" r="3"></circle>
        </svg>`;
      const eyeSlashIcon = `
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M3 3l18 18"></path>
          <path d="M10.6 10.7a2 2 0 0 0 2.7 2.7"></path>
          <path d="M9.4 5.6A11.5 11.5 0 0 1 12 5.3c6.5 0 10 6.7 10 6.7a17.6 17.6 0 0 1-4.3 4.8"></path>
          <path d="M6.2 6.2A17.3 17.3 0 0 0 2 12s3.5 6.7 10 6.7c1 0 2-.2 2.9-.5"></path>
        </svg>`;

      document.getElementById("brand-mark").src = "__APP_ICON__";
      document.documentElement.style.background = "transparent";
      document.body.style.background = "transparent";

      const card = document.getElementById("card");
      const code = document.getElementById("code");
      const source = document.getElementById("source");
      const receivedAt = document.getElementById("received-at");
      const copy = document.getElementById("copy");
      const toggle = document.getElementById("toggle");
      const meta = document.getElementById("meta");

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
        toggle.innerHTML = isRevealed ? eyeSlashIcon : eyeIcon;
        toggle.setAttribute("aria-label", isRevealed ? "Hide code" : "Reveal code");
        toggle.title = isRevealed ? "Hide code" : "Reveal code";
      }

      function hide() {
        window.clearTimeout(state.hideTimer);
        state.hideTimer = null;
        state.hideStartedAt = 0;
        state.remainingMs = 0;
        card.classList.remove("is-visible");
        card.hidden = true;
        state.revealed = false;
        renderCode();
      }

      function dismissAfterTimeout() {
        window.ipc.postMessage(JSON.stringify({ action: "dismiss" }));
        hide();
      }

      function scheduleHide(delayMs) {
        window.clearTimeout(state.hideTimer);
        state.hideTimer = null;
        state.remainingMs = Math.max(0, Math.round(delayMs));
        if (state.remainingMs === 0 || state.hovering) {
          return;
        }

        state.hideStartedAt = Date.now();
        state.hideTimer = window.setTimeout(() => {
          dismissAfterTimeout();
        }, state.remainingMs);
      }

      function pauseHide() {
        if (state.hideTimer === null) {
          return;
        }

        const elapsedMs = Date.now() - state.hideStartedAt;
        window.clearTimeout(state.hideTimer);
        state.hideTimer = null;
        state.hideStartedAt = 0;
        state.remainingMs = Math.max(0, state.remainingMs - elapsedMs);
      }

      function resumeHide() {
        if (card.hidden || state.remainingMs <= 0) {
          return;
        }

        scheduleHide(state.remainingMs);
      }

      card.addEventListener("pointerenter", () => {
        state.hovering = true;
        pauseHide();
      });

      card.addEventListener("pointerleave", () => {
        state.hovering = false;
        resumeHide();
      });

      window.__PROTON2FA_OVERLAY = {
        show(payload) {
          source.textContent = payload.source_label;
          state.maskedCode = payload.masked_code;
          state.rawCode = payload.raw_code;
          receivedAt.textContent = payload.received_at_label ? `Received ${payload.received_at_label}` : "Received now";
          meta.innerHTML = payload.copy_enabled
            ? "<span>Masked by default. <strong>Copy stays manual.</strong></span>"
            : "<span>Masked by default. Copy is currently disabled.</span>";
          copy.hidden = !payload.copy_enabled;
          state.revealed = false;
          renderCode();
          card.hidden = false;
          card.classList.remove("is-visible");
          void card.offsetWidth;
          window.requestAnimationFrame(() => {
            card.classList.add("is-visible");
          });
          state.hovering = false;
          scheduleHide(payload.duration_ms);
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
        position: relative;
        width: 100%;
        max-width: 760px;
        min-height: 560px;
        display: flex;
        flex-direction: column;
        gap: 72px;
      }
      .boot-skeleton {
        position: absolute;
        inset: 0;
        display: grid;
        align-content: start;
        gap: 36px;
        padding: 0;
        background: var(--bg);
        pointer-events: none;
        transition: opacity 180ms ease, visibility 0ms linear 180ms;
      }
      body:not(.loading) .boot-skeleton {
        opacity: 0;
        visibility: hidden;
      }
      .skeleton-header,
      .skeleton-grid,
      .skeleton-footer {
        display: grid;
      }
      .skeleton-header {
        gap: 16px;
      }
      .skeleton-grid {
        grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
        column-gap: 80px;
        row-gap: 64px;
      }
      .skeleton-footer {
        grid-template-columns: 140px 120px 1fr 110px;
        align-items: center;
        gap: 16px;
        padding-top: 40px;
        border-top: 1px solid rgba(30, 33, 40, 0.5);
      }
      .skeleton-block,
      .skeleton-pill,
      .skeleton-line {
        background: linear-gradient(90deg, rgba(255, 255, 255, 0.04), rgba(255, 255, 255, 0.09), rgba(255, 255, 255, 0.04));
        background-size: 220% 100%;
        animation: skeleton-shimmer 1.1s linear infinite;
      }
      .skeleton-block {
        border-radius: 20px;
      }
      .skeleton-line {
        height: 12px;
        border-radius: 999px;
      }
      .skeleton-pill {
        height: 38px;
        border-radius: 999px;
      }
      .skeleton-brand {
        width: 168px;
        height: 28px;
      }
      .skeleton-state {
        width: 138px;
      }
      .skeleton-version {
        width: 52px;
        height: 12px;
        justify-self: end;
      }
      .skeleton-metric {
        display: grid;
        gap: 18px;
      }
      .skeleton-label {
        width: 72px;
      }
      .skeleton-value {
        width: 168px;
        height: 62px;
      }
      .skeleton-toggle-group {
        display: grid;
        gap: 28px;
      }
      .skeleton-toggle-row,
      .skeleton-last-code {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 20px;
      }
      .skeleton-toggle-label {
        width: 128px;
      }
      .skeleton-switch {
        width: 40px;
        height: 24px;
        border-radius: 999px;
      }
      .skeleton-code-label {
        width: 76px;
      }
      .skeleton-code {
        width: 132px;
      }
      @keyframes skeleton-shimmer {
        from {
          background-position: 200% 0;
        }
        to {
          background-position: -20% 0;
        }
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
        inset: 0;
        margin: 0;
        opacity: 0;
        cursor: pointer;
      }
      .toggle-control {
        position: relative;
        display: inline-flex;
        align-items: center;
        width: 40px;
        height: 24px;
        flex-shrink: 0;
      }
      .toggle-switch {
        width: 40px;
        height: 24px;
        display: inline-block;
        position: relative;
        border-radius: 999px;
        background: #2d2d34;
        border: 1px solid rgba(255, 255, 255, 0.14);
        box-shadow: inset 0 1px 1px rgba(255, 255, 255, 0.04);
        pointer-events: none;
        transition: background 0.2s ease, border-color 0.2s ease, box-shadow 0.2s ease;
      }
      .toggle-switch::after {
        content: "";
        position: absolute;
        top: 3px;
        left: 3px;
        width: 16px;
        height: 16px;
        border-radius: 50%;
        background: #d5dce7;
        box-shadow: 0 1px 2px rgba(0, 0, 0, 0.25);
        transition: transform 0.2s ease, background 0.2s ease;
      }
      .toggle-switch.is-on {
        background: var(--accent);
        border-color: rgba(139, 92, 246, 0.7);
        box-shadow: 0 0 0 1px rgba(139, 92, 246, 0.12), 0 8px 18px rgba(91, 33, 182, 0.18);
      }
      .toggle-switch.is-on::after {
        transform: translateX(16px);
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
        position: absolute;
        left: 0;
        right: 0;
        bottom: 0;
        z-index: 20;
        display: flex;
        flex-direction: column;
        gap: 10px;
        padding: 16px 18px;
        border: 1px solid rgba(139, 92, 246, 0.18);
        border-radius: 18px;
        background: rgba(10, 10, 12, 0.96);
        box-shadow: 0 18px 40px rgba(0, 0, 0, 0.35);
      }
      .debug-panel[hidden] {
        display: none !important;
      }
      .debug-title {
        color: #ffffff;
        font-size: 11px;
        letter-spacing: 0.16em;
        text-transform: uppercase;
      }
      .debug-output {
        margin: 0;
        max-height: 180px;
        overflow: auto;
        color: #b6c0cd;
        font-family: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace;
        font-size: 11px;
        line-height: 1.55;
        white-space: pre-wrap;
        word-break: break-word;
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
  <body class="loading">
    <main id="app-shell">
      <header>
        <div>
          <h1>
            <img class="brand-mark" src="" alt="ProtonCode icon" id="brand-mark" />
            <span>ProtonCode</span>
          </h1>
          <p class="session-state" id="session-state">Starting Proton Mail...</p>
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
            <span class="toggle-control">
              <input class="toggle-input" id="copy-button-enabled" type="checkbox" />
              <span class="toggle-switch" id="copy-button-toggle"></span>
            </span>
          </label>

          <label class="toggle-row" for="launch-on-startup">
            <span class="toggle-label">Launch at Sign-In</span>
            <span class="toggle-control">
              <input class="toggle-input" id="launch-on-startup" type="checkbox" />
              <span class="toggle-switch" id="launch-on-startup-toggle"></span>
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
        <div class="debug-title">Debug Logs</div>
        <pre class="debug-output" id="debug-output">No debug logs yet.</pre>
      </section>

      <div class="boot-skeleton" aria-hidden="true">
        <div class="skeleton-header">
          <div class="skeleton-line skeleton-brand"></div>
          <div class="skeleton-line skeleton-state"></div>
          <div class="skeleton-line skeleton-version"></div>
        </div>
        <div class="skeleton-grid">
          <div class="skeleton-metric">
            <div class="skeleton-line skeleton-label"></div>
            <div class="skeleton-block skeleton-value"></div>
            <div class="skeleton-line skeleton-label"></div>
            <div class="skeleton-block skeleton-value"></div>
          </div>
          <div class="skeleton-toggle-group">
            <div class="skeleton-toggle-row">
              <div class="skeleton-line skeleton-toggle-label"></div>
              <div class="skeleton-block skeleton-switch"></div>
            </div>
            <div class="skeleton-toggle-row">
              <div class="skeleton-line skeleton-toggle-label"></div>
              <div class="skeleton-block skeleton-switch"></div>
            </div>
            <div class="skeleton-last-code">
              <div class="skeleton-line skeleton-code-label"></div>
              <div class="skeleton-line skeleton-code"></div>
            </div>
          </div>
        </div>
        <div class="skeleton-footer">
          <div class="skeleton-pill"></div>
          <div class="skeleton-pill"></div>
          <div></div>
          <div class="skeleton-pill"></div>
        </div>
      </div>

    </main>
    <script>
      document.getElementById("brand-mark").src = "__APP_ICON__";
      const appShell = document.getElementById("app-shell");

      const elements = {
        sessionState: document.getElementById("session-state"),
        lastCode: document.getElementById("last-code"),
        pollInterval: document.getElementById("poll-interval"),
        notificationDuration: document.getElementById("notification-duration"),
        copyEnabled: document.getElementById("copy-button-enabled"),
        launchOnStartup: document.getElementById("launch-on-startup"),
        copyToggle: document.getElementById("copy-button-toggle"),
        launchToggle: document.getElementById("launch-on-startup-toggle"),
        debugPanel: document.getElementById("debug-panel"),
        debugOutput: document.getElementById("debug-output")
      };
      const uiState = { debugVisible: false };

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

      function syncToggleVisual(input, visual) {
        visual.classList.toggle("is-on", Boolean(input.checked));
      }

      elements.copyEnabled.addEventListener("change", () => {
        syncToggleVisual(elements.copyEnabled, elements.copyToggle);
      });

      elements.launchOnStartup.addEventListener("change", () => {
        syncToggleVisual(elements.launchOnStartup, elements.launchToggle);
      });

      function isEditableTarget(target) {
        if (!(target instanceof Element)) {
          return false;
        }

        return target.closest("input, textarea, select, [contenteditable=\"true\"]") !== null;
      }

      function handleKeydown(event) {
        if (event.defaultPrevented || event.repeat) {
          return;
        }

        if (event.ctrlKey || event.metaKey || event.altKey) {
          return;
        }

        if (isEditableTarget(event.target)) {
          return;
        }

        if (event.code === "KeyP") {
          event.preventDefault();
          window.ipc.postMessage(JSON.stringify({ kind: "test_notification" }));
          return;
        }

        if (event.code === "KeyL") {
          event.preventDefault();
          uiState.debugVisible = !uiState.debugVisible;
          elements.debugPanel.hidden = !uiState.debugVisible;
          return;
        }
      }

      window.addEventListener("keydown", handleKeydown, { capture: true });
      document.addEventListener("keydown", handleKeydown, { capture: true });

      window.__PROTON2FA_STATUS = {
        render(payload) {
          const hasCode = Boolean(payload.last_masked_code);
          document.body.classList.remove("loading");
          appShell.dataset.ready = "true";
          elements.sessionState.textContent = payload.session_state;
          elements.lastCode.textContent = payload.last_masked_code || "No code received yet";
          elements.lastCode.classList.toggle("empty", !hasCode);
          elements.pollInterval.value = payload.poll_interval_seconds;
          elements.notificationDuration.value = payload.notification_duration_seconds;
          elements.copyEnabled.checked = payload.copy_button_enabled;
          elements.launchOnStartup.checked = payload.launch_on_startup;
          elements.debugOutput.textContent = payload.debug_logs.length
            ? payload.debug_logs.join("\n")
            : "No debug logs yet.";
          elements.debugPanel.hidden = !uiState.debugVisible;
          syncToggleVisual(elements.copyEnabled, elements.copyToggle);
          syncToggleVisual(elements.launchOnStartup, elements.launchToggle);
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

    fn snapshot(url: &str, page_text: &str) -> ProtonSnapshot {
        ProtonSnapshot {
            url: url.to_owned(),
            page_text: page_text.to_owned(),
            mailbox_label: Some("all mail".to_owned()),
            all_mail_ready: true,
            debug_log: None,
            candidates: Vec::new(),
        }
    }

    #[test]
    fn mail_page_is_not_marked_signed_out_when_message_mentions_sign_in() {
        let snapshot = snapshot(
            "https://mail.proton.me/u/0/all-mail",
            "Your verification code is 123456. Use it to sign in.",
        );

        assert_eq!(
            infer_session_state(&snapshot),
            MailSessionState::Authenticated
        );
    }

    #[test]
    fn account_login_page_is_marked_unauthenticated() {
        let snapshot = snapshot("https://account.proton.me/login", "Sign in to Proton");

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

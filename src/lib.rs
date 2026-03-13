pub mod app;
pub mod autostart;
pub mod config;
pub mod models;
pub mod otp;
pub mod secrets;

#[cfg(any(windows, target_os = "linux"))]
pub mod desktop_app;

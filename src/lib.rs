//! ProtonCode — a desktop application that monitors Proton Mail for incoming
//! one-time password (OTP) codes and surfaces them as desktop notifications.

pub mod app;
pub mod autostart;
pub mod config;
pub mod models;
pub mod otp;
pub mod secrets;

#[cfg(any(windows, target_os = "linux"))]
pub mod desktop_app;

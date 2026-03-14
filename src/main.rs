#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    #[cfg(windows)]
    unsafe {
        std::env::set_var("WEBVIEW2_DEFAULT_BACKGROUND_COLOR", "FF181818");
    }

    #[cfg(any(windows, target_os = "linux"))]
    {
        protoncode::desktop_app::run()
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        eprintln!(
            "protoncode currently runs only on windows and linux. core parsing and config tests still work on this platform."
        );
        Ok(())
    }
}

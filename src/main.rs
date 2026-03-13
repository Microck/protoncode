#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    #[cfg(windows)]
    {
        protoncode::windows_app::run()
    }

    #[cfg(not(windows))]
    {
        eprintln!(
            "protoncode currently runs only on windows 10/11. core parsing and config tests still work on this platform."
        );
        Ok(())
    }
}

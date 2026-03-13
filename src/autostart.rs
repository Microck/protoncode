use std::ffi::OsStr;
use std::path::Path;

use anyhow::Result;

pub const AUTOSTART_FLAG: &str = "--autostart";
pub const APP_RUN_KEY_VALUE: &str = "protoncode";

#[cfg(windows)]
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn has_autostart_flag<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .any(|arg| arg.as_ref() == OsStr::new(AUTOSTART_FLAG))
}

pub fn format_autostart_command(executable_path: &Path) -> String {
    format!("\"{}\" {AUTOSTART_FLAG}", executable_path.display())
}

#[cfg(windows)]
pub fn sync_launch_on_startup(enabled: bool) -> Result<()> {
    if enabled { enable() } else { disable() }
}

#[cfg(not(windows))]
pub fn sync_launch_on_startup(_enabled: bool) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
pub fn current_registration() -> Result<Option<String>> {
    use anyhow::Context;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let run_key = match current_user.open_subkey(RUN_KEY_PATH) {
        Ok(run_key) => run_key,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to open Windows Run key"),
    };

    match run_key.get_value::<String, _>(APP_RUN_KEY_VALUE) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("failed to read protoncode run key value"),
    }
}

#[cfg(not(windows))]
pub fn current_registration() -> Result<Option<String>> {
    Ok(None)
}

#[cfg(windows)]
pub fn is_enabled() -> Result<bool> {
    Ok(current_registration()?.is_some())
}

#[cfg(not(windows))]
pub fn is_enabled() -> Result<bool> {
    Ok(false)
}

#[cfg(windows)]
fn enable() -> Result<()> {
    use anyhow::Context;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let executable_path =
        std::env::current_exe().context("failed to resolve current executable path")?;
    let command = format_autostart_command(&executable_path);

    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let (run_key, _) = current_user
        .create_subkey(RUN_KEY_PATH)
        .context("failed to open or create Windows Run key")?;
    run_key
        .set_value(APP_RUN_KEY_VALUE, &command)
        .context("failed to write protoncode run key value")?;
    Ok(())
}

#[cfg(windows)]
fn disable() -> Result<()> {
    use anyhow::Context;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let run_key = match current_user.open_subkey_with_flags(
        RUN_KEY_PATH,
        winreg::enums::KEY_SET_VALUE | winreg::enums::KEY_QUERY_VALUE,
    ) {
        Ok(run_key) => run_key,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("failed to open Windows Run key for removal"),
    };

    match run_key.delete_value(APP_RUN_KEY_VALUE) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to remove protoncode run key value"),
    }
}

#[cfg(test)]
mod tests {
    use super::{AUTOSTART_FLAG, format_autostart_command, has_autostart_flag};

    #[test]
    fn detects_autostart_flag() {
        assert!(has_autostart_flag(["protoncode.exe", AUTOSTART_FLAG]));
        assert!(!has_autostart_flag(["protoncode.exe"]));
    }

    #[test]
    fn formats_command_with_quotes() {
        let command = format_autostart_command(std::path::Path::new(
            r"C:\Program Files\protoncode\protoncode.exe",
        ));
        assert_eq!(
            command,
            format!(r#""C:\Program Files\protoncode\protoncode.exe" {AUTOSTART_FLAG}"#)
        );
    }
}

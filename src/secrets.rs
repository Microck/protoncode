use anyhow::{Context, Result};
use keyring::Entry;

const SERVICE_NAME: &str = "protoncode";
const ACCOUNT_NAME: &str = "proton-session";

#[derive(Debug, Default, Clone)]
pub struct SecretStore;

impl SecretStore {
    pub fn new() -> Self {
        Self
    }

    fn entry(&self) -> Result<Entry> {
        Entry::new(SERVICE_NAME, ACCOUNT_NAME).context("failed to access platform credential store")
    }

    pub fn save_session_marker(&self, marker: &str) -> Result<()> {
        self.entry()?
            .set_password(marker)
            .context("failed to persist session marker")
    }

    pub fn load_session_marker(&self) -> Result<Option<String>> {
        match self.entry()?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("failed to read session marker"),
        }
    }

    pub fn clear_session_marker(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("failed to remove session marker"),
        }
    }
}

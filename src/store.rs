//! Persistence of the logged-in session and of user settings.
//!
//! The session file holds live credentials, so it is written `0600` and replaced
//! atomically (write to a temp file, then rename) so a crash mid-write cannot leave a
//! truncated file behind and log the user out.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::auth::session::Account;
use crate::auth::token::Tokens;
use crate::paths;

/// Which of the two credential styles this login produced. A Jagex account plays via a
/// game session; a legacy RuneScape account plays via the raw OAuth tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountSession {
    Jagex {
        session_id: String,
        accounts: Vec<Account>,
    },
    Runescape {
        display_name: String,
    },
}

impl AccountSession {
    /// Character display names, in the order they should be listed.
    pub fn character_names(&self) -> Vec<String> {
        match self {
            Self::Jagex { accounts, .. } => {
                accounts.iter().map(|a| a.display_name.clone()).collect()
            }
            Self::Runescape { display_name } => vec![display_name.clone()],
        }
    }
}

/// Everything needed to launch a game without showing the login window again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub tokens: Tokens,
    pub account: AccountSession,
    /// The account's own name (`DisplayName#1a2b` for a Jagex account), shown in the
    /// header so it is obvious which account is signed in.
    #[serde(default)]
    pub account_name: Option<String>,
    /// Display name last launched, so the UI can preselect it.
    #[serde(default)]
    pub selected_character: Option<String>,
}

impl Session {
    pub fn load() -> Result<Option<Self>> {
        let path = paths::session_file()?;
        read_json(&path)
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::session_file()?;
        write_json_private(&path, self)
    }

    pub fn clear() -> Result<()> {
        let path = paths::session_file()?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("could not remove {}", path.display())),
        }
    }
}

/// User settings. Every field has a working default, so a missing or partial file is fine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Use this jar instead of downloading RuneLite from GitHub.
    pub runelite_custom_jar: Option<String>,
    /// Wrapper command for each client, e.g. `gamemoderun %command%`.
    pub runelite_launch_command: Option<String>,
    pub rs3_launch_command: Option<String>,
    pub osrs_launch_command: Option<String>,
    /// Overrides the default RS3 `--configURI`.
    pub rs3_config_uri: Option<String>,
    /// Hide the launcher window after a successful launch.
    pub close_after_launch: bool,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = paths::config_file()?;
        Ok(read_json(&path)?.unwrap_or_default())
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::config_file()?;
        write_json_private(&path, self)
    }
}

/// Reads and parses a JSON file, treating "not there" as `None`.
///
/// A file that exists but does not parse is also treated as absent rather than fatal: a
/// corrupt session should send the user back to the login window, not wedge the launcher.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    Ok(serde_json::from_str(&text).ok())
}

fn write_json_private<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension("tmp");

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("could not open {}", tmp.display()))?;
    file.write_all(&text)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp, path)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

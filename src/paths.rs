//! XDG locations used by the launcher.
//!
//! The data dir doubles as the `HOME` we hand to game clients, so their dotfiles
//! (`.runelite`, `.java`, the RS3 client's settings) land in our directory instead of
//! polluting the user's real home. Bolt does the same thing, which is why an existing
//! Bolt install has a `~/.local/share/bolt-launcher/.runelite` in it.

use anyhow::{Context, Result};
use std::path::PathBuf;

pub const APP_NAME: &str = "rsclient";

/// `~/.local/share/rsclient` — game binaries, jars, session state, and the games' fake `HOME`.
pub fn data_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("could not determine XDG data dir")?
        .join(APP_NAME);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create data dir {}", dir.display()))?;
    Ok(dir)
}

/// `~/.config/rsclient` — user settings only.
pub fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("could not determine XDG config dir")?
        .join(APP_NAME);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create config dir {}", dir.display()))?;
    Ok(dir)
}

pub fn session_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("session.json"))
}

pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

/// Where a previous Bolt install keeps its data. We only ever read from this, to salvage
/// an already-downloaded `runelite.jar` and save the user a download.
pub fn bolt_data_dir() -> Option<PathBuf> {
    let dir = dirs::data_dir()?.join("bolt-launcher");
    dir.is_dir().then_some(dir)
}

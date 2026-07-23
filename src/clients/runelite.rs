//! RuneLite: fetch the launcher jar from GitHub releases and run it under Java.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

use super::{Launch, download, find_java, write_file};
use crate::log::Log;
use crate::paths;

const RELEASES_URL: &str = "https://api.github.com/repos/runelite/launcher/releases";
const JAR_NAME: &str = "runelite.jar";

#[derive(Deserialize)]
struct Release {
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize, Clone)]
struct Asset {
    name: String,
    id: u64,
    size: u64,
    browser_download_url: String,
}

/// Picks the newest `runelite.jar` across all releases.
///
/// The releases endpoint is ordered newest-first, and the asset name is matched
/// case-insensitively because the release asset is actually published as `RuneLite.jar`.
fn newest_jar(releases: &[Release]) -> Option<Asset> {
    releases
        .iter()
        .flat_map(|release| release.assets.iter())
        .find(|asset| asset.name.eq_ignore_ascii_case(JAR_NAME))
        .cloned()
}

fn jar_path() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join(JAR_NAME))
}

fn installed_id_path() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("runelite_id"))
}

/// Copies an already-downloaded jar out of an old Bolt install, if we have none.
///
/// Purely a convenience: it saves a ~2.5 MiB download on first run. The jar is
/// RuneLite's own signed launcher, which verifies itself on startup, so an unusable copy
/// would fail loudly rather than silently.
fn seed_from_bolt(log: &Log) -> Result<bool> {
    let destination = jar_path()?;
    if destination.exists() {
        return Ok(false);
    }
    let Some(source) = paths::bolt_data_dir().map(|d| d.join(JAR_NAME)) else {
        return Ok(false);
    };
    if !source.is_file() {
        return Ok(false);
    }

    std::fs::copy(&source, &destination)
        .with_context(|| format!("could not copy {}", source.display()))?;
    log.info("reused the runelite.jar from your existing Bolt install");
    // No id recorded, so the next update check still runs and will replace this if stale.
    Ok(true)
}

/// Ensures a usable `runelite.jar` exists, downloading or updating it as needed.
///
/// A failed update check is not fatal when a jar is already present — being offline
/// should not stop you playing.
fn ensure_jar(client: &reqwest::blocking::Client, log: &Log) -> Result<PathBuf> {
    let jar = jar_path()?;
    let id_file = installed_id_path()?;
    let installed_id = std::fs::read_to_string(&id_file).ok();

    seed_from_bolt(log)?;

    let check = (|| -> Result<Option<Asset>> {
        let releases: Vec<Release> = client
            .get(RELEASES_URL)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()?
            .error_for_status()?
            .json()
            .context("could not parse the GitHub releases response")?;
        Ok(newest_jar(&releases))
    })();

    let asset = match check {
        Ok(Some(asset)) => asset,
        Ok(None) => {
            log.info("no runelite.jar in the GitHub releases; using the installed copy");
            return existing_jar(&jar);
        }
        Err(e) if jar.is_file() => {
            log.info(format!("update check failed ({e}); using the installed copy"));
            return Ok(jar);
        }
        Err(e) => return Err(e).context("could not check for RuneLite updates"),
    };

    if installed_id.as_deref() == Some(asset.id.to_string().as_str()) && jar.is_file() {
        log.info("RuneLite is up to date");
        return Ok(jar);
    }

    let bytes = download(
        client,
        &asset.browser_download_url,
        "RuneLite",
        Some(asset.size),
        log,
    )?;
    write_file(&jar, &bytes, 0o644)?;
    std::fs::write(&id_file, asset.id.to_string())?;
    Ok(jar)
}

fn existing_jar(jar: &std::path::Path) -> Result<PathBuf> {
    if jar.is_file() {
        Ok(jar.to_path_buf())
    } else {
        anyhow::bail!("RuneLite is not installed and no download is available")
    }
}

/// Prepares the RuneLite launch command.
///
/// `custom_jar` skips the download entirely and uses the user's own jar.
/// `configure` opens RuneLite's launcher settings dialog instead of playing.
pub fn prepare(
    client: &reqwest::blocking::Client,
    custom_jar: Option<&str>,
    configure: bool,
    log: &Log,
) -> Result<Launch> {
    let jar = match custom_jar.map(str::trim).filter(|s| !s.is_empty()) {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_file() {
                anyhow::bail!("the configured RuneLite jar does not exist: {}", path.display());
            }
            path
        }
        None => ensure_jar(client, log)?,
    };

    let java = find_java()?;
    let home = paths::data_dir()?;
    let home_str = home.to_string_lossy().into_owned();

    // `-Duser.home` steers this JVM, and `-J-Duser.home` is forwarded to the client JVM
    // that RuneLite's launcher goes on to start. Both are needed for the RuneLite profile
    // to land in our data directory rather than the user's real home.
    let mut args = vec![
        format!("-Duser.home={home_str}"),
        "-jar".to_string(),
        jar.to_string_lossy().into_owned(),
        format!("-J-Duser.home={home_str}"),
    ];
    if configure {
        args.push("--configure".to_string());
    }

    Ok(Launch {
        program: java,
        args,
        env: vec![("HOME".to_string(), home_str)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn releases_from(json: &str) -> Vec<Release> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn finds_the_jar_asset_regardless_of_case() {
        // The real releases feed publishes it as "RuneLite.jar".
        let releases = releases_from(
            r#"[{"assets":[
                 {"name":"RuneLite.jar","id":485157515,"size":2505172,
                  "browser_download_url":"https://example.invalid/RuneLite.jar"}]}]"#,
        );
        let asset = newest_jar(&releases).unwrap();
        assert_eq!(asset.id, 485157515);
        assert_eq!(asset.size, 2505172);
    }

    #[test]
    fn prefers_the_newest_release_and_skips_other_assets() {
        let releases = releases_from(
            r#"[
                {"assets":[
                    {"name":"RuneLite.exe","id":1,"size":1,"browser_download_url":"u1"},
                    {"name":"runelite.jar","id":2,"size":2,"browser_download_url":"u2"}]},
                {"assets":[
                    {"name":"runelite.jar","id":3,"size":3,"browser_download_url":"u3"}]}
            ]"#,
        );
        // releases are newest-first, so the first jar encountered is the newest
        assert_eq!(newest_jar(&releases).unwrap().id, 2);
    }

    #[test]
    fn missing_jar_asset_is_reported_as_absent() {
        let releases = releases_from(r#"[{"assets":[{"name":"RuneLite.exe","id":1,"size":1,"browser_download_url":"u"}]}]"#);
        assert!(newest_jar(&releases).is_none());
        assert!(newest_jar(&releases_from("[]")).is_none());
    }

    #[test]
    fn releases_without_assets_are_tolerated() {
        let releases = releases_from(r#"[{},{"assets":[]}]"#);
        assert!(newest_jar(&releases).is_none());
    }
}

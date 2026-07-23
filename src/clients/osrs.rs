//! The official Old School client, which Jagex only ships for Windows — so it runs under
//! Wine (or umu-run/Proton).
//!
//! There is no plain download URL. Jagex serve it through "direct6", a content-addressed
//! CDN: a chain of JWT-shaped metadata documents leads to a list of gzipped chunks which
//! concatenate into one blob, and the `.exe` is a byte range inside that blob.
//!
//! Note that RuneLite already plays Old School natively, so this path only matters to
//! someone who specifically wants Jagex's own client.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::io::Read;
use std::path::PathBuf;

use super::{Launch, find_in_path, write_file};
use crate::auth::jwt;
use crate::log::Log;
use crate::paths;

const DIRECT6_URL: &str = "https://jagex.akamaized.net/direct6/";
const PLATFORM: &str = "osrs-win";

/// umu-launcher's app id for Old School RuneScape, used when running under Proton.
const UMU_GAME_ID: &str = "1343370";

#[derive(Deserialize)]
struct Environments {
    environments: Environment,
}

#[derive(Deserialize)]
struct Environment {
    production: Production,
}

#[derive(Deserialize)]
struct Production {
    id: String,
    version: String,
}

#[derive(Deserialize)]
struct Catalog {
    metafile: String,
    config: CatalogConfig,
}

#[derive(Deserialize)]
struct CatalogConfig {
    remote: CatalogRemote,
}

#[derive(Deserialize)]
struct CatalogRemote {
    #[serde(rename = "baseUrl")]
    base_url: String,
    #[serde(rename = "pieceFormat")]
    piece_format: String,
}

#[derive(Deserialize)]
struct Metafile {
    pieces: Pieces,
    files: Vec<MetaFileEntry>,
}

#[derive(Deserialize)]
struct Pieces {
    digests: Vec<String>,
}

#[derive(Deserialize)]
struct MetaFileEntry {
    name: String,
    size: u64,
}

/// Rewrites CDN URLs that are unusable as published.
///
/// The catalog hands out plain-HTTP URLs, and one host (`*-akamai.aws.snxd.com`) has no
/// valid certificate — its Akamai alias does. Both fixes are needed to fetch over TLS.
fn fix_url(url: &str) -> String {
    const SUFFIX: &str = "-akamai.aws.snxd.com/";
    if let Some(rest) = url.strip_prefix("http://") {
        if let Some((host_prefix, path)) = rest.split_once(SUFFIX)
            && host_prefix.len() == 5
        {
            return format!("https://{host_prefix}.akamaized.net/{path}");
        }
        return format!("https://{rest}");
    }
    url.to_string()
}

/// Builds the URL of one chunk from its digest.
///
/// `pieceFormat` looks like `{SubString:0,2,{TargetDigest}}/{TargetDigest}`; the nested
/// placeholder must be substituted first, or the outer one would be corrupted by the
/// inner replacement.
fn piece_url(base_url: &str, piece_format: &str, digest_hex: &str) -> Result<String> {
    if digest_hex.len() < 2 {
        bail!("chunk digest was too short: {digest_hex}");
    }
    let path = piece_format
        .replace("{SubString:0,2,{TargetDigest}}", &digest_hex[..2])
        .replace("{TargetDigest}", digest_hex);
    Ok(format!("{}{}", fix_url(base_url), path))
}

/// Decodes a base64 chunk digest into the hex form the URL uses.
fn digest_to_hex(digest_b64: &str) -> Result<String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(digest_b64)
        .context("could not decode a chunk digest")?;
    Ok(hex::encode(bytes))
}

/// Locates the client executable within the concatenated blob.
///
/// The blob is the files in `files` laid end to end, so the executable's offset is the
/// sum of the sizes before it.
fn locate_exe(files: &[MetaFileEntry]) -> Result<(u64, u64)> {
    let mut offset = 0u64;
    let mut found: Option<(u64, u64)> = None;

    for file in files {
        if file.name.to_ascii_lowercase().ends_with(".exe") {
            if found.is_some() {
                bail!(
                    "the OSRS metafile listed more than one .exe — cannot tell which is the client"
                );
            }
            found = Some((offset, file.size));
        }
        offset += file.size;
    }

    found.context("the OSRS metafile listed no .exe")
}

/// Fetches a direct6 metadata document and decodes its JWT-shaped payload.
fn fetch_metadata<T: serde::de::DeserializeOwned>(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<T> {
    let text = client
        .get(url)
        .send()
        .with_context(|| format!("could not fetch {url}"))?
        .error_for_status()?
        .text()?;

    let payload = text
        .split('.')
        .nth(1)
        .with_context(|| format!("{url} was not in the expected token format"))?;
    let bytes = jwt::decode_segment(payload)
        .with_context(|| format!("could not decode the payload of {url}"))?;
    serde_json::from_slice(&bytes).with_context(|| format!("could not parse {url}"))
}

/// Downloads every chunk and concatenates them into the full blob.
///
/// Each chunk is gzip, but with a 6-byte header of Jagex's own in front that has to be
/// skipped before the gzip stream begins.
fn download_blob(
    client: &reqwest::blocking::Client,
    base_url: &str,
    piece_format: &str,
    digests: &[String],
    log: &Log,
) -> Result<Vec<u8>> {
    const CHUNK_PREFIX: usize = 6;

    log.info("downloading the OSRS client...");
    let mut blob = Vec::new();

    for (index, digest) in digests.iter().enumerate() {
        let hex = digest_to_hex(digest)?;
        let url = piece_url(base_url, piece_format, &hex)?;

        let compressed = client
            .get(&url)
            .send()
            .with_context(|| format!("could not fetch chunk {}", &hex[..8.min(hex.len())]))?
            .error_for_status()?
            .bytes()?;

        if compressed.len() <= CHUNK_PREFIX {
            bail!("chunk {} was truncated", &hex[..8.min(hex.len())]);
        }

        let mut decoder = flate2::read::GzDecoder::new(&compressed[CHUNK_PREFIX..]);
        decoder
            .read_to_end(&mut blob)
            .with_context(|| format!("could not decompress chunk {}", &hex[..8.min(hex.len())]))?;

        let percent = (index + 1) * 100 / digests.len();
        log.replace_last(format!("downloading the OSRS client... {percent}%"));
    }

    log.replace_last(format!(
        "downloading the OSRS client... done ({:.1} MiB)",
        blob.len() as f64 / (1024.0 * 1024.0)
    ));
    Ok(blob)
}

fn exe_path() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("osrs-win.exe"))
}

fn installed_id_path() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("osrs_id"))
}

/// Ensures the OSRS executable is present and current.
fn ensure_exe(client: &reqwest::blocking::Client, log: &Log) -> Result<PathBuf> {
    let exe = exe_path()?;
    let id_file = installed_id_path()?;
    let installed_id = std::fs::read_to_string(&id_file).ok();

    let environments: Result<Environments> =
        fetch_metadata(client, &format!("{DIRECT6_URL}{PLATFORM}/{PLATFORM}.json"));

    let production = match environments {
        Ok(e) => e.environments.production,
        Err(e) if exe.is_file() => {
            log.info(format!(
                "update check failed ({e}); using the installed client"
            ));
            return Ok(exe);
        }
        Err(e) => return Err(e).context("could not check for OSRS client updates"),
    };

    if installed_id.as_deref() == Some(production.id.as_str()) && exe.is_file() {
        log.info("the OSRS client is up to date");
        return Ok(exe);
    }
    log.info(format!(
        "updating the OSRS client to {}",
        production.version
    ));

    let catalog: Catalog = fetch_metadata(
        client,
        &format!(
            "{DIRECT6_URL}{PLATFORM}/catalog/{}/catalog.json",
            production.id
        ),
    )?;
    let metafile: Metafile = fetch_metadata(client, &fix_url(&catalog.metafile))?;

    let (offset, size) = locate_exe(&metafile.files)?;
    let blob = download_blob(
        client,
        &catalog.config.remote.base_url,
        &catalog.config.remote.piece_format,
        &metafile.pieces.digests,
        log,
    )?;

    let end = offset
        .checked_add(size)
        .filter(|end| *end as usize <= blob.len())
        .context("the OSRS download was shorter than its file listing claimed")?;
    write_file(&exe, &blob[offset as usize..end as usize], 0o755)?;
    std::fs::write(&id_file, &production.id)?;

    Ok(exe)
}

/// Finds a Windows compatibility runner, preferring umu-run for its Proton integration.
fn find_wine() -> Result<PathBuf> {
    find_in_path("umu-run")
        .or_else(|| find_in_path("wine"))
        .context(
            "could not find `umu-run` or `wine` on PATH. The official OSRS client is a \
             Windows binary and needs one of them — install `wine` (`pacman -S wine`) or \
             umu-launcher. RuneLite plays Old School natively and needs neither.",
        )
}

/// Prepares the OSRS launch command.
pub fn prepare(client: &reqwest::blocking::Client, log: &Log) -> Result<Launch> {
    // Check for a runner before downloading ~100 MiB that could not be run anyway.
    let wine = find_wine()?;
    let exe = ensure_exe(client, log)?;
    let prefix = paths::data_dir()?.join("osrs-wineprefix");

    Ok(Launch {
        program: wine,
        args: vec![exe.to_string_lossy().into_owned()],
        env: vec![
            (
                "WINEPREFIX".to_string(),
                prefix.to_string_lossy().into_owned(),
            ),
            // Only meaningful under umu-run; harmless under plain wine.
            ("GAMEID".to_string(), UMU_GAME_ID.to_string()),
            ("PROTONPATH".to_string(), "GE-Latest".to_string()),
            ("PROTON_VERB".to_string(), "runinprefix".to_string()),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_the_certificateless_akamai_host() {
        assert_eq!(
            fix_url("http://abcde-akamai.aws.snxd.com/some/path/file"),
            "https://abcde.akamaized.net/some/path/file"
        );
    }

    #[test]
    fn upgrades_plain_http_to_https() {
        assert_eq!(
            fix_url("http://jagex.akamaized.net/direct6/x"),
            "https://jagex.akamaized.net/direct6/x"
        );
    }

    #[test]
    fn leaves_https_urls_alone() {
        let url = "https://jagex.akamaized.net/direct6/x";
        assert_eq!(fix_url(url), url);
        // a host prefix of the wrong length is not the special case
        assert_eq!(
            fix_url("http://toolong-akamai.aws.snxd.com/p"),
            "https://toolong-akamai.aws.snxd.com/p"
        );
    }

    #[test]
    fn builds_a_chunk_url_substituting_the_nested_placeholder_first() {
        let url = piece_url(
            "http://abcde-akamai.aws.snxd.com/pieces/",
            "{SubString:0,2,{TargetDigest}}/{TargetDigest}",
            "ab12cd34",
        )
        .unwrap();
        assert_eq!(url, "https://abcde.akamaized.net/pieces/ab/ab12cd34");
    }

    #[test]
    fn rejects_an_unusably_short_digest() {
        assert!(piece_url("https://h/", "{TargetDigest}", "a").is_err());
    }

    #[test]
    fn converts_base64_digests_to_hex() {
        // 0xAB 0x12 0xCD
        assert_eq!(digest_to_hex("qxLN").unwrap(), "ab12cd");
        assert!(digest_to_hex("not valid base64!!").is_err());
    }

    fn entry(name: &str, size: u64) -> MetaFileEntry {
        MetaFileEntry {
            name: name.to_string(),
            size,
        }
    }

    #[test]
    fn locates_the_exe_by_summing_preceding_file_sizes() {
        let files = vec![
            entry("data/cache.dat", 100),
            entry("osclient.exe", 250),
            entry("readme.txt", 30),
        ];
        assert_eq!(locate_exe(&files).unwrap(), (100, 250));
    }

    #[test]
    fn an_exe_at_the_start_has_offset_zero() {
        let files = vec![entry("osclient.EXE", 42), entry("other", 1)];
        assert_eq!(locate_exe(&files).unwrap(), (0, 42));
    }

    #[test]
    fn an_ambiguous_or_absent_exe_is_an_error() {
        assert!(locate_exe(&[entry("a.dll", 1), entry("b.dat", 2)]).is_err());
        assert!(locate_exe(&[entry("a.exe", 1), entry("b.exe", 2)]).is_err());
        assert!(locate_exe(&[]).is_err());
    }

    #[test]
    fn parses_the_live_environments_document() {
        let doc: Environments = serde_json::from_str(
            r#"{"environments":{"production":{
                 "id":"06ef287e5c67494cbc1ccf63995854daaa6df94b3d216208b0bc7cc410c9997f",
                 "scanTime":1783707194,"promoteTime":1784021825,"version":"239.4"}}}"#,
        )
        .unwrap();
        assert_eq!(doc.environments.production.version, "239.4");
        assert!(doc.environments.production.id.starts_with("06ef287e"));
    }

    #[test]
    fn parses_a_catalog_document() {
        let catalog: Catalog = serde_json::from_str(
            r#"{"metafile":"http://abcde-akamai.aws.snxd.com/meta/x",
                "config":{"remote":{"baseUrl":"http://abcde-akamai.aws.snxd.com/p/",
                                    "pieceFormat":"{SubString:0,2,{TargetDigest}}/{TargetDigest}"}}}"#,
        )
        .unwrap();
        assert_eq!(
            fix_url(&catalog.metafile),
            "https://abcde.akamaized.net/meta/x"
        );
        assert_eq!(
            catalog.config.remote.piece_format,
            "{SubString:0,2,{TargetDigest}}/{TargetDigest}"
        );
    }
}

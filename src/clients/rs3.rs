//! RuneScape 3: Jagex ships a real Linux build as a Debian package. Fetch it, pull the
//! ELF out, and run it natively — no Wine involved.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::PathBuf;

use super::{Launch, download, write_file};
use crate::log::Log;
use crate::paths;

const CONTENT_URL: &str = "https://content.runescape.com/downloads/ubuntu/";
const PACKAGES_PATH: &str = "dists/trusty/non-free/binary-amd64/Packages";

pub const DEFAULT_CONFIG_URI: &str = "https://www.runescape.com/k=5/l=0/jav_config.ws";

/// Path of the game binary inside the package's `data.tar.xz`.
const GAME_PATH_IN_TAR: &str = "./usr/share/games/runescape-launcher/runescape";

/// The fields we need out of a Debian `Packages` index.
#[derive(Debug, PartialEq, Eq)]
struct PackageEntry {
    filename: String,
    size: u64,
    sha256: String,
}

/// Parses a Debian `Packages` index.
///
/// The format is `Key: value` lines; continuation lines start with a space and belong to
/// the previous field, so they must not be mistaken for new keys. There is only ever one
/// stanza in this particular index.
fn parse_packages(text: &str) -> Result<PackageEntry> {
    let mut filename = None;
    let mut size = None;
    let mut sha256 = None;

    for line in text.lines() {
        if line.starts_with([' ', '\t']) {
            continue; // continuation of the previous field
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "Filename" => filename = Some(value.to_string()),
            "Size" => size = value.parse().ok(),
            "SHA256" => sha256 = Some(value.to_string()),
            _ => {}
        }
    }

    Ok(PackageEntry {
        filename: filename.context("Packages index had no Filename")?,
        size: size.context("Packages index had no valid Size")?,
        sha256: sha256.context("Packages index had no SHA256")?,
    })
}

/// Pulls the game binary out of a `.deb`.
///
/// A `.deb` is an `ar` archive; the payload we want is the `data.tar.xz` member, inside
/// which is the ELF.
fn extract_game_binary(deb: &[u8]) -> Result<Vec<u8>> {
    let mut archive = ar::Archive::new(std::io::Cursor::new(deb));
    let mut data_tar_xz = None;

    while let Some(entry) = archive.next_entry() {
        let mut entry = entry.context("malformed .deb: could not read an ar member")?;
        let name = String::from_utf8_lossy(entry.header().identifier()).into_owned();
        if name.trim_end_matches('/') == "data.tar.xz" {
            let mut buffer = Vec::with_capacity(entry.header().size() as usize);
            entry.read_to_end(&mut buffer)?;
            data_tar_xz = Some(buffer);
            break;
        }
    }

    let data_tar_xz = data_tar_xz.context("the .deb contained no data.tar.xz")?;
    let decoder = xz2::read::XzDecoder::new(std::io::Cursor::new(data_tar_xz));
    let mut tar = tar::Archive::new(decoder);

    for entry in tar.entries().context("malformed data.tar.xz")? {
        let mut entry = entry.context("malformed data.tar.xz")?;
        let path = entry.path()?.to_string_lossy().into_owned();
        if normalise_tar_path(&path) == normalise_tar_path(GAME_PATH_IN_TAR) {
            let mut buffer = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buffer)?;
            return Ok(buffer);
        }
    }

    bail!("the RS3 package did not contain {GAME_PATH_IN_TAR}");
}

/// Strips the leading `./` that tar members in a `.deb` carry.
///
/// Whether it survives depends on the writer and on the reader's path normalisation, so
/// both sides of a comparison get normalised rather than assuming either form.
fn normalise_tar_path(path: &str) -> &str {
    path.strip_prefix("./").unwrap_or(path)
}

fn binary_path() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("rs3-linux"))
}

fn installed_hash_path() -> Result<PathBuf> {
    Ok(paths::data_dir()?.join("rs3_hash"))
}

/// Ensures the RS3 binary is present and current.
fn ensure_binary(client: &reqwest::blocking::Client, log: &Log) -> Result<PathBuf> {
    let binary = binary_path()?;
    let hash_file = installed_hash_path()?;
    let installed_hash = std::fs::read_to_string(&hash_file).ok();

    let index = client
        .get(format!("{CONTENT_URL}{PACKAGES_PATH}"))
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text());

    let entry = match index {
        Ok(text) => parse_packages(&text)?,
        Err(e) if binary.is_file() => {
            log.info(format!("update check failed ({e}); using the installed client"));
            return Ok(binary);
        }
        Err(e) => return Err(e).context("could not fetch the RS3 package index"),
    };

    if installed_hash.as_deref() == Some(entry.sha256.as_str()) && binary.is_file() {
        log.info("the RS3 client is up to date");
        return Ok(binary);
    }

    let deb = download(
        client,
        &format!("{CONTENT_URL}{}", entry.filename),
        "the RS3 client",
        Some(entry.size),
        log,
    )?;

    verify_sha256(&deb, &entry.sha256)?;

    log.info("extracting the RS3 client...");
    let game = extract_game_binary(&deb)?;
    write_file(&binary, &game, 0o755)?;
    std::fs::write(&hash_file, &entry.sha256)?;

    Ok(binary)
}

/// Checks the download against the digest in the package index.
///
/// This is the only integrity check available for the RS3 client — the package index is
/// fetched over TLS, so a matching digest means the bytes are the ones Jagex published.
fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let actual = hex::encode(Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("the downloaded RS3 package did not match its published checksum — refusing to run it");
    }
    Ok(())
}

/// Prepares the RS3 launch command.
pub fn prepare(
    client: &reqwest::blocking::Client,
    config_uri: Option<&str>,
    log: &Log,
) -> Result<Launch> {
    let binary = ensure_binary(client, log)?;
    let home = paths::data_dir()?;
    let config_uri = config_uri
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_CONFIG_URI);

    Ok(Launch {
        program: binary,
        args: vec!["--configURI".to_string(), config_uri.to_string()],
        env: vec![
            ("HOME".to_string(), home.to_string_lossy().into_owned()),
            // The client's SDL2 has no Wayland backend, so pin it to X11 (XWayland).
            // Without this it fails to open a window on a Wayland session.
            ("SDL_VIDEODRIVER".to_string(), "x11".to_string()),
            ("SDL_VIDEO_X11_WMCLASS".to_string(), "RuneScape".to_string()),
            (
                "PULSE_PROP_OVERRIDE".to_string(),
                "application.name='RuneScape' application.icon_name='runescape' media.role='game'"
                    .to_string(),
            ),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed copy of the live index at the time of writing.
    const SAMPLE_INDEX: &str = "\
Package: runescape-launcher
Version: 2.2.12
Architecture: amd64
Maintainer: RuneScape Linux <noreply@jagex.com>
Filename: pool/non-free/r/runescape-launcher/runescape-launcher_2.2.12_amd64.deb
Size: 3600532
SHA256: 49a594c1c77113dff14b696f9c1308ed1c1a7e9166dd39275e8597ec0529fa04
SHA1: f9b8cfa7fdbbff9c2ef66dbfc409642c8be584b6
MD5sum: c9c1eccc115bb2589a78617e8021f70c
Description: RuneScape Game Client
 RuneScape is a massively multiplayer online role-playing game.
 Size: 999999999
";

    #[test]
    fn parses_the_live_packages_index() {
        let entry = parse_packages(SAMPLE_INDEX).unwrap();
        assert_eq!(
            entry.filename,
            "pool/non-free/r/runescape-launcher/runescape-launcher_2.2.12_amd64.deb"
        );
        assert_eq!(entry.size, 3600532);
        assert_eq!(
            entry.sha256,
            "49a594c1c77113dff14b696f9c1308ed1c1a7e9166dd39275e8597ec0529fa04"
        );
    }

    #[test]
    fn indented_continuation_lines_do_not_override_real_fields() {
        // The Description block contains an indented "Size:" line that must be ignored.
        assert_eq!(parse_packages(SAMPLE_INDEX).unwrap().size, 3600532);
    }

    #[test]
    fn an_incomplete_index_is_an_error() {
        assert!(parse_packages("Package: x\nVersion: 1\n").is_err());
        assert!(parse_packages("Filename: a.deb\nSHA256: abc\n").is_err());
        assert!(parse_packages("Filename: a.deb\nSize: notanumber\nSHA256: abc\n").is_err());
    }

    #[test]
    fn checksum_mismatch_is_refused() {
        let expected = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"real"));
        assert!(verify_sha256(b"real", &expected).is_ok());
        // case differences in the published digest are fine
        assert!(verify_sha256(b"real", &expected.to_uppercase()).is_ok());
        assert!(verify_sha256(b"tampered", &expected).is_err());
    }

    /// Builds a `.deb`-shaped archive around a tar.xz containing one named file.
    fn fake_deb(inner_path: &str, contents: &[u8]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, inner_path, contents).unwrap();
            builder.finish().unwrap();
        }

        let mut xz_bytes = Vec::new();
        {
            let mut encoder = xz2::write::XzEncoder::new(&mut xz_bytes, 1);
            std::io::Write::write_all(&mut encoder, &tar_bytes).unwrap();
            encoder.finish().unwrap();
        }

        let mut deb = Vec::new();
        {
            let mut builder = ar::Builder::new(&mut deb);
            builder
                .append(&ar::Header::new(b"debian-binary".to_vec(), 4), &b"2.0\n"[..])
                .unwrap();
            builder
                .append(
                    &ar::Header::new(b"data.tar.xz".to_vec(), xz_bytes.len() as u64),
                    &xz_bytes[..],
                )
                .unwrap();
        }
        deb
    }

    #[test]
    fn extracts_the_game_binary_from_a_deb() {
        let deb = fake_deb(GAME_PATH_IN_TAR, b"\x7fELF fake game binary");
        assert_eq!(
            extract_game_binary(&deb).unwrap(),
            b"\x7fELF fake game binary"
        );
    }

    #[test]
    fn a_deb_without_the_game_binary_is_an_error() {
        let deb = fake_deb("./usr/share/doc/runescape-launcher/copyright", b"legal text");
        let err = extract_game_binary(&deb).unwrap_err().to_string();
        assert!(err.contains(GAME_PATH_IN_TAR), "unexpected error: {err}");
    }

    #[test]
    fn a_non_deb_is_rejected_rather_than_panicking() {
        assert!(extract_game_binary(b"this is not an ar archive").is_err());
    }
}

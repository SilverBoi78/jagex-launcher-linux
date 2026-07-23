//! Downloading and launching the game clients.

pub mod osrs;
pub mod rs3;
pub mod runelite;

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::log::Log;
use crate::store::{AccountSession, Session};

/// The credentials a game client reads out of its environment.
///
/// Jagex accounts authenticate with a game session; legacy RuneScape accounts with the
/// raw OAuth tokens. These are mutually exclusive — setting both makes RuneLite reject
/// the login — which is why this is an enum rather than a struct of optional fields.
pub enum JxEnv {
    Jagex {
        session_id: String,
        character_id: String,
        display_name: String,
    },
    Runescape {
        access_token: String,
        refresh_token: String,
        display_name: String,
    },
}

impl JxEnv {
    /// Builds the environment for the named character of a stored session.
    pub fn for_character(session: &Session, display_name: &str) -> Result<Self> {
        match &session.account {
            AccountSession::Jagex {
                session_id,
                accounts,
            } => {
                let account = accounts
                    .iter()
                    .find(|a| a.display_name.eq_ignore_ascii_case(display_name))
                    .with_context(|| {
                        format!("no character named {display_name} on this account")
                    })?;
                Ok(Self::Jagex {
                    session_id: session_id.clone(),
                    character_id: account.account_id.clone(),
                    display_name: account.display_name.clone(),
                })
            }
            AccountSession::Runescape { display_name } => Ok(Self::Runescape {
                access_token: session.tokens.access_token.clone(),
                refresh_token: session.tokens.refresh_token.clone(),
                display_name: display_name.clone(),
            }),
        }
    }

    fn vars(&self) -> Vec<(&'static str, &str)> {
        match self {
            Self::Jagex {
                session_id,
                character_id,
                display_name,
            } => vec![
                ("JX_SESSION_ID", session_id),
                ("JX_CHARACTER_ID", character_id),
                ("JX_DISPLAY_NAME", display_name),
            ],
            Self::Runescape {
                access_token,
                refresh_token,
                display_name,
            } => vec![
                ("JX_ACCESS_TOKEN", access_token),
                ("JX_REFRESH_TOKEN", refresh_token),
                ("JX_DISPLAY_NAME", display_name),
            ],
        }
    }
}

/// Refreshes the OAuth tokens if they are at or near expiry, saving the session if so.
///
/// This only matters for legacy RuneScape accounts, which hand the access token straight
/// to the game — a stale one means the game cannot log in. A Jagex account plays from its
/// session id, which has its own lifetime and is checked separately.
///
/// Returns whether the session changed, so the caller can refresh what it is holding.
pub fn refresh_tokens_if_needed(
    session: &mut Session,
    client: &reqwest::blocking::Client,
    log: &Log,
) -> Result<bool> {
    if !matches!(session.account, AccountSession::Runescape { .. }) || !session.tokens.is_expired()
    {
        return Ok(false);
    }

    log.info("access token expired, refreshing...");
    session.tokens = crate::auth::token::refresh(client, &session.tokens)
        .context("could not refresh the session — you may need to sign in again")?;
    session.save()?;
    Ok(true)
}

/// A command to run, before the user's optional wrapper is applied.
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Extra environment on top of the `JX_*` variables.
    pub env: Vec<(String, String)>,
}

/// Spawns a game client, fully detached.
///
/// The child gets its own session (`setsid`) and null stdio, so closing the launcher — or
/// the terminal it was started from — does not take the game down with it.
pub fn spawn(launch: Launch, jx: &JxEnv, wrapper: Option<&str>, log: &Log) -> Result<u32> {
    let (program, args) = apply_wrapper(&launch.program, &launch.args, wrapper)?;

    let mut command = Command::new(&program);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    for (key, value) in &launch.env {
        command.env(key, value);
    }
    for (key, value) in jx.vars() {
        command.env(key, value);
    }

    // SAFETY: `setsid` is async-signal-safe, which is the requirement for anything run
    // between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let child = command
        .spawn()
        .with_context(|| format!("could not start {}", program.display()))?;

    log.info(format!(
        "launched {} (pid {})",
        program.display(),
        child.id()
    ));
    Ok(child.id())
}

/// Applies a user-supplied wrapper command such as `gamemoderun %command%`.
///
/// `%command%` is replaced by the real program and its arguments, matching the convention
/// Steam uses. Without the placeholder, the real command is appended.
fn apply_wrapper(
    program: &Path,
    args: &[String],
    wrapper: Option<&str>,
) -> Result<(PathBuf, Vec<String>)> {
    let Some(wrapper) = wrapper.map(str::trim).filter(|w| !w.is_empty()) else {
        return Ok((program.to_path_buf(), args.to_vec()));
    };

    let real: Vec<String> = std::iter::once(program.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .collect();

    let mut parts: Vec<String> = Vec::new();
    let mut substituted = false;
    for token in wrapper.split_whitespace() {
        if token == "%command%" {
            parts.extend(real.iter().cloned());
            substituted = true;
        } else {
            parts.push(token.to_string());
        }
    }
    if !substituted {
        parts.extend(real.iter().cloned());
    }

    let mut parts = parts.into_iter();
    let program = parts.next().context("launch command was empty")?;
    Ok((PathBuf::from(program), parts.collect()))
}

/// Finds an executable by name on `PATH`.
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Locates a Java runtime: `JAVA_HOME` first, then `PATH`.
pub fn find_java() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let candidate = PathBuf::from(home).join("bin/java");
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    find_in_path("java").context(
        "could not find Java: JAVA_HOME is unset or does not point at a JDK, \
         and there is no `java` on PATH. Install a JRE (e.g. `pacman -S jre17-openjdk`).",
    )
}

/// Downloads a URL into memory, reporting progress on `log`.
///
/// `expected_size` is a fallback for servers that do not send `Content-Length`.
pub fn download(
    client: &reqwest::blocking::Client,
    url: &str,
    what: &str,
    expected_size: Option<u64>,
    log: &Log,
) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("could not fetch {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("{url} returned {status}");
    }

    let total = response.content_length().or(expected_size);
    log.info(format!("downloading {what}..."));

    let mut buffer = match total {
        Some(size) => Vec::with_capacity(size as usize),
        None => Vec::new(),
    };
    let mut reader = response;
    let mut chunk = vec![0u8; 64 * 1024];
    let mut last_reported = 0u64;

    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);

        // Report each whole percent, so a big download does not spam the log.
        if let Some(total) = total.filter(|t| *t > 0) {
            let percent = buffer.len() as u64 * 100 / total;
            if percent > last_reported {
                last_reported = percent;
                log.replace_last(format!("downloading {what}... {percent}%"));
            }
        }
    }

    log.replace_last(format!(
        "downloading {what}... done ({:.1} MiB)",
        buffer.len() as f64 / (1024.0 * 1024.0)
    ));
    Ok(buffer)
}

/// Writes a file, creating parents, with the given permission bits.
pub fn write_file(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .with_context(|| {
            format!(
                "could not write {} — if the game is running, close it and try again",
                path.display()
            )
        })?;
    file.write_all(contents)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::jwt::IdToken;
    use crate::auth::session::Account;
    use crate::auth::token::Tokens;
    use std::time::{Duration, SystemTime};

    fn jagex_session() -> Session {
        Session {
            tokens: Tokens {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                id_token: IdToken::new("h.p.s"),
                expires_at: SystemTime::now() + Duration::from_secs(3600),
            },
            account_name: Some("Zezima#1a2b".into()),
            account: AccountSession::Jagex {
                session_id: "sess".into(),
                accounts: vec![
                    Account {
                        account_id: "id-1".into(),
                        display_name: "Zezima".into(),
                        user_hash: None,
                    },
                    Account {
                        account_id: "id-2".into(),
                        display_name: "Alt".into(),
                        user_hash: None,
                    },
                ],
            },
            selected_character: None,
        }
    }

    #[test]
    fn jagex_env_selects_the_named_character() {
        let env = JxEnv::for_character(&jagex_session(), "Alt").unwrap();
        let vars: std::collections::HashMap<_, _> = env.vars().into_iter().collect();
        assert_eq!(vars["JX_SESSION_ID"], "sess");
        assert_eq!(vars["JX_CHARACTER_ID"], "id-2");
        assert_eq!(vars["JX_DISPLAY_NAME"], "Alt");
        // the legacy variables must never appear alongside these
        assert!(!vars.contains_key("JX_ACCESS_TOKEN"));
        assert!(!vars.contains_key("JX_REFRESH_TOKEN"));
    }

    #[test]
    fn character_lookup_is_case_insensitive_and_rejects_unknown_names() {
        assert!(JxEnv::for_character(&jagex_session(), "zEzImA").is_ok());
        assert!(JxEnv::for_character(&jagex_session(), "Nobody").is_err());
    }

    #[test]
    fn legacy_accounts_use_tokens_and_never_a_session() {
        let mut session = jagex_session();
        session.account = AccountSession::Runescape {
            display_name: "Old Timer".into(),
        };
        let env = JxEnv::for_character(&session, "anything").unwrap();
        let vars: std::collections::HashMap<_, _> = env.vars().into_iter().collect();
        assert_eq!(vars["JX_ACCESS_TOKEN"], "at");
        assert_eq!(vars["JX_REFRESH_TOKEN"], "rt");
        assert_eq!(vars["JX_DISPLAY_NAME"], "Old Timer");
        assert!(!vars.contains_key("JX_SESSION_ID"));
        assert!(!vars.contains_key("JX_CHARACTER_ID"));
    }

    #[test]
    fn no_wrapper_leaves_the_command_alone() {
        let (program, args) = apply_wrapper(
            Path::new("/usr/bin/java"),
            &["-jar".into(), "rl.jar".into()],
            None,
        )
        .unwrap();
        assert_eq!(program, Path::new("/usr/bin/java"));
        assert_eq!(args, vec!["-jar", "rl.jar"]);

        // whitespace-only is treated as absent
        let (program, _) = apply_wrapper(Path::new("/usr/bin/java"), &[], Some("   ")).unwrap();
        assert_eq!(program, Path::new("/usr/bin/java"));
    }

    #[test]
    fn wrapper_substitutes_the_command_placeholder() {
        let (program, args) = apply_wrapper(
            Path::new("/usr/bin/java"),
            &["-jar".into(), "rl.jar".into()],
            Some("gamemoderun %command% --extra"),
        )
        .unwrap();
        assert_eq!(program, Path::new("gamemoderun"));
        assert_eq!(args, vec!["/usr/bin/java", "-jar", "rl.jar", "--extra"]);
    }

    #[test]
    fn wrapper_without_a_placeholder_prefixes_the_command() {
        let (program, args) = apply_wrapper(
            Path::new("/usr/bin/java"),
            &["-jar".into()],
            Some("strace -f"),
        )
        .unwrap();
        assert_eq!(program, Path::new("strace"));
        assert_eq!(args, vec!["-f", "/usr/bin/java", "-jar"]);
    }
}

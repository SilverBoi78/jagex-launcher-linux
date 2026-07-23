//! Game sessions and character lists.
//!
//! A Jagex account trades the consent `id_token` for a session id, then lists the
//! characters attached to it. A legacy RuneScape account has no session and no character
//! list — just the display name on its profile.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{AUTH_ORIGIN, PROFILE_ORIGIN, jwt::IdToken};

/// One playable character on a Jagex account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    #[serde(rename = "accountId")]
    pub account_id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "userHash", default)]
    pub user_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionResponse {
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct ProfileResponse {
    #[serde(default)]
    display_name: Option<String>,
}

/// Creates a game session from the consent leg's `id_token`.
pub fn create_session(client: &reqwest::blocking::Client, id_token: &IdToken) -> Result<String> {
    let response = client
        .post(format!("{AUTH_ORIGIN}/game-session/v1/sessions"))
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&serde_json::json!({ "idToken": id_token.encoded }))
        .send()
        .context("could not reach the game session service")?;

    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        bail!("game session request returned {status}: {}", body.trim());
    }

    let parsed: SessionResponse = serde_json::from_str(&body)
        .with_context(|| format!("could not parse the game session response: {}", body.trim()))?;
    Ok(parsed.session_id)
}

/// Lists the characters reachable with a session id.
///
/// A `401` here means the session has lapsed and the user has to log in again; that is
/// reported distinctly so callers can clear the stored session rather than show a generic
/// network error.
pub fn fetch_accounts(
    client: &reqwest::blocking::Client,
    session_id: &str,
) -> Result<Vec<Account>> {
    let response = client
        .get(format!("{AUTH_ORIGIN}/game-session/v1/accounts"))
        .header(reqwest::header::ACCEPT, "application/json")
        .bearer_auth(session_id)
        .send()
        .context("could not reach the game session service")?;

    let status = response.status();
    let body = response.text().unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        bail!("the game session has expired — please log in again");
    }
    if !status.is_success() {
        bail!("character list request returned {status}: {}", body.trim());
    }

    serde_json::from_str(&body)
        .with_context(|| format!("could not parse the character list: {}", body.trim()))
}

/// Fetches the display name of a legacy RuneScape account.
pub fn fetch_profile_display_name(
    client: &reqwest::blocking::Client,
    id_token: &IdToken,
) -> Result<String> {
    let response = client
        .get(format!("{PROFILE_ORIGIN}/rs-profile/v1/profile"))
        .header(reqwest::header::ACCEPT, "application/json")
        .bearer_auth(&id_token.encoded)
        .send()
        .context("could not reach the profile service")?;

    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        bail!("profile request returned {status}: {}", body.trim());
    }

    let parsed: ProfileResponse = serde_json::from_str(&body)
        .with_context(|| format!("could not parse the profile response: {}", body.trim()))?;
    parsed
        .display_name
        .context("this RuneScape account has no display name set — set one on the website first")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_character_list() {
        let accounts: Vec<Account> = serde_json::from_str(
            r#"[{"accountId":"id-1","displayName":"Zezima","userHash":"h1"},
                {"accountId":"id-2","displayName":"Alt"}]"#,
        )
        .unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].account_id, "id-1");
        assert_eq!(accounts[0].display_name, "Zezima");
        assert_eq!(accounts[0].user_hash.as_deref(), Some("h1"));
        // userHash is optional
        assert_eq!(accounts[1].user_hash, None);
    }

    #[test]
    fn parses_the_session_id() {
        let parsed: SessionResponse = serde_json::from_str(r#"{"sessionId":"sess-123"}"#).unwrap();
        assert_eq!(parsed.session_id, "sess-123");
    }

    #[test]
    fn parses_a_profile_without_a_display_name() {
        let parsed: ProfileResponse =
            serde_json::from_str(r#"{"display_name_set":false,"display_name":null}"#).unwrap();
        assert!(parsed.display_name.is_none());
    }
}

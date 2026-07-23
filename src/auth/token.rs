//! Token exchange and refresh against `account.jagex.com/oauth2/token`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

use super::{ACCOUNT_ORIGIN, LAUNCHER_CLIENT_ID, REDIRECT_URI, jwt::IdToken};

/// Refresh this long before actual expiry, so a token cannot go stale between the check
/// and the game reading it.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// The launcher leg's tokens, as persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: IdToken,
    pub expires_at: SystemTime,
}

impl Tokens {
    pub fn is_expired(&self) -> bool {
        SystemTime::now() + EXPIRY_MARGIN >= self.expires_at
    }
}

/// The wire format of a token response.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

impl TokenResponse {
    /// `previous` supplies fallbacks: a refresh response may legitimately omit the
    /// refresh token (meaning "keep using the one you have").
    fn into_tokens(self, previous: Option<&Tokens>) -> Result<Tokens> {
        let refresh_token = self
            .refresh_token
            .or_else(|| previous.map(|p| p.refresh_token.clone()))
            .context("token response had no refresh_token")?;
        let id_token = self
            .id_token
            .map(IdToken::new)
            .or_else(|| previous.map(|p| p.id_token.clone()))
            .context("token response had no id_token")?;
        let expires_in = self.expires_in.context("token response had no expires_in")?;

        Ok(Tokens {
            access_token: self.access_token,
            refresh_token,
            id_token,
            expires_at: SystemTime::now() + Duration::from_secs(expires_in),
        })
    }
}

/// Exchanges the launcher leg's authorization code for tokens.
pub fn exchange_code(client: &reqwest::blocking::Client, code: &str, verifier: &str) -> Result<Tokens> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", LAUNCHER_CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", REDIRECT_URI),
    ];
    post_token(client, &form, None).context("could not exchange the authorization code for tokens")
}

/// Trades the refresh token for a fresh access token.
pub fn refresh(client: &reqwest::blocking::Client, tokens: &Tokens) -> Result<Tokens> {
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", LAUNCHER_CLIENT_ID),
        ("refresh_token", tokens.refresh_token.as_str()),
    ];
    post_token(client, &form, Some(tokens)).context("could not refresh the session")
}

fn post_token(
    client: &reqwest::blocking::Client,
    form: &[(&str, &str)],
    previous: Option<&Tokens>,
) -> Result<Tokens> {
    let response = client
        .post(format!("{ACCOUNT_ORIGIN}/oauth2/token"))
        .header(reqwest::header::ACCEPT, "application/json")
        .form(form)
        .send()?;

    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        bail!("token endpoint returned {status}: {}", body.trim());
    }

    serde_json::from_str::<TokenResponse>(&body)
        .with_context(|| format!("could not parse the token response: {}", body.trim()))?
        .into_tokens(previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str, previous: Option<&Tokens>) -> Result<Tokens> {
        serde_json::from_str::<TokenResponse>(json)
            .unwrap()
            .into_tokens(previous)
    }

    #[test]
    fn parses_a_full_token_response() {
        let tokens = parse(
            r#"{"access_token":"at","refresh_token":"rt","id_token":"h.p.s",
                "expires_in":3600,"token_type":"Bearer","scope":"openid offline"}"#,
            None,
        )
        .unwrap();
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token, "rt");
        assert_eq!(tokens.id_token.encoded, "h.p.s");
        assert!(!tokens.is_expired());
    }

    #[test]
    fn a_refresh_may_omit_the_refresh_and_id_tokens() {
        let previous = parse(
            r#"{"access_token":"old","refresh_token":"rt","id_token":"h.p.s","expires_in":1}"#,
            None,
        )
        .unwrap();
        let refreshed = parse(r#"{"access_token":"new","expires_in":3600}"#, Some(&previous)).unwrap();
        assert_eq!(refreshed.access_token, "new");
        assert_eq!(refreshed.refresh_token, "rt");
        assert_eq!(refreshed.id_token.encoded, "h.p.s");
    }

    #[test]
    fn a_first_exchange_must_carry_everything() {
        assert!(parse(r#"{"access_token":"at","expires_in":3600}"#, None).is_err());
        assert!(
            parse(r#"{"access_token":"at","refresh_token":"rt","id_token":"h.p.s"}"#, None)
                .is_err(),
            "missing expires_in should be an error"
        );
    }

    #[test]
    fn expiry_uses_a_margin() {
        let nearly = parse(
            r#"{"access_token":"a","refresh_token":"r","id_token":"h.p.s","expires_in":10}"#,
            None,
        )
        .unwrap();
        assert!(
            nearly.is_expired(),
            "a token expiring inside the margin should count as expired"
        );
    }
}

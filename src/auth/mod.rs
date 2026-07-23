//! The Jagex login flow.
//!
//! Logging in takes two OAuth legs against `account.jagex.com`, then a session exchange
//! against `auth.jagex.com`:
//!
//! 1. **Launcher leg** — authorization code + PKCE, `client_id=com_jagex_auth_desktop_launcher`.
//!    The user authenticates in a real browser engine; the flow ends at a redirect to
//!    `secure.runescape.com/m=weblogin/launcher-redirect?code=…`. We cancel that navigation
//!    and exchange the code for tokens ourselves.
//! 2. **Consent leg** — hybrid `id_token code`, `client_id=1fddee4e-…`, driven by the
//!    `id_token` from leg 1. It ends at `http://localhost/#id_token=…`, which we also
//!    cancel — so, unlike other Linux launchers, nothing here ever needs to bind port 80.
//! 3. **Game session** — trade the consent `id_token` for a session id and the account's
//!    character list.
//!
//! Legacy (pre-Jagex-account) RuneScape logins skip legs 2 and 3 and play using the raw
//! OAuth tokens instead; leg 1's `id_token` says which kind of account this is.
//!
//! This module is pure: it builds URLs and interprets the ones the browser lands on, but
//! performs no I/O, so the whole state machine is unit-testable. [`token`] and [`session`]
//! do the HTTP.

pub mod jwt;
pub mod pkce;
pub mod session;
pub mod token;

use anyhow::{Result, bail};

use jwt::IdToken;
use pkce::Pkce;

pub const ACCOUNT_ORIGIN: &str = "https://account.jagex.com";
pub const AUTH_ORIGIN: &str = "https://auth.jagex.com";
pub const PROFILE_ORIGIN: &str = "https://secure.jagex.com";

/// The desktop launcher's OAuth client. Not a secret — it ships in every copy of the
/// official launcher and identifies the application, not the user.
pub const LAUNCHER_CLIENT_ID: &str = "com_jagex_auth_desktop_launcher";

/// The consent client, hard-coded PRODUCTION value.
pub const CONSENT_CLIENT_ID: &str = "1fddee4e-b100-4f4e-b2b0-097f9088f9d2";

/// Where the launcher leg lands. We never actually fetch it.
pub const REDIRECT_URI: &str = "https://secure.runescape.com/m=weblogin/launcher-redirect";
const REDIRECT_HOST: &str = "secure.runescape.com";
const REDIRECT_PATH: &str = "/m=weblogin/launcher-redirect";

/// Where the consent leg lands, in the URL fragment. Also never fetched.
pub const CONSENT_REDIRECT_URI: &str = "http://localhost";

const LAUNCHER_SCOPES: &str = concat!(
    "openid offline gamesso.token.create user.profile.read ",
    "user.entitlement.read user.game.read user.sku.read user.voucher.redeem",
);
const CONSENT_SCOPES: &str = "openid offline";

const STATE_LENGTH: usize = 32;
const NONCE_LENGTH: usize = 48;

/// What the browser landed on, once we recognise it as part of the flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redirect {
    /// End of the launcher leg: an authorization code.
    Launcher { code: String, state: String },
    /// End of the consent leg: an id token, delivered in the fragment.
    Consent { id_token: String, state: String },
}

/// Holds the per-attempt secrets and validates what comes back against them.
pub struct LoginFlow {
    pkce: Pkce,
    launcher_state: String,
    consent_state: String,
    nonce: String,
    /// `sub` from the launcher leg, to check the consent token belongs to the same user.
    sub: Option<String>,
}

impl Default for LoginFlow {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginFlow {
    pub fn new() -> Self {
        Self {
            pkce: Pkce::generate(),
            launcher_state: pkce::random_token(STATE_LENGTH),
            consent_state: pkce::random_token(STATE_LENGTH),
            nonce: pkce::random_token(NONCE_LENGTH),
            sub: None,
        }
    }

    pub fn verifier(&self) -> &str {
        &self.pkce.verifier
    }

    /// The URL to load first: Jagex's real login page.
    pub fn launcher_url(&self) -> String {
        // `auth_method` and `login_type` are sent empty, and `flow=launcher` is what makes
        // the page redirect to the launcher redirect URI when it is done.
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("auth_method", "")
            .append_pair("login_type", "")
            .append_pair("flow", "launcher")
            .append_pair("response_type", "code")
            .append_pair("client_id", LAUNCHER_CLIENT_ID)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &self.pkce.challenge)
            .append_pair("prompt", "login")
            .append_pair("scope", LAUNCHER_SCOPES)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("state", &self.launcher_state)
            .finish();
        format!("{ACCOUNT_ORIGIN}/oauth2/auth?{query}")
    }

    /// The URL to load second, once the launcher leg produced an `id_token`.
    ///
    /// This must be loaded in the *same* browser context as the launcher leg: it relies on
    /// the login cookies set there.
    pub fn consent_url(&self, id_token: &IdToken) -> String {
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("prompt", "consent")
            .append_pair("redirect_uri", CONSENT_REDIRECT_URI)
            .append_pair("response_type", "id_token code")
            .append_pair("client_id", CONSENT_CLIENT_ID)
            .append_pair("scope", CONSENT_SCOPES)
            .append_pair("id_token_hint", &id_token.encoded)
            .append_pair("state", &self.consent_state)
            .append_pair("nonce", &self.nonce)
            .finish();
        format!("{ACCOUNT_ORIGIN}/oauth2/auth?{query}")
    }

    /// Records the launcher leg's `sub`, so the consent token can be checked against it.
    pub fn set_sub(&mut self, sub: impl Into<String>) {
        self.sub = Some(sub.into());
    }

    /// Validates a redirect's `state` against the value we generated for that leg.
    ///
    /// This is the CSRF check: it proves the response belongs to the request we started.
    pub fn verify_state(&self, redirect: &Redirect) -> Result<()> {
        let (got, expected, leg) = match redirect {
            Redirect::Launcher { state, .. } => (state, &self.launcher_state, "launcher"),
            Redirect::Consent { state, .. } => (state, &self.consent_state, "consent"),
        };
        if got != expected {
            bail!("state mismatch on the {leg} leg — aborting login");
        }
        Ok(())
    }

    /// Checks the consent `id_token` really answers our request: right `nonce`, and the
    /// same user as the launcher leg.
    pub fn verify_consent_token(&self, id_token: &IdToken) -> Result<jwt::Claims> {
        let claims = id_token.claims()?;
        if claims.nonce.as_deref() != Some(self.nonce.as_str()) {
            bail!("nonce mismatch in the consent token — aborting login");
        }
        match &self.sub {
            Some(sub) if *sub != claims.sub => {
                bail!("subject mismatch between the login and consent tokens — aborting login");
            }
            _ => {}
        }
        Ok(claims)
    }
}

/// Recognises the URLs that end each leg of the flow.
///
/// Called for every navigation the login window attempts; returns `None` for the many
/// ordinary page loads that make up the login journey, which are then allowed through.
pub fn classify_redirect(url: &str) -> Option<Redirect> {
    // The official launcher registers a `jagex:` URI scheme and lets the redirect page
    // bounce to it. We intercept the https redirect before that happens, but a change on
    // Jagex's side could start us down the custom-scheme path, so handle it too.
    if let Some(rest) = url.strip_prefix("jagex:") {
        return parse_jagex_scheme(rest);
    }

    let parsed = url::Url::parse(url).ok()?;

    // Launcher leg: `?code=…&state=…` on the redirect URI.
    if parsed.host_str() == Some(REDIRECT_HOST) && parsed.path() == REDIRECT_PATH {
        let mut code = None;
        let mut state = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if let (Some(code), Some(state)) = (code, state) {
            return Some(Redirect::Launcher { code, state });
        }
    }

    // Consent leg: everything arrives in the fragment, not the query.
    if parsed.host_str() == Some("localhost") {
        let fragment = parsed.fragment()?;
        let mut id_token = None;
        let mut state = None;
        for (k, v) in form_urlencoded::parse(fragment.as_bytes()) {
            match k.as_ref() {
                "id_token" => id_token = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if let (Some(id_token), Some(state)) = (id_token, state) {
            return Some(Redirect::Consent { id_token, state });
        }
    }

    None
}

/// Parses `jagex:code=…,state=…,intent=…` — comma-separated, not a normal query string.
fn parse_jagex_scheme(rest: &str) -> Option<Redirect> {
    let mut code = None;
    let mut state = None;
    for pair in rest.split(',') {
        match pair.split_once('=') {
            Some(("code", v)) => code = Some(v.to_string()),
            Some(("state", v)) => state = Some(v.to_string()),
            _ => {}
        }
    }
    Some(Redirect::Launcher {
        code: code?,
        state: state?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_url_carries_every_parameter_the_server_expects() {
        let flow = LoginFlow::new();
        let url = flow.launcher_url();
        let parsed = url::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(parsed.host_str(), Some("account.jagex.com"));
        assert_eq!(parsed.path(), "/oauth2/auth");
        assert_eq!(q["client_id"], LAUNCHER_CLIENT_ID);
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["prompt"], "login");
        assert_eq!(q["flow"], "launcher");
        assert_eq!(q["redirect_uri"], REDIRECT_URI);
        assert_eq!(q["auth_method"], "");
        assert_eq!(q["login_type"], "");
        assert_eq!(q["code_challenge"], pkce::challenge_for(flow.verifier()));
        // scopes are space-separated in the decoded form
        assert!(q["scope"].starts_with("openid offline gamesso.token.create"));
        assert!(q["scope"].contains("user.profile.read"));
    }

    #[test]
    fn consent_url_carries_every_parameter_the_server_expects() {
        let flow = LoginFlow::new();
        let id_token = IdToken::new("header.payload.signature");
        let parsed = url::Url::parse(&flow.consent_url(&id_token)).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(q["client_id"], CONSENT_CLIENT_ID);
        assert_eq!(q["response_type"], "id_token code");
        assert_eq!(q["prompt"], "consent");
        assert_eq!(q["scope"], "openid offline");
        assert_eq!(q["redirect_uri"], CONSENT_REDIRECT_URI);
        assert_eq!(q["id_token_hint"], "header.payload.signature");
        // the two legs must not reuse the same state
        assert_ne!(q["state"], {
            let l = url::Url::parse(&flow.launcher_url()).unwrap();
            l.query_pairs()
                .find(|(k, _)| k == "state")
                .unwrap()
                .1
                .into_owned()
        });
    }

    #[test]
    fn recognises_the_launcher_redirect() {
        let redirect = classify_redirect(
            "https://secure.runescape.com/m=weblogin/launcher-redirect?code=abc123&state=xyz",
        );
        assert_eq!(
            redirect,
            Some(Redirect::Launcher {
                code: "abc123".into(),
                state: "xyz".into()
            })
        );
    }

    #[test]
    fn recognises_the_jagex_uri_scheme_fallback() {
        let redirect = classify_redirect("jagex:code=abc123,state=xyz,intent=social_auth");
        assert_eq!(
            redirect,
            Some(Redirect::Launcher {
                code: "abc123".into(),
                state: "xyz".into()
            })
        );
    }

    #[test]
    fn recognises_the_consent_redirect_in_the_fragment() {
        let redirect =
            classify_redirect("http://localhost/#code=c&id_token=header.payload.sig&state=s2");
        assert_eq!(
            redirect,
            Some(Redirect::Consent {
                id_token: "header.payload.sig".into(),
                state: "s2".into()
            })
        );
    }

    #[test]
    fn ignores_ordinary_navigation_during_login() {
        // Every page of the real login journey must be allowed through untouched.
        for url in [
            "https://account.jagex.com/oauth2/auth?client_id=x",
            "https://secure.runescape.com/m=weblogin/loginform.ws",
            // the redirect URI without a code is not the end of the leg
            "https://secure.runescape.com/m=weblogin/launcher-redirect",
            // localhost without the token payload is not the end of the leg
            "http://localhost/#error=access_denied",
            "https://auth.jagex.com/shield/oauth/token",
            "not a url at all",
        ] {
            assert_eq!(classify_redirect(url), None, "should have ignored {url}");
        }
    }

    #[test]
    fn state_mismatch_is_rejected() {
        let flow = LoginFlow::new();
        let good = classify_redirect(&format!(
            "https://secure.runescape.com/m=weblogin/launcher-redirect?code=c&state={}",
            flow.launcher_state
        ))
        .unwrap();
        assert!(flow.verify_state(&good).is_ok());

        let bad = Redirect::Launcher {
            code: "c".into(),
            state: "attacker-supplied".into(),
        };
        assert!(flow.verify_state(&bad).is_err());

        // the launcher state must not be accepted on the consent leg
        let crossed = Redirect::Consent {
            id_token: "t".into(),
            state: flow.launcher_state.clone(),
        };
        assert!(flow.verify_state(&crossed).is_err());
    }

    fn token_with(payload: serde_json::Value) -> IdToken {
        use base64::Engine;
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        IdToken::new(format!("h.{body}.s"))
    }

    #[test]
    fn consent_token_must_match_nonce_and_subject() {
        let mut flow = LoginFlow::new();
        flow.set_sub("user-1");

        let good = token_with(serde_json::json!({ "sub": "user-1", "nonce": flow.nonce }));
        assert!(flow.verify_consent_token(&good).is_ok());

        let wrong_nonce =
            token_with(serde_json::json!({ "sub": "user-1", "nonce": "replayed" }));
        assert!(flow.verify_consent_token(&wrong_nonce).is_err());

        let wrong_sub = token_with(serde_json::json!({ "sub": "user-2", "nonce": flow.nonce }));
        assert!(flow.verify_consent_token(&wrong_sub).is_err());

        let no_nonce = token_with(serde_json::json!({ "sub": "user-1" }));
        assert!(flow.verify_consent_token(&no_nonce).is_err());
    }
}

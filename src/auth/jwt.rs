//! Just enough JWT to read the claims out of Jagex's `id_token`.
//!
//! Signatures are deliberately not verified. The token arrives over TLS from the issuer
//! and is handed straight back to the issuer; we only read claims to decide which login
//! path to take and to echo `nonce`/`sub` checks. Verifying would mean shipping and
//! rotating Jagex's JWKS for no security gain here — the official launcher does not do it
//! either.

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

/// The claims we care about, plus everything else kept in `extra`.
#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub nonce: Option<String>,
    /// `DisplayName#discriminator` for a Jagex account.
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub login_provider: Option<String>,
}

impl Claims {
    /// A legacy RuneScape account logs in with `login_provider: "runescape"`; anything
    /// else (including the claim being absent) is a Jagex account.
    pub fn is_legacy_runescape(&self) -> bool {
        self.login_provider.as_deref() == Some("runescape")
    }
}

/// An `id_token`: the original encoded string plus its decoded claims.
///
/// The original is kept because every downstream call (`id_token_hint`, the game-session
/// request, the rs-profile bearer token) wants the encoded form back verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdToken {
    pub encoded: String,
}

impl IdToken {
    pub fn new(encoded: impl Into<String>) -> Self {
        Self {
            encoded: encoded.into(),
        }
    }

    pub fn claims(&self) -> Result<Claims> {
        decode_claims(&self.encoded)
    }
}

/// Decodes the payload segment of a JWT.
pub fn decode_claims(token: &str) -> Result<Claims> {
    let mut parts = token.split('.');
    let (Some(_header), Some(payload), Some(_signature)) =
        (parts.next(), parts.next(), parts.next())
    else {
        bail!("malformed JWT: expected three '.'-separated segments");
    };
    if parts.next().is_some() {
        bail!("malformed JWT: too many segments");
    }

    let bytes = decode_segment(payload).context("could not base64-decode JWT payload")?;
    serde_json::from_slice(&bytes).context("could not parse JWT claims as JSON")
}

/// base64url-decodes a JWT segment, tolerating the padding some encoders leave on.
pub fn decode_segment(segment: &str) -> Result<Vec<u8>> {
    let trimmed = segment.trim_end_matches('=');
    Ok(URL_SAFE_NO_PAD.decode(trimmed)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn make_jwt(payload: &serde_json::Value) -> String {
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        format!("eyJhbGciOiJSUzI1NiJ9.{body}.c2lnbmF0dXJl")
    }

    #[test]
    fn reads_jagex_account_claims() {
        let token = make_jwt(&serde_json::json!({
            "sub": "abc-123",
            "nonce": "n0nce",
            "nickname": "Zezima#1a2b",
            "auth_time": 1_700_000_000u64,
        }));
        let claims = decode_claims(&token).unwrap();
        assert_eq!(claims.sub, "abc-123");
        assert_eq!(claims.nonce.as_deref(), Some("n0nce"));
        assert_eq!(claims.nickname.as_deref(), Some("Zezima#1a2b"));
        assert!(!claims.is_legacy_runescape());
    }

    #[test]
    fn detects_legacy_runescape_login_provider() {
        let token = make_jwt(&serde_json::json!({
            "sub": "s",
            "login_provider": "runescape",
        }));
        assert!(decode_claims(&token).unwrap().is_legacy_runescape());
    }

    #[test]
    fn rejects_malformed_tokens() {
        assert!(decode_claims("not-a-jwt").is_err());
        assert!(decode_claims("a.b").is_err());
        assert!(decode_claims("a.b.c.d").is_err());
        assert!(decode_claims("a.!!!!.c").is_err());
    }

    #[test]
    fn tolerates_padded_segments() {
        let payload = serde_json::json!({ "sub": "padded" });
        let body =
            base64::engine::general_purpose::URL_SAFE.encode(serde_json::to_vec(&payload).unwrap());
        let token = format!("h.{body}.s");
        assert_eq!(decode_claims(&token).unwrap().sub, "padded");
    }
}

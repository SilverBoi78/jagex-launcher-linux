//! PKCE verifier/challenge and the random `state`/`nonce` values.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use sha2::{Digest, Sha256};

/// Characters allowed in a PKCE verifier. RFC 7636 also permits `~`, but leaving it out
/// means the verifier is URL-safe as-is and never needs escaping.
const VERIFIER_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._";

/// The official launcher uses 64; RFC 7636 allows 43..=128.
const VERIFIER_LENGTH: usize = 64;

/// A PKCE code verifier and its S256 challenge.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        let verifier = random_from(VERIFIER_CHARS, VERIFIER_LENGTH);
        let challenge = challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }
}

/// base64url(sha256(verifier)), unpadded — the `S256` code challenge method.
pub fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// An opaque random token for `state` / `nonce`. Alphanumeric so it needs no escaping.
pub fn random_token(len: usize) -> String {
    random_from(
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
        len,
    )
}

fn random_from(alphabet: &[u8], len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| alphabet[rng.random_range(0..alphabet.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_rfc7636_example() {
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge_for(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_is_url_safe_and_correct_length() {
        let pkce = Pkce::generate();
        assert_eq!(pkce.verifier.len(), VERIFIER_LENGTH);
        assert!(
            pkce.verifier
                .bytes()
                .all(|b| VERIFIER_CHARS.contains(&b)),
            "verifier contained a character needing escaping: {}",
            pkce.verifier
        );
        assert_eq!(challenge_for(&pkce.verifier), pkce.challenge);
    }

    #[test]
    fn tokens_are_random() {
        assert_ne!(random_token(24), random_token(24));
        assert_eq!(random_token(24).len(), 24);
    }
}

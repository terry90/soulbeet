//! The random secrets the authorization server hands out.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::secret_key::SecretKey;

/// 32 random bytes in unpadded base64url, 43 characters (D3).
pub(crate) fn new_secret() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

/// The form a secret is stored in: its SHA-256, in lowercase hex.
pub(crate) fn hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The S256 code challenge of `verifier`: base64url(SHA-256(verifier))
/// without padding (RFC 7636 section 4.2).
pub(crate) fn s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 43 to 128 characters of `[A-Za-z0-9-._~]` (RFC 7636 section 4.1).
pub(crate) fn valid_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~'))
}

/// The authorization code an Allow of consent request `request_id` issues: an HMAC-SHA256 of
/// the request id under `key`, in unpadded base64url. The request id is a secret only the
/// signed-in browser holds, so a repeat of the answer rebuilds the same code and no code is ever
/// stored in the clear.
pub(crate) fn code_for(key: &SecretKey, request_id: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(b"soulful-oauth-code\0");
    mac.update(request_id.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// The (access, refresh) pair that replaces refresh token `refresh`: each an
/// HMAC-SHA256 of `refresh` under `key` with its own label, in unpadded
/// base64url (D3). A repeat of `refresh` recomputes the same pair, so no
/// token is ever stored in the clear.
pub(crate) fn successor(key: &SecretKey, refresh: &str) -> (String, String) {
    let derive = |label: &[u8]| {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC takes a key of any length");
        mac.update(label);
        mac.update(refresh.as_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    };
    (
        derive(b"soulful-oauth-successor-access\0"),
        derive(b"soulful-oauth-successor-refresh\0"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_43_url_safe_characters_and_differ() {
        let (a, b) = (new_secret(), new_secret());
        assert_ne!(a, b);
        assert!(
            a.len() == 43
                && a.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "{a}"
        );
    }

    #[test]
    fn pkce_s256_matches_rfc_7636_appendix_b() {
        assert_eq!(
            s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(valid_verifier(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        ));
        assert!(
            !valid_verifier(&"a".repeat(42))
                && valid_verifier(&"a".repeat(128))
                && !valid_verifier(&"a".repeat(129))
        );
        assert!(
            valid_verifier(&format!("{}~._-", "a".repeat(40)))
                && !valid_verifier(&format!("{}+", "a".repeat(43)))
        );
    }

    #[test]
    fn a_successor_depends_on_the_token_and_the_key() {
        let k1 = SecretKey::new("k".repeat(32));
        let k2 = SecretKey::new("j".repeat(32));
        let r = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(successor(&k1, r), successor(&k1, r));
        assert_ne!(successor(&k1, r), successor(&k2, r));
        let (a, b) = successor(&k1, r);
        assert!(a != b && a.len() == 43 && b.len() == 43);
    }

    #[test]
    fn hashes_are_sha256_hex() {
        // FIPS 180-2 appendix B.1
        assert_eq!(
            hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

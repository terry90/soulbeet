//! The random secrets the authorization server hands out.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

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
    fn hashes_are_sha256_hex() {
        // FIPS 180-2 appendix B.1
        assert_eq!(
            hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

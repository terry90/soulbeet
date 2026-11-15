//! The random secrets the authorization server hands out.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

/// 32 random bytes in unpadded base64url, 43 characters (D3).
pub(crate) fn new_secret() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
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
}

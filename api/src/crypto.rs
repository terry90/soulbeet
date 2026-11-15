#[cfg(feature = "server")]
use aes_gcm::{
    aead::{Aead, OsRng},
    AeadCore, Aes256Gcm, KeyInit,
};
#[cfg(feature = "server")]
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
#[cfg(feature = "server")]
use sha2::{Digest, Sha256};

#[cfg(feature = "server")]
use crate::secret_key::SecretKey;

#[cfg(feature = "server")]
fn derive_key(secret: &SecretKey) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let result = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&result);
    key
}

#[cfg(feature = "server")]
pub fn encrypt(secret: &SecretKey, plaintext: &str) -> Result<String, String> {
    let cipher = Aes256Gcm::new_from_slice(&derive_key(secret))
        .map_err(|e| format!("Failed to create cipher: {e}"))?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|e| format!("Encryption failed: {e}"))?;

    let mut combined = nonce.to_vec();
    combined.extend_from_slice(&ciphertext);
    Ok(BASE64.encode(combined))
}

#[cfg(feature = "server")]
pub fn decrypt(secret: &SecretKey, encoded: &str) -> Result<String, String> {
    let combined = BASE64
        .decode(encoded)
        .map_err(|e| format!("Base64 decode failed: {e}"))?;

    if combined.len() < 12 {
        return Err("Ciphertext too short".to_string());
    }

    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);

    let cipher = Aes256Gcm::new_from_slice(&derive_key(secret))
        .map_err(|e| format!("Failed to create cipher: {e}"))?;
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("Decryption failed: {e}"))?;

    String::from_utf8(plaintext).map_err(|e| format!("Invalid UTF-8: {e}"))
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn a_value_encrypted_under_another_key_does_not_decrypt() {
        let a = SecretKey::new("a".repeat(32));
        let b = SecretKey::new("b".repeat(32));
        let ct = encrypt(&a, "pw").unwrap();
        assert_eq!(decrypt(&a, &ct).unwrap(), "pw");
        assert!(decrypt(&b, &ct).is_err());
    }
}

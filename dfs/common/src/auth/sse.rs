use crate::auth::AuthError;
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::RngExt;
use zeroize::Zeroizing;

pub struct SseManager {
    kek: [u8; 32],
}

impl SseManager {
    pub fn new(kek: [u8; 32]) -> Self {
        Self { kek }
    }

    pub fn encrypt_object(&self, plaintext: &[u8]) -> Result<(Vec<u8>, String), AuthError> {
        // Generate a random 256-bit DEK
        let mut dek = Zeroizing::new([0u8; 32]);
        rand::rng().fill(dek.as_mut());

        // Encrypt plaintext with DEK
        let data_cipher = Aes256Gcm::new_from_slice(dek.as_ref())
            .map_err(|e| AuthError::InternalError(format!("Invalid DEK: {}", e)))?;

        let mut data_nonce_bytes = [0u8; 12];
        rand::rng().fill(&mut data_nonce_bytes);
        let data_nonce = Nonce::from_slice(&data_nonce_bytes);

        // TODO: add per-object AAD (object path) to bind ciphertext to identity
        let data_ciphertext = data_cipher
            .encrypt(data_nonce, plaintext)
            .map_err(|e| AuthError::InternalError(format!("Data encryption failed: {}", e)))?;

        // Format: [12-byte nonce][ciphertext]
        let mut ciphertext = Vec::with_capacity(12 + data_ciphertext.len());
        ciphertext.extend_from_slice(&data_nonce_bytes);
        ciphertext.extend_from_slice(&data_ciphertext);

        // Encrypt DEK with KEK
        let kek_cipher = Aes256Gcm::new_from_slice(&self.kek)
            .map_err(|e| AuthError::InternalError(format!("Invalid KEK: {}", e)))?;

        let mut kek_nonce_bytes = [0u8; 12];
        rand::rng().fill(&mut kek_nonce_bytes);
        let kek_nonce = Nonce::from_slice(&kek_nonce_bytes);

        let encrypted_dek = kek_cipher
            .encrypt(kek_nonce, dek.as_ref())
            .map_err(|e| AuthError::InternalError(format!("DEK encryption failed: {}", e)))?;

        // Format: [12-byte nonce][ciphertext]
        let mut dek_blob = Vec::with_capacity(12 + encrypted_dek.len());
        dek_blob.extend_from_slice(&kek_nonce_bytes);
        dek_blob.extend_from_slice(&encrypted_dek);

        let dek_b64 = STANDARD.encode(dek_blob);

        Ok((ciphertext, dek_b64))
    }

    pub fn decrypt_object(&self, ciphertext: &[u8], dek_b64: &str) -> Result<Vec<u8>, AuthError> {
        // Decode and decrypt DEK
        let dek_blob = STANDARD
            .decode(dek_b64)
            .map_err(|e| AuthError::InvalidToken(format!("Invalid base64 DEK: {}", e)))?;

        if dek_blob.len() < 60 {
            return Err(AuthError::InvalidToken(
                "Encrypted DEK too short".to_string(),
            ));
        }

        let (kek_nonce_bytes, encrypted_dek) = dek_blob.split_at(12);
        let kek_nonce = Nonce::from_slice(kek_nonce_bytes);

        let kek_cipher = Aes256Gcm::new_from_slice(&self.kek)
            .map_err(|e| AuthError::InternalError(format!("Invalid KEK: {}", e)))?;

        let dek = Zeroizing::new(
            kek_cipher
                .decrypt(kek_nonce, encrypted_dek)
                .map_err(|e| AuthError::InvalidToken(format!("DEK decryption failed: {}", e)))?,
        );

        if dek.len() != 32 {
            return Err(AuthError::InternalError(
                "Decrypted DEK has wrong length".into(),
            ));
        }

        // Decrypt data with DEK
        if ciphertext.len() < 28 {
            return Err(AuthError::InvalidToken("Ciphertext too short".to_string()));
        }

        let (data_nonce_bytes, data_ciphertext) = ciphertext.split_at(12);
        let data_nonce = Nonce::from_slice(data_nonce_bytes);

        let data_cipher = Aes256Gcm::new_from_slice(&dek)
            .map_err(|e| AuthError::InternalError(format!("Invalid DEK: {}", e)))?;

        let plaintext = data_cipher
            .decrypt(data_nonce, data_ciphertext)
            .map_err(|e| AuthError::InvalidToken(format!("Data decryption failed: {}", e)))?;

        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let kek = [42u8; 32];
        let mgr = SseManager::new(kek);
        let plaintext = b"hello world secret data";
        let (ciphertext, dek_b64) = mgr.encrypt_object(plaintext).unwrap();
        assert_ne!(ciphertext.as_slice(), plaintext.as_slice());
        let decrypted = mgr.decrypt_object(&ciphertext, &dek_b64).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_wrong_kek_fails() {
        let kek1 = [1u8; 32];
        let kek2 = [2u8; 32];
        let mgr1 = SseManager::new(kek1);
        let mgr2 = SseManager::new(kek2);
        let plaintext = b"secret";
        let (ciphertext, dek_b64) = mgr1.encrypt_object(plaintext).unwrap();
        assert!(mgr2.decrypt_object(&ciphertext, &dek_b64).is_err());
    }

    #[test]
    fn test_tampered_ciphertext_rejected() {
        let kek = [7u8; 32];
        let mgr = SseManager::new(kek);
        let (mut ciphertext, dek_b64) = mgr.encrypt_object(b"sensitive").unwrap();
        ciphertext[13] ^= 0xff; // flip a byte in the ciphertext
        assert!(mgr.decrypt_object(&ciphertext, &dek_b64).is_err());
    }

    #[test]
    fn test_tampered_dek_rejected() {
        let kek = [8u8; 32];
        let mgr = SseManager::new(kek);
        let (ciphertext, mut dek_b64) = mgr.encrypt_object(b"sensitive").unwrap();
        // flip a byte in the base64-decoded DEK blob
        let mut dek_bytes = base64::engine::general_purpose::STANDARD
            .decode(&dek_b64)
            .unwrap();
        dek_bytes[13] ^= 0xff;
        dek_b64 = base64::engine::general_purpose::STANDARD.encode(&dek_bytes);
        assert!(mgr.decrypt_object(&ciphertext, &dek_b64).is_err());
    }

    #[test]
    fn test_truncated_inputs_rejected() {
        let kek = [9u8; 32];
        let mgr = SseManager::new(kek);
        let (ciphertext, dek_b64) = mgr.encrypt_object(b"data").unwrap();
        // Truncated ciphertext
        assert!(mgr.decrypt_object(&ciphertext[..5], &dek_b64).is_err());
        // Truncated DEK blob
        let short_dek = base64::engine::general_purpose::STANDARD.encode(&[0u8; 5]);
        assert!(mgr.decrypt_object(&ciphertext, &short_dek).is_err());
    }
}

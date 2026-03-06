use crate::auth::oidc::Claims;
use crate::auth::AuthError;
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::RngExt;
use serde::{Deserialize, Serialize};

use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StsSessionData {
    pub role_arn: String,
    pub temp_secret_key: String,
    pub expiration: u64,
    pub claims: Claims,
}

pub struct StsTokenManager {
    keys: HashMap<u32, [u8; 32]>,
    active_kid: u32,
}

impl StsTokenManager {
    pub fn new(keys: HashMap<u32, [u8; 32]>, active_kid: u32) -> Self {
        Self { keys, active_kid }
    }

    pub fn generate_token(&self, data: &StsSessionData) -> Result<String, AuthError> {
        let key = self.keys.get(&self.active_kid).ok_or_else(|| {
            AuthError::InternalError(format!("Active KID {} not found", self.active_kid))
        })?;

        let plaintext = serde_json::to_vec(data).map_err(|e| {
            AuthError::InternalError(format!("Failed to serialize STS data: {}", e))
        })?;

        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|e| AuthError::InternalError(format!("Invalid AES key: {}", e)))?;

        let mut nonce_bytes = [0u8; 12];
        rand::rng().fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_ref())
            .map_err(|e| AuthError::InternalError(format!("Encryption failed: {}", e)))?;

        // Format: [KID (4 bytes, BE)] [Nonce (12 bytes)] [Ciphertext]
        let mut combined = Vec::with_capacity(4 + nonce_bytes.len() + ciphertext.len());
        combined.extend_from_slice(&self.active_kid.to_be_bytes());
        combined.extend_from_slice(&nonce_bytes);
        combined.extend_from_slice(&ciphertext);

        Ok(STANDARD.encode(combined))
    }

    pub fn decrypt_token(&self, token: &str) -> Result<StsSessionData, AuthError> {
        let combined = STANDARD
            .decode(token)
            .map_err(|e| AuthError::InvalidToken(format!("Invalid base64: {}", e)))?;

        if combined.len() < 16 {
            // 4 (KID) + 12 (Nonce)
            return Err(AuthError::InvalidToken("Token too short".to_string()));
        }

        let (kid_bytes, rest) = combined.split_at(4);
        let mut kid_arr = [0u8; 4];
        kid_arr.copy_from_slice(kid_bytes);
        let kid = u32::from_be_bytes(kid_arr);

        let (nonce_bytes, ciphertext) = rest.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);

        let key = self
            .keys
            .get(&kid)
            .ok_or_else(|| AuthError::InvalidToken(format!("Unknown KID: {}", kid)))?;

        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|e| AuthError::InternalError(format!("Invalid AES key: {}", e)))?;

        let plaintext = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| AuthError::InvalidToken(format!("Decryption failed: {}", e)))?;

        let data: StsSessionData = serde_json::from_slice(&plaintext).map_err(|e| {
            AuthError::InvalidToken(format!("Failed to parse decrypted STS data: {}", e))
        })?;

        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sts_token_roundtrip() {
        let mut keys = HashMap::new();
        keys.insert(1, [0u8; 32]);
        let manager = StsTokenManager::new(keys, 1);

        let data = StsSessionData {
            role_arn: "arn:dfs:iam:::role/test".to_string(),
            temp_secret_key: "secret".to_string(),
            expiration: 123456789,
            claims: Claims {
                sub: "user1".to_string(),
                aud: "aud1".to_string(),
                iss: "iss1".to_string(),
                exp: 0,
                iat: 0,
                groups: vec!["group1".to_string()],
                extra: serde_json::json!({}),
            },
        };

        let token = manager
            .generate_token(&data)
            .expect("Failed to generate token");
        let decoded = manager
            .decrypt_token(&token)
            .expect("Failed to decrypt token");

        assert_eq!(decoded.role_arn, data.role_arn);
        assert_eq!(decoded.temp_secret_key, data.temp_secret_key);
        assert_eq!(decoded.claims.sub, data.claims.sub);
    }

    #[test]
    fn test_sts_token_key_rotation() {
        let mut keys = HashMap::new();
        keys.insert(1, [1u8; 32]);
        keys.insert(2, [2u8; 32]);

        // Manager with active KID = 1
        let manager1 = StsTokenManager::new(keys.clone(), 1);
        let data = StsSessionData {
            role_arn: "arn:dfs:iam:::role/test".to_string(),
            temp_secret_key: "secret".to_string(),
            expiration: 123456789,
            claims: Claims {
                sub: "user1".to_string(),
                aud: "aud".to_string(),
                iss: "iss".to_string(),
                exp: 0,
                iat: 0,
                groups: vec![],
                extra: serde_json::json!({}),
            },
        };

        let token1 = manager1.generate_token(&data).unwrap();

        // Switch to manager with active KID = 2, still containing key 1
        let manager2 = StsTokenManager::new(keys, 2);
        let decoded1 = manager2
            .decrypt_token(&token1)
            .expect("Should decrypt with KID 1");
        assert_eq!(decoded1.role_arn, data.role_arn);

        let token2 = manager2.generate_token(&data).unwrap();
        assert_ne!(token1, token2); // Different KID and Nonce
    }
}

use crate::auth::AuthError;
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub aud: String,
    pub iss: String,
    pub exp: u64,
    pub iat: u64,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

impl Claims {
    pub fn to_policy_context(&self) -> crate::auth::policy::EvaluationContext {
        let mut context = crate::auth::policy::EvaluationContext {
            principal_id: self.sub.clone(),
            ..Default::default()
        };
        context.claims.insert("sub".to_string(), self.sub.clone());
        context.claims.insert("iss".to_string(), self.iss.clone());

        // Convert groups to context
        for group in &self.groups {
            context.groups.push(group.clone());
        }

        context
    }
}

pub struct OidcValidator {
    issuer_url: String,
    client_id: String,
    jwks: Arc<RwLock<Option<JwkSet>>>,
}

impl OidcValidator {
    pub fn new(issuer_url: String, client_id: String) -> Self {
        Self {
            issuer_url,
            client_id,
            jwks: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn fetch_jwks(&self) -> Result<(), AuthError> {
        let config_url = format!(
            "{}/.well-known/openid-configuration",
            self.issuer_url.trim_end_matches('/')
        );
        let config: serde_json::Value = reqwest::get(&config_url)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to fetch OIDC config: {}", e)))?
            .json()
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to parse OIDC config: {}", e)))?;

        let jwks_uri = config["jwks_uri"].as_str().ok_or_else(|| {
            AuthError::InternalError("Missing jwks_uri in OIDC config".to_string())
        })?;

        let jwks: JwkSet = reqwest::get(jwks_uri)
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to fetch JWKS: {}", e)))?
            .json()
            .await
            .map_err(|e| AuthError::InternalError(format!("Failed to parse JWKS: {}", e)))?;

        let mut lock = self.jwks.write().await;
        *lock = Some(jwks);
        Ok(())
    }

    pub async fn validate_token(&self, token: &str) -> Result<Claims, AuthError> {
        let header = decode_header(token)
            .map_err(|e| AuthError::InvalidToken(format!("Invalid JWT header: {}", e)))?;

        let kid = header
            .kid
            .ok_or_else(|| AuthError::InvalidToken("Missing kid in JWT header".to_string()))?;

        // 1. Try to get JWKS, if None, fetch. (Also ideally fetch if kid is missing, but keep it simple for now)
        let needs_fetch = {
            let jwks_lock = self.jwks.read().await;
            jwks_lock.is_none() || jwks_lock.as_ref().unwrap().find(&kid).is_none()
        };

        if needs_fetch {
            tracing::info!("JWKS not found or kid not found, fetching JWKS from issuer...");
            if let Err(e) = self.fetch_jwks().await {
                tracing::warn!("Failed to fetch JWKS during validation: {}", e);
            }
        }

        let jwks_lock = self.jwks.read().await;
        let jwks = jwks_lock
            .as_ref()
            .ok_or_else(|| AuthError::InternalError("JWKS is not available".to_string()))?;

        let jwk = jwks
            .find(&kid)
            .ok_or_else(|| AuthError::InvalidToken(format!("kid {} not found in JWKS", kid)))?;

        let decoding_key = DecodingKey::from_jwk(jwk).map_err(|e| {
            AuthError::InternalError(format!("Failed to create decoding key from JWK: {}", e))
        })?;

        let mut validation = Validation::new(Algorithm::RS256);
        // Special case for tests: allow HS256 if we are in test mode or if the key is symmetric
        #[cfg(test)]
        {
            if jwk.common.key_algorithm == Some(jsonwebtoken::jwk::KeyAlgorithm::HS256) {
                validation = Validation::new(Algorithm::HS256);
            }
        }

        validation.set_audience(&[&self.client_id]);
        validation.set_issuer(&[&self.issuer_url]);

        let token_data = decode::<Claims>(token, &decoding_key, &validation)
            .map_err(|e| AuthError::InvalidToken(format!("Token validation failed: {}", e)))?;

        Ok(token_data.claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use jsonwebtoken::{encode, Header};
    use mockito::Server;
    use serde_json::json;

    #[tokio::test]
    async fn test_oidc_validation_flow() {
        let mut server = Server::new_async().await;
        let secret = "test-secret-12345678901234567890123456789012";
        let kid = "test-kid";
        let issuer = server.url();
        let client_id = "test-client";

        // Mock Discovery
        let _m1 = server
            .mock("GET", "/.well-known/openid-configuration")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "jwks_uri": format!("{}/jwks", server.url())
                })
                .to_string(),
            )
            .create_async()
            .await;

        // Mock JWKS with a symmetric key (HS256) for testing simplicity
        // In reality OIDC uses RSA, but for unit testing the logic, HS256 is easier to setup
        let _m2 = server
            .mock("GET", "/jwks")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "keys": [
                        {
                            "kty": "oct",
                            "kid": kid,
                            "alg": "HS256",
                            "k": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret)
                        }
                    ]
                })
                .to_string(),
            )
            .create_async()
            .await;

        let validator = OidcValidator::new(issuer.clone(), client_id.to_string());
        validator.fetch_jwks().await.expect("Failed to fetch JWKS");

        // Generate a valid token
        let my_claims = Claims {
            sub: "user-123".to_string(),
            aud: client_id.to_string(),
            iss: issuer.clone(),
            exp: 10000000000,
            iat: 1516239022,
            groups: vec!["admin".to_string()],
            extra: json!({}),
        };

        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(kid.to_string());
        let token = encode(
            &header,
            &my_claims,
            &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();

        // Validate
        let validated_claims = validator
            .validate_token(&token)
            .await
            .expect("Validation failed");
        assert_eq!(validated_claims.sub, "user-123");
        assert_eq!(validated_claims.groups, vec!["admin"]);
    }
}

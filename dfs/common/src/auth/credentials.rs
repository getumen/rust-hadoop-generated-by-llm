/// Trait for fetching secret keys based on AccessKeyId.
pub trait CredentialProvider: Send + Sync {
    fn get_secret_key(&self, access_key: &str) -> Option<String>;
}

/// Simple implementation that reads credentials from environment variables.
pub struct EnvCredentialProvider {
    access_key: Option<String>,
    secret_key: Option<String>,
}

impl EnvCredentialProvider {
    pub fn new() -> Self {
        Self {
            access_key: std::env::var("S3_ACCESS_KEY").ok(),
            secret_key: std::env::var("S3_SECRET_KEY").ok(),
        }
    }
}

impl Default for EnvCredentialProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialProvider for EnvCredentialProvider {
    fn get_secret_key(&self, access_key: &str) -> Option<String> {
        match (&self.access_key, &self.secret_key) {
            (Some(ak), Some(sk)) if access_key == ak => Some(sk.clone()),
            _ => None,
        }
    }
}

/// Static credential provider for tests.
pub struct StaticCredentialProvider {
    pub access_key: String,
    pub secret_key: String,
}

impl Default for StaticCredentialProvider {
    fn default() -> Self {
        Self {
            access_key: "ak".to_string(),
            secret_key: "sk".to_string(),
        }
    }
}

impl CredentialProvider for StaticCredentialProvider {
    fn get_secret_key(&self, access_key: &str) -> Option<String> {
        if access_key == self.access_key {
            Some(self.secret_key.clone())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    /// Helper that saves current S3_ACCESS_KEY and S3_SECRET_KEY, runs `f`,
    /// then restores the previous values.
    fn with_env_vars<F: FnOnce()>(f: F) {
        let prev_access = env::var("S3_ACCESS_KEY").ok();
        let prev_secret = env::var("S3_SECRET_KEY").ok();

        // Run the test body.
        f();

        // Restore previous environment.
        match prev_access {
            Some(v) => env::set_var("S3_ACCESS_KEY", v),
            None => env::remove_var("S3_ACCESS_KEY"),
        }
        match prev_secret {
            Some(v) => env::set_var("S3_SECRET_KEY", v),
            None => env::remove_var("S3_SECRET_KEY"),
        }
    }

    #[test]
    fn env_credential_provider_returns_secret_when_keys_match() {
        with_env_vars(|| {
            env::set_var("S3_ACCESS_KEY", "test-access");
            env::set_var("S3_SECRET_KEY", "test-secret");

            let provider = EnvCredentialProvider::new();
            let secret = provider.get_secret_key("test-access");
            assert_eq!(secret.as_deref(), Some("test-secret"));
        });
    }

    #[test]
    fn env_credential_provider_returns_none_when_keys_do_not_match() {
        with_env_vars(|| {
            env::set_var("S3_ACCESS_KEY", "test-access");
            env::set_var("S3_SECRET_KEY", "test-secret");

            let provider = EnvCredentialProvider::new();
            let secret = provider.get_secret_key("other-access");
            assert!(secret.is_none());
        });
    }

    #[test]
    fn static_credential_provider_returns_secret_when_keys_match() {
        let provider = StaticCredentialProvider {
            access_key: "static-access".to_string(),
            secret_key: "static-secret".to_string(),
        };

        let secret = provider.get_secret_key("static-access");
        assert_eq!(secret.as_deref(), Some("static-secret"));
    }
}

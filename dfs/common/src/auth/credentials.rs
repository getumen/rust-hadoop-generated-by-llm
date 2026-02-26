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

impl CredentialProvider for StaticCredentialProvider {
    fn get_secret_key(&self, access_key: &str) -> Option<String> {
        if access_key == self.access_key {
            Some(self.secret_key.clone())
        } else {
            None
        }
    }
}

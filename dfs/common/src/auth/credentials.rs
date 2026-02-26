/// Trait for fetching secret keys based on AccessKeyId.
pub trait CredentialProvider: Send + Sync {
    fn get_secret_key(&self, access_key: &str) -> Option<String>;
}

/// Simple implementation that reads credentials from environment variables.
pub struct EnvCredentialProvider;

impl CredentialProvider for EnvCredentialProvider {
    fn get_secret_key(&self, access_key: &str) -> Option<String> {
        let env_key = std::env::var("S3_ACCESS_KEY").ok()?;
        if access_key == env_key {
            std::env::var("S3_SECRET_KEY").ok()
        } else {
            None
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

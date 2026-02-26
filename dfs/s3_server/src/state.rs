use dfs_client::Client;
use dfs_common::auth::{cache::SigningKeyCache, credentials::CredentialProvider};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub client: Client,
    pub auth_enabled: bool,
    pub credential_provider: Arc<dyn CredentialProvider>,
    pub signing_key_cache: Arc<SigningKeyCache>,
    pub server_region: String,
    pub require_tls: bool,
    pub allow_unsigned_payload: bool,
}

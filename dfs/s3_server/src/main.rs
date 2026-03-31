mod audit;
mod auth_middleware;
#[cfg(test)]
mod handler_tests;
mod handlers;
mod iam_metrics;
mod s3_types;
mod state;
mod sts_handler;

use crate::state::{AppState as S3AppState, OidcValidator, PolicyEvaluator, StsTokenManager};
use axum::{
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{any, get},
    Router,
};
use dfs_client::Client;
use dfs_common::auth::cache::SigningKeyCache;
use dfs_common::auth::credentials::EnvCredentialProvider;
use prometheus::{Encoder, IntCounterVec, Registry, TextEncoder};
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Prometheus metrics for S3 server
pub static S3_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        prometheus::opts!("s3_requests_total", "Total number of S3 requests"),
        &["method", "path", "status"],
    )
    .unwrap()
});

// Custom error type for Axum
struct InternalError;

impl IntoResponse for InternalError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "s3_server=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let master_addr =
        std::env::var("MASTER_ADDR").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());
    let config_servers_env = std::env::var("CONFIG_SERVERS").unwrap_or_default();
    let config_servers: Vec<String> = config_servers_env
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    tracing::info!(
        "Connecting to Master at {}, Config Servers: {:?}",
        master_addr,
        config_servers
    );

    let ca_cert = std::env::var("CA_CERT").ok();
    let domain_name = std::env::var("DOMAIN_NAME").ok();

    // The Client::new expects (master_addrs, config_server_addrs)
    let client =
        Client::new(vec![master_addr], config_servers).with_tls_config(ca_cert, domain_name);

    // Load shard map if config is provided (optional, Config Server is preferred)
    let shard_config_path = std::env::var("SHARD_CONFIG").ok();
    if let Some(path) = shard_config_path {
        tracing::info!("Loading shard config from {}", path);
        let shard_map = dfs_common::sharding::load_shard_map_from_config(Some(&path), 100);
        client.set_shard_map(shard_map);
    }

    // Initialize Auth state
    let auth_enabled = std::env::var("S3_AUTH_ENABLED").unwrap_or_default() == "true";
    let server_region = std::env::var("S3_REGION").unwrap_or_else(|_| "us-east-1".to_string());
    let require_tls = std::env::var("S3_REQUIRE_TLS").unwrap_or_default() == "true";
    let allow_unsigned_payload =
        std::env::var("S3_ALLOW_UNSIGNED_PAYLOAD").unwrap_or_else(|_| "true".to_string()) == "true";

    let credential_provider = Arc::new(EnvCredentialProvider::new());
    let signing_key_cache = Arc::new(SigningKeyCache::default());

    let oidc_validator = if let (Ok(issuer), Ok(client_id)) = (
        std::env::var("OIDC_ISSUER_URL"),
        std::env::var("OIDC_CLIENT_ID"),
    ) {
        let validator = Arc::new(OidcValidator::new(issuer, client_id));
        // Fetch JWKS in the background periodically with metrics
        let v_clone = validator.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                let start = std::time::Instant::now();
                match v_clone.fetch_jwks().await {
                    Ok(_) => {
                        iam_metrics::IAM_OIDC_JWKS_FETCHES
                            .with_label_values(&["success"])
                            .inc();
                        iam_metrics::IAM_OIDC_JWKS_FETCH_DURATION
                            .with_label_values(&[] as &[&str])
                            .observe(start.elapsed().as_secs_f64());
                        iam_metrics::IAM_OIDC_JWKS_LAST_FETCH.set(chrono::Utc::now().timestamp());
                    }
                    Err(e) => {
                        iam_metrics::IAM_OIDC_JWKS_FETCHES
                            .with_label_values(&["failure"])
                            .inc();
                        iam_metrics::IAM_OIDC_JWKS_FETCH_DURATION
                            .with_label_values(&[] as &[&str])
                            .observe(start.elapsed().as_secs_f64());
                        tracing::error!("Failed to fetch JWKS: {}", e);
                    }
                }
            }
        });
        Some(validator)
    } else {
        None
    };

    let sts_token_manager = if let Ok(key_str) = std::env::var("STS_SIGNING_KEY") {
        let mut key = [0u8; 32];
        let key_bytes = key_str.as_bytes();
        let len = key_bytes.len().min(32);
        key[..len].copy_from_slice(&key_bytes[..len]);
        let mut keys = std::collections::HashMap::new();
        keys.insert(1, key);
        Some(Arc::new(StsTokenManager::new(keys, 1)))
    } else {
        None
    };

    let policy_evaluator = if let Ok(path) = std::env::var("IAM_CONFIG_PATH") {
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                match serde_json::from_str::<dfs_common::auth::policy::IamConfig>(&content) {
                    Ok(config) => Some(Arc::new(PolicyEvaluator::new(config))),
                    Err(e) => {
                        tracing::error!("Failed to parse IAM config: {}", e);
                        None
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to read IAM config at {}: {}", path, e);
                None
            }
        }
    } else {
        None
    };

    let audit_logger = if std::env::var("AUDIT_LOG_ENABLED").unwrap_or_else(|_| "true".to_string())
        == "true"
    {
        let log_dir =
            std::env::var("AUDIT_LOG_DIR").unwrap_or_else(|_| "/tmp/s3_audit_log".to_string());
        let retention = std::env::var("AUDIT_LOG_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(30);
        let batch_size = std::env::var("AUDIT_LOG_BATCH_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(100);
        let hmac_secret = match std::env::var("AUDIT_HMAC_SECRET") {
            Ok(s) if s.len() >= 16 => Some(s),
            Ok(_) => {
                tracing::error!("AUDIT_HMAC_SECRET must be at least 16 bytes for secure tamper-evidence. Disabling AuditLogger.");
                None
            }
            Err(_) => {
                tracing::error!("AUDIT_HMAC_SECRET must be set when AUDIT_LOG_ENABLED=true. Disabling AuditLogger.");
                None
            }
        };

        if let Some(secret) = hmac_secret {
            match crate::audit::AuditLogger::new(log_dir, retention, batch_size, secret) {
                Ok(logger) => Some(Arc::new(logger)),
                Err(e) => {
                    tracing::error!("Failed to initialize AuditLogger: {}", e);
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let state = S3AppState {
        client,
        auth_enabled,
        credential_provider,
        signing_key_cache,
        server_region,
        require_tls,
        allow_unsigned_payload,
        oidc_validator,
        sts_token_manager,
        policy_evaluator,
        audit_logger,
    };

    let authed_routes = Router::new()
        .route("/", any(handlers::handle_root))
        .route("/{*path}", any(handlers::handle_request))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware::auth_middleware,
        ));

    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/metrics", get(handle_metrics))
        .merge(authed_routes)
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let port = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(9000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let tls_cert = std::env::var("TLS_CERT").ok();
    let tls_key = std::env::var("TLS_KEY").ok();

    tracing::info!("S3 Server listening on {}", addr);

    if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
        let config = dfs_common::security::get_axum_tls_config(&cert, &key).await?;
        axum_server::bind_rustls(addr, config)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
    }

    Ok(())
}

async fn handle_health() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

async fn handle_metrics() -> Result<String, InternalError> {
    let registry = Registry::new();
    registry
        .register(Box::new(S3_REQUESTS.clone()))
        .map_err(|e| {
            tracing::error!("Failed to register S3 metrics: {}", e);
            InternalError
        })?;

    // Register Audit Logger metrics
    registry
        .register(Box::new(crate::audit::AUDIT_LOG_TOTAL.clone()))
        .map_err(|e| {
            tracing::error!("Failed to register audit_log_total: {}", e);
            InternalError
        })?;
    registry
        .register(Box::new(crate::audit::AUDIT_LOG_DROPPED.clone()))
        .map_err(|e| {
            tracing::error!("Failed to register audit_log_dropped: {}", e);
            InternalError
        })?;
    registry
        .register(Box::new(crate::audit::AUDIT_LOG_FLUSH_ERRORS.clone()))
        .map_err(|e| {
            tracing::error!("Failed to register audit_log_flush_errors: {}", e);
            InternalError
        })?;

    // Register IAM metrics
    crate::iam_metrics::register_iam_metrics(&registry).map_err(|e| {
        tracing::error!("Failed to register IAM metrics: {}", e);
        InternalError
    })?;

    let mut buffer = vec![];
    let encoder = TextEncoder::new();
    encoder
        .encode(&registry.gather(), &mut buffer)
        .map_err(|e| {
            tracing::error!("Failed to encode S3 metrics: {}", e);
            InternalError
        })?;

    String::from_utf8(buffer).map_err(|e| {
        tracing::error!("Failed to convert metrics buffer to string: {}", e);
        InternalError
    })
}

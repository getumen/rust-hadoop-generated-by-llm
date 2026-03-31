use crate::iam_metrics::*;
use crate::state::AppState as S3AppState;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use dfs_common::auth::audit::AuditRecord;
use dfs_common::auth::sts::StsSessionData;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Debug, Deserialize, Clone)]
pub struct StsQueryParams {
    #[serde(rename = "Action")]
    pub action: Option<String>,
    #[serde(rename = "DurationSeconds")]
    pub duration_seconds: Option<u64>,
    #[serde(rename = "RoleArn")]
    pub role_arn: Option<String>,
    #[serde(rename = "RoleSessionName")]
    pub role_session_name: Option<String>,
    #[serde(rename = "WebIdentityToken")]
    pub web_identity_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AssumeRoleWithWebIdentityResponse {
    #[serde(rename = "AssumeRoleWithWebIdentityResult")]
    pub result: AssumeRoleWithWebIdentityResult,
}

#[derive(Debug, Serialize)]
pub struct AssumeRoleWithWebIdentityResult {
    #[serde(rename = "Credentials")]
    pub credentials: Credentials,
    #[serde(rename = "SubjectFromWebIdentityToken")]
    pub subject_from_web_identity_token: String,
    #[serde(rename = "AssumedRoleUser")]
    pub assumed_role_user: AssumedRoleUser,
}

#[derive(Debug, Serialize)]
pub struct Credentials {
    #[serde(rename = "AccessKeyId")]
    pub access_key_id: String,
    #[serde(rename = "SecretAccessKey")]
    pub secret_access_key: String,
    #[serde(rename = "SessionToken")]
    pub session_token: String,
    #[serde(rename = "Expiration")]
    pub expiration: String,
}

#[derive(Debug, Serialize)]
pub struct AssumedRoleUser {
    #[serde(rename = "AssumedRoleId")]
    pub assumed_role_id: String,
    #[serde(rename = "Arn")]
    pub arn: String,
}

pub async fn handle_sts(
    State(state): State<S3AppState>,
    connect_info: axum::extract::ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Query(params): Query<StsQueryParams>,
) -> Response {
    let start_time = chrono::Utc::now();
    let sts_timer = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let remote_ip = connect_info.ip().to_string();
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let log_audit = |status_code: StatusCode,
                     error_code: Option<&str>,
                     action: &str,
                     user_id: &str,
                     role_arn: Option<&str>| {
        if let Some(logger) = &state.audit_logger {
            let now = chrono::Utc::now();
            let duration = now.signed_duration_since(start_time).num_milliseconds() as u64;
            logger.log(AuditRecord {
                timestamp: now.to_rfc3339(),
                timestamp_ms: now.timestamp_millis() as u64,
                request_id: request_id.clone(),
                remote_ip: remote_ip.clone(),
                user_id: user_id.to_string(),
                role_arn: role_arn.map(|s| s.to_string()),
                action: action.to_string(),
                resource: "arn:dfs:sts:::*".to_string(), // STS is global-ish or service-level
                status_code: status_code.as_u16(),
                error_code: error_code.map(|s| s.to_string()),
                user_agent: user_agent.clone(),
                duration_ms: Some(duration),
                previous_hash: None,
                record_hash: None,
            });
        }
    };

    let action = params.action.as_deref().unwrap_or("Unknown");

    if params.action != Some("AssumeRoleWithWebIdentity".to_string()) {
        log_audit(
            StatusCode::BAD_REQUEST,
            Some("InvalidAction"),
            action,
            "anonymous",
            None,
        );
        IAM_STS_REQUESTS
            .with_label_values(&["failure", "invalid_action"])
            .inc();
        return StatusCode::BAD_REQUEST.into_response();
    }

    let oidc = match &state.oidc_validator {
        Some(o) => o,
        None => {
            log_audit(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("OIDC_NOT_ENABLED"),
                action,
                "anonymous",
                None,
            );
            IAM_STS_REQUESTS
                .with_label_values(&["failure", "oidc_not_enabled"])
                .inc();
            return sts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OIDC_NOT_ENABLED",
                "OIDC validation is not enabled on this server.",
            );
        }
    };

    let sts_mgr = match &state.sts_token_manager {
        Some(s) => s,
        None => {
            log_audit(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("STS_NOT_ENABLED"),
                action,
                "anonymous",
                None,
            );
            IAM_STS_REQUESTS
                .with_label_values(&["failure", "sts_not_enabled"])
                .inc();
            return sts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "STS_NOT_ENABLED",
                "STS is not enabled on this server.",
            );
        }
    };

    let policy_eval = match &state.policy_evaluator {
        Some(p) => p,
        None => {
            log_audit(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("IAM_NOT_ENABLED"),
                action,
                "anonymous",
                None,
            );
            IAM_STS_REQUESTS
                .with_label_values(&["failure", "iam_not_enabled"])
                .inc();
            return sts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "IAM_NOT_ENABLED",
                "IAM policy evaluation is not enabled on this server.",
            );
        }
    };

    let web_identity_token = match &params.web_identity_token {
        Some(t) => t,
        None => {
            log_audit(
                StatusCode::BAD_REQUEST,
                Some("MissingToken"),
                action,
                "anonymous",
                None,
            );
            IAM_STS_REQUESTS
                .with_label_values(&["failure", "missing_token"])
                .inc();
            return sts_error(
                StatusCode::BAD_REQUEST,
                "MissingToken",
                "WebIdentityToken is required",
            );
        }
    };

    let role_arn = match &params.role_arn {
        Some(r) => r,
        None => {
            log_audit(
                StatusCode::BAD_REQUEST,
                Some("MissingRole"),
                action,
                "anonymous",
                None,
            );
            IAM_STS_REQUESTS
                .with_label_values(&["failure", "missing_role"])
                .inc();
            return sts_error(
                StatusCode::BAD_REQUEST,
                "MissingRole",
                "RoleArn is required",
            );
        }
    };

    // 1. Validate OIDC Token
    let claims = match oidc.validate_token(web_identity_token).await {
        Ok(c) => {
            IAM_OIDC_VALIDATIONS.with_label_values(&["success"]).inc();
            c
        }
        Err(e) => {
            tracing::error!("OIDC validation failed: {}", e);
            IAM_OIDC_VALIDATIONS.with_label_values(&["failure"]).inc();
            IAM_STS_REQUESTS
                .with_label_values(&["failure", "invalid_identity_token"])
                .inc();
            log_audit(
                StatusCode::FORBIDDEN,
                Some("InvalidIdentityToken"),
                action,
                "anonymous",
                Some(role_arn),
            );
            return sts_error(
                StatusCode::FORBIDDEN,
                "InvalidIdentityToken",
                &e.to_string(),
            );
        }
    };

    // 2. Check Role Trust Policy
    let context = claims.to_policy_context();
    if !policy_eval.can_assume_role(role_arn, &context) {
        IAM_STS_REQUESTS
            .with_label_values(&["failure", "access_denied"])
            .inc();
        log_audit(
            StatusCode::FORBIDDEN,
            Some("AccessDenied"),
            action,
            &claims.sub,
            Some(role_arn),
        );
        return sts_error(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "User is not authorized to assume this role.",
        );
    }

    // 3. Generate Temporary Credentials
    let duration = params.duration_seconds.unwrap_or(3600);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expiration_ts = now + duration;

    // Generate a random temp secret key (e.g. 40 chars like AWS)
    use rand::RngExt;
    let temp_secret_key: String = rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(40)
        .map(char::from)
        .collect();

    // Use a synthetic AccessKeyId for the session (e.g. ASIA...)
    let access_key_id = format!(
        "ASIA{}",
        &uuid::Uuid::new_v4().to_string().replace("-", "")[..16]
    )
    .to_uppercase();

    let session_data = StsSessionData {
        role_arn: role_arn.clone(),
        temp_secret_key: temp_secret_key.clone(),
        expiration: expiration_ts,
        claims: claims.clone(),
    };

    let session_token = match sts_mgr.generate_token(&session_data) {
        Ok(t) => t,
        Err(e) => {
            IAM_STS_REQUESTS
                .with_label_values(&["failure", "internal_error"])
                .inc();
            log_audit(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("InternalError"),
                action,
                &claims.sub,
                Some(role_arn),
            );
            return sts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                &e.to_string(),
            );
        }
    };

    let expiration_str = chrono::DateTime::from_timestamp(expiration_ts as i64, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let session_name = params.role_session_name.as_deref().unwrap_or("session");

    let result = AssumeRoleWithWebIdentityResponse {
        result: AssumeRoleWithWebIdentityResult {
            credentials: Credentials {
                access_key_id,
                secret_access_key: temp_secret_key,
                session_token,
                expiration: expiration_str,
            },
            subject_from_web_identity_token: claims.sub.clone(),
            assumed_role_user: AssumedRoleUser {
                assumed_role_id: format!(
                    "{}:{}",
                    role_arn.split('/').next_back().unwrap_or("role"),
                    session_name
                ),
                arn: format!(
                    "arn:dfs:sts:::assumed-role/{}/{}",
                    role_arn.split('/').next_back().unwrap_or("role"),
                    session_name
                ),
            },
        },
    };

    log_audit(StatusCode::OK, None, action, &claims.sub, Some(role_arn));

    // Record STS success metrics
    IAM_STS_REQUESTS
        .with_label_values(&["success", "none"])
        .inc();
    IAM_STS_DURATION
        .with_label_values(&[] as &[&str])
        .observe(sts_timer.elapsed().as_secs_f64());
    IAM_STS_ACTIVE_SESSIONS.inc();

    // Schedule a delayed decrement for the active sessions gauge
    let session_duration = duration;
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(session_duration)).await;
        IAM_STS_ACTIVE_SESSIONS.dec();
    });

    match quick_xml::se::to_string(&result) {
        Ok(xml) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/xml")
            .body(axum::body::Body::from(xml))
            .unwrap(),
        Err(e) => {
            tracing::error!("Failed to serialize STS response: {}", e);
            IAM_STS_REQUESTS
                .with_label_values(&["failure", "internal_error"])
                .inc();
            log_audit(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("InternalError"),
                action,
                &claims.sub,
                Some(role_arn),
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn sts_error(status: StatusCode, code: &str, message: &str) -> Response {
    let xml = format!(
        r#"<ErrorResponse><Error><Code>{}</Code><Message>{}</Message></Error><RequestId></RequestId></ErrorResponse>"#,
        code, message
    );
    Response::builder()
        .status(status)
        .header("Content-Type", "application/xml")
        .body(axum::body::Body::from(xml))
        .unwrap()
}

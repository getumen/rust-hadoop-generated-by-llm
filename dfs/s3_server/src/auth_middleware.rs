use crate::state::AppState;
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use dfs_common::auth::{parse_credentials, AuthError, SigningInput};
use std::collections::BTreeMap;

pub async fn auth_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // 1. Auth enabled check
    if !state.auth_enabled {
        return next.run(req).await;
    }

    // 2. TLS check
    if state.require_tls {
        // In this environment, we can check if the request was made via TLS.
        // If we are behind a proxy, we might check X-Forwarded-Proto.
        let is_tls = req.uri().scheme_str() == Some("https")
            || req
                .headers()
                .get("X-Forwarded-Proto")
                .and_then(|v| v.to_str().ok())
                == Some("https");

        if !is_tls {
            return s3_error_response(AuthError::InsecureTransport);
        }
    }

    // 3. Extract Query parameters for cred parsing and normalization
    let query_string_raw = req.uri().query().unwrap_or("");
    let query_params: BTreeMap<String, String> =
        serde_urlencoded::from_str(query_string_raw).unwrap_or_default();

    // Canonical Query String: sort by param name, then value; URI-encode; empty values as key=
    let mut normalized_query_parts = Vec::new();
    for (k, v) in &query_params {
        let encoded_k = dfs_common::auth::encoding::uri_encode(k, true);
        let encoded_v = dfs_common::auth::encoding::uri_encode(v, true);
        normalized_query_parts.push(format!("{}={}", encoded_k, encoded_v));
    }
    let normalized_query_string = normalized_query_parts.join("&");

    // 4. Parse credentials
    let credentials = match parse_credentials(req.headers(), &query_params) {
        Ok(c) => c,
        Err(e) => return s3_error_response(e),
    };

    // 5. Clock Skew Validation (Security requirement)
    if let Ok(req_time) = DateTime::parse_from_rfc3339(&credentials.timestamp)
        .or_else(|_| DateTime::parse_from_str(&credentials.timestamp, "%Y%m%dT%H%M%SZ"))
    {
        let now = Utc::now();
        let skew = (now - req_time.with_timezone(&Utc)).num_minutes().abs();
        if skew > 15 {
            return s3_error_response(AuthError::RequestTimeTooSkewed {
                server_time: now.to_rfc3339(),
                request_time: req_time.to_rfc3339(),
            });
        }
    }

    // 6. Validate credential scope
    if credentials.region != state.server_region || credentials.service != "s3" {
        return s3_error_response(AuthError::InvalidCredentialScope {
            expected: format!(
                "{}/{}/s3/aws4_request",
                credentials.date, state.server_region
            ),
            received: format!(
                "{}/{}/{}/{}",
                credentials.date, credentials.region, credentials.service, "aws4_request"
            ),
        });
    }

    // 7. Retrieve secret key
    let secret_key = match state
        .credential_provider
        .get_secret_key(&credentials.access_key)
    {
        Some(k) => k,
        None => {
            return s3_error_response(AuthError::InvalidAccessKey {
                access_key: credentials.access_key,
            })
        }
    };

    // Use cache for signing key
    let signing_key = if let Some(key) = state
        .signing_key_cache
        .get(&credentials.access_key, &credentials.date)
    {
        key
    } else {
        let key = dfs_common::auth::signing::derive_signing_key(
            &secret_key,
            &credentials.date,
            &credentials.region,
            &credentials.service,
        );
        state
            .signing_key_cache
            .insert(&credentials.access_key, &credentials.date, key.clone());
        key
    };

    // 8. Build SigningInput
    let method = req.method().to_string();
    let raw_path = req.uri().path();
    // Re-encode path for canonical request if it's already decoded by axum
    let path = dfs_common::auth::encoding::uri_encode(raw_path, false);

    let mut normalized_headers = BTreeMap::new();
    let mut signed_headers_vec = Vec::new();
    for name in &credentials.signed_headers {
        let name_lower = name.to_lowercase();
        let val_str = req
            .headers()
            .get(&name_lower)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // Collapse spaces and trim
        let collapsed = val_str.split_whitespace().collect::<Vec<_>>().join(" ");
        normalized_headers.insert(name_lower.clone(), vec![collapsed]);
        signed_headers_vec.push(name_lower);
    }
    signed_headers_vec.sort();
    let signed_headers_list = signed_headers_vec.join(";");

    // Payload hash. Default to UNSIGNED-PAYLOAD if not provided
    let payload_hash = req
        .headers()
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "UNSIGNED-PAYLOAD".to_string());

    if payload_hash == "UNSIGNED-PAYLOAD" && !state.allow_unsigned_payload {
        return s3_error_response(AuthError::MissingAuth);
    }

    let input = SigningInput {
        method,
        path,
        query_string: normalized_query_string,
        headers: normalized_headers,
        signed_headers_list,
        payload_hash,
    };

    // 9. Verify Signature
    match dfs_common::auth::signing::verify_signature_with_key(&input, &credentials, &signing_key) {
        Ok(_) => next.run(req).await,
        Err(e) => {
            if let AuthError::SignatureDoesNotMatch {
                canonical_request,
                string_to_sign,
            } = &e
            {
                tracing::warn!(
                    "Signature mismatch. CR: {}, S2S: {}",
                    canonical_request,
                    string_to_sign
                );
            }
            s3_error_response(e)
        }
    }
}

fn s3_error_response(err: AuthError) -> Response {
    let (code, message) = err.to_s3_error();
    let status = match err {
        AuthError::MissingAuth => StatusCode::FORBIDDEN,
        AuthError::InvalidAccessKey { .. } => StatusCode::FORBIDDEN,
        AuthError::SignatureDoesNotMatch { .. } => StatusCode::FORBIDDEN,
        AuthError::RequestTimeTooSkewed { .. } => StatusCode::FORBIDDEN,
        AuthError::InvalidCredentialScope { .. } => StatusCode::BAD_REQUEST,
        AuthError::InsecureTransport => StatusCode::FORBIDDEN,
        AuthError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>{}</Code>
  <Message>{}</Message>
  <Resource>/</Resource>
</Error>"#,
        code, message
    );

    (status, [("Content-Type", "application/xml")], xml).into_response()
}

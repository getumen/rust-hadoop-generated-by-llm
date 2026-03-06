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

    // 1.5 Skip auth for STS requests (they authenticate via OIDC JWT internally)
    if let Some(query) = req.uri().query() {
        if query.contains("Action=AssumeRoleWithWebIdentity") {
            return next.run(req).await;
        }
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

    // Canonical Query String: derive from raw query without re-encoding to
    // avoid mismatches with the client's signature calculation.
    let mut raw_query_pairs: Vec<(&str, &str)> = query_string_raw
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let mut split = pair.splitn(2, '=');
            let k = split.next().unwrap_or("");
            let v = split.next().unwrap_or("");
            (k, v)
        })
        .collect();

    raw_query_pairs.sort_by(|(k1, v1), (k2, v2)| match k1.cmp(k2) {
        std::cmp::Ordering::Equal => v1.cmp(v2),
        other => other,
    });

    let mut normalized_query_parts = Vec::new();
    for (k, v) in raw_query_pairs {
        normalized_query_parts.push(format!("{}={}", k, v));
    }
    let normalized_query_string = normalized_query_parts.join("&");

    // 4. Parse credentials
    // We still need a map for cred parsing
    let query_params: BTreeMap<String, String> =
        serde_urlencoded::from_str(query_string_raw).unwrap_or_default();
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
    let mut evaluation_context = None;
    let mut role_arn_from_token = None;

    // Check for STS token in headers or query
    let sts_token = req
        .headers()
        .get("x-amz-security-token")
        .and_then(|v| v.to_str().ok())
        .or_else(|| query_params.get("X-Amz-Security-Token").map(|s| s.as_str()));

    let secret_key = if let Some(token) = sts_token {
        let sts_mgr = match state.sts_token_manager.as_ref() {
            Some(m) => m,
            None => {
                return s3_error_response(AuthError::InternalError(
                    "STS is not enabled on this server".to_string(),
                ));
            }
        };

        let session_data = match sts_mgr.decrypt_token(token) {
            Ok(data) => data,
            Err(e) => return s3_error_response(e),
        };

        // Check session expiration
        let now = Utc::now().timestamp() as u64;
        if session_data.expiration < now {
            return s3_error_response(AuthError::ExpiredToken);
        }

        role_arn_from_token = Some(session_data.role_arn.clone());
        evaluation_context = Some(session_data.claims.to_policy_context());
        session_data.temp_secret_key
    } else {
        match state
            .credential_provider
            .get_secret_key(&credentials.access_key)
        {
            Some(k) => k,
            None => {
                return s3_error_response(AuthError::InvalidAccessKey {
                    access_key: credentials.access_key,
                })
            }
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
    let path = req.uri().path().to_string();

    let mut normalized_headers = BTreeMap::new();
    let mut signed_headers_vec = Vec::new();
    for name in &credentials.signed_headers {
        let name_lower = name.to_lowercase();
        let header_values = req.headers().get_all(&name_lower);
        let mut vals = Vec::new();
        for val in header_values {
            if let Ok(s) = val.to_str() {
                vals.push(s.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        }
        let joined = vals.join(",");
        normalized_headers.insert(name_lower.clone(), vec![joined]);
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
        Ok(_) => {
            // 10. Policy Evaluation (Phase 3)
            if let Some(role_arn) = role_arn_from_token {
                if let (Some(pe), Some(ctx)) = (&state.policy_evaluator, &evaluation_context) {
                    let (s3_action, s3_resource) = resolve_s3_action_and_resource(&req);
                    if !pe.evaluate(&s3_action, &s3_resource, &role_arn, ctx) {
                        tracing::warn!(
                            "Policy evaluation failed for {} on {}",
                            s3_action,
                            s3_resource
                        );
                        return s3_error_response(AuthError::MissingAuth); // S3 returns AccessDenied (hidden as MissingAuth helper)
                    }
                }
            }

            let mut req = req;
            if let Some(ctx) = evaluation_context {
                req.extensions_mut().insert(ctx);
            }
            next.run(req).await
        }
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
        AuthError::InvalidToken(_) => StatusCode::FORBIDDEN,
        AuthError::ExpiredToken => StatusCode::FORBIDDEN,
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

fn resolve_s3_action_and_resource(req: &Request<Body>) -> (String, String) {
    let method = req.method();
    let path = req.uri().path();

    // Path format: /bucket or /bucket/key
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    let (action, resource) = match (method, parts.as_slice()) {
        (&axum::http::Method::GET, []) => ("s3:ListAllMyBuckets", "arn:dfs:s3:::*".to_string()),
        (&axum::http::Method::GET, [_bucket]) => {
            ("s3:ListBucket", format!("arn:dfs:s3:::{}", parts[0]))
        }
        (&axum::http::Method::GET, [_bucket, ..]) => {
            ("s3:GetObject", format!("arn:dfs:s3:::{}", parts.join("/")))
        }
        (&axum::http::Method::PUT, [_bucket, ..]) => {
            ("s3:PutObject", format!("arn:dfs:s3:::{}", parts.join("/")))
        }
        (&axum::http::Method::DELETE, [_bucket, ..]) => (
            "s3:DeleteObject",
            format!("arn:dfs:s3:::{}", parts.join("/")),
        ),
        (&axum::http::Method::HEAD, [_bucket]) => {
            ("s3:ListBucket", format!("arn:dfs:s3:::{}", parts[0]))
        }
        (&axum::http::Method::HEAD, [_bucket, ..]) => {
            ("s3:GetObject", format!("arn:dfs:s3:::{}", parts.join("/")))
        }
        _ => ("s3:Unknown", "arn:dfs:s3:::*".to_string()),
    };

    (action.to_string(), resource.to_string())
}

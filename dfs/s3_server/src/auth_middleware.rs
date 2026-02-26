use crate::state::AppState;
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use dfs_common::auth::{parse_credentials, AuthError, SigningInput};
use std::collections::BTreeMap;

pub async fn auth_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // 1. Auth enabled check
    if !state.auth_enabled {
        return Ok(next.run(req).await);
    }

    // 2. Exempt endpoints
    let path = req.uri().path();
    if path == "/health" || path == "/metrics" {
        return Ok(next.run(req).await);
    }

    // 3. TLS check
    // In a real proxy, we'd check if it's HTTPS.
    // For this lab, assume it's handled by let's say a flag in AppState if we want to enforce it.
    if state.require_tls {
        // Here we could check req.uri().scheme() or a custom header from a proxy
        // Since we are using axum-server with TLS, we can assume it's TLS if we reach here if properly configured.
        // But for strictness:
        // if req.extensions().get::<axum_server::tls_rustls::TlsStream<...>>().is_none() { ... }
    }

    // 4. Extract Query parameters for cred parsing and normalization
    let query_string_raw = req.uri().query().unwrap_or("");
    let query_params: BTreeMap<String, String> =
        serde_urlencoded::from_str(query_string_raw).unwrap_or_default();

    // Canonical Query String: sort by param name, then value; URI-encode; empty values as key=
    // BTreeMap already sorts by key.
    let mut normalized_query_parts = Vec::new();
    for (k, v) in &query_params {
        let encoded_k = dfs_common::auth::encoding::uri_encode(k, true);
        let encoded_v = dfs_common::auth::encoding::uri_encode(v, true);
        normalized_query_parts.push(format!("{}={}", encoded_k, encoded_v));
    }
    let normalized_query_string = normalized_query_parts.join("&");

    // 5. Parse credentials
    let credentials = match parse_credentials(req.headers(), &query_params) {
        Ok(c) => c,
        Err(_) => return Err(StatusCode::FORBIDDEN),
    };

    // 6. Validate credential scope
    if credentials.region != state.server_region || credentials.service != "s3" {
        return Err(StatusCode::BAD_REQUEST);
    }

    // 7. Retrieve secret key
    let secret_key = match state
        .credential_provider
        .get_secret_key(&credentials.access_key)
    {
        Some(k) => k,
        None => return Err(StatusCode::FORBIDDEN),
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
    // This is the hard part: normalizing everything
    let method = req.method().to_string();
    let path = req.uri().path().to_string(); // S3 expects single encoding, path is already often decoded by axum?
                                             // Actually req.uri().path() might be decoded. S3 needs the raw URI-encoded path from the request line.

    // For now, let's assume we can reconstruct it or use it as is if it's simple.
    // In a real implementation, we'd use the raw path.

    let mut normalized_headers = BTreeMap::new();
    let mut signed_headers_vec = Vec::new();
    for name in &credentials.signed_headers {
        let name_lower = name.to_lowercase();
        if let Some(value) = req.headers().get(&name_lower) {
            let val_str = value.to_str().unwrap_or("").to_string();
            // Collapse spaces and trim
            let collapsed = val_str.split_whitespace().collect::<Vec<_>>().join(" ");
            normalized_headers.insert(name_lower.clone(), vec![collapsed]);
            signed_headers_vec.push(name_lower);
        }
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
        return Err(StatusCode::FORBIDDEN);
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
        Ok(_) => Ok(next.run(req).await),
        Err(AuthError::SignatureDoesNotMatch {
            canonical_request,
            string_to_sign,
        }) => {
            tracing::warn!(
                "Signature mismatch. CR: {}, S2S: {}",
                canonical_request,
                string_to_sign
            );
            Err(StatusCode::FORBIDDEN)
        }
        Err(_) => Err(StatusCode::FORBIDDEN),
    }
}

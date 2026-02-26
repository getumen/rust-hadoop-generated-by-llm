use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub mod cache;
pub mod chunked;
pub mod credentials;
pub mod encoding;
pub mod signing;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedCredentials {
    pub access_key: String,
    pub date: String,    // YYYYMMDD
    pub region: String,  // e.g. "us-east-1"
    pub service: String, // "s3"
    pub signed_headers: Vec<String>,
    pub signature: String, // hex-encoded
    pub timestamp: String, // ISO 8601
}

#[derive(Debug, Clone)]
pub struct SigningInput {
    pub method: String,
    pub path: String,
    pub query_string: String,
    pub headers: BTreeMap<String, Vec<String>>,
    pub signed_headers_list: String,
    pub payload_hash: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Missing or malformed Authorization header or query parameters")]
    MissingAuth,
    #[error("Invalid Access Key ID: {access_key}")]
    InvalidAccessKey { access_key: String },
    #[error(
        "Signature mismatch. CanonicalRequest: {canonical_request}, StringToSign: {string_to_sign}"
    )]
    SignatureDoesNotMatch {
        canonical_request: String,
        string_to_sign: String,
    },
    #[error("Request time too skewed. Server: {server_time}, Request: {request_time}")]
    RequestTimeTooSkewed {
        server_time: String,
        request_time: String,
    },
    #[error("Invalid Credential Scope. Expected: {expected}, Received: {received}")]
    InvalidCredentialScope { expected: String, received: String },
    #[error("Insecure transport. SigV4 requires TLS")]
    InsecureTransport,
    #[error("Internal authentication error: {0}")]
    InternalError(String),
}

impl AuthError {
    pub fn to_s3_error(&self) -> (String, String) {
        match self {
            AuthError::MissingAuth => ("AccessDenied".to_string(), "Access Denied".to_string()),
            AuthError::InvalidAccessKey { .. } => (
                "InvalidAccessKeyId".to_string(),
                "The AWS Access Key Id you provided does not exist in our records.".to_string(),
            ),
            AuthError::SignatureDoesNotMatch { .. } => (
                "SignatureDoesNotMatch".to_string(),
                "The request signature we calculated does not match the signature you provided."
                    .to_string(),
            ),
            AuthError::RequestTimeTooSkewed { .. } => (
                "RequestTimeTooSkewed".to_string(),
                "The difference between the request time and the current time is too large."
                    .to_string(),
            ),
            AuthError::InvalidCredentialScope { .. } => (
                "AuthorizationHeaderMalformed".to_string(),
                "The authorization header is malformed; the region or service is wrong."
                    .to_string(),
            ),
            AuthError::InsecureTransport => (
                "AccessDenied".to_string(),
                "Access Denied (Insecure Transport)".to_string(),
            ),
            AuthError::InternalError(_) => (
                "InternalError".to_string(),
                "An internal error occurred during authentication.".to_string(),
            ),
        }
    }
}

/// Parses SigV4 credentials from Authorization header or Query parameters.
pub fn parse_credentials(
    headers: &http::HeaderMap,
    query: &BTreeMap<String, String>,
) -> Result<ParsedCredentials, AuthError> {
    // Try Header first
    if let Some(auth) = headers.get(http::header::AUTHORIZATION) {
        let auth_str = auth.to_str().map_err(|_| AuthError::MissingAuth)?;
        if !auth_str.starts_with("AWS4-HMAC-SHA256") {
            return Err(AuthError::MissingAuth);
        }

        // Authorization: AWS4-HMAC-SHA256 Credential=<ak>/<date>/<region>/<service>/aws4_request, SignedHeaders=..., Signature=...
        let parts: Vec<&str> = auth_str.split(',').map(|s| s.trim()).collect();
        if parts.len() < 3 {
            return Err(AuthError::MissingAuth);
        }

        // parts[0]: AWS4-HMAC-SHA256 Credential=<ak>/<date>/...
        let cred_kv: Vec<&str> = parts[0].split_whitespace().collect();
        let cred_entry = cred_kv
            .iter()
            .find(|s| s.starts_with("Credential="))
            .ok_or(AuthError::MissingAuth)?;
        let cred_part = cred_entry.split('=').nth(1).ok_or(AuthError::MissingAuth)?;

        let cred_subparts: Vec<&str> = cred_part.split('/').collect();
        if cred_subparts.len() < 5 {
            return Err(AuthError::MissingAuth);
        }

        let signed_headers_part = parts[1]
            .split('=')
            .nth(1)
            .ok_or(AuthError::MissingAuth)?
            .trim();
        let signed_headers: Vec<String> = signed_headers_part
            .split(';')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let signature = parts[2]
            .split('=')
            .nth(1)
            .ok_or(AuthError::MissingAuth)?
            .trim()
            .to_string();

        let timestamp = headers
            .get("x-amz-date")
            .or_else(|| headers.get(http::header::DATE))
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthError::MissingAuth)?
            .to_string();

        return Ok(ParsedCredentials {
            access_key: cred_subparts[0].to_string(),
            date: cred_subparts[1].to_string(),
            region: cred_subparts[2].to_string(),
            service: cred_subparts[3].to_string(),
            signed_headers,
            signature,
            timestamp,
        });
    }

    // Try Query parameters
    if let Some(algo) = query.get("X-Amz-Algorithm") {
        if algo != "AWS4-HMAC-SHA256" {
            return Err(AuthError::MissingAuth);
        }

        let cred_part = query
            .get("X-Amz-Credential")
            .ok_or(AuthError::MissingAuth)?;
        let cred_subparts: Vec<&str> = cred_part.split('/').collect();
        if cred_subparts.len() < 5 {
            return Err(AuthError::MissingAuth);
        }

        let signed_headers_part = query
            .get("X-Amz-SignedHeaders")
            .ok_or(AuthError::MissingAuth)?;
        let signed_headers: Vec<String> = signed_headers_part
            .split(';')
            .map(|s| s.to_string())
            .collect();

        let signature = query
            .get("X-Amz-Signature")
            .ok_or(AuthError::MissingAuth)?
            .clone();
        let timestamp = query
            .get("X-Amz-Date")
            .ok_or(AuthError::MissingAuth)?
            .clone();

        return Ok(ParsedCredentials {
            access_key: cred_subparts[0].to_string(),
            date: cred_subparts[1].to_string(),
            region: cred_subparts[2].to_string(),
            service: cred_subparts[3].to_string(),
            signed_headers,
            signature,
            timestamp,
        });
    }

    Err(AuthError::MissingAuth)
}

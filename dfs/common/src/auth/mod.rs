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
        if cred_subparts.len() < 5 || cred_subparts[4] != "aws4_request" {
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
        if cred_subparts.len() < 5 || cred_subparts[4] != "aws4_request" {
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

#[cfg(test)]
mod tests {
    use super::{parse_credentials, AuthError};
    use http::header::AUTHORIZATION;
    use http::HeaderMap;
    use std::collections::BTreeMap;

    #[test]
    fn parse_credentials_query_valid() {
        let headers = HeaderMap::new();
        let mut query = BTreeMap::new();

        query.insert(
            "X-Amz-Algorithm".to_string(),
            "AWS4-HMAC-SHA256".to_string(),
        );
        query.insert(
            "X-Amz-Credential".to_string(),
            "AKIDEXAMPLE/20240220/us-east-1/s3/aws4_request".to_string(),
        );
        query.insert(
            "X-Amz-SignedHeaders".to_string(),
            "host;x-amz-date".to_string(),
        );
        query.insert(
            "X-Amz-Signature".to_string(),
            "abcdef1234567890".to_string(),
        );
        query.insert("X-Amz-Date".to_string(), "20240220T010203Z".to_string());

        let creds = parse_credentials(&headers, &query).expect("expected valid credentials");

        assert_eq!(creds.access_key, "AKIDEXAMPLE");
        assert_eq!(creds.date, "20240220");
        assert_eq!(creds.region, "us-east-1");
        assert_eq!(creds.service, "s3");
        assert_eq!(
            creds.signed_headers,
            vec!["host".to_string(), "x-amz-date".to_string()]
        );
        assert_eq!(creds.signature, "abcdef1234567890");
        assert_eq!(creds.timestamp, "20240220T010203Z");
    }

    #[test]
    fn parse_credentials_header_valid() {
        let mut headers = HeaderMap::new();
        let query = BTreeMap::new();

        headers.insert(AUTHORIZATION, "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20240220/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=abcdef1234567890".parse().unwrap());
        headers.insert("x-amz-date", "20240220T010203Z".parse().unwrap());

        let creds = parse_credentials(&headers, &query).expect("expected valid credentials");

        assert_eq!(creds.access_key, "AKIDEXAMPLE");
        assert_eq!(creds.date, "20240220");
        assert_eq!(creds.region, "us-east-1");
        assert_eq!(creds.service, "s3");
        assert_eq!(
            creds.signed_headers,
            vec!["host".to_string(), "x-amz-date".to_string()]
        );
        assert_eq!(creds.signature, "abcdef1234567890");
        assert_eq!(creds.timestamp, "20240220T010203Z");
    }

    #[test]
    fn parse_credentials_query_invalid_scope() {
        let headers = HeaderMap::new();
        let mut query = BTreeMap::new();

        query.insert(
            "X-Amz-Algorithm".to_string(),
            "AWS4-HMAC-SHA256".to_string(),
        );
        // Missing "aws4_request" at the end
        query.insert(
            "X-Amz-Credential".to_string(),
            "AKIDEXAMPLE/20240220/us-east-1/s3/invalid".to_string(),
        );
        query.insert("X-Amz-SignedHeaders".to_string(), "host".to_string());
        query.insert("X-Amz-Signature".to_string(), "sig".to_string());
        query.insert("X-Amz-Date".to_string(), "date".to_string());

        let result = parse_credentials(&headers, &query);
        assert!(matches!(result, Err(AuthError::MissingAuth)));
    }

    #[test]
    fn parse_credentials_header_invalid_scope() {
        let mut headers = HeaderMap::new();
        let query = BTreeMap::new();

        headers.insert(AUTHORIZATION, "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20240220/us-east-1/s3/bad, SignedHeaders=host, Signature=sig".parse().unwrap());
        headers.insert("x-amz-date", "date".parse().unwrap());

        let result = parse_credentials(&headers, &query);
        assert!(matches!(result, Err(AuthError::MissingAuth)));
    }

    #[test]
    fn parse_credentials_missing_auth() {
        let headers = HeaderMap::new();
        let query = BTreeMap::new();

        let result = parse_credentials(&headers, &query);
        assert!(matches!(result, Err(AuthError::MissingAuth)));
    }
}

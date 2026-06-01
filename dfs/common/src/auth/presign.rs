use crate::auth::encoding::uri_encode;
use crate::auth::signing::{
    calculate_signature, create_canonical_request, create_string_to_sign, derive_signing_key,
};
use crate::auth::SigningInput;
use chrono::Utc;
use std::collections::BTreeMap;

pub struct PresignParams<'a> {
    pub endpoint: &'a str,
    pub bucket: &'a str,
    pub key: &'a str,
    pub method: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
    pub expires_secs: u64,
}

pub fn generate_presigned_url(params: &PresignParams<'_>) -> String {
    let now = Utc::now();
    let date = now.format("%Y%m%d").to_string();
    let datetime = now.format("%Y%m%dT%H%M%SZ").to_string();

    let scope = format!("{}/{}/s3/aws4_request", date, params.region);
    let credential = format!("{}/{}", params.access_key, scope);

    // Build canonical query params (sorted, no X-Amz-Signature)
    let mut query_params: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".to_string(), "AWS4-HMAC-SHA256".to_string()),
        ("X-Amz-Credential".to_string(), credential),
        ("X-Amz-Date".to_string(), datetime.clone()),
        ("X-Amz-Expires".to_string(), params.expires_secs.to_string()),
        ("X-Amz-SignedHeaders".to_string(), "host".to_string()),
    ];
    query_params.sort_by(|(a, _), (b, _)| a.cmp(b));

    let canonical_query_string: String = query_params
        .iter()
        .map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true)))
        .collect::<Vec<_>>()
        .join("&");

    // Extract host from endpoint (strip scheme, trailing slash)
    let host = params
        .endpoint
        .split("://")
        .nth(1)
        .unwrap_or(params.endpoint)
        .trim_end_matches('/');

    // URI-encode the path: encode bucket and each key segment individually
    let encoded_bucket = uri_encode(params.bucket, true);
    let encoded_key = params
        .key
        .split('/')
        .map(|seg| uri_encode(seg, true))
        .collect::<Vec<_>>()
        .join("/");
    let path = format!("/{}/{}", encoded_bucket, encoded_key);
    let mut headers = BTreeMap::new();
    headers.insert("host".to_string(), vec![host.to_string()]);

    let input = SigningInput {
        method: params.method.to_uppercase(),
        path: path.clone(),
        query_string: canonical_query_string.clone(),
        headers,
        signed_headers_list: "host".to_string(),
        payload_hash: "UNSIGNED-PAYLOAD".to_string(),
    };

    let canonical_request = create_canonical_request(&input);
    let string_to_sign = create_string_to_sign(&datetime, &scope, &canonical_request);
    let signing_key = derive_signing_key(params.secret_key, &date, params.region, "s3");
    let signature = calculate_signature(&signing_key, &string_to_sign);

    let base = params.endpoint.trim_end_matches('/');
    format!("{base}{path}?{canonical_query_string}&X-Amz-Signature={signature}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_params() -> PresignParams<'static> {
        PresignParams {
            endpoint: "http://localhost:9000",
            bucket: "mybucket",
            key: "myfile.txt",
            method: "GET",
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            expires_secs: 3600,
        }
    }

    #[test]
    fn test_url_starts_with_endpoint_and_path() {
        let url = generate_presigned_url(&test_params());
        assert!(
            url.starts_with("http://localhost:9000/mybucket/myfile.txt?"),
            "URL: {}",
            url
        );
    }

    #[test]
    fn test_url_contains_required_query_params() {
        let url = generate_presigned_url(&test_params());
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"), "URL: {}", url);
        assert!(url.contains("X-Amz-Expires=3600"), "URL: {}", url);
        assert!(url.contains("X-Amz-SignedHeaders=host"), "URL: {}", url);
        assert!(url.contains("X-Amz-Signature="), "URL: {}", url);
    }

    #[test]
    fn test_signature_comes_last() {
        let url = generate_presigned_url(&test_params());
        let sig_idx = url.find("X-Amz-Signature=").unwrap();
        let algo_idx = url.find("X-Amz-Algorithm=").unwrap();
        assert!(sig_idx > algo_idx, "X-Amz-Signature must be last param");
    }

    #[test]
    fn test_credential_contains_access_key() {
        let url = generate_presigned_url(&test_params());
        assert!(url.contains("AKIAIOSFODNN7EXAMPLE"), "URL: {}", url);
    }

    #[test]
    fn test_put_method_works() {
        let mut p = test_params();
        p.method = "PUT";
        let url = generate_presigned_url(&p);
        assert!(url.contains("X-Amz-Signature="), "URL: {}", url);
    }

    #[test]
    fn test_key_with_spaces_and_slashes_is_encoded() {
        let mut p = test_params();
        p.key = "dir/file name.txt";
        let url = generate_presigned_url(&p);
        // Path in URL should have space encoded as %20
        assert!(url.contains("/mybucket/dir/file%20name.txt"), "URL: {}", url);
    }

    #[test]
    fn test_credential_slashes_are_percent_encoded() {
        let url = generate_presigned_url(&test_params());
        // X-Amz-Credential value should have '/' encoded as %2F
        assert!(
            url.contains("X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F"),
            "Credential slashes must be percent-encoded. URL: {}",
            url
        );
    }
}

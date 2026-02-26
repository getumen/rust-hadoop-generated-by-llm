use crate::auth::{AuthError, ParsedCredentials, SigningInput};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Step 1: Create a Canonical Request.
pub fn create_canonical_request(input: &SigningInput) -> String {
    let mut canonical = String::new();

    // HTTPMethod
    canonical.push_str(&input.method);
    canonical.push('\n');

    // CanonicalURI
    canonical.push_str(&input.path);
    canonical.push('\n');

    // CanonicalQueryString
    canonical.push_str(&input.query_string);
    canonical.push('\n');

    // CanonicalHeaders
    for (name, values) in &input.headers {
        canonical.push_str(name);
        canonical.push(':');
        // Join multiple values with comma and collapse spaces in implementation if not done earlier
        // Here we assume values are already normalized as per §1.2
        let joined = values.join(",");
        canonical.push_str(&joined);
        canonical.push('\n');
    }
    canonical.push('\n');

    // SignedHeaders
    canonical.push_str(&input.signed_headers_list);
    canonical.push('\n');

    // HashedPayload
    canonical.push_str(&input.payload_hash);

    canonical
}

/// Step 2: Create a String to Sign.
pub fn create_string_to_sign(timestamp: &str, scope: &str, canonical_request: &str) -> String {
    let mut s2s = String::new();
    s2s.push_str("AWS4-HMAC-SHA256\n");
    s2s.push_str(timestamp);
    s2s.push('\n');
    s2s.push_str(scope);
    s2s.push('\n');

    let mut hasher = Sha256::new();
    hasher.update(canonical_request.as_bytes());
    s2s.push_str(&hex::encode(hasher.finalize()));

    s2s
}

/// Step 3: Derive Signing Key.
pub fn derive_signing_key(secret_key: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let secret = format!("AWS4{}", secret_key);

    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(date.as_bytes());
    let k_date = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_date).expect("HMAC can take key of any size");
    mac.update(region.as_bytes());
    let k_region = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_region).expect("HMAC can take key of any size");
    mac.update(service.as_bytes());
    let k_service = mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&k_service).expect("HMAC can take key of any size");
    mac.update(b"aws4_request");
    mac.finalize().into_bytes().to_vec()
}

/// Step 4: Calculate Signature.
pub fn calculate_signature(signing_key: &[u8], string_to_sign: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(signing_key).expect("HMAC can take key of any size");
    mac.update(string_to_sign.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Verification logic using derived signing key.
pub fn verify_signature_with_key(
    input: &SigningInput,
    credentials: &ParsedCredentials,
    signing_key: &[u8],
) -> Result<(), AuthError> {
    let canonical_request = create_canonical_request(input);
    let scope = format!(
        "{}/{}/{}/aws4_request",
        credentials.date, credentials.region, credentials.service
    );
    let string_to_sign = create_string_to_sign(&credentials.timestamp, &scope, &canonical_request);

    let expected_signature = calculate_signature(signing_key, &string_to_sign);

    // Constant-time comparison
    if expected_signature
        .as_bytes()
        .ct_eq(credentials.signature.as_bytes())
        .unwrap_u8()
        == 1
    {
        Ok(())
    } else {
        Err(AuthError::SignatureDoesNotMatch {
            canonical_request,
            string_to_sign,
        })
    }
}

/// Verification logic using secret key.
pub fn verify_signature(
    input: &SigningInput,
    credentials: &ParsedCredentials,
    secret_key: &str,
) -> Result<(), AuthError> {
    let signing_key = derive_signing_key(
        secret_key,
        &credentials.date,
        &credentials.region,
        &credentials.service,
    );
    verify_signature_with_key(input, credentials, &signing_key)
}

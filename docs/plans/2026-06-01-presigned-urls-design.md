# Pre-signed URLs Design

**Date:** 2026-06-01

## Goal

Allow any holder of a time-limited URL to perform GET, PUT, or DELETE operations on an object without needing AWS credentials directly. The URL embeds all authentication parameters as query strings.

## Architecture

Three components are touched:

1. **`dfs/common/src/auth/presign.rs`** (new) — pure function `generate_presigned_url` that computes a SigV4-signed URL client-side.
2. **`dfs/client/src/bin/dfs_cli.rs`** (modify) — `presign` subcommand that reads credentials from environment variables and prints the URL.
3. **`dfs/s3_server/src/auth_middleware.rs`** (modify) — detects presigned requests (presence of `X-Amz-Expires` query param) and validates expiry instead of clock skew.

URL generation is entirely client-side (no server roundtrip). This matches AWS SDK behavior and avoids exposing secret keys to the server.

## URL Format

```
http://localhost:9000/bucket/key
  ?X-Amz-Algorithm=AWS4-HMAC-SHA256
  &X-Amz-Credential=AKID%2F20240101%2Fus-east-1%2Fs3%2Faws4_request
  &X-Amz-Date=20240101T000000Z
  &X-Amz-Expires=3600
  &X-Amz-SignedHeaders=host
  &X-Amz-Signature=<hex>
```

Supported methods: GET, PUT, DELETE. Maximum expiry: 604800 seconds (7 days).

## Generation Logic (`presign.rs`)

Input:

```rust
pub struct PresignParams<'a> {
    pub endpoint: &'a str,    // e.g. "http://localhost:9000"
    pub bucket: &'a str,
    pub key: &'a str,
    pub method: &'a str,      // "GET" | "PUT" | "DELETE"
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
    pub expires_secs: u64,    // max 604800
}
```

Steps:

1. Compute `X-Amz-Date` from current UTC time (`%Y%m%dT%H%M%SZ`).
2. Build sorted query parameters (excluding `X-Amz-Signature`):
   - `X-Amz-Algorithm=AWS4-HMAC-SHA256`
   - `X-Amz-Credential={access_key}/{date}/{region}/s3/aws4_request` (percent-encoded)
   - `X-Amz-Date={datetime}`
   - `X-Amz-Expires={expires_secs}`
   - `X-Amz-SignedHeaders=host`
3. Build CanonicalRequest:
   - Method, URI path, canonical query string (URL-encoded, sorted)
   - Headers: `host:{host}\n`
   - SignedHeaders: `host`
   - Payload hash: `UNSIGNED-PAYLOAD`
4. Compute StringToSign using existing `create_string_to_sign`.
5. Derive signing key using existing `derive_signing_key`.
6. Compute HMAC-SHA256 signature (hex).
7. Append `&X-Amz-Signature={sig}` to construct the final URL.

## CLI Interface

```bash
export AWS_ACCESS_KEY_ID=mykey
export AWS_SECRET_ACCESS_KEY=mysecret
export AWS_REGION=us-east-1          # optional, default "us-east-1"
export S3_ENDPOINT=http://localhost:9000  # optional

dfs_cli presign s3://mybucket/myfile.txt --method GET --expires 3600
# Output: http://localhost:9000/mybucket/myfile.txt?X-Amz-Algorithm=...

dfs_cli presign s3://mybucket/upload.bin --method PUT --expires 300
```

## Server-side Validation Changes

### Presigned URL detection

```rust
let is_presigned = query_params.contains_key("X-Amz-Expires");
```

### Expiry check (replaces clock-skew for presigned requests)

```rust
if is_presigned {
    let expires_secs: u64 = query_params["X-Amz-Expires"].parse().unwrap_or(0);
    let request_dt = parse_amz_date(&credentials.timestamp)?;
    let expiry = request_dt + chrono::Duration::seconds(expires_secs as i64);
    if Utc::now() > expiry {
        return s3_error_response(AuthError::ExpiredToken);
    }
} else {
    // existing 15-minute clock skew check
}
```

### Canonical query string excludes `X-Amz-Signature`

`normalize_query_string` gains an `is_presigned: bool` parameter. When true, `X-Amz-Signature` is filtered out before sorting, matching the signing algorithm spec.

### `UNSIGNED-PAYLOAD` always allowed for presigned requests

The `allow_unsigned_payload` gate is skipped when `is_presigned == true`.

## Testing

- Unit tests in `presign.rs`: verify generated URL contains expected query params, signature round-trips correctly.
- Unit tests in `auth_middleware.rs`: verify expired presigned URL returns 403, valid URL passes.
- Shell test extension in `test_scripts/run_s3_test.sh`: upload object, generate presigned GET URL, `curl` it, verify response.

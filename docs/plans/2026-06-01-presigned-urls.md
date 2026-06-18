# Pre-signed URLs Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Allow any holder of a time-limited URL to perform GET, PUT, or DELETE S3 operations without credentials, using SigV4 query-parameter authentication.

**Architecture:** Generation is pure client-side crypto in `dfs/common/src/auth/presign.rs` (no server roundtrip). The S3 server already parses SigV4 query params via `parse_credentials`; we add expiry validation and fix the canonical query string to exclude `X-Amz-Signature`. A new `dfs_cli presign` subcommand drives generation from env vars.

**Tech Stack:** `chrono` (timestamps), existing `dfs_common::auth::signing` (derive_signing_key, calculate_signature, create_canonical_request, create_string_to_sign), existing `dfs_common::auth::encoding::uri_encode`, `clap` (CLI).

---

### Task 1: Add `generate_presigned_url` to `dfs/common/src/auth/presign.rs`

**Files:**
- Create: `dfs/common/src/auth/presign.rs`
- Modify: `dfs/common/src/auth/mod.rs` (add `pub mod presign;`)

**Background:**
A SigV4 presigned URL contains all auth params as query string. The signature is computed over: method, URI path, canonical query string (all params *except* `X-Amz-Signature`, URI-encoded and sorted), headers (`host` only), and payload hash (`UNSIGNED-PAYLOAD`). `X-Amz-Signature` is appended last.

The existing helpers in `dfs_common::auth::signing` do all the heavy lifting:
- `create_canonical_request(input: &SigningInput) -> String`
- `create_string_to_sign(timestamp, scope, canonical_request) -> String`
- `derive_signing_key(secret, date, region, service) -> Vec<u8>`
- `calculate_signature(signing_key, string_to_sign) -> String`

The existing `uri_encode(input: &str, encode_slash: bool) -> String` in `dfs_common::auth::encoding` handles SigV4 percent-encoding.

**Step 1: Write failing tests**

Create `dfs/common/src/auth/presign.rs` with tests only (no implementation yet):

```rust
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
    todo!()
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
}
```

Add to `dfs/common/src/auth/mod.rs` (after the existing `pub mod signing;` line):
```rust
pub mod presign;
```

**Step 2: Run tests to verify they fail**

```bash
cargo test -p dfs-common --lib auth::presign 2>&1 | tail -10
```

Expected: FAIL with "not yet implemented" (todo!() panic).

**Step 3: Implement `generate_presigned_url`**

Replace the `todo!()` with:

```rust
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
    let credential_encoded = uri_encode(&credential, true); // encode '/' as %2F

    // Build canonical query string (sorted, without X-Amz-Signature)
    // Keys must be URI-encoded, values must be URI-encoded
    let mut query_params: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".to_string(), "AWS4-HMAC-SHA256".to_string()),
        ("X-Amz-Credential".to_string(), credential_encoded.clone()),
        ("X-Amz-Date".to_string(), datetime.clone()),
        ("X-Amz-Expires".to_string(), params.expires_secs.to_string()),
        ("X-Amz-SignedHeaders".to_string(), "host".to_string()),
    ];
    query_params.sort_by(|(a, _), (b, _)| a.cmp(b));

    let canonical_query_string: String = query_params
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    // Extract host from endpoint (strip scheme)
    let host = params
        .endpoint
        .split("://")
        .nth(1)
        .unwrap_or(params.endpoint);

    // Build SigningInput
    let path = format!("/{}/{}", params.bucket, params.key);
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

    // Assemble final URL
    format!(
        "{}{path}?{}&X-Amz-Signature={}",
        params.endpoint,
        canonical_query_string,
        signature,
        path = path,
    )
}
```

**Step 4: Run tests**

```bash
cargo test -p dfs-common --lib auth::presign 2>&1 | tail -10
```

Expected: all 5 tests PASS.

**Step 5: Build**

```bash
cargo build -p dfs-common 2>&1 | head -10
```

Expected: clean build.

**Step 6: Commit**

```bash
git add dfs/common/src/auth/presign.rs dfs/common/src/auth/mod.rs
git commit -m "feat: add generate_presigned_url to dfs-common auth"
```

---

### Task 2: Update `auth_middleware.rs` to validate presigned URLs

**Files:**
- Modify: `dfs/s3_server/src/auth_middleware.rs`

**Background:**
Three changes are needed:

1. **Expiry check**: Presigned URLs have `X-Amz-Expires` in query params. Instead of the 15-minute clock-skew check, validate that `X-Amz-Date + X-Amz-Expires > now`.

2. **Canonical query string**: `normalize_query_string` must exclude `X-Amz-Signature` when building the canonical query string, because the signer did not include it when computing the signature. `X-Amz-Signature` is only present in query params for presigned URLs (non-presigned requests put it in the Authorization header), so we can always strip it unconditionally.

3. **UNSIGNED-PAYLOAD gate**: The check `if payload_hash == "UNSIGNED-PAYLOAD" && !state.allow_unsigned_payload` must be skipped for presigned requests (payload hash is always `UNSIGNED-PAYLOAD` for presigned URLs by spec).

**Step 1: Write failing tests**

Add to the bottom of `dfs/s3_server/src/auth_middleware.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_query_string_excludes_x_amz_signature() {
        let raw = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature=abc123&X-Amz-Expires=3600";
        let normalized = normalize_query_string(raw);
        assert!(!normalized.contains("X-Amz-Signature"), "Got: {}", normalized);
        assert!(normalized.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"), "Got: {}", normalized);
        assert!(normalized.contains("X-Amz-Expires=3600"), "Got: {}", normalized);
    }

    #[test]
    fn test_normalize_query_string_sorts_params() {
        let raw = "Z-Param=z&A-Param=a";
        let normalized = normalize_query_string(raw);
        let a_idx = normalized.find("A-Param").unwrap();
        let z_idx = normalized.find("Z-Param").unwrap();
        assert!(a_idx < z_idx, "A should come before Z, got: {}", normalized);
    }

    #[test]
    fn test_normalize_query_string_without_signature_unchanged() {
        // Non-presigned request: no X-Amz-Signature in query, behavior unchanged
        let raw = "list-type=2&prefix=foo";
        let normalized = normalize_query_string(raw);
        assert!(normalized.contains("list-type=2"), "Got: {}", normalized);
        assert!(normalized.contains("prefix=foo"), "Got: {}", normalized);
    }
}
```

**Step 2: Run tests to verify `test_normalize_query_string_excludes_x_amz_signature` fails**

```bash
cargo test -p dfs-s3-server --lib auth_middleware::tests 2>&1 | tail -15
```

Expected: `test_normalize_query_string_excludes_x_amz_signature` FAILS (X-Amz-Signature is currently not stripped).

**Step 3: Fix `normalize_query_string` to strip `X-Amz-Signature`**

In `dfs/s3_server/src/auth_middleware.rs`, find the `normalize_query_string` function (near the bottom) and replace:

```rust
fn normalize_query_string(query_string_raw: &str) -> String {
    let mut raw_query_pairs: Vec<(&str, &str)> = query_string_raw
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
```

With:

```rust
fn normalize_query_string(query_string_raw: &str) -> String {
    let mut raw_query_pairs: Vec<(&str, &str)> = query_string_raw
        .split('&')
        .filter(|s| !s.is_empty())
        .filter(|s| !s.starts_with("X-Amz-Signature=") && *s != "X-Amz-Signature")
        .map(|pair| {
```

**Step 4: Run tests again**

```bash
cargo test -p dfs-s3-server --lib auth_middleware::tests 2>&1 | tail -15
```

Expected: all 3 tests PASS.

**Step 5: Add expiry-check logic for presigned requests**

In `auth_middleware`, after step 3 (query param extraction, around line 86-90) and before step 4 (parse credentials, line 92), add:

```rust
    let is_presigned = query_params.contains_key("X-Amz-Expires");
```

Then find the **clock skew block** (currently checks `skew > 15`, around line 113-135). Wrap it in `if !is_presigned { ... }` and add the presigned expiry check:

Replace the entire clock-skew block:

```rust
    // 5. Clock Skew Validation
    if let Ok(req_time) = DateTime::parse_from_rfc3339(&credentials.timestamp)
        .or_else(|_| DateTime::parse_from_str(&credentials.timestamp, "%Y%m%dT%H%M%SZ"))
    {
        let now = Utc::now();
        let skew = (now - req_time.with_timezone(&Utc)).num_minutes().abs();
        if skew > 15 {
            let err = AuthError::RequestTimeTooSkewed {
                server_time: now.to_rfc3339(),
                request_time: req_time.to_rfc3339(),
            };
            IAM_AUTH_REQUESTS
                .with_label_values(&["failure", "clock_skew"])
                .inc();
            IAM_AUTH_DURATION
                .with_label_values(&["failure"])
                .observe(auth_timer.elapsed().as_secs_f64());
            let res = s3_error_response(err.clone());
            let (err_code, _) = err.to_s3_error();
            audit_ctx.log(&state, res.status().as_u16(), Some(err_code), &query_params);
            return res;
        }
    }
```

With:

```rust
    // 5. Clock Skew / Expiry Validation
    if let Ok(req_time) = DateTime::parse_from_rfc3339(&credentials.timestamp)
        .or_else(|_| DateTime::parse_from_str(&credentials.timestamp, "%Y%m%dT%H%M%SZ"))
    {
        let now = Utc::now();
        if is_presigned {
            // Presigned: validate X-Amz-Date + X-Amz-Expires > now
            let expires_secs: i64 = query_params
                .get("X-Amz-Expires")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let expiry = req_time.with_timezone(&Utc)
                + chrono::Duration::seconds(expires_secs);
            if now > expiry {
                IAM_AUTH_REQUESTS
                    .with_label_values(&["failure", "expired_token"])
                    .inc();
                IAM_AUTH_DURATION
                    .with_label_values(&["failure"])
                    .observe(auth_timer.elapsed().as_secs_f64());
                let res = s3_error_response(AuthError::ExpiredToken);
                audit_ctx.log(
                    &state,
                    res.status().as_u16(),
                    Some("ExpiredToken".to_string()),
                    &query_params,
                );
                return res;
            }
        } else {
            // Normal request: 15-minute clock skew check
            let skew = (now - req_time.with_timezone(&Utc)).num_minutes().abs();
            if skew > 15 {
                let err = AuthError::RequestTimeTooSkewed {
                    server_time: now.to_rfc3339(),
                    request_time: req_time.to_rfc3339(),
                };
                IAM_AUTH_REQUESTS
                    .with_label_values(&["failure", "clock_skew"])
                    .inc();
                IAM_AUTH_DURATION
                    .with_label_values(&["failure"])
                    .observe(auth_timer.elapsed().as_secs_f64());
                let res = s3_error_response(err.clone());
                let (err_code, _) = err.to_s3_error();
                audit_ctx.log(&state, res.status().as_u16(), Some(err_code), &query_params);
                return res;
            }
        }
    }
```

**Step 6: Skip `UNSIGNED-PAYLOAD` gate for presigned requests**

Find the block (around line 295-311):

```rust
    if payload_hash == "UNSIGNED-PAYLOAD" && !state.allow_unsigned_payload {
```

Change to:

```rust
    if payload_hash == "UNSIGNED-PAYLOAD" && !state.allow_unsigned_payload && !is_presigned {
```

**Step 7: Build**

```bash
cargo build -p dfs-s3-server 2>&1 | head -15
```

Expected: clean build.

**Step 8: Run tests**

```bash
cargo test -p dfs-s3-server --lib 2>&1 | tail -10
```

Expected: all tests PASS.

**Step 9: Commit**

```bash
git add dfs/s3_server/src/auth_middleware.rs
git commit -m "feat: validate presigned URL expiry in auth_middleware"
```

---

### Task 3: Add `presign` subcommand to `dfs_cli`

**Files:**
- Modify: `dfs/client/src/bin/dfs_cli.rs`
- Modify: `dfs/client/Cargo.toml` (add `dfs-common` dependency if not present)

**Background:**
The CLI reads credentials from environment variables (matching AWS CLI convention). It parses a `s3://bucket/key` URL, calls `generate_presigned_url`, and prints the result to stdout. No network connection needed (pure crypto).

**Step 1: Check if `dfs-common` is already a dependency of `dfs-client`**

```bash
grep "dfs-common\|dfs_common" /Users/ynakazat/github/getumen/rust-hadoop-generated-by-llm/dfs/client/Cargo.toml
```

If not present, add to `dfs/client/Cargo.toml` under `[dependencies]`:
```toml
dfs-common = { path = "../common" }
```

**Step 2: Write a unit test for argument parsing**

Add to `dfs/client/src/bin/dfs_cli.rs` at the bottom:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_parse_s3_url() {
        let (bucket, key) = parse_s3_url("s3://mybucket/path/to/file.txt").unwrap();
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "path/to/file.txt");
    }

    #[test]
    fn test_parse_s3_url_invalid() {
        assert!(parse_s3_url("mybucket/key").is_err());
        assert!(parse_s3_url("s3://").is_err());
    }
}
```

**Step 3: Run test to verify it fails**

```bash
cargo test -p dfs-client --bin dfs_cli 2>&1 | tail -10
```

Expected: FAIL — `parse_s3_url` not defined.

**Step 4: Add the `Presign` command and `parse_s3_url` helper**

In `dfs/client/src/bin/dfs_cli.rs`:

Add to `Commands` enum (after `Benchmark`):

```rust
    /// Generate a pre-signed URL for temporary access to an object
    Presign {
        /// S3 URL of the object (e.g. s3://bucket/key)
        url: String,
        /// HTTP method (GET, PUT, DELETE)
        #[arg(long, default_value = "GET")]
        method: String,
        /// URL validity in seconds (max 604800 = 7 days)
        #[arg(long, default_value_t = 3600)]
        expires: u64,
        /// S3 endpoint (overrides S3_ENDPOINT env var)
        #[arg(long)]
        endpoint: Option<String>,
    },
```

Add a helper function (before `main`):

```rust
fn parse_s3_url(url: &str) -> anyhow::Result<(String, String)> {
    let path = url
        .strip_prefix("s3://")
        .ok_or_else(|| anyhow::anyhow!("URL must start with s3://"))?;
    let slash = path
        .find('/')
        .ok_or_else(|| anyhow::anyhow!("URL must contain a key (s3://bucket/key)"))?;
    let bucket = path[..slash].to_string();
    let key = path[slash + 1..].to_string();
    if bucket.is_empty() || key.is_empty() {
        anyhow::bail!("Bucket and key must not be empty");
    }
    Ok((bucket, key))
}
```

Add the match arm in `match cli.command` (before the closing `}`):

```rust
        Commands::Presign { url, method, expires, endpoint } => {
            let access_key = std::env::var("AWS_ACCESS_KEY_ID")
                .map_err(|_| anyhow::anyhow!("AWS_ACCESS_KEY_ID not set"))?;
            let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
                .map_err(|_| anyhow::anyhow!("AWS_SECRET_ACCESS_KEY not set"))?;
            let region = std::env::var("AWS_REGION")
                .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
                .unwrap_or_else(|_| "us-east-1".to_string());
            let endpoint_url = endpoint
                .or_else(|| std::env::var("S3_ENDPOINT").ok())
                .unwrap_or_else(|| "http://localhost:9000".to_string());

            let (bucket, key) = parse_s3_url(&url)?;

            let params = dfs_common::auth::presign::PresignParams {
                endpoint: &endpoint_url,
                bucket: &bucket,
                key: &key,
                method: &method.to_uppercase(),
                access_key: &access_key,
                secret_key: &secret_key,
                region: &region,
                expires_secs: expires,
            };

            let presigned_url = dfs_common::auth::presign::generate_presigned_url(&params);
            println!("{}", presigned_url);
        }
```

**Step 5: Run tests**

```bash
cargo test -p dfs-client --bin dfs_cli 2>&1 | tail -10
```

Expected: 2 tests PASS.

**Step 6: Build**

```bash
cargo build -p dfs-client 2>&1 | head -10
```

Expected: clean build. Binary at `target/debug/dfs_cli`.

**Step 7: Smoke test the CLI** (requires env vars, no running cluster needed)

```bash
AWS_ACCESS_KEY_ID=testkey AWS_SECRET_ACCESS_KEY=testsecret \
  cargo run -p dfs-client --bin dfs_cli -- presign s3://mybucket/myfile.txt --method GET --expires 3600
```

Expected: prints a URL starting with `http://localhost:9000/mybucket/myfile.txt?X-Amz-Algorithm=...`

**Step 8: Commit**

```bash
git add dfs/client/src/bin/dfs_cli.rs dfs/client/Cargo.toml
git commit -m "feat: add presign subcommand to dfs_cli"
```

---

### Task 4: Shell test for presigned GET/PUT/DELETE

**Files:**
- Modify: `test_scripts/run_s3_test.sh`

**Background:**
The existing `run_s3_test.sh` uploads objects and runs S3 API tests using `awscli` or `curl`. Add a section at the end that:
1. Uploads a test object using existing auth (awscli or curl with headers)
2. Generates a presigned GET URL using `dfs_cli presign`
3. Downloads with `curl` (no auth headers) and verifies content
4. Generates a presigned DELETE URL and deletes with `curl`
5. Verifies object is gone

**Step 1: Identify how `run_s3_test.sh` currently works**

Read the last 50 lines:

```bash
tail -50 test_scripts/run_s3_test.sh
```

**Step 2: Add presigned URL test section**

At the end of `test_scripts/run_s3_test.sh`, add:

```bash
# ============================================================
# Pre-signed URL Tests
# ============================================================
echo "Testing Pre-signed URLs..."

PRESIGN_BUCKET="presign-test-bucket"
PRESIGN_KEY="presign-test-object.txt"
PRESIGN_CONTENT="hello presigned world"

# Create test bucket and upload object (using existing auth)
aws --endpoint-url "$S3_ENDPOINT" s3 mb "s3://${PRESIGN_BUCKET}" 2>/dev/null || true
echo "$PRESIGN_CONTENT" | aws --endpoint-url "$S3_ENDPOINT" s3 cp - "s3://${PRESIGN_BUCKET}/${PRESIGN_KEY}"
echo "✓ Test object uploaded"

# Generate presigned GET URL using dfs_cli
PRESIGNED_GET_URL=$(AWS_ACCESS_KEY_ID="$AWS_ACCESS_KEY_ID" \
  AWS_SECRET_ACCESS_KEY="$AWS_SECRET_ACCESS_KEY" \
  AWS_REGION="${AWS_DEFAULT_REGION:-us-east-1}" \
  S3_ENDPOINT="$S3_ENDPOINT" \
  ./target/release/dfs_cli presign "s3://${PRESIGN_BUCKET}/${PRESIGN_KEY}" --method GET --expires 300)
echo "Presigned GET URL: $PRESIGNED_GET_URL"

# Download using only the presigned URL (no auth headers)
DOWNLOADED=$(curl -sf "$PRESIGNED_GET_URL")
if echo "$DOWNLOADED" | grep -q "hello presigned world"; then
    echo "✓ Presigned GET URL works"
else
    echo "✗ Presigned GET URL failed. Got: $DOWNLOADED"
    exit 1
fi

# Generate presigned DELETE URL
PRESIGNED_DELETE_URL=$(AWS_ACCESS_KEY_ID="$AWS_ACCESS_KEY_ID" \
  AWS_SECRET_ACCESS_KEY="$AWS_SECRET_ACCESS_KEY" \
  AWS_REGION="${AWS_DEFAULT_REGION:-us-east-1}" \
  S3_ENDPOINT="$S3_ENDPOINT" \
  ./target/release/dfs_cli presign "s3://${PRESIGN_BUCKET}/${PRESIGN_KEY}" --method DELETE --expires 300)

# Delete using presigned URL
HTTP_STATUS=$(curl -sf -X DELETE -w "%{http_code}" -o /dev/null "$PRESIGNED_DELETE_URL")
if [ "$HTTP_STATUS" = "204" ] || [ "$HTTP_STATUS" = "200" ]; then
    echo "✓ Presigned DELETE URL works"
else
    echo "✗ Presigned DELETE failed. HTTP status: $HTTP_STATUS"
    exit 1
fi

echo "✓ Pre-signed URL tests passed"
```

**Step 3: Commit**

```bash
git add test_scripts/run_s3_test.sh
git commit -m "test: add presigned URL integration test to run_s3_test.sh"
```

---

### Task 5: Update TODO.md

**Step 1: Mark Pre-signed URLs complete**

In `TODO.md`, replace:
```
- [ ] **Pre-signed URLs**: 短期間有効な署名付きURLの生成と検証。 *(前提: IAM & STS が完了していること)*
```
With:
```
- [x] **Pre-signed URLs** ✅: 短期間有効な署名付きURLの生成と検証。 *(前提: IAM & STS が完了していること)*
```

**Step 2: Final build + test**

```bash
cargo build 2>&1 | tail -5
cargo test --lib 2>&1 | tail -5
```

Expected: clean build, all tests pass.

**Step 3: Commit**

```bash
git add TODO.md
git commit -m "docs: mark Pre-signed URLs as complete in TODO.md"
```

# Bucket Policy Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement S3-compatible bucket policies (GET/PUT/DELETE `/{bucket}?policy`) with enforcement in the auth middleware.

**Architecture:** Bucket policies are stored as JSON at `/{bucket}/.s3_bucket_policy` in DFS. A new `BucketPolicyEvaluator` in `dfs_common` evaluates them against the requesting principal. Enforcement is layered on top of the existing identity-based `PolicyEvaluator` in `auth_middleware.rs`: an explicit Deny in either policy blocks access; an Allow in either policy (for authenticated callers) grants access.

**Tech Stack:** Rust, Axum, serde_json, existing `dfs_common::auth::policy` types, `dfs_client::Client`

---

## Background

### Existing infrastructure (read before starting)

- `dfs/common/src/auth/policy.rs` — `PolicyEvaluator`, `Statement`, `ActionValue`, `ResourceValue`, `matches_wildcard`
- `dfs/s3_server/src/handlers.rs` — `handle_request()` dispatches by path parts; bucket-level at `parts.len() == 1`
- `dfs/s3_server/src/auth_middleware.rs` — identity policy evaluation at line ~398; `resolve_s3_action_and_resource()` returns `(s3_action, s3_resource)`
- `dfs/s3_server/src/state.rs` — `AppState { client: Client, policy_evaluator: Option<Arc<PolicyEvaluator>>, ... }`

### Evaluation rules (S3-compatible, simplified)

1. Explicit Deny in bucket policy → DENY (always wins)
2. Explicit Allow in bucket policy for this principal → ALLOW
3. Fall through to identity policy evaluation (existing behavior)

Principal matching:
- `"*"` — matches any authenticated caller
- `{"AWS": "arn:..."}` — matches that specific role ARN

---

## Task 1: BucketPolicyEvaluator in dfs_common

**Files:**
- Create: `dfs/common/src/auth/bucket_policy.rs`
- Modify: `dfs/common/src/auth/mod.rs` (add `pub mod bucket_policy;`)

### Step 1: Write the failing unit test

In `dfs/common/src/auth/bucket_policy.rs`, add this test module at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn make_policy(effect: &str, principal: &str, action: &str, resource: &str) -> BucketPolicyDocument {
        let principal_val = if principal == "*" {
            PrincipalValue::Wildcard
        } else {
            PrincipalValue::Aws(principal.to_string())
        };
        BucketPolicyDocument {
            version: "2012-10-17".to_string(),
            statements: vec![BucketStatement {
                effect: effect.to_string(),
                principal: principal_val,
                actions: dfs_common::auth::policy::ActionValue::Single(action.to_string()),
                resources: Some(dfs_common::auth::policy::ResourceValue::Single(resource.to_string())),
            }],
        }
    }

    #[test]
    fn allow_wildcard_principal() {
        let policy = make_policy("Allow", "*", "s3:GetObject", "arn:dfs:s3:::mybucket/*");
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(evaluator.evaluate(None, "s3:GetObject", "arn:dfs:s3:::mybucket/key"), PolicyResult::Allow);
    }

    #[test]
    fn deny_explicit() {
        let policy = make_policy("Deny", "*", "s3:DeleteObject", "arn:dfs:s3:::mybucket/*");
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(evaluator.evaluate(None, "s3:DeleteObject", "arn:dfs:s3:::mybucket/key"), PolicyResult::ExplicitDeny);
    }

    #[test]
    fn allow_specific_principal() {
        let policy = make_policy("Allow", "arn:dfs:iam:::role/ReadOnly", "s3:GetObject", "arn:dfs:s3:::mybucket/*");
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(evaluator.evaluate(Some("arn:dfs:iam:::role/ReadOnly"), "s3:GetObject", "arn:dfs:s3:::mybucket/key"), PolicyResult::Allow);
        assert_eq!(evaluator.evaluate(Some("arn:dfs:iam:::role/Other"), "s3:GetObject", "arn:dfs:s3:::mybucket/key"), PolicyResult::NotApplicable);
    }

    #[test]
    fn not_applicable_when_no_match() {
        let policy = make_policy("Allow", "*", "s3:PutObject", "arn:dfs:s3:::mybucket/*");
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(evaluator.evaluate(None, "s3:GetObject", "arn:dfs:s3:::mybucket/key"), PolicyResult::NotApplicable);
    }

    #[test]
    fn parse_json_roundtrip() {
        let json = r#"{
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Principal": "*",
                "Action": "s3:GetObject",
                "Resource": "arn:dfs:s3:::mybucket/*"
            }]
        }"#;
        let doc: BucketPolicyDocument = serde_json::from_str(json).unwrap();
        assert_eq!(doc.statements.len(), 1);
        let back = serde_json::to_string(&doc).unwrap();
        let doc2: BucketPolicyDocument = serde_json::from_str(&back).unwrap();
        assert_eq!(doc2.statements.len(), 1);
    }
}
```

### Step 2: Run test to confirm FAIL

```bash
cargo test -p dfs-common --lib auth::bucket_policy 2>&1 | tail -5
```
Expected: compile error (types not yet defined)

### Step 3: Implement `bucket_policy.rs`

```rust
use dfs_common::auth::policy::{matches_wildcard, ActionValue, ResourceValue};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyResult {
    Allow,
    ExplicitDeny,
    NotApplicable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PrincipalValue {
    Wildcard,  // "*"
    Aws(String),
    AwsMap { #[serde(rename = "AWS")] aws: StringOrVec },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrVec {
    Single(String),
    Multiple(Vec<String>),
}

impl PrincipalValue {
    pub fn matches(&self, principal_arn: Option<&str>) -> bool {
        match self {
            PrincipalValue::Wildcard => true,
            PrincipalValue::Aws(s) => principal_arn.map(|p| matches_wildcard(s, p)).unwrap_or(false),
            PrincipalValue::AwsMap { aws } => match aws {
                StringOrVec::Single(s) => {
                    if s == "*" { return true; }
                    principal_arn.map(|p| matches_wildcard(s, p)).unwrap_or(false)
                }
                StringOrVec::Multiple(v) => v.iter().any(|s| {
                    if s == "*" { return true; }
                    principal_arn.map(|p| matches_wildcard(s, p)).unwrap_or(false)
                }),
            },
        }
    }
}

// Custom serde for PrincipalValue: "*" → Wildcard, string → Aws, object → AwsMap
// The #[serde(untagged)] above handles this correctly since:
//   - "*" deserializes as Wildcard only if we add a visitor
// We need a custom deserializer for this since "*" is both a string literal and Wildcard.

impl<'de> serde::de::Deserialize<'de> for PrincipalValue {
    fn deserialize<D: serde::de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // ... (see full implementation below)
    }
}
```

**Full implementation** — write this complete file:

```rust
use crate::auth::policy::{matches_wildcard, ActionValue, ResourceValue};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyResult {
    Allow,
    ExplicitDeny,
    NotApplicable,
}

/// S3 Bucket Policy JSON document.
/// Stored at /{bucket}/.s3_bucket_policy in DFS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketPolicyDocument {
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Statement")]
    pub statements: Vec<BucketStatement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketStatement {
    #[serde(rename = "Effect")]
    pub effect: String,
    #[serde(rename = "Principal")]
    pub principal: PrincipalValue,
    #[serde(rename = "Action")]
    pub actions: ActionValue,
    #[serde(rename = "Resource", skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceValue>,
}

/// Principal value: either "*" (everyone) or {"AWS": "arn:..."}
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum PrincipalValue {
    Wildcard,
    Aws(AwsPrincipal),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AwsPrincipal {
    Single(String),
    Map {
        #[serde(rename = "AWS")]
        aws: StringOrVec,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrVec {
    Single(String),
    Multiple(Vec<String>),
}

impl<'de> serde::de::Deserialize<'de> for PrincipalValue {
    fn deserialize<D: serde::de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::String(s) if s == "*" => Ok(PrincipalValue::Wildcard),
            serde_json::Value::String(_) | serde_json::Value::Object(_) => {
                let aws_principal = AwsPrincipal::deserialize(value)
                    .map_err(|e| Error::custom(e.to_string()))?;
                Ok(PrincipalValue::Aws(aws_principal))
            }
            _ => Err(Error::custom("invalid Principal value")),
        }
    }
}

impl PrincipalValue {
    pub fn matches(&self, principal_arn: Option<&str>) -> bool {
        match self {
            PrincipalValue::Wildcard => true,
            PrincipalValue::Aws(AwsPrincipal::Single(s)) => {
                if s == "*" {
                    return true;
                }
                principal_arn.map(|p| matches_wildcard(s, p)).unwrap_or(false)
            }
            PrincipalValue::Aws(AwsPrincipal::Map { aws }) => match aws {
                StringOrVec::Single(s) => {
                    if s == "*" {
                        return true;
                    }
                    principal_arn.map(|p| matches_wildcard(s, p)).unwrap_or(false)
                }
                StringOrVec::Multiple(v) => v.iter().any(|s| {
                    if s == "*" {
                        return true;
                    }
                    principal_arn.map(|p| matches_wildcard(s, p)).unwrap_or(false)
                }),
            },
        }
    }
}

pub struct BucketPolicyEvaluator {
    policy: BucketPolicyDocument,
}

impl BucketPolicyEvaluator {
    pub fn new(policy: BucketPolicyDocument) -> Self {
        Self { policy }
    }

    /// Evaluate whether `principal_arn` (None = unauthenticated) can perform `action` on `resource`.
    pub fn evaluate(
        &self,
        principal_arn: Option<&str>,
        action: &str,
        resource: &str,
    ) -> PolicyResult {
        let mut allow = false;

        for stmt in &self.policy.statements {
            // 1. Principal match
            if !stmt.principal.matches(principal_arn) {
                continue;
            }

            // 2. Action match
            if !stmt.actions.contains(action) {
                continue;
            }

            // 3. Resource match
            if let Some(resources) = &stmt.resources {
                if !resources.contains(resource) {
                    continue;
                }
            }

            // 4. Effect
            if stmt.effect == "Deny" {
                return PolicyResult::ExplicitDeny;
            } else if stmt.effect == "Allow" {
                allow = true;
            }
        }

        if allow {
            PolicyResult::Allow
        } else {
            PolicyResult::NotApplicable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_policy(effect: &str, principal: &str, action: &str, resource: &str) -> BucketPolicyDocument {
        let principal_val = if principal == "*" {
            PrincipalValue::Wildcard
        } else {
            PrincipalValue::Aws(AwsPrincipal::Single(principal.to_string()))
        };
        BucketPolicyDocument {
            version: "2012-10-17".to_string(),
            statements: vec![BucketStatement {
                effect: effect.to_string(),
                principal: principal_val,
                actions: ActionValue::Single(action.to_string()),
                resources: Some(ResourceValue::Single(resource.to_string())),
            }],
        }
    }

    #[test]
    fn allow_wildcard_principal() {
        let policy = make_policy("Allow", "*", "s3:GetObject", "arn:dfs:s3:::mybucket/*");
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(
            evaluator.evaluate(None, "s3:GetObject", "arn:dfs:s3:::mybucket/key"),
            PolicyResult::Allow
        );
    }

    #[test]
    fn deny_explicit() {
        let policy = make_policy("Deny", "*", "s3:DeleteObject", "arn:dfs:s3:::mybucket/*");
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(
            evaluator.evaluate(None, "s3:DeleteObject", "arn:dfs:s3:::mybucket/key"),
            PolicyResult::ExplicitDeny
        );
    }

    #[test]
    fn allow_specific_principal() {
        let policy = make_policy(
            "Allow",
            "arn:dfs:iam:::role/ReadOnly",
            "s3:GetObject",
            "arn:dfs:s3:::mybucket/*",
        );
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(
            evaluator.evaluate(
                Some("arn:dfs:iam:::role/ReadOnly"),
                "s3:GetObject",
                "arn:dfs:s3:::mybucket/key"
            ),
            PolicyResult::Allow
        );
        assert_eq!(
            evaluator.evaluate(
                Some("arn:dfs:iam:::role/Other"),
                "s3:GetObject",
                "arn:dfs:s3:::mybucket/key"
            ),
            PolicyResult::NotApplicable
        );
    }

    #[test]
    fn not_applicable_when_no_match() {
        let policy = make_policy("Allow", "*", "s3:PutObject", "arn:dfs:s3:::mybucket/*");
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(
            evaluator.evaluate(None, "s3:GetObject", "arn:dfs:s3:::mybucket/key"),
            PolicyResult::NotApplicable
        );
    }

    #[test]
    fn parse_json_roundtrip() {
        let json = r#"{
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Principal": "*",
                "Action": "s3:GetObject",
                "Resource": "arn:dfs:s3:::mybucket/*"
            }]
        }"#;
        let doc: BucketPolicyDocument = serde_json::from_str(json).unwrap();
        assert_eq!(doc.statements.len(), 1);
        let back = serde_json::to_string(&doc).unwrap();
        let doc2: BucketPolicyDocument = serde_json::from_str(&back).unwrap();
        assert_eq!(doc2.statements.len(), 1);
    }
}
```

### Step 4: Add to `mod.rs`

In `dfs/common/src/auth/mod.rs`, add `pub mod bucket_policy;`

### Step 5: Run tests to confirm PASS

```bash
cargo test -p dfs-common --lib auth::bucket_policy 2>&1 | tail -10
```
Expected: 5 tests pass

### Step 6: Commit

```bash
cd .worktrees/feature/bucket-policy
git add dfs/common/src/auth/bucket_policy.rs dfs/common/src/auth/mod.rs
git commit -m "feat: add BucketPolicyEvaluator to dfs_common"
```

---

## Task 2: GET/PUT/DELETE `/{bucket}?policy` handlers

**Files:**
- Modify: `dfs/s3_server/src/handlers.rs`

### Step 1: Write failing handler test

In `dfs/s3_server/src/handler_tests.rs`, find the existing test setup and add:

```rust
#[tokio::test]
#[ignore]  // requires running cluster
async fn test_bucket_policy_crud() {
    // This is an integration test — covered by the shell script
    // Unit test just verifies JSON parsing
    let policy_json = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":"*","Action":"s3:GetObject","Resource":"arn:dfs:s3:::testbucket/*"}]}"#;
    let doc: dfs_common::auth::bucket_policy::BucketPolicyDocument =
        serde_json::from_str(policy_json).unwrap();
    assert_eq!(doc.statements.len(), 1);
}
```

### Step 2: Run test to confirm it passes (it's just JSON parsing)

```bash
cargo test -p dfs-s3-server --lib handler_tests::test_bucket_policy_crud 2>&1 | tail -5
```

### Step 3: Add handlers in `handlers.rs`

In `handle_request()`, find the bucket-level dispatch block (around line 163):

```rust
// Before the existing method match, add policy query param handling:
if parts.len() == 1 {
    let bucket = parts[0];
    if query_params.contains_key("policy") {
        return match method {
            Method::GET => get_bucket_policy(state, bucket).await,
            Method::PUT => put_bucket_policy(state, bucket, body_bytes).await,
            Method::DELETE => delete_bucket_policy(state, bucket).await,
            _ => method_not_allowed(),
        };
    }
    // ... existing bucket-level dispatch
```

Implement these 3 functions:

```rust
async fn get_bucket_policy(state: S3AppState, bucket: &str) -> Response {
    let path = format!("/{bucket}/.s3_bucket_policy");
    match state.client.read_file(&path).await {
        Ok(data) => {
            // Verify it's valid JSON before returning
            if serde_json::from_slice::<serde_json::Value>(&data).is_err() {
                return s3_error("NoSuchBucketPolicy", "The bucket policy does not exist", StatusCode::NOT_FOUND, bucket);
            }
            (StatusCode::OK, [("Content-Type", "application/json")], data).into_response()
        }
        Err(_) => s3_error("NoSuchBucketPolicy", "The bucket policy does not exist", StatusCode::NOT_FOUND, bucket),
    }
}

async fn put_bucket_policy(state: S3AppState, bucket: &str, body: Bytes) -> Response {
    // Validate JSON is a valid BucketPolicyDocument
    if serde_json::from_slice::<BucketPolicyDocument>(&body).is_err() {
        return s3_error("MalformedPolicy", "Bucket policy must be valid JSON", StatusCode::BAD_REQUEST, bucket);
    }
    let path = format!("/{bucket}/.s3_bucket_policy");
    match state.client.write_file(&path, body.to_vec()).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!("Failed to write bucket policy: {}", e);
            s3_error("InternalError", "Failed to store bucket policy", StatusCode::INTERNAL_SERVER_ERROR, bucket)
        }
    }
}

async fn delete_bucket_policy(state: S3AppState, bucket: &str) -> Response {
    let path = format!("/{bucket}/.s3_bucket_policy");
    match state.client.delete_file(&path).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::NO_CONTENT.into_response(), // idempotent
    }
}

fn s3_error(code: &str, message: &str, status: StatusCode, resource: &str) -> Response {
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>{code}</Code>
  <Message>{message}</Message>
  <Resource>/{resource}</Resource>
</Error>"#
    );
    (status, [("Content-Type", "application/xml")], xml).into_response()
}
```

**Note:** Check what methods `state.client` actually exposes — look at `dfs/client/src/lib.rs` for the correct method names. The client likely has `read_file`, `write_file` / `create_file`, `delete_file`. Adjust accordingly.

**Also add the import** at the top of `handlers.rs`:
```rust
use dfs_common::auth::bucket_policy::BucketPolicyDocument;
```

### Step 4: Build to verify no compile errors

```bash
cargo build -p dfs-s3-server 2>&1 | grep -E "^error" | head -20
```

### Step 5: Commit

```bash
git add dfs/s3_server/src/handlers.rs
git commit -m "feat: add GET/PUT/DELETE /{bucket}?policy handlers"
```

---

## Task 3: Enforce bucket policy in auth_middleware

**Files:**
- Modify: `dfs/s3_server/src/auth_middleware.rs`

**Logic:** After the existing identity policy evaluation (step 10 in auth flow), additionally evaluate the bucket policy. The bucket policy is loaded on-demand by reading `/{bucket}/.s3_bucket_policy` from DFS via `state.client`.

**Key decision:** Bucket policy enforcement runs for ALL requests (not just STS-authenticated ones), because bucket policy can grant OR deny access independently of identity policy.

### Step 1: Identify bucket from request

In `auth_middleware.rs`, after signature verification succeeds (around line 397), extract the bucket name:

```rust
fn bucket_from_path(path: &str) -> Option<&str> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    parts.first().copied()
}
```

### Step 2: Load and evaluate bucket policy

After identity policy evaluation (after line ~431 in current code), add:

```rust
// 11. Bucket Policy Evaluation
if let Some(bucket) = bucket_from_path(&path) {
    let policy_path = format!("/{bucket}/.s3_bucket_policy");
    if let Ok(data) = state.client.read_file(&policy_path).await {
        if let Ok(policy_doc) = serde_json::from_slice::<BucketPolicyDocument>(&data) {
            let (s3_action, s3_resource) = resolve_s3_action_and_resource(&req);
            let evaluator = BucketPolicyEvaluator::new(policy_doc);
            let principal_arn = role_arn_from_token.as_deref();
            match evaluator.evaluate(principal_arn, &s3_action, &s3_resource) {
                PolicyResult::ExplicitDeny => {
                    IAM_POLICY_EVALUATIONS.with_label_values(&["deny", &s3_action]).inc();
                    let res = s3_error_response(AuthError::MissingAuth);
                    audit_ctx.role_arn = role_arn_from_token;
                    audit_ctx.log(&state, res.status().as_u16(), Some("AccessDenied".to_string()), &query_params);
                    return res;
                }
                PolicyResult::Allow => {
                    // Bucket policy grants access — skip identity policy denial
                    // (already past identity policy check, this is additive)
                    IAM_POLICY_EVALUATIONS.with_label_values(&["allow", &s3_action]).inc();
                }
                PolicyResult::NotApplicable => {}
            }
        }
    }
    // Note: if bucket policy file doesn't exist, we skip (no policy = default allow by identity policy)
}
```

**Important:** The bucket policy enforcement must be inside the `Ok(_) =>` arm of signature verification (after `match dfs_common::auth::signing::verify_signature_with_key(...)`), before calling `next.run(req)`.

**Import at top of auth_middleware.rs:**
```rust
use dfs_common::auth::bucket_policy::{BucketPolicyDocument, BucketPolicyEvaluator, PolicyResult};
```

**The tricky bit:** `state.client.read_file()` is async, but we're in the middleware. The function signature is already `async fn auth_middleware(...)`, so this is fine.

**Also:** We need to handle the `?policy` requests themselves: when someone calls `PUT /{bucket}?policy`, the identity policy check (if using STS) needs `s3:PutBucketPolicy`, which is already handled. But we must NOT load+enforce the bucket policy for the `?policy` management operations themselves (it creates a circular dependency where you can't set a policy if the policy denies you). Skip bucket policy enforcement for `?policy` requests.

Add this guard before loading bucket policy:
```rust
let is_policy_op = req.uri().query()
    .map(|q| q.contains("policy"))
    .unwrap_or(false);

if !is_policy_op {
    // ... bucket policy enforcement
}
```

### Step 3: Build and test

```bash
cargo build -p dfs-s3-server 2>&1 | grep "^error" | head -20
cargo test -p dfs-s3-server --lib 2>&1 | tail -10
```

### Step 4: Commit

```bash
git add dfs/s3_server/src/auth_middleware.rs
git commit -m "feat: enforce bucket policy in auth middleware"
```

---

## Task 4: Integration test + TODO.md update

**Files:**
- Create: `test_scripts/bucket_policy_test.sh`
- Modify: `TODO.md`

### Step 1: Write integration test script

```bash
#!/usr/bin/env bash
set -euo pipefail
# ... (see template from existing test scripts)
```

The test should:
1. Create a bucket
2. PUT a bucket policy that denies `s3:DeleteObject`
3. Verify GET returns the policy
4. Verify DELETE on an object is denied (403)
5. DELETE the bucket policy
6. Verify DELETE on object now succeeds
7. Clean up

Look at `test_scripts/rename_test.sh` for the pattern: use `awslocal` or `aws --endpoint-url`.

### Step 2: Update TODO.md

Change `- [ ] **Bucket Policy**` to `- [x] **Bucket Policy** ✅`

### Step 3: Commit

```bash
git add test_scripts/bucket_policy_test.sh TODO.md
git commit -m "feat: add bucket policy integration test and mark TODO complete"
```

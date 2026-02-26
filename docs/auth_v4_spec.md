# S3 Signature Version 4 Authentication Specification

This document defines the technical specification for the AWS Signature Version 4 (SigV4) authentication implementation in the Rust Hadoop DFS S3 server.

---

## 1. Overview
The S3 server implements the AWS Signature Version 4 protocol to ensure the authenticity and integrity of incoming requests. The implementation is split into a reusable core engine in `dfs-common` and an integration middleware in `s3-server`.

### 1.1 Scope
- **Included**: SigV4 signature derivation, URI encoding rules (S3 flavor), streaming payload verification, chunked payload verification, and basic credential management.
- **Excluded**: Virtual-Host style routing, Bucket-level IAM policies, and STS (handled by separate modules).

---

## 2. Request Normalization

### 2.1 URI Encoding
The implementation follows a specific subset of RFC 3986 as required by Amazon S3:
- **Allowed Characters**: `A-Z`, `a-z`, `0-9`, `_`, `.`, `-`, `~`.
- **Special Case (S3 Exception)**: Unlike other AWS services, S3 does **not** double-encode the URI path.
- **Rules**:
    - Spaces are encoded as `%20`.
    - `/` in path segments is preserved (single-encoded).
    - `/` in query values is percent-encoded.

### 2.2 Canonical Headers
Headers are normalized as follows:
1. **Lowercase**: Convert all header names to lowercase.
2. **Sort**: Sort headers alphabetically by name.
3. **Space Collapsing**: Trim leading/trailing whitespace and collapse internal sequential spaces into a single space.
4. **Multi-value**: If a header appears multiple times, join its values with a comma (`,`) in the order received.

### 2.3 Canonical Query String
1. Sort parameters alphabetically by name, then by value.
2. URI-encode both name and value according to S3 rules.
3. **Empty Values**: Parameters without a value (e.g., `?acl`) are serialized as `name=`.

---

## 3. Authentication Algorithm

### Step 1: Canonical Request
Standard form:
```
CanonicalRequest =
  HTTPMethod + "\n" +
  CanonicalURI + "\n" +
  CanonicalQueryString + "\n" +
  CanonicalHeaders + "\n" +
  SignedHeaders + "\n" +
  HashedPayload
```
`HashedPayload` is either the hex-encoded SHA256 of the body or the literal string `UNSIGNED-PAYLOAD`.

### Step 2: String to Sign
Standard form:
```
StringToSign =
  "AWS4-HMAC-SHA256" + "\n" +
  Timestamp + "\n" +
  CredentialScope + "\n" +
  hex(SHA256(CanonicalRequest))
```
`CredentialScope` format: `YYYYMMDD/<region>/s3/aws4_request`.

### Step 3: Deriving Signing Key
The key is derived through a recursive HMAC-SHA256 chain:
1. `kDate = HMAC-SHA256("AWS4" + SecretKey, "YYYYMMDD")`
2. `kRegion = HMAC-SHA256(kDate, Region)`
3. `kService = HMAC-SHA256(kRegion, "s3")`
4. `kSigning = HMAC-SHA256(kService, "aws4_request")`

### Step 4: Final Signature
`Signature = hex(HMAC-SHA256(kSigning, StringToSign))`
Comparison is performed using **constant-time equality** to prevent timing side-channel attacks.

---

## 4. Server Integration & Security

### 4.1 Middleware Constraints
- **Clock Skew**: Requests are rejected if the timestamp is more than 15 minutes away from the server time (`RequestTimeTooSkewed`).
- **TLS Enforcement**: If `S3_REQUIRE_TLS=true`, non-HTTPS authenticated requests are rejected.
- **Credential Scope**: The `region` and `service` in the credential MUST match the server's configuration (`server_region` and `"s3"`).

### 4.2 Configuration Flags
- `S3_AUTH_ENABLED` (bool): If false, authentication is bypassed (backward compatibility).
- `S3_ALLOW_UNSIGNED_PAYLOAD` (bool): If false, requests with `x-amz-content-sha256: UNSIGNED-PAYLOAD` are rejected.

### 4.3 Streaming & Chunked Payloads
- **Standard Streaming**: Hashing is performed on-the-fly during data transfer to the backend.
- **Chunked Payload**: Support for `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` is implemented via a chunk decoder that verifies each signed data segment.

---

## 5. Error Handling
Authentication errors are mapped to standard S3 XML error responses:
- `SignatureDoesNotMatch`: Signature verification failed (includes debug info).
- `RequestTimeTooSkewed`: Request too old or from the future.
- `InvalidAccessKeyId`: Access key not found in records.
- `AuthorizationHeaderMalformed`: Invalid scope or malformed header structure.
- `AccessDenied`: General auth failure or missing credentials.

---

## 6. Performance Optimization
To minimize HMAC overhead for high-frequency clients:
- **Signing Key Cache**: A thread-safe LRU cache holds derived `kSigning` keys.
- **TTL**: Cache entries are valid for 24 hours (matching the credential date scope).

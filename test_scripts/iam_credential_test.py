#!/usr/bin/env python3
"""
IAM Credential Design — 機能テスト

設計書: docs/iam_credentials_design.md
対象: Phase 1 (OIDC) / Phase 2 (STS) / Phase 3 (Policy Engine) / ミドルウェア統合

テストケース一覧:
  [Phase 1] OIDC
    TC-01: 正常な JWT で AssumeRoleWithWebIdentity が成功する
    TC-02: 期限切れの JWT が拒否される
    TC-03: 不正な署名の JWT が拒否される
    TC-04: Audience が一致しない JWT が拒否される
  [Phase 2] STS
    TC-05: STS で取得した一時クレデンシャルで SigV4 リクエストが成功する
    TC-06: 期限切れ SessionToken が拒否される
    TC-07: 改ざんされた SessionToken が拒否される
    TC-08: 必須パラメータ (RoleArn, WebIdentityToken) 欠落時のエラー
  [Phase 3] Policy Engine
    TC-09: 許可されたバケットへの操作が成功する
    TC-10: 許可されていないバケットへの操作が拒否される
    TC-11: 許可されていないアクション (DeleteObject) が拒否される
    TC-12: Trust Policy の groups 条件を満たさないユーザが AssumeRole を拒否される
  [ミドルウェア統合]
    TC-13: 静的クレデンシャル (管理者) は認可スキップでフルアクセス可能
    TC-14: STS クレデンシャル無しのリクエストは静的キーとして処理される
"""

import base64
import hashlib
import hmac
import http.server
import json
import os
import signal
import subprocess
import sys
import threading
import time
import urllib.parse
import urllib.request
from datetime import datetime, timezone

# --- Mock OIDC Provider ---

try:
    from cryptography.hazmat.primitives.asymmetric import rsa
    from cryptography.hazmat.primitives import serialization
    import jwt as pyjwt
except ImportError:
    print("ERROR: pip install pyjwt cryptography")
    sys.exit(1)

MOCK_OIDC_PORT = 18080
S3_PORT = 19000
MOCK_ISSUER = f"http://127.0.0.1:{MOCK_OIDC_PORT}"
CLIENT_ID = "test-client"
STS_SIGNING_KEY = "abcdefghijklmnopqrstuvwxyz012345"  # 32 bytes
S3_REGION = "us-east-1"
ADMIN_ACCESS_KEY = "TESTADMINKEY"
ADMIN_SECRET_KEY = "TESTADMINSECRETKEY1234567890ABCDEFGH"

# RSA keypair for the mock OIDC
_private_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
_public_key = _private_key.public_key()


def _to_base64url(n: int) -> str:
    b = n.to_bytes((n.bit_length() + 7) // 8, "big")
    return base64.urlsafe_b64encode(b).decode("utf-8").rstrip("=")


def _jwks():
    nums = _public_key.public_numbers()
    return {
        "keys": [
            {
                "kty": "RSA",
                "kid": "test-kid",
                "use": "sig",
                "alg": "RS256",
                "n": _to_base64url(nums.n),
                "e": _to_base64url(nums.e),
            }
        ]
    }


class _MockOidcHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass  # suppress logs

    def do_GET(self):
        if self.path == "/.well-known/openid-configuration":
            body = json.dumps({"issuer": MOCK_ISSUER, "jwks_uri": f"{MOCK_ISSUER}/jwks"})
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body.encode())
        elif self.path == "/jwks":
            body = json.dumps(_jwks())
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body.encode())
        else:
            self.send_response(404)
            self.end_headers()


def generate_jwt(sub="user-1", groups=None, aud=CLIENT_ID, iss=MOCK_ISSUER,
                 exp_offset=3600, key=_private_key):
    """Generate a valid JWT. exp_offset < 0 means expired."""
    now = int(time.time())
    payload = {
        "sub": sub,
        "aud": aud,
        "iss": iss,
        "iat": now,
        "exp": now + exp_offset,
        "groups": groups or [],
    }
    return pyjwt.encode(payload, key, algorithm="RS256", headers={"kid": "test-kid"})


# --- SigV4 helper (minimal) ---

def _sign(key: bytes, msg: str) -> bytes:
    return hmac.new(key, msg.encode("utf-8"), hashlib.sha256).digest()


def _get_signature_key(secret: str, date: str, region: str, service: str) -> bytes:
    k = _sign(("AWS4" + secret).encode("utf-8"), date)
    k = _sign(k, region)
    k = _sign(k, service)
    k = _sign(k, "aws4_request")
    return k


def sigv4_request(method, path, access_key, secret_key, session_token=None,
                  host="127.0.0.1", port=S3_PORT, body=b"", extra_headers=None):
    """Perform a SigV4-signed HTTP request and return (status, headers, body)."""
    now = datetime.now(timezone.utc)
    date_stamp = now.strftime("%Y%m%d")
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")

    headers_to_sign = {
        "host": f"{host}:{port}",
        "x-amz-date": amz_date,
        "x-amz-content-sha256": hashlib.sha256(body).hexdigest(),
    }
    if session_token:
        headers_to_sign["x-amz-security-token"] = session_token

    if extra_headers:
        headers_to_sign.update(extra_headers)

    signed_header_names = sorted(headers_to_sign.keys())
    signed_headers_str = ";".join(signed_header_names)

    canonical_headers = "".join(f"{k}:{headers_to_sign[k]}\n" for k in signed_header_names)
    payload_hash = hashlib.sha256(body).hexdigest()

    # no query string for simplicity
    canonical_request = f"{method}\n{path}\n\n{canonical_headers}\n{signed_headers_str}\n{payload_hash}"
    credential_scope = f"{date_stamp}/{S3_REGION}/s3/aws4_request"
    string_to_sign = f"AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{hashlib.sha256(canonical_request.encode()).hexdigest()}"

    signing_key = _get_signature_key(secret_key, date_stamp, S3_REGION, "s3")
    signature = hmac.new(signing_key, string_to_sign.encode("utf-8"), hashlib.sha256).hexdigest()

    auth_header = (
        f"AWS4-HMAC-SHA256 Credential={access_key}/{credential_scope}, "
        f"SignedHeaders={signed_headers_str}, Signature={signature}"
    )

    url = f"http://{host}:{port}{path}"
    req = urllib.request.Request(url, data=body if body else None, method=method)
    req.add_header("Authorization", auth_header)
    for k, v in headers_to_sign.items():
        req.add_header(k, v)

    try:
        resp = urllib.request.urlopen(req)
        return resp.status, dict(resp.headers), resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode("utf-8", errors="replace")


def sts_assume_role(jwt_token, role_arn, duration=3600):
    """Call AssumeRoleWithWebIdentity and return parsed credentials or error."""
    params = urllib.parse.urlencode({
        "Action": "AssumeRoleWithWebIdentity",
        "RoleArn": role_arn,
        "RoleSessionName": "test-session",
        "WebIdentityToken": jwt_token,
        "DurationSeconds": str(duration),
    })
    url = f"http://127.0.0.1:{S3_PORT}/?{params}"
    try:
        resp = urllib.request.urlopen(url)
        body = resp.read().decode()
        return resp.status, body
    except urllib.error.HTTPError as e:
        body = e.read().decode()
        return e.code, body


def extract_creds(xml_body):
    """Rough XML parsing to extract credentials from STS response."""
    import re
    ak = re.search(r"<AccessKeyId>(.*?)</AccessKeyId>", xml_body)
    sk = re.search(r"<SecretAccessKey>(.*?)</SecretAccessKey>", xml_body)
    st = re.search(r"<SessionToken>(.*?)</SessionToken>", xml_body)
    exp = re.search(r"<Expiration>(.*?)</Expiration>", xml_body)
    if ak and sk and st:
        return {
            "AccessKeyId": ak.group(1),
            "SecretAccessKey": sk.group(1),
            "SessionToken": st.group(1),
            "Expiration": exp.group(1) if exp else "",
        }
    return None


# --- Test Infrastructure ---

class TestResult:
    def __init__(self):
        self.passed = 0
        self.failed = 0
        self.errors = []

    def ok(self, name):
        self.passed += 1
        print(f"  ✅ PASS: {name}")

    def fail(self, name, reason=""):
        self.failed += 1
        self.errors.append((name, reason))
        print(f"  ❌ FAIL: {name} — {reason}")

    def summary(self):
        total = self.passed + self.failed
        print(f"\n{'='*60}")
        print(f"Results: {self.passed}/{total} passed, {self.failed} failed")
        if self.errors:
            print("Failures:")
            for name, reason in self.errors:
                print(f"  - {name}: {reason}")
        print(f"{'='*60}")
        return self.failed == 0


# --- IAM Config ---

IAM_CONFIG = {
    "Roles": [
        {
            "RoleName": "tenant-a-role",
            "Arn": "arn:dfs:iam:::role/tenant-a-role",
            "AssumeRolePolicyDocument": {
                "Statement": [
                    {
                        "Effect": "Allow",
                        "Action": "sts:AssumeRoleWithWebIdentity",
                        "Condition": {
                            "ForAnyValue:StringEquals": {
                                "OIDC_ISSUER:groups": ["tenant-a"]
                            }
                        },
                    }
                ]
            },
            "Policies": [
                {
                    "PolicyName": "TenantAPolicy",
                    "PolicyDocument": {
                        "Statement": [
                            {
                                "Effect": "Allow",
                                "Action": ["s3:GetObject", "s3:PutObject", "s3:ListBucket"],
                                "Resource": ["arn:dfs:s3:::tenant-a-*"],
                            }
                        ]
                    },
                }
            ],
        }
    ]
}


# --- Main ---

def main():
    results = TestResult()

    # 1. Write IAM config
    iam_config_path = "/tmp/iam_test_config.json"
    with open(iam_config_path, "w") as f:
        json.dump(IAM_CONFIG, f)

    # 2. Start Mock OIDC server
    print("[SETUP] Starting Mock OIDC Server...")
    mock_server = http.server.HTTPServer(("127.0.0.1", MOCK_OIDC_PORT), _MockOidcHandler)
    mock_thread = threading.Thread(target=mock_server.serve_forever, daemon=True)
    mock_thread.start()
    time.sleep(0.5)

    # 3. Start S3 Server
    print("[SETUP] Building and starting S3 Server...")
    env = os.environ.copy()
    env.update({
        "S3_AUTH_ENABLED": "true",
        "S3_REGION": S3_REGION,
        "S3_ACCESS_KEY": ADMIN_ACCESS_KEY,
        "S3_SECRET_KEY": ADMIN_SECRET_KEY,
        "S3_ALLOW_UNSIGNED_PAYLOAD": "true",
        "OIDC_ISSUER_URL": MOCK_ISSUER,
        "OIDC_CLIENT_ID": CLIENT_ID,
        "STS_SIGNING_KEY": STS_SIGNING_KEY,
        "IAM_CONFIG_PATH": iam_config_path,
        "PORT": str(S3_PORT),
        "MASTER_ADDR": "http://127.0.0.1:50051",
        "RUST_LOG": "warn",
    })

    project_root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
    s3_proc = subprocess.Popen(
        ["cargo", "run", "-p", "s3-server"],
        cwd=project_root,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    # Wait for S3 server
    print("[SETUP] Waiting for S3 Server to start...")
    s3_ready = False
    for i in range(30):
        try:
            resp = urllib.request.urlopen(f"http://127.0.0.1:{S3_PORT}/health")
            if resp.status == 200:
                s3_ready = True
                break
        except Exception:
            pass
        time.sleep(1)

    if not s3_ready:
        print("[ERROR] S3 Server failed to start within 30s")
        s3_proc.terminate()
        _, stderr = s3_proc.communicate(timeout=5)
        print(stderr.decode())
        sys.exit(1)

    print("[SETUP] S3 Server is ready!\n")

    try:
        # ================================================================
        # Phase 1: OIDC テスト
        # ================================================================
        print("=" * 60)
        print("[Phase 1] OIDC Validation Tests")
        print("=" * 60)

        # TC-01: 正常な JWT で AssumeRoleWithWebIdentity が成功する
        token_ok = generate_jwt(sub="user-1", groups=["tenant-a"])
        status, body = sts_assume_role(token_ok, "arn:dfs:iam:::role/tenant-a-role")
        if status == 200 and "<AccessKeyId>" in body:
            results.ok("TC-01: Valid JWT → AssumeRole succeeds")
        else:
            results.fail("TC-01: Valid JWT → AssumeRole succeeds", f"status={status}, body={body[:200]}")

        # TC-02: 期限切れの JWT が拒否される (leeway is 60s, so use -120s)
        token_expired = generate_jwt(sub="user-1", groups=["tenant-a"], exp_offset=-120)
        status, body = sts_assume_role(token_expired, "arn:dfs:iam:::role/tenant-a-role")
        if status == 403:
            results.ok("TC-02: Expired JWT → Rejected (403)")
        else:
            results.fail("TC-02: Expired JWT → Rejected (403)", f"status={status}")

        # TC-03: 不正な署名の JWT が拒否される
        bad_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        token_bad_sig = generate_jwt(sub="user-1", groups=["tenant-a"], key=bad_key)
        status, body = sts_assume_role(token_bad_sig, "arn:dfs:iam:::role/tenant-a-role")
        if status == 403:
            results.ok("TC-03: Invalid signature JWT → Rejected (403)")
        else:
            results.fail("TC-03: Invalid signature JWT → Rejected (403)", f"status={status}")

        # TC-04: Audience が一致しない JWT が拒否される
        token_bad_aud = generate_jwt(sub="user-1", groups=["tenant-a"], aud="wrong-client")
        status, body = sts_assume_role(token_bad_aud, "arn:dfs:iam:::role/tenant-a-role")
        if status == 403:
            results.ok("TC-04: Wrong audience JWT → Rejected (403)")
        else:
            results.fail("TC-04: Wrong audience JWT → Rejected (403)", f"status={status}")

        # ================================================================
        # Phase 2: STS テスト
        # ================================================================
        print()
        print("=" * 60)
        print("[Phase 2] STS Token Tests")
        print("=" * 60)

        # TC-05: STS で取得した一時クレデンシャルで SigV4 リクエストが成功する
        token_ok = generate_jwt(sub="user-1", groups=["tenant-a"])
        status, body = sts_assume_role(token_ok, "arn:dfs:iam:::role/tenant-a-role")
        creds = extract_creds(body) if status == 200 else None
        if creds:
            # Use temp creds for an authenticated request on allowed resource
            st, _, resp_body = sigv4_request(
                "GET", "/tenant-a-bucket",
                creds["AccessKeyId"], creds["SecretAccessKey"],
                session_token=creds["SessionToken"],
            )
            # 503 is acceptable (no master connected) — we just check auth passes (not 403)
            if st != 403:
                results.ok("TC-05: STS creds → SigV4 auth passes (not 403)")
            else:
                results.fail("TC-05: STS creds → SigV4 auth passes", f"status={st}, body={resp_body[:200]}")
        else:
            results.fail("TC-05: STS creds → SigV4 auth passes", "Could not obtain STS creds")

        # TC-06: 期限切れ SessionToken が拒否される
        # Generate a token with 1-second duration, then wait
        token_short = generate_jwt(sub="user-1", groups=["tenant-a"])
        status, body = sts_assume_role(token_short, "arn:dfs:iam:::role/tenant-a-role", duration=1)
        creds_short = extract_creds(body) if status == 200 else None
        if creds_short:
            time.sleep(2)  # wait for expiration
            st, _, resp_body = sigv4_request(
                "GET", "/",
                creds_short["AccessKeyId"], creds_short["SecretAccessKey"],
                session_token=creds_short["SessionToken"],
            )
            if st == 403:
                results.ok("TC-06: Expired SessionToken → Rejected (403)")
            else:
                results.fail("TC-06: Expired SessionToken → Rejected (403)", f"status={st}")
        else:
            results.fail("TC-06: Expired SessionToken → Rejected (403)", "Could not obtain STS creds")

        # TC-07: 改ざんされた SessionToken が拒否される
        if creds:
            tampered_token = creds["SessionToken"][:-4] + "XXXX"
            st, _, _ = sigv4_request(
                "GET", "/",
                creds["AccessKeyId"], creds["SecretAccessKey"],
                session_token=tampered_token,
            )
            if st == 403:
                results.ok("TC-07: Tampered SessionToken → Rejected (403)")
            else:
                results.fail("TC-07: Tampered SessionToken → Rejected (403)", f"status={st}")
        else:
            results.fail("TC-07: Tampered SessionToken → Rejected (403)", "No creds available")

        # TC-08: 必須パラメータ欠落時のエラー
        # Missing RoleArn
        params_no_role = urllib.parse.urlencode({
            "Action": "AssumeRoleWithWebIdentity",
            "WebIdentityToken": token_ok,
        })
        try:
            resp = urllib.request.urlopen(f"http://127.0.0.1:{S3_PORT}/?{params_no_role}")
            st = resp.status
        except urllib.error.HTTPError as e:
            st = e.code
        if st == 400:
            results.ok("TC-08: Missing RoleArn → 400 Bad Request")
        else:
            results.fail("TC-08: Missing RoleArn → 400 Bad Request", f"status={st}")

        # ================================================================
        # Phase 3: Policy Engine テスト
        # ================================================================
        print()
        print("=" * 60)
        print("[Phase 3] Policy Engine Tests")
        print("=" * 60)

        # Get valid creds for tenant-a
        token_a = generate_jwt(sub="user-1", groups=["tenant-a"])
        status, body = sts_assume_role(token_a, "arn:dfs:iam:::role/tenant-a-role")
        creds_a = extract_creds(body) if status == 200 else None

        if creds_a:
            # TC-09: 許可されたバケットへの操作が成功する
            st, _, _ = sigv4_request(
                "GET", "/tenant-a-bucket",
                creds_a["AccessKeyId"], creds_a["SecretAccessKey"],
                session_token=creds_a["SessionToken"],
            )
            # Accept non-403 as success (503/404 from no-master is ok)
            if st != 403:
                results.ok("TC-09: Allowed bucket access → not denied")
            else:
                results.fail("TC-09: Allowed bucket access → not denied", f"status={st}")

            # TC-10: 許可されていないバケットへの操作が拒否される
            st, _, _ = sigv4_request(
                "GET", "/other-bucket",
                creds_a["AccessKeyId"], creds_a["SecretAccessKey"],
                session_token=creds_a["SessionToken"],
            )
            if st == 403:
                results.ok("TC-10: Unauthorized bucket → denied (403)")
            else:
                results.fail("TC-10: Unauthorized bucket → denied (403)", f"status={st}")

            # TC-11: 許可されていないアクション (DELETE) が拒否される
            st, _, _ = sigv4_request(
                "DELETE", "/tenant-a-bucket/file.txt",
                creds_a["AccessKeyId"], creds_a["SecretAccessKey"],
                session_token=creds_a["SessionToken"],
            )
            if st == 403:
                results.ok("TC-11: Unauthorized action (DELETE) → denied (403)")
            else:
                results.fail("TC-11: Unauthorized action (DELETE) → denied (403)", f"status={st}")
        else:
            results.fail("TC-09: Allowed bucket access", "No creds")
            results.fail("TC-10: Unauthorized bucket", "No creds")
            results.fail("TC-11: Unauthorized action", "No creds")

        # TC-12: Trust Policy の groups 条件を満たさないユーザが AssumeRole を拒否される
        token_no_group = generate_jwt(sub="user-2", groups=["other-group"])
        status, body = sts_assume_role(token_no_group, "arn:dfs:iam:::role/tenant-a-role")
        if status == 403:
            results.ok("TC-12: Wrong group → AssumeRole denied (403)")
        else:
            results.fail("TC-12: Wrong group → AssumeRole denied (403)", f"status={status}")

        # ================================================================
        # ミドルウェア統合テスト
        # ================================================================
        print()
        print("=" * 60)
        print("[Middleware Integration] Tests")
        print("=" * 60)

        # TC-13: 静的クレデンシャル (管理者) はフルアクセス可能
        st, _, _ = sigv4_request(
            "GET", "/any-bucket",
            ADMIN_ACCESS_KEY, ADMIN_SECRET_KEY,
        )
        # Admin should not get 403 — any other code is fine (503 for no master, etc.)
        if st != 403:
            results.ok("TC-13: Admin static creds → full access (not 403)")
        else:
            results.fail("TC-13: Admin static creds → full access", f"status={st}")

        # TC-14: 不正なアクセスキーは拒否される
        st, _, _ = sigv4_request(
            "GET", "/",
            "INVALID_KEY", "INVALID_SECRET",
        )
        if st == 403:
            results.ok("TC-14: Invalid static creds → denied (403)")
        else:
            results.fail("TC-14: Invalid static creds → denied (403)", f"status={st}")

    finally:
        # Cleanup
        print("\n[CLEANUP] Stopping S3 Server...")
        s3_proc.terminate()
        try:
            s3_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            s3_proc.kill()

        mock_server.shutdown()
        os.remove(iam_config_path)

    # Summary
    success = results.summary()
    sys.exit(0 if success else 1)


if __name__ == "__main__":
    main()

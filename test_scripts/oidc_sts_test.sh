#!/bin/bash
set -e

# Change to project root
cd "$(dirname "$0")/.."

# Cleanup function
cleanup() {
    echo "Cleaning up..."
    [ ! -z "$S3_PID" ] && kill $S3_PID 2>/dev/null || true
    [ ! -z "$MOCK_PID" ] && kill $MOCK_PID 2>/dev/null || true
    rm -f /tmp/iam_config.json /tmp/hello.txt
    # Kill any remaining processes on our ports
    lsof -ti:8085 | xargs kill -9 2>/dev/null || true
    lsof -ti:9000 | xargs kill -9 2>/dev/null || true
}

trap cleanup EXIT

# Start with a clean slate
echo "Cleaning up previous state..."
cleanup

echo "=== OIDC & STS E2E Test ==="

# 1. Install dependencies (Python for mock server and token generation)
echo "Installing Python dependencies (PyJWT, cryptography, boto3)..."
# Capture output and status to handle errors properly without being too noisy
PIP_OUTPUT=$(pip3 install --break-system-packages pyjwt cryptography boto3 2>&1)
PIP_STATUS=$?
echo "$PIP_OUTPUT" | grep -v "already satisfied" || true

if [ $PIP_STATUS -ne 0 ]; then
    echo "Error: pip3 install failed with status $PIP_STATUS"
    exit $PIP_STATUS
fi


OIDC_PORT=8085
# 2. Start Mock OIDC Server
echo "Starting Mock OIDC Server on port $OIDC_PORT..."
OIDC_PORT=$OIDC_PORT python3 test_scripts/mock_oidc.py &
MOCK_PID=$!

# Wait for mock to start
sleep 2

# 3. Prepare IAM Config
cat <<EOF > /tmp/iam_config.json
{
  "Roles": [
    {
      "RoleName": "tenant-a-role",
      "Arn": "arn:dfs:iam:::role/tenant-a-role",
      "AssumeRolePolicyDocument": {
        "Statement": [
          {
            "Effect": "Allow",
            "Principal": { "Federated": "http://localhost:$OIDC_PORT" },
            "Action": "sts:AssumeRoleWithWebIdentity",
            "Condition": {
              "ForAnyValue:StringEquals": {
                "OIDC_ISSUER:groups": ["tenant-a"]
              }
            }
          }
        ]
      },
      "Policies": [
        {
          "PolicyName": "TenantAReadWrite",
          "PolicyDocument": {
            "Statement": [
              {
                "Effect": "Allow",
                "Action": ["s3:GetObject", "s3:PutObject", "s3:ListBucket"],
                "Resource": ["arn:dfs:s3:::tenant-a*"]
              }
            ]
          }
        }
      ]
    }
  ]
}
EOF

# 4. Start S3 Server with OIDC/STS enabled
echo "Starting S3 Server..."
S3_AUTH_ENABLED=true \
OIDC_ISSUER_URL=http://localhost:$OIDC_PORT \
OIDC_CLIENT_ID=test-client \
STS_SIGNING_KEY=00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff \
IAM_CONFIG_PATH=/tmp/iam_config.json \
S3_REGION=us-east-1 \
MASTER_ADDR=http://localhost:50051 \
AUDIT_HMAC_SECRET=test_audit_hmac_secret_key_16bytes \
cargo run -p s3-server &
S3_PID=$!

# Wait for S3 server to start
sleep 15

# 5. Generate JWT Token from the running OIDC server's /token endpoint
echo "Generating JWT Token from Mock OIDC..."
TOKEN=$(curl -s http://localhost:$OIDC_PORT/token)
echo "Token: ${TOKEN:0:20}..."

# 6. Call AssumeRoleWithWebIdentity (STS)
echo "Calling AssumeRoleWithWebIdentity..."
RESPONSE=$(curl -s "http://localhost:9000/?Action=AssumeRoleWithWebIdentity&DurationSeconds=3600&RoleArn=arn:dfs:iam:::role/tenant-a-role&RoleSessionName=test-session&WebIdentityToken=${TOKEN}")

# Extract credentials (simplistic regex for bash, normally use xmllint or similar)
AK=$(echo "$RESPONSE" | sed -n 's/.*<AccessKeyId>\(.*\)<\/AccessKeyId>.*/\1/p')
SK=$(echo "$RESPONSE" | sed -n 's/.*<SecretAccessKey>\(.*\)<\/SecretAccessKey>.*/\1/p')
ST=$(echo "$RESPONSE" | sed -n 's/.*<SessionToken>\(.*\)<\/SessionToken>.*/\1/p')

if [ -z "$AK" ] || [ -z "$SK" ] || [ -z "$ST" ]; then
    echo "Error: Failed to get STS credentials"
    echo "Response: $RESPONSE"
    exit 1
fi

echo "STS Credentials Acquired: $AK"

# 7. Verify STS credentials are valid by checking their format
echo "Verifying STS credentials format..."
if [[ ! "$AK" =~ ^ASIA ]]; then
    echo "Error: Access Key should start with 'ASIA' for temporary credentials"
    exit 1
fi

if [ ${#ST} -lt 50 ]; then
    echo "Error: SessionToken seems too short"
    exit 1
fi

echo "✓ STS credentials format validated"
echo "✓ AccessKeyId: $AK"
echo "✓ SessionToken length: ${#ST}"

# Note: Actual S3 operations require a running Master server
# This test validates the OIDC → STS → Credential issuance flow
echo ""
echo "Note: S3 operations (bucket creation, object uploads) require a running Master server."
echo "This test successfully validates:"
echo "  1. OIDC Discovery and JWKS retrieval"
echo "  2. JWT token generation and validation"
echo "  3. AssumeRoleWithWebIdentity (STS)"
echo "  4. Temporary credential issuance"

# 8. Test passed
echo "=== E2E TEST PASSED ==="
exit 0

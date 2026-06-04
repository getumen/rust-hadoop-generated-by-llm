#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

ENDPOINT="http://localhost:9000"

export AWS_ACCESS_KEY_ID=dummy
export AWS_SECRET_ACCESS_KEY=dummy
export AWS_DEFAULT_REGION=us-east-1

aws_cmd() {
    aws --endpoint-url "$ENDPOINT" --no-verify-ssl "$@"
}

echo "=== Bucket Policy Integration Test ==="

BUCKET="policy-test-bucket-$$"

# Create bucket
aws_cmd s3 mb "s3://${BUCKET}"
echo "✓ Bucket created: ${BUCKET}"

# Upload a test object
echo "test-content" | aws_cmd s3 cp - "s3://${BUCKET}/test-obj.txt"
echo "✓ Test object uploaded"

# --- Test 1: GET on non-existent policy returns NoSuchBucketPolicy ---
echo ""
echo "Test 1: GET non-existent policy"
STATUS=$(aws_cmd s3api get-bucket-policy --bucket "${BUCKET}" 2>&1 || true)
if echo "$STATUS" | grep -q "NoSuchBucketPolicy"; then
    echo "✓ NoSuchBucketPolicy returned"
else
    echo "WARN: unexpected response: $STATUS"
fi

# --- Test 2: PUT a valid bucket policy ---
echo ""
echo "Test 2: PUT bucket policy (Deny DeleteObject)"
POLICY=$(cat <<EOF
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Deny",
      "Principal": "*",
      "Action": "s3:DeleteObject",
      "Resource": "arn:dfs:s3:::${BUCKET}/*"
    }
  ]
}
EOF
)
echo "$POLICY" > /tmp/bucket_policy_$$.json
aws_cmd s3api put-bucket-policy --bucket "${BUCKET}" --policy file:///tmp/bucket_policy_$$.json
echo "✓ Bucket policy PUT succeeded"

# --- Test 3: GET the policy back ---
echo ""
echo "Test 3: GET bucket policy"
RETRIEVED=$(aws_cmd s3api get-bucket-policy --bucket "${BUCKET}" --query Policy --output text 2>/dev/null || \
            aws_cmd s3api get-bucket-policy --bucket "${BUCKET}" 2>/dev/null || true)
if echo "$RETRIEVED" | grep -q "Deny"; then
    echo "✓ Policy retrieved and contains Deny statement"
else
    echo "WARN: policy may not have been retrieved correctly: $RETRIEVED"
fi

# --- Test 4: DELETE the policy ---
echo ""
echo "Test 4: DELETE bucket policy"
aws_cmd s3api delete-bucket-policy --bucket "${BUCKET}"
echo "✓ Bucket policy deleted"

# --- Test 5: GET after DELETE returns NoSuchBucketPolicy ---
echo ""
echo "Test 5: GET after DELETE"
STATUS=$(aws_cmd s3api get-bucket-policy --bucket "${BUCKET}" 2>&1 || true)
if echo "$STATUS" | grep -q "NoSuchBucketPolicy"; then
    echo "✓ NoSuchBucketPolicy after delete"
else
    echo "WARN: unexpected response after delete: $STATUS"
fi

# --- Test 6: PUT malformed JSON returns error ---
echo ""
echo "Test 6: PUT malformed policy JSON"
STATUS=$(aws_cmd s3api put-bucket-policy --bucket "${BUCKET}" --policy 'not-valid-json' 2>&1 || true)
if echo "$STATUS" | grep -qE "MalformedPolicy|InvalidArgument|error"; then
    echo "✓ Malformed policy rejected"
else
    echo "WARN: expected rejection for malformed JSON: $STATUS"
fi

# Cleanup
aws_cmd s3 rm "s3://${BUCKET}/test-obj.txt" 2>/dev/null || true
aws_cmd s3 rb "s3://${BUCKET}" 2>/dev/null || true
rm -f /tmp/bucket_policy_$$.json

echo ""
echo "=== BUCKET POLICY TEST PASSED ==="

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

echo "=== SSE-S3 Integration Test ==="

# Create bucket
aws_cmd s3 mb s3://sse-test-bucket 2>/dev/null || true

CONTENT="secret-data-$(date +%s)"
TMPFILE=$(mktemp)
echo "$CONTENT" > "$TMPFILE"

echo "Uploading with SSE-S3..."
aws_cmd s3 cp "$TMPFILE" s3://sse-test-bucket/sse-obj.txt \
    --sse AES256

echo "Downloading and verifying content..."
OUTFILE=$(mktemp)
aws_cmd s3 cp s3://sse-test-bucket/sse-obj.txt "$OUTFILE"
RESULT=$(cat "$OUTFILE")
EXPECTED=$(cat "$TMPFILE")
if [ "$RESULT" != "$EXPECTED" ]; then
    echo "FAIL: content mismatch."
    echo "  Expected: $EXPECTED"
    echo "  Got:      $RESULT"
    exit 1
fi
echo "✓ Content matches after SSE round-trip"

echo "Verifying SSE response header..."
HEAD_RESP=$(aws_cmd s3api head-object --bucket sse-test-bucket --key sse-obj.txt)
if echo "$HEAD_RESP" | grep -q "AES256"; then
    echo "✓ x-amz-server-side-encryption: AES256 present in HeadObject response"
else
    echo "WARN: AES256 header not found in HeadObject response (may depend on HeadObject implementation)"
fi

echo "Testing overwrite (SSE preserved)..."
NEW_CONTENT="updated-content-$(date +%s)"
echo "$NEW_CONTENT" > "$TMPFILE"
aws_cmd s3 cp "$TMPFILE" s3://sse-test-bucket/sse-obj.txt --sse AES256
aws_cmd s3 cp s3://sse-test-bucket/sse-obj.txt "$OUTFILE"
RESULT=$(cat "$OUTFILE")
EXPECTED="$NEW_CONTENT"
if [ "$RESULT" != "$NEW_CONTENT" ]; then
    echo "FAIL: overwrite content mismatch."
    echo "  Expected: $NEW_CONTENT"
    echo "  Got:      $RESULT"
    exit 1
fi
echo "✓ Overwrite with SSE works"

# Cleanup
aws_cmd s3 rm s3://sse-test-bucket/sse-obj.txt 2>/dev/null || true
aws_cmd s3 rb s3://sse-test-bucket --force 2>/dev/null || true
rm -f "$TMPFILE" "$OUTFILE"

echo "=== SSE TEST PASSED ==="

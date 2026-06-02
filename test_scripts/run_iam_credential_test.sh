#!/bin/bash
set -e

# IAM Credential Design — 機能テストランナー
# 設計書: docs/iam_credentials_design.md
#
# 前提:
#   - Python 3.8+
#   - pip install pyjwt cryptography
#   - cargo がインストール済み
#   - Metaserver が起動していなくても認証・認可レイヤーのテストは実行可能
#     (バックエンド未接続でも SigV4 検証、STS、ポリシー評価は動作する)

cd "$(dirname "$0")/.."
SCRIPT_DIR="$(pwd)"

# Kill any stale processes on the ports used by this test (mock OIDC: 18080, S3: 19000)
for _port in 18080 19000; do
    lsof -ti:$_port 2>/dev/null | while read _pid; do
        _comm=$(ps -p "$_pid" -o comm= 2>/dev/null || echo "")
        if [ "$_comm" != "ssh" ] && [ "$_comm" != "limactl" ]; then
            kill -9 "$_pid" 2>/dev/null || true
        fi
    done
done

echo "========================================"
echo " IAM Credential Design — 機能テスト"
echo "========================================"
echo ""

# 1. Check Python dependencies
echo "[PREP] Checking Python dependencies..."
python3 -c "import jwt; import cryptography" 2>/dev/null || {
    echo "[PREP] Installing PyJWT and cryptography..."
    pip3 install --break-system-packages --trusted-host pypi.org --trusted-host files.pythonhosted.org pyjwt cryptography
}

# 2. Build
echo "[PREP] Building project..."
cargo build -p s3-server

# 3. Run tests
echo ""
echo "[RUN] Starting functional tests..."
echo ""
python3 test_scripts/iam_credential_test.py
EXIT_CODE=$?

exit $EXIT_CODE

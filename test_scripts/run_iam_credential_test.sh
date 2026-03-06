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

echo "========================================"
echo " IAM Credential Design — 機能テスト"
echo "========================================"
echo ""

# 1. Check Python dependencies
echo "[PREP] Checking Python dependencies..."
python3 -c "import jwt; import cryptography" 2>/dev/null || {
    echo "[PREP] Installing PyJWT and cryptography..."
    pip3 install --break-system-packages pyjwt cryptography
}

# 2. Build
echo "[PREP] Building project..."
cargo build -p s3-server 2>&1 | tail -1

# 3. Run tests
echo ""
echo "[RUN] Starting functional tests..."
echo ""
python3 test_scripts/iam_credential_test.py
EXIT_CODE=$?

exit $EXIT_CODE

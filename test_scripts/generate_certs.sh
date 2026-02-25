#!/bin/bash
set -e

# Get script directory
DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
CERTS_DIR="$DIR/../certs"

# Clean up previous certs
rm -rf "$CERTS_DIR"
mkdir -p "$CERTS_DIR"
cd "$CERTS_DIR"

echo "Generating certificates in $CERTS_DIR..."

# 1. Generate CA Private Key
openssl genrsa -out ca.key 4096

# 2. Generate CA Certificate
openssl req -x509 -new -nodes -key ca.key -sha256 -days 3650 -out ca.crt -subj "/CN=MyDFSRootCA"

# 3. Generate Server Private Key
openssl genrsa -out server.key 4096

# 4. Generate Server CSR with SANs
openssl req -new -key server.key -out server.csr -subj "/CN=localhost" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"

# 5. Sign Server Certificate with CA
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
    -out server.crt -days 365 -sha256 \
    -copy_extensions copy

# 6. Verify
openssl verify -CAfile ca.crt server.crt

echo " Certificates generated successfully:"
ls -l "$CERTS_DIR"

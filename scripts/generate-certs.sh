#!/bin/bash
# Generate self-signed certificates for local mTLS testing of brokkr.
# Creates: ca.crt, server.crt, server.key, client.crt, client.key
#
# Usage: ./generate-certs.sh [--out-dir <path>]
#   Certificates are written to the current directory by default.
#
set -euo pipefail

OUT_DIR="${1:-.}"

echo "Generating brokkr mTLS certificates in $OUT_DIR"

pushd "$OUT_DIR" > /dev/null

# 1. CA (self-signed RSA)
openssl req -x509 -newkey rsa:4096 -keyout ca.key -out ca.crt -days 365 -nodes \
  -subj "/CN=brokkr-ca"

# 2. Server certificate (for localhost)
openssl req -newkey rsa:4096 -keyout server.key -out server.csr -nodes \
  -subj "/CN=localhost"

openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key \
  -CAcreateserial -out server.crt -days 365 \
  -extfile <(printf "subjectAltName=DNS:localhost,IP:127.0.0.1")

# 3. Client certificate (for workers)
openssl req -newkey rsa:4096 -keyout client.key -out client.csr -nodes \
  -subj "/CN=brokkr-worker"

openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key \
  -CAcreateserial -out client.crt -days 365

# Cleanup
rm -f server.csr client.csr ca.srl ca.key 2>/dev/null || true

echo "Created:"
echo "  CA root:           ca.crt (use as --ca / --tls-client-ca)"
echo "  Server cert+key:   server.crt, server.key (use as --tls-cert / --tls-key)"
echo "  Client cert+key:   client.crt, client.key (use as --client-cert / --client-key)"

popd > /dev/null
#!/usr/bin/env bash
# Regenerate the test TLS fixtures under
# `crates/brokkr-control/tests/fixtures/`. Run from the repo root. Idempotent:
# overwrites every file in that directory and then re-prints the chain
# verification for each cert. See fixtures/README.md for the rationale.
set -euo pipefail

FIX=crates/brokkr-control/tests/fixtures
mkdir -p "$FIX"

DAYS=36500   # 100 years; test-only fixtures, see fixtures/README.md.

# Two independent CAs: `ca` signs the good server+worker pair, `badca`
# signs the cert that should be rejected when `--tls-client-ca ca.pem` is
# configured.
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$FIX/ca.key" -out "$FIX/ca.pem" \
    -subj "/CN=brokkr-test-ca" -days "$DAYS" >/dev/null 2>&1
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$FIX/badca.key" -out "$FIX/badca.pem" \
    -subj "/CN=brokkr-test-badca" -days "$DAYS" >/dev/null 2>&1

# Server cert, signed by `ca`. The SAN includes 127.0.0.1 +
# localhost so TLS hostname verification (rustls default) passes when
# tests connect to `https://127.0.0.1:<port>` (issue #139).
cat > "$FIX/server.cnf" <<EOF
[req]
distinguished_name = dn
req_extensions     = v3_req
prompt             = no
[dn]
CN = brokkr-test-server
[v3_req]
subjectAltName = @alt_names
[alt_names]
IP.1 = 127.0.0.1
DNS.1 = localhost
EOF
openssl req -newkey rsa:2048 -nodes -keyout "$FIX/server.key" -out "$FIX/server.csr" \
    -config "$FIX/server.cnf" >/dev/null 2>&1
openssl x509 -req -in "$FIX/server.csr" -CA "$FIX/ca.pem" -CAkey "$FIX/ca.key" -CAcreateserial \
    -out "$FIX/server.pem" -days "$DAYS" -extensions v3_req -extfile "$FIX/server.cnf" >/dev/null 2>&1
rm -f "$FIX/server.csr" "$FIX/server.cnf" "$FIX/ca.srl"

# Worker cert, signed by `ca`.
openssl req -newkey rsa:2048 -nodes -keyout "$FIX/worker.key" -out "$FIX/worker.csr" \
    -subj "/CN=brokkr-test-worker" >/dev/null 2>&1
openssl x509 -req -in "$FIX/worker.csr" -CA "$FIX/ca.pem" -CAkey "$FIX/ca.key" -CAcreateserial \
    -out "$FIX/worker.pem" -days "$DAYS" >/dev/null 2>&1
rm -f "$FIX/worker.csr" "$FIX/ca.srl"

# Bad-worker cert, signed by `badca`.
openssl req -newkey rsa:2048 -nodes -keyout "$FIX/badworker.key" -out "$FIX/badworker.csr" \
    -subj "/CN=brokkr-test-badworker" >/dev/null 2>&1
openssl x509 -req -in "$FIX/badworker.csr" -CA "$FIX/badca.pem" -CAkey "$FIX/badca.key" -CAcreateserial \
    -out "$FIX/badworker.pem" -days "$DAYS" >/dev/null 2>&1
rm -f "$FIX/badworker.csr" "$FIX/badca.srl"

# Re-verify the chain. `server`/`worker` should verify under `ca`,
# `badworker` should verify under `badca` and be rejected by `ca`.
echo "=== server.pem verified by ca.pem ==="
openssl verify -CAfile "$FIX/ca.pem" "$FIX/server.pem"
echo "=== worker.pem verified by ca.pem ==="
openssl verify -CAfile "$FIX/ca.pem" "$FIX/worker.pem"
echo "=== badworker.pem verified by badca.pem ==="
openssl verify -CAfile "$FIX/badca.pem" "$FIX/badworker.pem"
echo "=== badworker.pem rejected by ca.pem (expected: fail) ==="
openssl verify -CAfile "$FIX/ca.pem" "$FIX/badworker.pem" || true

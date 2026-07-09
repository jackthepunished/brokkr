# Test TLS fixtures

These PEM files are checked in so that the mTLS integration tests under
`../two_process_cluster.rs` can build a real, end-to-end TLS chain without
needing `openssl` at test time. They are **test-only** and have no
production value.

| File           | Role                                                       |
| -------------- | ---------------------------------------------------------- |
| `ca.pem`       | Self-signed root CA that signs `server` and `worker`.      |
| `server.pem`/`.key` | Server identity for `brokkr-control` (signed by `ca`). |
| `worker.pem`/`.key` | Worker identity for `brokkr-worker` (signed by `ca`).  |
| `badca.pem`    | A *second* self-signed CA, unrelated to `ca`.              |
| `badworker.pem`/`.key` | Worker identity signed by `badca`; rejected by `ca`. |

The validity window is 100 years (the maximum OpenSSL allows). That is fine
because these keys are used only inside the in-process test TLS context —
they are not exposed to any network reachable from outside the test
process. Replacing them would require updating both the `#[ignore]` tests
that load them and any operator documentation that cites the file names.

## Regeneration

To regenerate everything from scratch (e.g. if a key is leaked into a
public log), run from the repo root:

```sh
scripts/gen-test-certs.sh
```

This recreates every file in this directory and then re-prints the chain
verification (`openssl verify`) for each cert so you can eyeball the
result.

## Threat model

These are **not secrets**. They are fixtures for tests that need a working
PKI. The private keys are committed so the tests run hermetically; they
should not be used to authenticate any real Brokkr component.

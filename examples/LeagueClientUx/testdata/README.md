# TEST-ONLY TLS fixtures

`cert.der` and `key.der` are **test-only, non-secret fixtures** for the mock
League client. The private key is public by design: it authenticates nothing
outside the loopback mock, and it must never be rotated or treated as a real
credential.

- They are used only by `examples/LeagueClientUx/mock_core.rs`, which binds
  `127.0.0.1` only. See ADR-0003 (`docs/adr/0003-loopback-https-committed-test-cert.md`)
  and the threat model and non-negotiable rules in ADR-0007
  (`docs/adr/0007-mock-threat-model.md`).
- Never install `cert.der` into a trust store and never reference either file
  from production code, `build.rs`, the app, or release packaging.
- They must never be packaged or shipped. `Cargo.toml` excludes
  `examples/LeagueClientUx/` so `cargo package` omits this directory.

The certificate is a self-signed leaf (`basicConstraints=critical,CA:FALSE`,
`keyUsage=critical,digitalSignature,keyEncipherment`, `extendedKeyUsage=serverAuth`,
SAN `IP:127.0.0.1,DNS:localhost`), RSA 2048, valid for about ten years.
Regenerate both files together with the OpenSSL commands in ADR-0003 as amended
by the slice fix for leaf extensions, then re-commit the DER files.

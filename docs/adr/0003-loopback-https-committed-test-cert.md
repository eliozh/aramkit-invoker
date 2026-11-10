# ADR-0003: Serve loopback HTTPS with a committed test-only certificate

**Date**: 2026-11-10
**Status**: accepted
**Deciders**: architect (F5); pending G3

## Context

`LcuCredentials::base_url()` in `src/lcu/creds.rs` hardcodes `https://127.0.0.1:<p>`, and
`build_client()` in `src/lcu/client.rs` enables `danger_accept_invalid_certs(true)` for that
loopback client. Plain HTTP would require an app change and would stop exercising the real TLS path.

The approved spec (D2) resolves the fixture question: use a committed test-only self-signed
certificate and key, because the environment is offline and `rcgen` is not in the cargo registry
cache. The server must bind loopback only, and the certificate must never be used by the app or a
production path.

## Decision

The mock serves HTTPS only and binds only `127.0.0.1`. It loads a committed DER certificate and
PKCS#8 DER key with `include_bytes!`; no PEM parser is used.

Files:

```text
examples/LeagueClientUx/testdata/cert.der
examples/LeagueClientUx/testdata/key.der
```

Generation is a one-time developer step with OpenSSL. The commands were validated on OpenSSL 3.5.9:

```bash
openssl req -x509 -newkey rsa:2048 \
  -keyout key.pem -out cert.pem \
  -days 3650 -nodes \
  -subj "/CN=127.0.0.1" \
  -addext "subjectAltName=IP:127.0.0.1,DNS:localhost"

openssl x509 -in cert.pem -outform DER -out cert.der
openssl pkcs8 -topk8 -nocrypt -in key.pem -outform DER -out key.der
```

Properties:

- RSA 2048. Chosen over ECDSA P-256 because the app's Windows client uses SChannel through
  `native-tls`; RSA removes any ECDSA cipher-suite interop doubt. The cost is a larger fixture
  (about 0.8 KiB certificate and 1.2 KiB key).
- Self-signed, 10-year validity, no passphrase, SAN `IP:127.0.0.1` and `DNS:localhost`.
- Committed under `examples/`, used only by the mock, and never referenced from `src/`, `build.rs`,
  release packaging, or the app.
- The private key is public by design. It authenticates nothing outside a loopback mock, and the app
  already disables verification for loopback. It must be treated as a non-secret test fixture, not
  rotated like a real credential.
- Expiry is 2036. Regeneration uses the same commands and replaces both files; the ADR and the
  fixture directory note this.

The app side is unchanged: `build_client()` continues to accept invalid certificates for
`https://127.0.0.1:<p>` only.

## Alternatives Considered

### Alternative 1: Plain HTTP
- **Pros**: Simpler server, no certificate fixture.
- **Cons**: Requires changing the app's hardcoded scheme; loses coverage of the real TLS path.
- **Why not**: The spec explicitly rejects plain HTTP (Q12).

### Alternative 2: Runtime certificate generation with `rcgen`
- **Pros**: No committed key; fresh certificate per run.
- **Cons**: `rcgen` is not in the offline cargo cache; the environment cannot fetch it.
- **Why not**: Infeasible offline; the spec replaced runtime generation with the committed fixture.

### Alternative 3: Parse PEM at runtime with `rustls-pemfile`
- **Pros**: Commit readable PEM files.
- **Cons**: `rustls-pemfile` is not in the offline cargo cache; adds a parser for a one-time
  conversion.
- **Why not**: Converting to DER once with OpenSSL needs no runtime dependency.

### Alternative 4: Use a native-tls/OpenSSL server
- **Pros**: Reuses the platform TLS stack.
- **Cons**: Adds a server-side native TLS dependency and build risk; the app's `reqwest`/`native-tls`
  dependency is client-only and is not a server.
- **Why not**: rustls with the ring provider is cached, pure-Rust at the API level, and portable.

### Alternative 5: Install the certificate into a trust store or pin it
- **Pros**: More realistic trust behavior.
- **Cons**: Out of scope; both are production-like changes and machine-wide mutations.
- **Why not**: The app already disables verification for loopback; trust installation is not needed.

## Consequences

### Positive
- The real HTTPS path is exercised end to end with no app change.
- No runtime certificate-generation dependency, which is required offline.
- DER plus `include_bytes!` avoids any PEM parser and keeps startup synchronous and simple.

### Negative
- A TLS private key is committed to the repository. Secret scanners may flag it; the file and this
  ADR must state that it is a test-only, non-secret fixture.
- The fixture expires in 2036 and needs regeneration then.
- RSA 2048 is larger than an ECDSA P-256 fixture; the size is irrelevant for a test server.

### Risks
- If the fixture is ever copied into a production path or a trust store, it would be a real
  (if low-value) key exposure. The release-isolation checks in ADR-0002 and the path review guard
  this.
- The app's certificate verification is disabled only for loopback; the mock must never bind a
  non-loopback address, or the fixture would be exposed on the network. ADR-0007 makes loopback
  binding a non-negotiable rule.

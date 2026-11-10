# ADR-0004: Use a blocking `std` TCP server with rustls and the ring provider

**Date**: 2026-11-10
**Status**: accepted
**Deciders**: architect (F5); pending G3

## Context

The mock needs to terminate TLS and speak a tiny subset of HTTP/1.1 on loopback. The machine is
offline, so only crates already in `~/.cargo/registry` are usable. The app's existing `reqwest` +
`native-tls` stack is client-only and cannot serve TLS. Adding an async runtime for a single-client
test server would add dependency weight and complexity for no benefit.

Verified cached crates relevant to this decision:

| Crate | Cached versions | Verdict |
|-------|-----------------|---------|
| `rustls` | 0.23.37, 0.23.46 | use 0.23.46 |
| `rustls-pki-types` | 1.14.0, 1.15.1 | transitive, re-exported |
| `rustls-webpki` | 0.103.9, 0.103.16 | transitive |
| `ring` | 0.17.14 | use via rustls |
| `aws-lc-rs` | 1.15.3 | not usable with rustls 0.23.46 |
| `tokio-rustls` | 0.26.4, 0.26.6 | not needed |
| `base64` | 0.22.1, 0.23.1 | use 0.22.1 |
| `httparse` | 1.10.1 | use 1.10.1 |
| `getrandom` | 0.2.17, 0.3.4, 0.4.3 | use 0.3.4 direct |
| `rcgen` | not cached | rejected |
| `tiny_http` | not cached | rejected |
| `rustls-pemfile` | not cached | rejected |
| `openssl-src` | not cached | rejected |

A critical offline detail: rustls 0.23.46's `default` feature enables `aws_lc_rs`, which requires
`aws-lc-rs >= 1.18`. Only 1.15.3 is cached, and building `aws-lc-sys` also needs cmake/nasm on
Windows. Default features therefore cannot be used. The `ring` feature is cached and builds with a C
compiler through `cc`.

## Decision

Use a synchronous server with no async runtime:

- Listener: `std::net::TcpListener`, bound to `127.0.0.1`.
- TLS: `rustls::ServerConnection` wrapped with `rustls::StreamOwned<ServerConnection, TcpStream>`.
- Provider: `rustls::crypto::ring::default_provider()`, installed explicitly on the config builder
  rather than process-wide.
- Certificate loading: DER bytes with `include_bytes!`, converted through `rustls::pki_types`
  (re-exported by rustls); no PEM parser.
- HTTP request head: `httparse` (battle-tested, cached, already present in `Cargo.lock`).
- Response writing: a small hand-written writer; the response surface is fixed.
- Token generation: `getrandom::fill` over 32 bytes, encoded with base64 URL-safe without padding.
- Basic-auth header construction: `base64` standard alphabet with padding, precomputed once at
  startup.

Exact dev-dependencies:

```toml
[dev-dependencies]
rustls = { version = "=0.23.46", default-features = false, features = ["ring", "std", "tls12"] }
httparse = "=1.10.1"
base64 = "=0.22.1"
getrandom = "=0.3.4"
```

Transitive versions expected in the lockfile after resolution:

```text
ring 0.17.14
rustls-webpki 0.103.16
rustls-pki-types 1.15.1
subtle 2.6.1
zeroize 1.9.1
once_cell 1.21.4
untrusted 0.9.0
getrandom 0.2.17   (via ring)
cfg-if 1.0.5
cc 1.2.56          (build)
libc 0.2.191
```

`ring` and `rustls-pki-types` are not direct dependencies: the rustls `ring` feature enables ring,
and pki-types is re-exported as `rustls::pki_types`. `tls12` is enabled so the server offers both
TLS 1.2 and TLS 1.3; SChannel and OpenSSL clients both interoperate.

Configuration sketch:

```rust
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

static CERT_DER: &[u8] = include_bytes!("testdata/cert.der");
static KEY_DER: &[u8] = include_bytes!("testdata/key.der");

let provider = Arc::new(rustls::crypto::ring::default_provider());
let config = ServerConfig::builder_with_provider(provider)
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(&CERT_DER[..])],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(&KEY_DER[..])),
    )?;

let connection = ServerConnection::new(Arc::new(config))?;
let mut stream = StreamOwned::new(connection, tcp_stream);
```

Server loop and limits:

- `set_nonblocking(true)` on the listener; accept in a loop with a 10 ms sleep while checking an
  atomic stop flag. This gives a deterministic shutdown without a wake-up connection.
- Accepted `TcpStream`s are set back to blocking with 5 s read and write timeouts.
- One connection at a time, one request per connection, `Connection: close`. The app's watcher is
  sequential, so concurrency is unnecessary.
- Request head is capped at 8 KiB and must end at `\r\n\r\n`; oversized or malformed requests get a
  400 and are closed.
- TLS handshake errors close the connection without an HTTP response and without logging request
  bytes.
- ALPN is left unset. HTTP/1.1 is the only protocol, and the app's native-tls client does not
  require an ALPN protocol; advertising one adds an unnecessary negotiation failure mode.

## Alternatives Considered

### Alternative 1: tokio + tokio-rustls
- **Pros**: Familiar async server shape; crates are cached.
- **Cons**: Adds a runtime, task scheduling, and shutdown complexity for a single sequential client.
- **Why not**: The app is blocking; a blocking server is simpler and has fewer failure modes.

### Alternative 2: `tiny_http`
- **Pros**: Higher-level HTTP handling.
- **Cons**: Not cached; would still need a TLS layer.
- **Why not**: Infeasible offline.

### Alternative 3: rustls default features (aws-lc-rs)
- **Pros**: The rustls default provider.
- **Cons**: rustls 0.23.46 needs `aws-lc-rs >= 1.18`; only 1.15.3 is cached; `aws-lc-sys` has
  cmake/nasm build risk on Windows.
- **Why not**: Cannot resolve offline; ring is cached and sufficient.

### Alternative 4: A hand-written HTTP request parser
- **Pros**: No dependency.
- **Cons**: Request parsing is exactly where hand-rolled code tends to be wrong; `httparse` is
  already in the lockfile and has no dependencies.
- **Why not**: The cached, proven parser is safer for the same dependency cost.

### Alternative 5: rustls 0.23.37
- **Pros**: Also cached.
- **Cons**: Older than 0.23.46; no advantage.
- **Why not**: Choose the newest cached patch; 0.23.37 remains the fallback if a transitive version
  constraint fails.

## Consequences

### Positive
- No async runtime; one thread, one connection at a time, easy shutdown.
- Full control over status codes, headers, and connection close, which the endpoint contract needs.
- All dependencies are cached and verifiable offline.

### Negative
- The server handles one request at a time. This is sufficient for the app's sequential polling but
  not a general-purpose server.
- `ring` builds C code through `cc`. Both Linux and `windows-latest` have a C toolchain; the project
  already builds native TLS on both.
- The exact pins must be revisited when the registry cache changes.

### Risks
- rustls 0.23.46 is newer than the app's own test baseline; if a transitive version is missing, the
  fallback is 0.23.37 with the same feature set.
- A blocking read with a 5 s timeout still lets a local client hold one connection for 5 s. This is a
  dev-only denial-of-service surface; ADR-0007 records it.
- Misconfiguring rustls with default features would pull aws-lc-rs and fail offline. The exact
  `default-features = false` line and a lockfile review are required.

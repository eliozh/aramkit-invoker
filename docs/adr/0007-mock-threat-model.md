# ADR-0007: Threat model and safety boundaries for the mock LCU server

**Date**: 2026-11-10
**Status**: accepted
**Deciders**: architect (F5); pending G3

## Context

The mock deliberately looks like the League client to the production discovery code: it uses the
`LeagueClientUx` process name, advertises a remoting token in argv or a discovery file, serves
loopback HTTPS with Basic auth, and can write files. These behaviors are acceptable only if the
boundaries are explicit and enforced. This ADR records the threat model and the non-negotiable
rules the implementation and review must check.

The mock is a developer and CI tool. It is not distributed, not installed, and serves no real user
data. The local machine is assumed to be trusted to the same degree as any development machine; a
compromised OS or a malicious local administrator is out of scope.

## Assets and trust boundaries

| Asset | Sensitivity | Notes |
|-------|-------------|-------|
| Remoting token | Ephemeral, mock-only | Grants access to the mock; generated per run; grants nothing else |
| Test certificate private key | Non-secret | Committed by design; authenticates nothing outside the loopback mock |
| Mock state and fixtures | Non-sensitive | Static gameflow/champion data |
| Developer filesystem | Scoped | Only the chosen discovery root is written |
| Real League client/game | Protected | The mock never reads, injects into, or modifies them |

Trust boundaries:

- The network is untrusted; loopback binding excludes remote peers.
- Loopback is not a boundary against other local processes. Any local process can connect, so auth
  matters.
- The process list and argv are readable by other local processes on the same machine. The token is
  exposed there by design, exactly as the real client exposes it.

## Threat model

| Threat | Mitigation | Residual risk |
|--------|------------|---------------|
| Remote network access to the mock | Bind `127.0.0.1` only; never `0.0.0.0`/`[::]`; automated bind test; no port forwarding | None expected; a host-level redirect would be outside the mock |
| Unauthorized local access | Basic auth with a random per-run token; 401 for missing/wrong credentials; `--reject-auth`; constant-time credential compare; auth before routing | A local process can read argv or `/proc/<pid>/cmdline` and reuse the token. Accepted: the mock serves no sensitive data and mirrors the real LCU |
| Token written to the repository or logs | Generate at runtime; never persist except in explicit lockfile/log discovery modes; redact in stdout and `Debug`; never log the `Authorization` header; tests assert the token is absent from fixtures and captured output | The token is in argv and in discovery files by design; documented |
| Certificate/key misuse | Test-only self-signed cert; no trust-store installation; not used by the app; loopback only; never in release artifacts | The key is public in the repository. Accepted because it authenticates nothing outside the mock |
| File write escape or clobbering a real install | Write only under `--install-root` (absolute, validated) or the mock's own executable directory; canonicalize; reject relative and UNC paths, `..`, and symlink escapes; allowlist `lockfile` and the `Game/Logs/LeagueClient Logs` log; refuse a root containing `LeagueClient.exe`, or a `LeagueClientUx.exe` that is not the mock's own executable, or matching a known Riot/Tencent/WeGame install prefix (mirroring `COMMON_INSTALL_ROOTS`); create Unix files with mode `0600` and keep the default user ACL on Windows; delete on clean exit; docs recommend running from a temp directory | A user can pass a writable non-League `--install-root`; validation cannot prove intent. Do not point it at a real install |
| External network / SSRF | No outbound sockets, no DNS, no proxy, no redirects, no 3xx; only loopback accepted | None expected; code review must confirm no HTTP client is instantiated |
| Process injection or tampering | No injection, no other-process memory access, no DLL loading, no writing into the real client, no credential reads from the real client | Antivirus/EDR/Vanguard may flag the impersonated process name; accepted as a test-only risk (spec Q1) |
| Request flooding or slowloris | 8 KiB request-head cap, 5 s read/write timeouts, one connection at a time, close on malformed input, loopback only | A local client can hold the single connection for up to 5 s. Dev-only denial of service; accepted |
| Release contamination | Examples are not built by `cargo build --release`; server dependencies are dev-dependencies only; the shared core is referenced from `src/` only inside `#[cfg(test)]`; CI uploads only `aramkit-invoker.exe`; release verification in ADR-0002 | A future refactor could add a production reference; review and the release check guard it |
| Coexistence with a real client | The app prefers the newest matching PID; the README says to close the real client; the mock uses an ephemeral or user-selected port and never targets the real client's port | A user can still run both and get unexpected discovery. Documented as unsupported |
| Privilege escalation | The mock does not require elevation and does not request extra privileges | If run elevated, it has no additional capability beyond the local user's; still loopback only |

## Non-negotiable rules

The implementation and review must enforce all of the following:

1. **Loopback only.** Bind `127.0.0.1`; no wildcard or non-loopback address, ever.
2. **Authenticate before routing.** Every request, including unknown paths, requires the token;
   `--reject-auth` forces 401.
3. **Never log secrets.** No token, `Authorization` value, or raw request head in stdout, stderr, or
   test output. Redact with `<redacted>`.
4. **Scoped file writes.** Only `lockfile` or the fixed log path under the validated root; nothing
   else. Refuse real-install markers (excluding the mock's own executable) and known League install
   prefixes; use restrictive permissions.
5. **No external network.** No outbound connection, DNS, proxy, redirect, or third-party request.
6. **No process injection.** No writing to other processes, no DLLs, no modifying the real client or
   game, no reading real credentials.
7. **Test-only certificate.** Never install the fixture into a trust store; never reference it from
   production code, `build.rs`, or release packaging.
8. **Never ship the mock.** No release artifact, installer, or package may contain
   `examples/LeagueClientUx/`, its certificate, or its dependencies.

## Out of scope

- A compromised OS or a malicious local administrator.
- Antivirus, EDR, or Riot Vanguard policy decisions about the impersonated process name. The spec
  accepts this as a known risk (Q1).
- Certificate trust installation, certificate pinning, or real-client coexistence.
- Distributing the mock or supporting it on machines without a Rust toolchain.

## Consequences

### Positive
- The impersonation, token exposure, and committed key are documented and bounded rather than
  implicit.
- The endpoint and file-write rules give reviewers concrete checks tied to AC-10, AC-11, AC-13, and
  AC-15.

### Negative
- The token in argv is a real exposure to other local processes. The alternative (a control channel
  or an app change) contradicts the zero-app-change goal.
- The committed private key will be reported by secret scanners; the documentation must explain it.
- File-write validation is defense in depth, not a sandbox.

### Risks
- If the loopback binding is ever relaxed for convenience, the mock could be reached remotely. This
  is the highest-impact regression; the bind test and this ADR must catch it.
- If the mock is accidentally packaged, the process impersonation and committed key become
  distributable. The release checks in ADR-0002 are the guard.

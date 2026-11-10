# Architecture Decision Records

Architecture decisions for the dev-only mock LCU server (`01-mock-lcu-client`). All records below
are **accepted** at G3 (2026-11-10).

| ADR | Title | Status | Date |
|-----|-------|--------|------|
| [0001](0001-mock-discovery-process-name.md) | Discover the mock through a `LeagueClientUx`-named process | accepted | 2026-11-10 |
| [0002](0002-mock-location-example-shared-core.md) | Place the mock in an example target with a shared core and dev-dependencies | accepted | 2026-11-10 |
| [0003](0003-loopback-https-committed-test-cert.md) | Serve loopback HTTPS with a committed test-only certificate | accepted | 2026-11-10 |
| [0004](0004-blocking-rustls-server-dependencies.md) | Use a blocking `std` TCP server with rustls and the ring provider | accepted | 2026-11-10 |
| [0005](0005-mock-scenario-driving.md) | Drive the mock with static flags and a deterministic timeline | accepted | 2026-11-10 |
| [0006](0006-mock-http-endpoint-contract.md) | Define the mock HTTP endpoint contract and 401 behavior | accepted | 2026-11-10 |
| [0007](0007-mock-threat-model.md) | Threat model and safety boundaries for the mock LCU server | accepted | 2026-11-10 |

## Scope

The mock is a developer and CI surface, never a product surface. It exists so the unmodified app
can exercise process discovery, loopback HTTPS Basic auth, the four read-only LCU endpoints, and
the ARAMKit URL computation without League or a Windows account.

## Reading order

- New to the feature: 0001, 0002, 0003, 0006.
- Implementing the server: 0004, 0005, 0006, 0007.
- Reviewing security: 0003, 0006, 0007.

## Related documents

- Approved feature spec: `.claude/features/01-mock-lcu-client/spec.md`
- Baseline: `.claude/ecc-workflow/handoffs/F0-baseline.md`
- Architecture handoff: `.claude/ecc-workflow/handoffs/F5-architecture.md`

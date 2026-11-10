# ADR-0006: Define the mock HTTP endpoint contract and 401 behavior

**Date**: 2026-11-10
**Status**: accepted
**Deciders**: architect (F5); pending G3

## Context

The app's client (`src/lcu/client.rs`) and models (`src/lcu/model.rs`, `src/champion.rs`) define the
only contract that matters: four read-only endpoints, Basic auth as `riot:<token>`, a 404 fallback
from `/lol-gameflow/v1/session` to `/lol-gameflow/v1/gameflow-phase`, and a 2 MB response ceiling.
The mock must match those shapes exactly so the unmodified client path is exercised. The contract
below is normative for the implementation.

## Decision

### Transport and binding

- Base URL: `https://127.0.0.1:<port>`; HTTPS only, TLS 1.2 or 1.3.
- Bind `127.0.0.1` only. Never `0.0.0.0`, never `[::]`.
- HTTP/1.1, one request per connection, `Connection: close`, no keep-alive, no chunked encoding, no
  compression, no HTTP/2.
- Request head limit: 8 KiB, ending at `\r\n\r\n`. Read/write timeout: 5 s.
- No request bodies are accepted.

### Request processing order

1. TLS handshake. Failure closes the connection without an HTTP response.
2. Read and parse the request head. Malformed, oversized, non-HTTP/1.x, or a request with
   `Content-Length > 0` or `Transfer-Encoding` gets `400` and closes.
3. **Authentication**, before routing and before the method check:
   - `--reject-auth` forces `401`.
   - Otherwise the request must carry exactly one `Authorization` header whose scheme is `Basic`
     (ASCII case-insensitive) and whose credential is exactly
     `base64_standard(UTF-8("riot:" + token))`, compared in constant time.
   - Missing, malformed, wrong scheme, wrong username, or wrong password gets `401`.
4. **Method**: only `GET` is allowed. Anything else gets `405` with `Allow: GET`.
5. **Route**: exact, case-sensitive path match. Query strings, trailing slashes, and unknown paths
   get `404`.
6. Handler produces the status and JSON body.
7. Response is written with an exact `Content-Length`, flushed, and the connection is closed.

Because authentication precedes routing, unauthenticated clients cannot probe which paths exist.
A request with a body is rejected with `400` at step 2, before authentication; the status entries in
the endpoint table describe the accepted no-body case.

### Response headers

| Response | Headers |
|----------|---------|
| `200` | `Content-Type: application/json`, `Content-Length: <n>`, `Cache-Control: no-store`, `Connection: close` |
| `401` | `WWW-Authenticate: Basic realm="mock-lcu"`, `Content-Length: 0`, `Connection: close` |
| `400`, `404`, `500` | `Content-Length: 0`, `Connection: close` |
| `405` | `Allow: GET`, `Content-Length: 0`, `Connection: close` |

No `Location` header and no `3xx` status is ever returned (AC-15). No `Set-Cookie`, no `Server`
header, and no error detail in a body. Error bodies are empty; status codes carry the meaning.

### Endpoint table

| Method | Path | Auth | Status | Body |
|--------|------|------|--------|------|
| GET | `/lol-gameflow/v1/session` | required | `200`, or `404` when `--gameflow-phase-only` | gameflow object |
| GET | `/lol-gameflow/v1/gameflow-phase` | required | `200` | JSON string |
| GET | `/lol-champ-select/v1/session` | required | `200` when phase is `ChampSelect`, else `404` | champ-select object |
| GET | `/lol-game-data/assets/v1/champion-summary.json` | required | `200` always | champion array |
| any | any other path | required | `401` then `404` | empty |
| non-GET | any path | required | `401` then `405` | empty |

### Gameflow session

`GET /lol-gameflow/v1/session`, normal mode:

```json
{
  "phase": "ChampSelect",
  "gameData": {
    "gameId": 424242,
    "queue": { "id": 2400, "gameMode": "KIWI" }
  }
}
```

- `phase` is the current static/scheduled phase.
- `gameId` is the constant `424242`.
- `queue.id` and `queue.gameMode` come from the scenario or `--queue-id` / `--game-mode`.
- All IDs are JSON numbers. The app also accepts numeric strings; the mock does not need a
  string-ID mode because `src/lcu/model.rs` already tests that flexibility.
- The object is present in every phase. The app reads `gameData` in every phase: the queue decides
  Mayhem, and the game ID correlates the pending champion-select session.

`GET /lol-gameflow/v1/gameflow-phase`, every mode:

```json
"ChampSelect"
```

The body is a bare JSON string including the quotes, with `Content-Type: application/json`.
It is available in normal mode too, but the app calls it only after `/session` returns 404.

### 404 fallback (normative)

With `--gameflow-phase-only`:

- `GET /lol-gameflow/v1/session` returns `404` with an empty body.
- The app then calls `GET /lol-gameflow/v1/gameflow-phase`, which returns `200` and the bare phase
  string.
- `fetch_gameflow` constructs a `GameflowSession` with the phase and default `gameData`, so
  `game_id` is 0 and the queue is empty.

This mode exercises the fallback and the phase deserialization. Because there is no queue data, the
tracker has `mayhem = None`; it does not produce the full trigger. The full trigger path requires
normal `/session` mode with queue data. This distinction is intentional and must not be "fixed" by
inventing a queue in phase-only mode.

### Champ-select session

`GET /lol-champ-select/v1/session`, only when `phase == "ChampSelect"`; otherwise `404` (the app maps
that to `None`):

```json
{
  "localPlayerCellId": 1,
  "myTeam": [
    { "cellId": 1, "championId": 103 }
  ],
  "actions": [
    [
      { "actorCellId": 1, "championId": 103, "completed": true, "type": "pick" }
    ]
  ]
}
```

- `localPlayerCellId` equals `--local-cell-id` (default 1).
- The single `myTeam` entry has that same cell and the scenario champion.
- The single completed pick action has the same actor cell and champion, `completed: true`, and
  `type: "pick"`.
- The app prefers `myTeam`; the completed-pick fallback is covered by existing model tests. No
  second member or extra action is needed.
- Outside `ChampSelect`, the endpoint is `404`, not an empty session.

### Champion summary

`GET /lol-game-data/assets/v1/champion-summary.json`, always `200`:

```json
[
  { "id": 103, "name": "Ahri", "alias": "Ahri" },
  { "id": 121, "name": "Kha'Zix", "alias": "Khazix" },
  { "id": 62, "name": "Wukong", "alias": "MonkeyKing" }
]
```

- If `--champion-id` is not one of these, append
  `{ "id": <id>, "name": "Mock Champion <id>", "alias": "MockChampion<id>" }` so the configured
  champion can always be resolved to a URL. `--champion-id` must be greater than 0.
- Entries with `id <= 0` or an empty alias are skipped by the app; the mock does not emit them.
- The response is under 4 KiB, far below the app's 2 MB cap (AC-15).

### Authentication behavior

- The correct header is exactly `Basic <base64("riot:<token>")>`, with standard base64 and padding,
  matching what `reqwest::basic_auth("riot", Some(&token))` sends.
- Any other value is `401`, with `WWW-Authenticate: Basic realm="mock-lcu"` and an empty body.
- `--reject-auth` returns `401` for every request, including a correct token, so the app's
  `credentials_rejected` path can be observed without restarting.
- The app maps a 401 to `credentials_rejected`: `fetch_gameflow` returns an error whose chain
  contains a `reqwest::Error` with status 401; the watcher logs
  `"客户端凭证已失效，正在重新读取"`, returns to discovery, and opens no page (AC-4).

### Explicit non-goals

WebSocket events, POST/PUT/PATCH/DELETE, a state-control endpoint, redirects, range requests,
conditional requests, chunked encoding, compression, HTTP/2, TLS client certificates, digest auth,
and any endpoint beyond the four above.

## Alternatives Considered

### Alternative 1: Mirror the real client's error bodies and headers
- **Pros**: Greater realism.
- **Cons**: The app ignores error bodies; inventing bodies risks leaking details and adds fixture
  surface for no coverage.
- **Why not**: Empty error bodies with correct status codes are sufficient and safer.

### Alternative 2: Return `200` with an error payload
- **Pros**: One status path.
- **Cons**: The app checks `error_for_status()` and 404 specifically; this would break the contract.
- **Why not**: Status codes are the contract.

### Alternative 3: Support keep-alive and multiple requests per connection
- **Pros**: Slightly faster polling.
- **Cons**: Connection state, framing, and timeout complexity for a single-client dev server.
- **Why not**: `Connection: close` is simpler and the app tolerates it.

### Alternative 4: Add an `--ids-as-strings` mode
- **Pros**: Exercises the flexible deserializer through HTTP.
- **Cons**: The same deserializer is already covered by `src/lcu/model.rs` tests; the flag adds
  surface outside the approved flag list.
- **Why not**: Not needed; defaults stay numeric.

## Consequences

### Positive
- The exact app path is exercised: discovery -> Basic auth -> four endpoints -> parse -> URL.
- 401, 404, and the `/session` -> `/gameflow-phase` fallback are unambiguous.
- No redirect or oversized-response behavior can leak past the app's guards.

### Negative
- `--gameflow-phase-only` cannot drive the full trigger because it has no queue data. This is a
  deliberate limitation of that mode, not a defect.
- One request per connection costs a TLS handshake per poll. At 700 ms polling on loopback this is
  negligible.

### Risks
- A future app change to another endpoint or shape would break the mock; the in-process test against
  the real client functions is the guard.
- Error-body choices are deliberately minimal; if a future app version starts reading error bodies,
  the contract must be revisited.

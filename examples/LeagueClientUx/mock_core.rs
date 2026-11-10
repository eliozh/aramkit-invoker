//! Shared, self-contained mock of the League client (LCU) API.
//!
//! This file is compiled twice: as the core of the `LeagueClientUx` example
//! and as a `#[cfg(test)]` module of the library crate. It therefore uses only
//! the standard library, the four pinned dev-dependencies, and `serde_json`.
//! It must never reference `crate::`, and no release build compiles it.
//!
//! The server terminates TLS with the committed test-only fixture, binds
//! `127.0.0.1` only, requires `Basic riot:<token>` for every request before
//! routing, and serves exactly the four read-only endpoints the app uses.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use httparse::Request as HttpRequest;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{json, Value};

const CERT_DER: &[u8] = include_bytes!("testdata/cert.der");
const KEY_DER: &[u8] = include_bytes!("testdata/key.der");

/// Request-head cap from ADR-0006; larger heads get `400`.
const MAX_HEAD_BYTES: usize = 8 * 1024;
/// Read and write timeout for one connection.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Total wall-clock budget for reading one request head. The per-read timeout
/// is an additional bound; together they stop a dripper from holding the
/// single-threaded accept loop indefinitely (ADR-0007 slowloris rule).
const HEAD_READ_DEADLINE: Duration = Duration::from_secs(5);
/// Poll interval while the non-blocking listener waits for a connection.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);

const SESSION_PATH: &str = "/lol-gameflow/v1/session";
const PHASE_PATH: &str = "/lol-gameflow/v1/gameflow-phase";
const CHAMP_SELECT_PATH: &str = "/lol-champ-select/v1/session";
const CHAMPION_SUMMARY_PATH: &str = "/lol-game-data/assets/v1/champion-summary.json";

const GAME_ID: i64 = 424_242;
const MAYHEM_QUEUE_ID: i64 = 2400;
const MAYHEM_GAME_MODE: &str = "KIWI";
const DEFAULT_CHAMPION_ID: i64 = 103;
const DEFAULT_LOCAL_CELL_ID: i64 = 1;
const DEFAULT_PHASE: &str = "Lobby";

/// (id, name, alias) entries shipped in every champion summary.
const FIXTURE_CHAMPIONS: [(i64, &str, &str); 3] = [
    (103, "Ahri", "Ahri"),
    (121, "Kha'Zix", "Khazix"),
    (62, "Wukong", "MonkeyKing"),
];

/// Mutable scenario state shared between the test handle and the server.
#[derive(Debug, Clone)]
pub struct MockState {
    pub phase: String,
    pub game_id: i64,
    pub queue_id: i64,
    pub game_mode: String,
    pub champion_id: i64,
    pub local_cell_id: i64,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            phase: DEFAULT_PHASE.to_owned(),
            game_id: GAME_ID,
            queue_id: MAYHEM_QUEUE_ID,
            game_mode: MAYHEM_GAME_MODE.to_owned(),
            champion_id: DEFAULT_CHAMPION_ID,
            local_cell_id: DEFAULT_LOCAL_CELL_ID,
        }
    }
}

/// Behavior switches that do not belong to the scenario state.
#[derive(Debug, Clone, Default)]
pub struct MockOptions {
    /// Return `401` for every request, including a correct token.
    pub reject_auth: bool,
    /// Return `404` from `/lol-gameflow/v1/session` so the app exercises the
    /// `/lol-gameflow/v1/gameflow-phase` fallback.
    pub gameflow_phase_only: bool,
}

/// A running loopback HTTPS mock server. Dropping it stops the thread.
pub struct MockServer {
    address: SocketAddr,
    token: String,
    state: Arc<RwLock<MockState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    /// Binds `127.0.0.1:0`, generates a per-run token, and starts serving.
    pub fn start(options: MockOptions) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let token = generate_token()?;
        let expected_credential = STANDARD.encode(format!("riot:{token}"));

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|error| std::io::Error::other(error.to_string()))?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(CERT_DER)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY_DER)),
            )
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let config = Arc::new(config);

        let state = Arc::new(RwLock::new(MockState::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("mock-lcu-core".to_owned())
            .spawn(move || {
                serve(
                    &listener,
                    &config,
                    &thread_state,
                    &options,
                    &expected_credential,
                    &thread_stop,
                );
            })?;

        Ok(Self {
            address,
            token,
            state,
            stop,
            thread: Some(thread),
        })
    }

    /// The address the listener actually bound (always `127.0.0.1`).
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// The shared scenario state, mirroring the handle the CLI exposes.
    pub fn state(&self) -> Arc<RwLock<MockState>> {
        Arc::clone(&self.state)
    }

    /// The command line a real client would advertise, for `parse_lcu_args`.
    pub fn argv(&self) -> Vec<String> {
        vec![
            "LeagueClientUx.exe".to_owned(),
            format!("--app-port={}", self.address.port()),
            format!("--remoting-auth-token={}", self.token),
        ]
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Generates the per-run remoting token: 32 OS-random bytes, base64 URL-safe
/// without padding (43 characters, a subset of the app's accepted charset).
pub fn generate_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// One connection at a time, matching the sequential app watcher.
fn serve(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
    state: &Arc<RwLock<MockState>>,
    options: &MockOptions,
    expected_credential: &str,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                // A socket we cannot configure (blocking mode and both
                // timeouts) has no slowloris bound; close it instead of
                // serving it without those guards.
                if configure_stream(&stream).is_err() {
                    continue;
                }

                let Ok(connection) = ServerConnection::new(Arc::clone(config)) else {
                    continue;
                };
                let mut tls = StreamOwned::new(connection, stream);
                handle_connection(&mut tls, state, options, expected_credential);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(_) => thread::sleep(ACCEPT_POLL_INTERVAL),
        }
    }
}

/// Applies the blocking mode and both I/O timeouts. Returns the first error so
/// the caller can decline to serve a socket without its slowloris bounds.
fn configure_stream(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(())
}

fn handle_connection(
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
    state: &Arc<RwLock<MockState>>,
    options: &MockOptions,
    expected_credential: &str,
) {
    let response = match read_request_head(stream) {
        Ok(Some(head)) => respond(&head, state, options, expected_credential),
        Ok(None) => MockResponse::empty(400),
        // TLS handshake and socket errors close without an HTTP response.
        Err(_) => return,
    };
    // Best-effort write: the connection closes right after, so a write error
    // only truncates the response and the socket drop ends the connection.
    let _ = write_response(stream, &response);
}

/// Reads one request head with two bounds: `MAX_HEAD_BYTES` and a total
/// `HEAD_READ_DEADLINE`. Returns `Ok(None)` when either bound is exceeded so
/// the caller answers `400` and closes.
fn read_request_head(stream: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
    let deadline = Instant::now() + HEAD_READ_DEADLINE;
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    loop {
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(Some(head));
        }
        if head.len() >= MAX_HEAD_BYTES {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }

        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        // Reject before appending: never let the head grow past the cap.
        if head.len() + read > MAX_HEAD_BYTES {
            return Ok(None);
        }
        head.extend_from_slice(&chunk[..read]);
    }
}

fn respond(
    head: &[u8],
    state: &Arc<RwLock<MockState>>,
    options: &MockOptions,
    expected_credential: &str,
) -> MockResponse {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = HttpRequest::new(&mut headers);

    match request.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return MockResponse::empty(400),
    }

    if request.version != Some(1) {
        return MockResponse::empty(400);
    }

    let (Some(method), Some(path)) = (request.method, request.path) else {
        return MockResponse::empty(400);
    };

    if has_body_or_transfer_encoding(&request) {
        return MockResponse::empty(400);
    }

    if !is_authorized(&request, options.reject_auth, expected_credential) {
        return MockResponse::unauthorized();
    }

    if method != "GET" {
        return MockResponse::method_not_allowed();
    }

    route(path, state, options)
}

fn has_body_or_transfer_encoding(request: &HttpRequest<'_, '_>) -> bool {
    request.headers.iter().any(|header| {
        if header.name.eq_ignore_ascii_case("transfer-encoding") {
            return true;
        }
        if header.name.eq_ignore_ascii_case("content-length") {
            let value = String::from_utf8_lossy(header.value);
            return value
                .trim()
                .parse::<u64>()
                .map_or(true, |length| length > 0);
        }
        false
    })
}

fn is_authorized(request: &HttpRequest<'_, '_>, reject_auth: bool, expected: &str) -> bool {
    if reject_auth {
        return false;
    }

    let mut values = request
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("authorization"));
    let Some(header) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }

    let Ok(value) = std::str::from_utf8(header.value) else {
        return false;
    };
    let Some((scheme, credential)) = value.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("Basic")
        && constant_time_eq(credential.as_bytes(), expected.as_bytes())
}

/// Compares credentials without an early exit on the first differing byte.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let difference = left
        .iter()
        .zip(right.iter())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right));
    // `black_box` keeps the compiler from folding or removing the OR
    // reduction, which is what makes this comparison constant time.
    std::hint::black_box(difference) == 0
}

fn route(path: &str, state: &Arc<RwLock<MockState>>, options: &MockOptions) -> MockResponse {
    let state = state
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();

    match path {
        SESSION_PATH if !options.gameflow_phase_only => {
            MockResponse::json(200, gameflow_body(&state))
        }
        SESSION_PATH => MockResponse::empty(404),
        PHASE_PATH => MockResponse::json(200, Value::String(state.phase.clone())),
        CHAMP_SELECT_PATH if state.phase == "ChampSelect" => {
            MockResponse::json(200, champ_select_body(&state))
        }
        CHAMP_SELECT_PATH => MockResponse::empty(404),
        CHAMPION_SUMMARY_PATH => MockResponse::json(200, champion_summary_body(&state)),
        _ => MockResponse::empty(404),
    }
}

fn gameflow_body(state: &MockState) -> Value {
    json!({
        "phase": &state.phase,
        "gameData": {
            "gameId": state.game_id,
            "queue": {
                "id": state.queue_id,
                "gameMode": &state.game_mode,
            },
        },
    })
}

fn champ_select_body(state: &MockState) -> Value {
    json!({
        "localPlayerCellId": state.local_cell_id,
        "myTeam": [{
            "cellId": state.local_cell_id,
            "championId": state.champion_id,
        }],
        "actions": [[{
            "actorCellId": state.local_cell_id,
            "championId": state.champion_id,
            "completed": true,
            "type": "pick",
        }]],
    })
}

fn champion_summary_body(state: &MockState) -> Value {
    let mut entries: Vec<Value> = FIXTURE_CHAMPIONS
        .iter()
        .map(|&(id, name, alias)| json!({ "id": id, "name": name, "alias": alias }))
        .collect();

    if !FIXTURE_CHAMPIONS
        .iter()
        .any(|(id, _, _)| *id == state.champion_id)
    {
        entries.push(json!({
            "id": state.champion_id,
            "name": format!("Mock Champion {}", state.champion_id),
            "alias": format!("MockChampion{}", state.champion_id),
        }));
    }

    Value::Array(entries)
}

struct MockResponse {
    status: u16,
    reason: &'static str,
    extra_headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl MockResponse {
    fn empty(status: u16) -> Self {
        Self {
            status,
            reason: reason_phrase(status),
            extra_headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn json(status: u16, value: Value) -> Self {
        match serde_json::to_vec(&value) {
            Ok(body) => Self {
                status,
                reason: reason_phrase(status),
                extra_headers: vec![
                    ("Content-Type", "application/json".to_owned()),
                    ("Cache-Control", "no-store".to_owned()),
                ],
                body,
            },
            Err(_) => Self::empty(500),
        }
    }

    fn unauthorized() -> Self {
        Self {
            status: 401,
            reason: reason_phrase(401),
            extra_headers: vec![("WWW-Authenticate", "Basic realm=\"mock-lcu\"".to_owned())],
            body: Vec::new(),
        }
    }

    fn method_not_allowed() -> Self {
        Self {
            status: 405,
            reason: reason_phrase(405),
            extra_headers: vec![("Allow", "GET".to_owned())],
            body: Vec::new(),
        }
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    }
}

fn write_response(
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
    response: &MockResponse,
) -> std::io::Result<()> {
    let mut output = Vec::with_capacity(response.body.len() + 160);
    output.extend_from_slice(
        format!("HTTP/1.1 {} {}\r\n", response.status, response.reason).as_bytes(),
    );
    output.extend_from_slice(format!("Content-Length: {}\r\n", response.body.len()).as_bytes());
    output.extend_from_slice(b"Connection: close\r\n");
    for (name, value) in &response.extra_headers {
        output.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    output.extend_from_slice(b"\r\n");
    output.extend_from_slice(&response.body);

    // Best-effort on close: the caller discards any error from this write.
    stream.write_all(&output)?;
    stream.flush()
}

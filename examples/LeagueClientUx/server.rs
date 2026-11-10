//! Loopback HTTPS server, request parsing, authentication, and routing.
//!
//! Binds `127.0.0.1` only, requires `Basic riot:<token>` for every request
//! before routing, and serves exactly the four read-only endpoints the app
//! uses (ADR-0006/0007).

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
use serde_json::Value;

use super::cli::CliError;
use super::fixtures::{
    champ_select_body, champion_summary_body, gameflow_body, MockOptions, MockState,
};
use super::schedule::ScheduledState;

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
/// Stop serving after this many consecutive accept failures; a persistent
/// listener error is not recoverable by polling, and retrying forever would
/// spin the CPU and flood stderr.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 100;

const SESSION_PATH: &str = "/lol-gameflow/v1/session";
const PHASE_PATH: &str = "/lol-gameflow/v1/gameflow-phase";
const CHAMP_SELECT_PATH: &str = "/lol-champ-select/v1/session";
const CHAMPION_SUMMARY_PATH: &str = "/lol-game-data/assets/v1/champion-summary.json";

/// Process-wide shutdown flag, set by the signal handlers. It is a static so
/// the handler can reach it without capturing: signal handlers must be plain
/// functions and may only perform async-signal-safe work.
///
/// The signal path is exercised only by the runnable example; the in-crate
/// test build just drives [`run_until_stopped`] with an injected flag.
#[allow(dead_code)]
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Poll interval for [`run_until_stopped`]; short enough for a prompt stop.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Signal handler: an atomic store is async-signal-safe. No allocation, I/O,
/// locking, or formatting happens here.
#[allow(dead_code)]
extern "C" fn request_shutdown(_signal: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// The process-wide flag the SIGINT/SIGTERM handlers set.
#[allow(dead_code)]
pub fn shutdown_flag() -> &'static AtomicBool {
    &SHUTDOWN_REQUESTED
}

/// Installs the graceful-stop handlers: SIGINT everywhere (Windows maps it to
/// the CRT's Ctrl+C handling) and SIGTERM, SIGHUP, and SIGQUIT on Unix.
///
/// Returns an error when the OS refuses a registration, so the caller can fail
/// fast rather than run without the cleanup path.
#[allow(dead_code)]
pub fn install_shutdown_handlers() -> Result<(), CliError> {
    install_signal(libc::SIGINT)?;
    #[cfg(unix)]
    {
        install_signal(libc::SIGTERM)?;
        install_signal(libc::SIGHUP)?;
        install_signal(libc::SIGQUIT)?;
    }
    Ok(())
}

/// Registers [`request_shutdown`] for one signal and checks the result.
fn install_signal(signal: libc::c_int) -> Result<(), CliError> {
    // SAFETY: `request_shutdown` has the `sighandler_t`-compatible signature
    // and its body is a single atomic store, which is async-signal-safe. The
    // returned previous handler is compared, not called.
    let previous =
        unsafe { libc::signal(signal, request_shutdown as *const () as libc::sighandler_t) };
    // `SIG_ERR` is a `c_int` (-1) on Windows but a `sighandler_t` on Unix, so
    // compare through the handler type to keep this portable.
    if previous == libc::SIG_ERR as libc::sighandler_t {
        let error = std::io::Error::last_os_error();
        return Err(CliError::runtime(format!(
            "无法为信号 {signal} 注册清理处理器：{error}"
        )));
    }
    Ok(())
}

/// Blocks until `flag` is set, then runs `on_stop` exactly once.
///
/// The flag is injected so the loop is testable; the binary passes
/// [`shutdown_flag`]. Cleanup belongs in `on_stop`, which keeps the ordering
/// (stop the server, then remove discovery files) explicit.
pub fn run_until_stopped<F: FnOnce()>(flag: &AtomicBool, on_stop: F) {
    while !flag.load(Ordering::SeqCst) {
        thread::sleep(SHUTDOWN_POLL_INTERVAL);
    }
    on_stop();
}

/// A running loopback HTTPS mock server. Dropping it stops the thread.
pub struct MockServer {
    address: SocketAddr,
    /// Read by `argv`; the example advertises the pair through its real argv.
    #[allow(dead_code)]
    token: String,
    /// Mutated by in-process tests; the CLI uses a schedule instead.
    #[allow(dead_code)]
    state: Arc<RwLock<MockState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    /// Binds `127.0.0.1:0`, generates a per-run token, and starts serving.
    #[allow(dead_code)]
    pub fn start(options: MockOptions) -> std::io::Result<Self> {
        Self::start_on(0, None, options, None)
    }

    /// Binds `127.0.0.1:<port>` (`0` = ephemeral) with an optional token and
    /// schedule. A `None` token is generated; a `Some` schedule is evaluated
    /// per request from the start instant, otherwise the state handle is read.
    pub fn start_on(
        port: u16,
        token: Option<String>,
        options: MockOptions,
        schedule: Option<super::schedule::Schedule>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let token = match token {
            Some(token) => token,
            None => generate_token()?,
        };
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
        let scheduled = schedule.map(|schedule| ScheduledState {
            schedule,
            started: Instant::now(),
        });

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
                    scheduled.as_ref(),
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

    /// The shared static scenario state.
    ///
    /// A schedule takes precedence over this handle: when the server was
    /// started with `Some(schedule)`, every request is answered from the
    /// schedule and mutations to this handle have no effect. Without a
    /// schedule, this is the live state the server reads.
    #[allow(dead_code)]
    pub fn state(&self) -> Arc<RwLock<MockState>> {
        Arc::clone(&self.state)
    }

    /// The command line a real client would advertise, for `parse_lcu_args`.
    #[allow(dead_code)]
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
            if thread.join().is_err() {
                // The panic payload is discarded so nothing it carried (for
                // example a credential) can reach stderr.
                eprintln!("mock-lcu: server thread panicked during shutdown");
            }
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
#[allow(clippy::too_many_arguments)]
fn serve(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
    state: &Arc<RwLock<MockState>>,
    options: &MockOptions,
    expected_credential: &str,
    stop: &AtomicBool,
    scheduled: Option<&ScheduledState>,
) {
    let mut consecutive_accept_errors = 0u32;
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                consecutive_accept_errors = 0;
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
                handle_connection(&mut tls, state, options, expected_credential, scheduled);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                // No pending connection: not an error, and it ends any burst.
                consecutive_accept_errors = 0;
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) => {
                // Log once per burst, then give up after the bounded count.
                if consecutive_accept_errors == 0 {
                    eprintln!("mock-lcu: accept 失败：{error}");
                }
                consecutive_accept_errors = consecutive_accept_errors.saturating_add(1);
                if consecutive_accept_errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                    eprintln!("mock-lcu: accept 连续失败 {consecutive_accept_errors} 次，停止监听");
                    break;
                }
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
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
    scheduled: Option<&ScheduledState>,
) {
    let response = match read_request_head(stream) {
        Ok(Some(head)) => respond(
            &head,
            &resolved_state(state, scheduled),
            options,
            expected_credential,
        ),
        Ok(None) => MockResponse::empty(400),
        // TLS handshake and socket errors close without an HTTP response.
        Err(_) => return,
    };
    // Best-effort write: the connection closes right after, so a write error
    // only truncates the response and the socket drop ends the connection.
    let _ = write_response(stream, &response);
}

/// A scheduled server evaluates the timeline; a static server reads its handle.
fn resolved_state(state: &Arc<RwLock<MockState>>, scheduled: Option<&ScheduledState>) -> MockState {
    if let Some(scheduled) = scheduled {
        return scheduled.schedule.state_at(scheduled.started.elapsed());
    }
    state
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
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
    state: &MockState,
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

fn route(path: &str, state: &MockState, options: &MockOptions) -> MockResponse {
    match path {
        SESSION_PATH if !options.gameflow_phase_only => {
            MockResponse::json(200, gameflow_body(state))
        }
        SESSION_PATH => MockResponse::empty(404),
        PHASE_PATH => MockResponse::json(200, Value::String(state.phase.clone())),
        CHAMP_SELECT_PATH if state.phase == "ChampSelect" => {
            MockResponse::json(200, champ_select_body(state))
        }
        CHAMP_SELECT_PATH => MockResponse::empty(404),
        CHAMPION_SUMMARY_PATH => MockResponse::json(200, champion_summary_body(state)),
        _ => MockResponse::empty(404),
    }
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

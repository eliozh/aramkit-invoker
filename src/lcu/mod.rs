mod client;
mod creds;
mod model;
mod watcher;

pub use creds::LcuCredentials;
pub use watcher::WatcherHandle;

/// Status snapshot sent from the background LCU watcher to the UI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    pub connected: bool,
    pub phase: String,
    pub queue_id: i64,
    pub champion_id: i64,
    pub champion_name: String,
    pub message: String,
}

impl Status {
    pub fn disconnected(message: impl Into<String>) -> Self {
        Self {
            connected: false,
            message: message.into(),
            ..Self::default()
        }
    }
}

/// The ARAMKit page that was opened after a champion was finalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChampionPage {
    pub champion_id: i64,
    pub champion_name: String,
    pub url: String,
    pub error: Option<String>,
}

/// Events produced by the LCU watcher thread.
#[derive(Debug, Clone)]
pub enum LcuEvent {
    Status(Status),
    ChampionOpened(ChampionPage),
    Log(String),
}

/// Test-only seam for the shared mock core.
///
/// These two `#[cfg(test)]` declarations are the only `src/` reference to the
/// mock. Neither is compiled for release builds, and the release binary and
/// `cargo tree -e normal` never see the mock's server dependencies.
#[cfg(test)]
#[path = "../../examples/LeagueClientUx/mock_core.rs"]
mod mock_core;

/// In-process end-to-end tests against the shared mock core.
#[cfg(test)]
mod mock_lcu_tests {
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use base64::Engine;
    use reqwest::blocking::{Client, Response};
    use reqwest::header::{ACCEPT, AUTHORIZATION, LOCATION, WWW_AUTHENTICATE};
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{
        ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned,
    };

    use super::client::{build_client, fetch_champ_select, fetch_champion_catalog, fetch_gameflow};
    use super::creds::{parse_lcu_args, parse_lockfile, LcuCredentials};
    use super::mock_core::{
        generate_token, resolve_launch, run_until_stopped, write_lockfile, write_log, CliConfig,
        MockOptions, MockServer,
    };
    use super::model::{ChampSelectSession, GameflowSession};
    use crate::champion::{champion_url_for_alias, is_mayhem_queue};
    use crate::tracker::ChampionTracker;

    const SESSION_PATH: &str = "/lol-gameflow/v1/session";
    const PHASE_PATH: &str = "/lol-gameflow/v1/gameflow-phase";
    const CHAMP_SELECT_PATH: &str = "/lol-champ-select/v1/session";
    const CHAMPION_SUMMARY_PATH: &str = "/lol-game-data/assets/v1/champion-summary.json";

    fn start_server(options: MockOptions) -> (MockServer, LcuCredentials) {
        let server = MockServer::start(options).expect("mock server");
        let credentials = parse_lcu_args(server.argv()).expect("credentials from advertised argv");
        (server, credentials)
    }

    fn raw_get(client: &Client, credentials: &LcuCredentials, path: &str) -> Response {
        client
            .get(format!("{}{}", credentials.base_url(), path))
            .basic_auth("riot", Some(&credentials.token))
            .header(ACCEPT, "application/json")
            .send()
            .expect("request")
    }

    fn mayhem_of(session: &GameflowSession) -> Option<bool> {
        let queue = session.queue();
        if queue.id > 0 || is_mayhem_queue(queue.id, &queue.game_mode) {
            Some(is_mayhem_queue(queue.id, &queue.game_mode))
        } else {
            None
        }
    }

    fn non_loopback_address() -> Option<IpAddr> {
        // Connecting a UDP socket selects the outbound interface without
        // sending a packet. Skip the check when the host has no route.
        let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
        socket.connect(("192.0.2.1", 9)).ok()?;
        let address = socket.local_addr().ok()?.ip();
        (!address.is_unspecified() && !address.is_loopback()).then_some(address)
    }

    /// Accepts the committed loopback test certificate without verifying it,
    /// so raw TLS requests can be driven against the mock.
    #[derive(Debug)]
    struct AcceptAnyCertificate(Arc<CryptoProvider>);

    impl ServerCertVerifier for AcceptAnyCertificate {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    fn tls_client(port: u16) -> StreamOwned<ClientConnection, TcpStream> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .expect("supported TLS versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider)))
            .with_no_client_auth();
        let server_name = ServerName::try_from("127.0.0.1".to_owned()).expect("server name");
        let connection =
            ClientConnection::new(Arc::new(config), server_name).expect("client connection");
        let tcp = TcpStream::connect(("127.0.0.1", port)).expect("tls connect");
        StreamOwned::new(connection, tcp)
    }

    #[test]
    fn generated_token_uses_app_charset_and_is_not_empty() {
        let token = generate_token().expect("token");
        assert_eq!(token.len(), 43);
        assert!(token
            .chars()
            .all(|character| character.is_ascii_alphanumeric()
                || matches!(character, '-' | '_' | '=')));

        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&token)
            .expect("url-safe base64");
        assert_eq!(decoded.len(), 32);

        let other = generate_token().expect("token");
        assert_ne!(token, other, "tokens must be generated per run");
    }

    #[test]
    fn end_to_end_argv_auth_endpoints_and_aramkit_url() {
        let (server, credentials) = start_server(MockOptions::default());
        assert_eq!(credentials.port, server.local_addr().port());

        // AC-1: the advertised port accepts a raw loopback TCP connection.
        {
            let connection =
                TcpStream::connect(("127.0.0.1", credentials.port)).expect("raw loopback connect");
            drop(connection);
        }

        let client = build_client().expect("client");
        let state = server.state();
        state.write().expect("state").phase = "Lobby".to_owned();

        let lobby = fetch_gameflow(&client, &credentials).expect("lobby gameflow");
        assert_eq!(lobby.phase, "Lobby");
        assert_eq!(lobby.game_id(), 424_242);
        assert_eq!(lobby.queue().id, 2400);
        assert_eq!(lobby.queue().game_mode, "KIWI");
        assert!(fetch_champ_select(&client, &credentials)
            .expect("lobby champ select")
            .is_none());

        // The phase endpoint is available in normal mode too.
        let phase_response = raw_get(&client, &credentials, PHASE_PATH);
        assert_eq!(phase_response.status(), reqwest::StatusCode::OK);
        assert_eq!(phase_response.text().expect("phase body"), "\"Lobby\"");

        let catalog = fetch_champion_catalog(&client, &credentials).expect("catalog");
        assert_eq!(catalog.name_of(103), Some("Ahri"));

        let mut tracker = ChampionTracker::default();
        assert_eq!(
            tracker.on_phase("Lobby", lobby.game_id(), mayhem_of(&lobby)),
            None
        );

        state.write().expect("state").phase = "ChampSelect".to_owned();
        let champ_select_flow = fetch_gameflow(&client, &credentials).expect("champ select flow");
        assert_eq!(champ_select_flow.phase, "ChampSelect");
        let champ_select = fetch_champ_select(&client, &credentials)
            .expect("champ select")
            .expect("session");
        let champion_id = champ_select.local_champion_id().expect("local champion");
        assert_eq!(champion_id, 103);

        tracker.begin_champ_select(champ_select_flow.game_id(), mayhem_of(&champ_select_flow));
        tracker.update_champ_select(
            champ_select_flow.game_id(),
            mayhem_of(&champ_select_flow),
            Some(champion_id),
        );

        state.write().expect("state").phase = "GameStart".to_owned();
        let game_start = fetch_gameflow(&client, &credentials).expect("game start");
        assert_eq!(game_start.phase, "GameStart");
        assert_eq!(
            tracker.on_phase("GameStart", game_start.game_id(), mayhem_of(&game_start)),
            None
        );

        state.write().expect("state").phase = "InProgress".to_owned();
        let in_progress = fetch_gameflow(&client, &credentials).expect("in progress");
        assert_eq!(in_progress.phase, "InProgress");
        let trigger = tracker
            .on_phase("InProgress", in_progress.game_id(), mayhem_of(&in_progress))
            .expect("trigger");
        assert_eq!(trigger.game_id, 424_242);
        assert_eq!(trigger.champion_id, 103);
        assert_eq!(
            tracker.on_phase("InProgress", in_progress.game_id(), mayhem_of(&in_progress)),
            None,
            "the trigger must fire exactly once"
        );

        let alias = catalog
            .get(trigger.champion_id)
            .expect("champion info")
            .alias
            .clone();
        assert_eq!(
            champion_url_for_alias(&alias).as_deref(),
            Some("https://aramkit.cn/zh-CN/champions/ahri")
        );
    }

    #[test]
    fn missing_wrong_and_rejected_credentials_are_401() {
        let (_server, credentials) = start_server(MockOptions::default());
        let client = build_client().expect("client");
        let url = format!("{}{}", credentials.base_url(), SESSION_PATH);

        let assert_unauthorized = |response: Response| {
            assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
            let challenge = response
                .headers()
                .get(WWW_AUTHENTICATE)
                .expect("WWW-Authenticate")
                .to_str()
                .expect("ascii header");
            assert!(challenge.starts_with("Basic"), "{challenge}");
            assert!(response.bytes().expect("body").is_empty());
        };

        assert_unauthorized(client.get(&url).send().expect("missing authorization"));
        assert_unauthorized(
            client
                .get(&url)
                .basic_auth("riot", Some("wrong-password"))
                .send()
                .expect("wrong password"),
        );
        assert_unauthorized(
            client
                .get(&url)
                .basic_auth("not-riot", Some(&credentials.token))
                .send()
                .expect("wrong username"),
        );
        assert_unauthorized(
            client
                .get(&url)
                .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
                .send()
                .expect("wrong scheme"),
        );
        assert_unauthorized(
            client
                .get(&url)
                .basic_auth("riot", Some(""))
                .send()
                .expect("empty password"),
        );

        let ok = client
            .get(&url)
            .basic_auth("riot", Some(&credentials.token))
            .send()
            .expect("correct credentials");
        assert_eq!(ok.status(), reqwest::StatusCode::OK);

        let rejecting = MockServer::start(MockOptions {
            reject_auth: true,
            ..MockOptions::default()
        })
        .expect("rejecting server");
        let rejected_credentials = parse_lcu_args(rejecting.argv()).expect("rejecting credentials");
        let rejected_url = format!("{}{}", rejected_credentials.base_url(), SESSION_PATH);
        assert_unauthorized(
            client
                .get(&rejected_url)
                .basic_auth("riot", Some(&rejected_credentials.token))
                .send()
                .expect("rejected credentials"),
        );
    }

    #[test]
    fn fetch_gameflow_surfaces_reqwest_401() {
        let (_server, credentials) = start_server(MockOptions::default());
        let client = build_client().expect("client");
        let bad_credentials = LcuCredentials {
            port: credentials.port,
            token: "wrong-token".to_owned(),
        };

        let error = fetch_gameflow(&client, &bad_credentials).expect_err("401 is an error");
        let status = error.chain().find_map(|cause| {
            cause
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status)
        });
        assert_eq!(status, Some(reqwest::StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn gameflow_session_and_phase_only_shapes() {
        let (server, credentials) = start_server(MockOptions::default());
        let client = build_client().expect("client");
        {
            let state = server.state();
            let mut state = state.write().expect("state");
            state.phase = "ChampSelect".to_owned();
            state.queue_id = 2400;
            state.game_mode = "KIWI".to_owned();
        }

        let session = fetch_gameflow(&client, &credentials).expect("gameflow");
        assert_eq!(session.phase, "ChampSelect");
        assert_eq!(session.game_id(), 424_242);
        assert_eq!(session.queue().id, 2400);
        assert_eq!(session.queue().game_mode, "KIWI");

        let (phase_only, credentials) = start_server(MockOptions {
            gameflow_phase_only: true,
            ..MockOptions::default()
        });
        phase_only.state().write().expect("state").phase = "InProgress".to_owned();

        let response = raw_get(&client, &credentials, SESSION_PATH);
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(response.bytes().expect("body").is_empty());

        let phase_response = raw_get(&client, &credentials, PHASE_PATH);
        assert_eq!(phase_response.status(), reqwest::StatusCode::OK);
        assert_eq!(phase_response.text().expect("phase body"), "\"InProgress\"");

        let session = fetch_gameflow(&client, &credentials).expect("phase fallback");
        assert_eq!(session.phase, "InProgress");
        assert_eq!(session.game_id(), 0);
        assert_eq!(session.queue().id, 0);
        assert!(session.queue().game_mode.is_empty());
    }

    #[test]
    fn champ_select_shape_and_404_outside_champ_select() {
        let (server, credentials) = start_server(MockOptions::default());
        let client = build_client().expect("client");
        {
            let state = server.state();
            let mut state = state.write().expect("state");
            state.phase = "ChampSelect".to_owned();
            state.champion_id = 103;
            state.local_cell_id = 2;
        }

        let session = fetch_champ_select(&client, &credentials)
            .expect("champ select")
            .expect("session");
        assert_eq!(session.local_player_cell_id, Some(2));
        assert_eq!(session.local_champion_id(), Some(103));

        // The same body carries the completed-pick fallback action.
        let body = raw_get(&client, &credentials, CHAMP_SELECT_PATH)
            .text()
            .expect("body");
        let mut value: serde_json::Value = serde_json::from_str(&body).expect("json body");
        value["myTeam"][0]["championId"] = serde_json::json!(0);
        let fallback: ChampSelectSession = serde_json::from_value(value).expect("fallback session");
        assert_eq!(fallback.local_champion_id(), Some(103));

        server.state().write().expect("state").phase = "Lobby".to_owned();
        assert!(fetch_champ_select(&client, &credentials)
            .expect("champ select outside champ select")
            .is_none());
        let response = raw_get(&client, &credentials, CHAMP_SELECT_PATH);
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    }

    #[test]
    fn champion_summary_shapes_and_url_slugs() {
        let (server, credentials) = start_server(MockOptions::default());
        let client = build_client().expect("client");

        let catalog = fetch_champion_catalog(&client, &credentials).expect("catalog");
        assert_eq!(catalog.name_of(103), Some("Ahri"));
        assert_eq!(catalog.name_of(121), Some("Kha'Zix"));
        assert_eq!(catalog.name_of(62), Some("Wukong"));

        let alias = |champion_id: i64| {
            catalog
                .get(champion_id)
                .expect("fixture entry")
                .alias
                .clone()
        };
        assert_eq!(
            champion_url_for_alias(&alias(103)).as_deref(),
            Some("https://aramkit.cn/zh-CN/champions/ahri")
        );
        assert_eq!(
            champion_url_for_alias(&alias(121)).as_deref(),
            Some("https://aramkit.cn/zh-CN/champions/khazix")
        );
        assert_eq!(
            champion_url_for_alias(&alias(62)).as_deref(),
            Some("https://aramkit.cn/zh-CN/champions/monkeyking")
        );

        server.state().write().expect("state").champion_id = 777;
        let catalog = fetch_champion_catalog(&client, &credentials).expect("catalog with append");
        assert_eq!(catalog.name_of(777), Some("Mock Champion 777"));
        assert_eq!(
            champion_url_for_alias(&catalog.get(777).expect("appended entry").alias).as_deref(),
            Some("https://aramkit.cn/zh-CN/champions/mockchampion777")
        );
    }

    #[test]
    fn server_binds_127_0_0_1_only() {
        let (server, _credentials) = start_server(MockOptions::default());
        let address = server.local_addr();
        assert_eq!(address.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(!address.ip().is_unspecified());
        assert_ne!(address.port(), 0);

        if let Some(non_loopback) = non_loopback_address() {
            let destination = SocketAddr::new(non_loopback, address.port());
            let result = TcpStream::connect_timeout(&destination, Duration::from_millis(500));
            assert!(
                result.is_err(),
                "the mock accepted a non-loopback connection via {non_loopback}"
            );
        }
    }

    #[test]
    fn no_redirects_and_champion_summary_is_bounded() {
        let (_server, credentials) = start_server(MockOptions::default());
        let client = build_client().expect("client");
        let base_url = credentials.base_url();

        let mut responses = Vec::new();
        for path in [
            SESSION_PATH,
            PHASE_PATH,
            CHAMP_SELECT_PATH,
            CHAMPION_SUMMARY_PATH,
            "/unknown-path",
            "/",
        ] {
            responses.push(raw_get(&client, &credentials, path));
        }
        responses.push(
            client
                .get(format!("{base_url}{SESSION_PATH}"))
                .send()
                .expect("no auth"),
        );
        responses.push(
            client
                .post(format!("{base_url}{SESSION_PATH}"))
                .basic_auth("riot", Some(&credentials.token))
                .send()
                .expect("non-GET"),
        );
        responses.push(
            client
                .post(format!("{base_url}{SESSION_PATH}"))
                .basic_auth("riot", Some(&credentials.token))
                .body("unexpected body")
                .send()
                .expect("request body"),
        );

        for response in responses {
            let status = response.status().as_u16();
            assert!(
                matches!(status, 200 | 400 | 401 | 404 | 405),
                "unexpected status {status}"
            );
            assert!(
                response.headers().get(LOCATION).is_none(),
                "no response may redirect"
            );
            let content_length = response
                .content_length()
                .expect("exact Content-Length header");
            let body = response.bytes().expect("body");
            assert_eq!(content_length, body.len() as u64);
        }

        let summary = raw_get(&client, &credentials, CHAMPION_SUMMARY_PATH);
        assert_eq!(summary.status(), reqwest::StatusCode::OK);
        assert!(summary.content_length().expect("Content-Length") < 4096);
        assert!(summary.bytes().expect("body").len() < 4096);
    }

    #[test]
    fn oversized_request_head_gets_400_without_panicking() {
        let (_server, credentials) = start_server(MockOptions::default());
        let mut tls = tls_client(credentials.port);

        // Build a valid request head well over the 8 KiB cap. Flush 8000
        // bytes first so the server drains them, then send the remainder.
        // The final read therefore crosses the cap, which must be rejected
        // before any of those bytes are appended to the head buffer.
        let prefix = "GET /lol-gameflow/v1/session HTTP/1.1\r\nHost: localhost\r\nX-Pad: ";
        let total = 8 * 1024 + 128;
        let padding = "a".repeat(total - prefix.len() - 4);
        let head = format!("{prefix}{padding}\r\n\r\n");
        assert!(head.len() > 8 * 1024);

        let bytes = head.as_bytes();
        let split = 8000;
        tls.write_all(&bytes[..split]).expect("first head chunk");
        tls.flush().expect("flush first head chunk");
        // Let the server consume the first chunk so the next read is partial.
        thread::sleep(Duration::from_millis(250));
        tls.write_all(&bytes[split..]).expect("second head chunk");
        tls.flush().expect("flush second head chunk");

        let mut response = Vec::new();
        // The server sends the response and closes; a missing TLS
        // close_notify is irrelevant to the assertion.
        let _ = tls.read_to_end(&mut response);
        let response_text = String::from_utf8_lossy(&response);
        assert!(
            response_text.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "an oversized request head must get 400, got: {response_text:?}"
        );
    }

    #[test]
    fn static_state_handle_pins_all_fields() {
        let (server, credentials) = start_server(MockOptions::default());
        let client = build_client().expect("client");
        {
            let state = server.state();
            let mut state = state.write().expect("state");
            state.phase = "ChampSelect".to_owned();
            state.queue_id = 450;
            state.game_mode = "ARAM".to_owned();
            state.champion_id = 121;
            state.local_cell_id = 2;
        }

        let session = fetch_gameflow(&client, &credentials).expect("gameflow");
        assert_eq!(session.phase, "ChampSelect");
        assert_eq!(session.queue().id, 450);
        assert_eq!(session.queue().game_mode, "ARAM");

        let champ_select = fetch_champ_select(&client, &credentials)
            .expect("champ select")
            .expect("session");
        assert_eq!(champ_select.local_player_cell_id, Some(2));
        assert_eq!(champ_select.local_champion_id(), Some(121));

        let catalog = fetch_champion_catalog(&client, &credentials).expect("catalog");
        assert_eq!(catalog.name_of(121), Some("Kha'Zix"));
    }

    // --- Slice 02: CLI parsing, re-exec resolution, and scripted timelines ---

    fn parse_config(args: &[&str]) -> Result<CliConfig, super::mock_core::CliError> {
        CliConfig::parse_from(args.iter().copied())
    }

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn mayhem_for_state(state: &super::mock_core::MockState) -> Option<bool> {
        if state.queue_id > 0 || is_mayhem_queue(state.queue_id, &state.game_mode) {
            Some(is_mayhem_queue(state.queue_id, &state.game_mode))
        } else {
            None
        }
    }

    #[test]
    fn cli_rejects_unknown_flags_and_invalid_values() {
        let error = parse_config(&["LeagueClientUx", "--nope"]).expect_err("unknown flag");
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("--nope"), "{error}");

        // Port: 0 selects an ephemeral port, anything else must be a valid u16.
        for port in ["65536", "-1", "abc"] {
            assert!(
                parse_config(&["LeagueClientUx", "--app-port", port]).is_err(),
                "port {port} must be rejected"
            );
        }
        assert!(parse_config(&["LeagueClientUx", "--app-port", "0"]).is_ok());
        assert!(parse_config(&["LeagueClientUx", "--app-port=54321"]).is_ok());

        // Token: non-empty, at most 256 bytes, charset [A-Za-z0-9_=-].
        assert!(parse_config(&["LeagueClientUx", "--remoting-auth-token", ""]).is_err());
        let long = "a".repeat(257);
        assert!(parse_config(&["LeagueClientUx", "--remoting-auth-token", long.as_str()]).is_err());
        assert!(parse_config(&["LeagueClientUx", "--remoting-auth-token", "bad token!"]).is_err());
        assert!(parse_config(&["LeagueClientUx", "--remoting-auth-token", "bad/token"]).is_err());
        let maximum = "a".repeat(256);
        assert!(
            parse_config(&["LeagueClientUx", "--remoting-auth-token", maximum.as_str()]).is_ok()
        );
        assert!(parse_config(&["LeagueClientUx", "--remoting-auth-token", "Good_-Token="]).is_ok());

        // Enumerated values and numeric IDs fail with a message naming the flag.
        for (flag, value) in [
            ("--mode", "bogus"),
            ("--scenario", "bogus"),
            ("--phase", "Bogus"),
            ("--game-mode", ""),
            ("--game-mode", "bad mode"),
            ("--queue-id", "-1"),
            ("--champion-id", "0"),
            ("--local-cell-id", "-2"),
            ("--step-ms", "0"),
            ("--step-ms", "600001"),
        ] {
            let error = parse_config(&["LeagueClientUx", flag, value]).expect_err(flag);
            assert!(error.to_string().contains(flag), "{flag}: {}", error);
        }
    }

    #[test]
    fn phase_and_explicit_scenario_are_mutually_exclusive() {
        let error = parse_config(&[
            "LeagueClientUx",
            "--phase",
            "ChampSelect",
            "--scenario",
            "dodge",
        ])
        .expect_err("phase plus explicit scenario");
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("--phase"), "{error}");

        let config = parse_config(&["LeagueClientUx", "--phase", "ChampSelect"]).expect("static");
        let schedule = config.schedule();
        assert_eq!(schedule.steps.len(), 1);
        assert_eq!(schedule.steps[0].state.phase, "ChampSelect");
        assert_eq!(
            schedule.state_at(Duration::from_secs(3_600)).phase,
            "ChampSelect",
            "static mode must hold forever"
        );
        assert!(
            schedule
                .steps
                .iter()
                .all(|step| step.state.phase == "ChampSelect"),
            "the default mayhem timeline must not be applied in phase mode"
        );
    }

    #[test]
    fn install_root_is_rejected_in_process_mode() {
        for argv in [
            owned(&["LeagueClientUx", "--install-root", "/tmp/mock-root"]),
            owned(&[
                "LeagueClientUx",
                "--mode",
                "process",
                "--install-root",
                "/tmp/mock-root",
            ]),
        ] {
            let error = CliConfig::parse_from(argv).expect_err("install-root in process mode");
            assert_eq!(error.exit_code(), 2);
            assert!(error.to_string().contains("--install-root"), "{error}");
        }

        assert!(parse_config(&[
            "LeagueClientUx",
            "--mode",
            "lockfile",
            "--install-root",
            "/tmp/mock-root",
        ])
        .is_ok());
        assert!(parse_config(&[
            "LeagueClientUx",
            "--mode",
            "log",
            "--install-root",
            "/tmp/mock-root",
        ])
        .is_ok());
    }

    #[test]
    fn token_redaction_hides_the_generated_token() {
        let token = generate_token().expect("token");
        let config = CliConfig::parse_from([
            "LeagueClientUx",
            "--app-port",
            "54321",
            "--remoting-auth-token",
            token.as_str(),
            "--scenario",
            "dodge",
        ])
        .expect("config");

        let summary = config.summary(54_321);
        assert!(summary.contains("process"), "{summary}");
        assert!(summary.contains("dodge"), "{summary}");
        assert!(summary.contains("54321"), "{summary}");
        assert!(summary.contains("<redacted>"), "{summary}");
        assert!(!summary.contains(&token), "the token must never be printed");

        let debug = format!("{config:?}");
        assert!(!debug.contains(&token), "Debug must be redacted");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn launch_resolution_debug_redacts_token_and_args() {
        let token = "debug-secret-token";
        let argv = owned(&[
            "LeagueClientUx",
            "--remoting-auth-token",
            token,
            "--app-port",
            "1234",
        ]);
        let resolution = resolve_launch(
            &argv,
            None,
            None,
            || Ok::<u16, std::io::Error>(54_321),
            || Ok::<String, std::io::Error>(token.to_owned()),
        )
        .expect("resolution");

        let debug = format!("{resolution:?}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(
            !debug.contains(token),
            "Debug must not print the token: {debug}"
        );
    }

    #[test]
    fn cli_rejects_duplicate_credential_flags() {
        for argv in [
            owned(&["LeagueClientUx", "--app-port", "1234", "--app-port=1235"]),
            owned(&["LeagueClientUx", "--app-port=1234", "--app-port", "1235"]),
            owned(&[
                "LeagueClientUx",
                "--remoting-auth-token",
                "first-token",
                "--remoting-auth-token=second-token",
            ]),
        ] {
            let error = CliConfig::parse_from(argv).expect_err("duplicate credential flag");
            assert_eq!(error.exit_code(), 2);
            assert!(
                error.to_string().contains("--app-port")
                    || error.to_string().contains("--remoting-auth-token"),
                "{error}"
            );
        }
    }

    #[test]
    fn cli_take_value_accepts_dash_prefixed_token_and_rejects_missing_value() {
        let config = parse_config(&["LeagueClientUx", "--remoting-auth-token", "--abc"])
            .expect("a token may begin with --");
        assert_eq!(config.remoting_auth_token.as_deref(), Some("--abc"));

        let missing = parse_config(&["LeagueClientUx", "--app-port", "--step-ms", "500"])
            .expect_err("missing value before a known flag");
        assert_eq!(missing.exit_code(), 2);
        assert!(missing.to_string().contains("--app-port"), "{missing}");
        assert!(missing.to_string().contains("缺少取值"), "{missing}");
    }

    #[test]
    fn cli_rejects_empty_install_root() {
        for argv in [
            owned(&["LeagueClientUx", "--mode", "lockfile", "--install-root="]),
            owned(&["LeagueClientUx", "--mode", "log", "--install-root", ""]),
        ] {
            let error = CliConfig::parse_from(argv).expect_err("empty install root");
            assert_eq!(error.exit_code(), 2);
            assert!(error.to_string().contains("--install-root"), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_arguments_are_rejected_with_a_usage_error() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let invalid = vec![
            OsString::from("LeagueClientUx"),
            OsString::from_vec(vec![0xff, 0xfe]),
        ];
        let error = super::mock_core::collect_utf8_args(invalid)
            .expect_err("non-UTF-8 arguments must be rejected");
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("UTF-8"), "{error}");
    }

    // Not `#[cfg(unix)]`: this keeps `collect_utf8_args` live in the test build
    // on Windows too, where the non-UTF-8 case above is not compiled.
    #[test]
    fn utf8_arguments_pass_through_unchanged() {
        use std::ffi::OsString;

        let parsed = super::mock_core::collect_utf8_args(vec![
            OsString::from("LeagueClientUx"),
            OsString::from("--app-port"),
            OsString::from("54321"),
        ])
        .expect("valid UTF-8 arguments");
        assert_eq!(parsed, vec!["LeagueClientUx", "--app-port", "54321"]);
    }

    #[test]
    fn resolve_launch_advertises_exactly_one_flag_pair() {
        let argv = owned(&["LeagueClientUx", "--step-ms", "500"]);
        let resolution = resolve_launch(
            &argv,
            None,
            None,
            || Ok::<u16, std::io::Error>(54_321),
            || Ok::<String, std::io::Error>("generated-token".to_owned()),
        )
        .expect("resolution");

        assert!(resolution.reexec);
        assert_eq!(resolution.port, 54_321);
        assert_eq!(resolution.token, "generated-token");

        let port_flags = resolution
            .args
            .iter()
            .filter(|argument| argument.starts_with("--app-port"))
            .count();
        let token_flags = resolution
            .args
            .iter()
            .filter(|argument| argument.starts_with("--remoting-auth-token"))
            .count();
        assert_eq!(port_flags, 1, "exactly one --app-port");
        assert_eq!(token_flags, 1, "exactly one --remoting-auth-token");
        assert!(resolution.args.contains(&"--app-port=54321".to_owned()));
        assert!(resolution
            .args
            .contains(&"--remoting-auth-token=generated-token".to_owned()));

        let credentials = parse_lcu_args(&resolution.args).expect("parseable pair");
        assert_eq!(credentials.port, 54_321);
        assert_eq!(credentials.token, "generated-token");
    }

    #[test]
    fn resolve_launch_strips_previous_flag_forms() {
        let argv = owned(&[
            "LeagueClientUx",
            "--app-port=1111",
            "--remoting-auth-token",
            "old-token",
            "--step-ms",
            "400",
            "--app-port",
            "2222",
            "--remoting-auth-token=fake",
        ]);

        let resolution = resolve_launch(
            &argv,
            None,
            None,
            || Ok::<u16, std::io::Error>(3333),
            || Ok::<String, std::io::Error>("new-token".to_owned()),
        )
        .expect("resolution");

        assert!(resolution.reexec);
        assert_eq!(
            resolution
                .args
                .iter()
                .filter(|argument| argument.starts_with("--app-port"))
                .count(),
            1
        );
        assert_eq!(
            resolution
                .args
                .iter()
                .filter(|argument| argument.starts_with("--remoting-auth-token"))
                .count(),
            1
        );
        assert!(resolution.args.contains(&"--app-port=3333".to_owned()));
        assert!(resolution.args.contains(&"--step-ms".to_owned()));
        assert!(resolution.args.contains(&"400".to_owned()));
        for stale in ["1111", "2222", "old-token", "fake"] {
            assert!(
                !resolution
                    .args
                    .iter()
                    .any(|argument| argument.contains(stale)),
                "stale argument {stale} must be stripped"
            );
        }

        // A separated value that merely begins with `--` is a value, exactly
        // as `take_value` treats it, so it must be stripped with its flag.
        let dash_argv = owned(&[
            "LeagueClientUx",
            "--remoting-auth-token",
            "--dash-prefixed-value",
            "--app-port",
            "4444",
        ]);
        let resolution = resolve_launch(
            &dash_argv,
            None,
            None,
            || Ok::<u16, std::io::Error>(3333),
            || Ok::<String, std::io::Error>("new-token".to_owned()),
        )
        .expect("resolution with a dash-prefixed value");

        assert!(
            !resolution
                .args
                .iter()
                .any(|argument| argument.contains("dash-prefixed-value")),
            "a dash-prefixed separated token must be stripped: {:?}",
            resolution.args
        );
        assert_eq!(
            resolution
                .args
                .iter()
                .filter(|argument| argument.starts_with("--app-port"))
                .count(),
            1
        );
    }

    #[test]
    fn resolve_launch_child_does_not_reexec() {
        let argv = owned(&[
            "LeagueClientUx",
            "--app-port=54321",
            "--remoting-auth-token=child-token",
        ]);
        let config = CliConfig::parse_from(&argv).expect("child config");

        let resolution = resolve_launch(
            &argv,
            config.app_port,
            config.remoting_auth_token.as_deref(),
            || -> std::io::Result<u16> { panic!("a child must not select a port") },
            || -> std::io::Result<String> { panic!("a child must not generate a token") },
        )
        .expect("resolution");

        assert!(!resolution.reexec, "the child must serve without re-exec");
        assert_eq!(resolution.port, 54_321);
        assert_eq!(resolution.token, "child-token");
    }

    #[test]
    fn mayhem_timeline_state_at_boundaries() {
        let config = parse_config(&[
            "LeagueClientUx",
            "--scenario",
            "mayhem",
            "--step-ms",
            "1000",
        ])
        .expect("mayhem config");
        let schedule = config.schedule();

        assert_eq!(schedule.state_at(Duration::ZERO).phase, "Lobby");
        assert_ne!(
            schedule.state_at(Duration::ZERO).phase,
            "InProgress",
            "the timeline must never start in InProgress"
        );
        assert_eq!(
            schedule.state_at(Duration::from_millis(1_999)).phase,
            "Lobby"
        );
        assert_eq!(
            schedule.state_at(Duration::from_secs(2)).phase,
            "ChampSelect"
        );
        assert_eq!(
            schedule.state_at(Duration::from_secs(13)).phase,
            "ChampSelect"
        );
        assert_eq!(
            schedule.state_at(Duration::from_secs(14)).phase,
            "GameStart"
        );
        assert_eq!(
            schedule.state_at(Duration::from_secs(17)).phase,
            "GameStart"
        );
        assert_eq!(
            schedule.state_at(Duration::from_secs(18)).phase,
            "InProgress"
        );
        assert_eq!(
            schedule.state_at(Duration::from_secs(120)).phase,
            "InProgress"
        );

        let game_start = schedule
            .steps
            .iter()
            .find(|step| step.state.phase == "GameStart")
            .expect("GameStart step");
        assert!(
            game_start.dwell_ms > 2_000 + 700,
            "GameStart must outlast the 2 s grace plus one 700 ms poll"
        );
    }

    #[test]
    fn failed_scenarios_end_in_no_trigger_phases() {
        for (name, ending_phase, queue_id, game_mode) in [
            ("non-mayhem", "InProgress", 450, "ARAM"),
            ("dodge", "Lobby", 2400, "KIWI"),
            ("failed-to-launch", "FailedToLaunch", 2400, "KIWI"),
        ] {
            let config = parse_config(&["LeagueClientUx", "--scenario", name]).expect(name);
            let schedule = config.schedule();
            let last = schedule.state_at(Duration::from_secs(3_600));
            assert_eq!(last.phase, ending_phase, "{name}");
            assert_eq!(last.queue_id, queue_id, "{name}");
            assert_eq!(last.game_mode, game_mode, "{name}");

            let mut tracker = ChampionTracker::default();
            for step in &schedule.steps {
                let mayhem = mayhem_for_state(&step.state);
                if step.state.phase == "ChampSelect" {
                    tracker.begin_champ_select(step.state.game_id, mayhem);
                    tracker.update_champ_select(
                        step.state.game_id,
                        mayhem,
                        Some(step.state.champion_id),
                    );
                    continue;
                }
                assert!(
                    tracker
                        .on_phase(&step.state.phase, step.state.game_id, mayhem)
                        .is_none(),
                    "{name} must not trigger at {}",
                    step.state.phase
                );
                assert!(
                    tracker.fire_if_ready(step.state.game_id).is_none(),
                    "{name} must not fire a pending pick at {}",
                    step.state.phase
                );
            }
        }
    }

    #[test]
    fn static_flags_pin_phase_queue_champion_and_cell() {
        let config = parse_config(&[
            "LeagueClientUx",
            "--phase",
            "ChampSelect",
            "--queue-id",
            "2400",
            "--game-mode",
            "KIWI",
            "--champion-id",
            "103",
            "--local-cell-id",
            "2",
        ])
        .expect("static flags");

        let server = MockServer::start_on(0, None, config.mock_options(), Some(config.schedule()))
            .expect("server");
        let credentials = parse_lcu_args(server.argv()).expect("credentials");
        let client = build_client().expect("client");

        let session = fetch_gameflow(&client, &credentials).expect("gameflow");
        assert_eq!(session.phase, "ChampSelect");
        assert_eq!(session.queue().id, 2400);
        assert_eq!(session.queue().game_mode, "KIWI");

        let champ_select = fetch_champ_select(&client, &credentials)
            .expect("champ select")
            .expect("session");
        assert_eq!(champ_select.local_player_cell_id, Some(2));
        assert_eq!(champ_select.local_champion_id(), Some(103));

        let catalog = fetch_champion_catalog(&client, &credentials).expect("catalog");
        assert_eq!(catalog.name_of(103), Some("Ahri"));
    }

    #[test]
    fn mayhem_path_triggers_exactly_once_with_expected_url() {
        let config = parse_config(&["LeagueClientUx", "--scenario", "mayhem"]).expect("mayhem");
        let schedule = config.schedule();
        let (server, credentials) = start_server(MockOptions::default());
        let client = build_client().expect("client");
        let mut tracker = ChampionTracker::default();
        let mut triggers = Vec::new();

        for step in &schedule.steps {
            {
                let state = server.state();
                *state.write().expect("state") = step.state.clone();
            }
            let session = fetch_gameflow(&client, &credentials).expect("gameflow");
            let mayhem = mayhem_of(&session);

            if session.phase == "ChampSelect" {
                let champ_select = fetch_champ_select(&client, &credentials)
                    .expect("champ select")
                    .expect("session");
                tracker.begin_champ_select(session.game_id(), mayhem);
                tracker.update_champ_select(
                    session.game_id(),
                    mayhem,
                    champ_select.local_champion_id(),
                );
            } else if let Some(trigger) =
                tracker.on_phase(&session.phase, session.game_id(), mayhem)
            {
                triggers.push(trigger);
            }
        }

        assert_eq!(triggers.len(), 1, "exactly one trigger");
        assert_eq!(triggers[0].game_id, 424_242);
        assert_eq!(triggers[0].champion_id, 103);

        let catalog = fetch_champion_catalog(&client, &credentials).expect("catalog");
        let alias = catalog.get(103).expect("champion").alias.clone();
        assert_eq!(
            champion_url_for_alias(&alias).as_deref(),
            Some("https://aramkit.cn/zh-CN/champions/ahri")
        );
    }

    #[test]
    fn non_mayhem_dodge_and_failed_to_launch_do_not_trigger() {
        for name in ["non-mayhem", "dodge", "failed-to-launch"] {
            let config = parse_config(&["LeagueClientUx", "--scenario", name]).expect(name);
            let schedule = config.schedule();
            let (server, credentials) = start_server(MockOptions::default());
            let client = build_client().expect("client");
            let mut tracker = ChampionTracker::default();
            let mut triggered = false;

            for step in &schedule.steps {
                {
                    let state = server.state();
                    *state.write().expect("state") = step.state.clone();
                }
                let session = fetch_gameflow(&client, &credentials).expect("gameflow");
                let mayhem = mayhem_of(&session);

                if session.phase == "ChampSelect" {
                    let champ_select = fetch_champ_select(&client, &credentials)
                        .expect("champ select")
                        .expect("session");
                    tracker.begin_champ_select(session.game_id(), mayhem);
                    tracker.update_champ_select(
                        session.game_id(),
                        mayhem,
                        champ_select.local_champion_id(),
                    );
                } else {
                    triggered |= tracker
                        .on_phase(&session.phase, session.game_id(), mayhem)
                        .is_some();
                    triggered |= tracker.fire_if_ready(session.game_id()).is_some();
                }
            }

            assert!(!triggered, "{name} must not produce a trigger");
        }
    }

    #[test]
    fn phase_only_mode_returns_404_then_phase_without_trigger() {
        let config = parse_config(&[
            "LeagueClientUx",
            "--phase",
            "InProgress",
            "--gameflow-phase-only",
        ])
        .expect("phase-only config");
        let server = MockServer::start_on(0, None, config.mock_options(), Some(config.schedule()))
            .expect("server");
        let credentials = parse_lcu_args(server.argv()).expect("credentials");
        let client = build_client().expect("client");

        let response = raw_get(&client, &credentials, SESSION_PATH);
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(response.bytes().expect("body").is_empty());

        let phase_response = raw_get(&client, &credentials, PHASE_PATH);
        assert_eq!(phase_response.status(), reqwest::StatusCode::OK);
        assert_eq!(phase_response.text().expect("phase body"), "\"InProgress\"");

        let session = fetch_gameflow(&client, &credentials).expect("phase fallback");
        assert_eq!(session.phase, "InProgress");
        assert_eq!(session.game_id(), 0);
        assert_eq!(session.queue().id, 0);
        assert_eq!(session.queue().game_mode, "");

        let mut tracker = ChampionTracker::default();
        tracker.begin_champ_select(0, None);
        tracker.update_champ_select(0, None, Some(103));
        assert_eq!(
            tracker.on_phase(&session.phase, session.game_id(), mayhem_of(&session)),
            None,
            "phase-only mode has no queue data, so no trigger"
        );
        assert_eq!(tracker.fire_if_ready(session.game_id()), None);
    }

    // --- Slice 03: lockfile/log discovery writers and path validation ---

    #[test]
    fn lockfile_writer_emits_parseable_line() {
        let root = tempfile::tempdir().expect("temp dir");
        let port = 54_321u16;
        let token = generate_token().expect("token");

        // The writer returns paths under its canonicalized root. On Windows
        // that is a verbatim `\\?\` path with the 8.3 short name resolved, so
        // the expected path must be built from the canonical root as well.
        let canonical_root = std::fs::canonicalize(root.path()).expect("canonical root");
        let discovery = write_lockfile(root.path(), port, &token).expect("lockfile");
        let expected_path = canonical_root.join("lockfile");
        assert_eq!(discovery.path(), expected_path.as_path());
        assert!(expected_path.exists(), "the lockfile must be written");

        let contents = std::fs::read_to_string(&expected_path).expect("read lockfile");
        assert!(!contents.contains('\n'), "the lockfile is one line");
        let parts: Vec<&str> = contents.split(':').collect();
        assert_eq!(parts.len(), 5, "exactly five lockfile fields");
        assert_eq!(parts[0], "LeagueClientUx");
        assert_eq!(parts[1].parse::<u32>(), Ok(std::process::id()));
        assert_eq!(parts[2].parse::<u16>(), Ok(port));
        assert!(
            parts[3].as_bytes() == token.as_bytes(),
            "the token must round-trip"
        );
        assert_eq!(parts[4], "https");

        let parsed = parse_lockfile(&contents).expect("the app parses the mock lockfile");
        assert_eq!(parsed.port, port);
        assert!(
            parsed.token.as_bytes() == token.as_bytes(),
            "port and token must match"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&expected_path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the lockfile must be owner-only");
        }
    }

    #[test]
    fn log_writer_emits_expected_path_and_head() {
        let root = tempfile::tempdir().expect("temp dir");
        let port = 50_414u16;
        let token = generate_token().expect("token");

        let discovery = write_log(root.path(), port, &token).expect("log");
        // Compare against the canonicalized root: the writer resolves symlinked
        // ancestors, and on Windows it returns a verbatim `\\?\` path with the
        // 8.3 short name expanded.
        let canonical_root = std::fs::canonicalize(root.path()).expect("canonical root");
        let log_directory = canonical_root
            .join("Game")
            .join("Logs")
            .join("LeagueClient Logs");
        assert_eq!(discovery.path().parent(), Some(log_directory.as_path()));

        let name = discovery
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .expect("file name");
        assert!(name.ends_with("_LeagueClientUx.log"), "{name}");
        let stamp = name
            .strip_suffix("_LeagueClientUx.log")
            .expect("suffix was just checked");
        assert!(
            stamp
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_digit()),
            "the file name starts with a timestamp: {name}"
        );

        let mut head = Vec::new();
        std::fs::File::open(discovery.path())
            .expect("open log")
            .take(64 * 1024)
            .read_to_end(&mut head)
            .expect("read log head");
        let text = String::from_utf8_lossy(&head);
        assert!(
            text.contains(&format!("--app-port={port}")),
            "the first 64 KiB must carry the port"
        );
        assert!(
            text.contains(&format!("--remoting-auth-token={token}")),
            "the first 64 KiB must carry the token"
        );

        // The advertised token is in the charset the app's log parser accepts.
        assert!(token
            .chars()
            .all(|character| character.is_ascii_alphanumeric()
                || matches!(character, '-' | '_' | '=')));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(discovery.path())
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the log must be owner-only");
        }
    }

    #[test]
    fn rejects_relative_unc_and_escape_roots() {
        let root = tempfile::tempdir().expect("temp dir");
        let port = 50_414u16;
        let token = generate_token().expect("token");

        let invalid_roots = [
            std::path::PathBuf::from("relative/mock-root"),
            std::path::PathBuf::from(r"\\server\share\mock-root"),
            std::path::PathBuf::from("//server/share/mock-root"),
            std::path::PathBuf::from(r"C:\Riot Games\League of Legends"),
            root.path().join("nested").join("..").join("other"),
            root.path().join("does-not-exist"),
        ];
        for invalid in &invalid_roots {
            let error = write_lockfile(invalid, port, &token)
                .expect_err("an invalid root must be rejected");
            assert_eq!(error.exit_code(), 2, "{invalid:?}: {error}");
            assert!(
                error.to_string().contains("--install-root"),
                "{invalid:?}: {error}"
            );
        }
        assert!(
            !root.path().join("lockfile").exists(),
            "no lockfile may be written for a rejected root"
        );

        let marker_root = tempfile::tempdir().expect("marker root");
        std::fs::write(marker_root.path().join("LeagueClient.exe"), b"").expect("marker");
        let error = write_lockfile(marker_root.path(), port, &token)
            .expect_err("a real install marker must be rejected");
        assert_eq!(error.exit_code(), 2);
        assert!(!marker_root.path().join("lockfile").exists());

        let foreign_root = tempfile::tempdir().expect("foreign root");
        std::fs::write(foreign_root.path().join("LeagueClientUx.exe"), b"").expect("ux marker");
        let error = write_lockfile(foreign_root.path(), port, &token)
            .expect_err("a foreign LeagueClientUx.exe must be rejected");
        assert_eq!(error.exit_code(), 2);
        assert!(!foreign_root.path().join("lockfile").exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let outside = tempfile::tempdir().expect("outside");
            let linked_root = root.path().join("linked-root");
            symlink(outside.path(), &linked_root).expect("root symlink");
            let error = write_lockfile(&linked_root, port, &token)
                .expect_err("a symlinked root must be rejected");
            assert_eq!(error.exit_code(), 2);
            assert!(
                !outside.path().join("lockfile").exists(),
                "no write may follow a symlinked root"
            );

            let log_root = tempfile::tempdir().expect("log root");
            symlink(outside.path(), log_root.path().join("Game")).expect("game symlink");
            let error = write_log(log_root.path(), port, &token)
                .expect_err("a symlinked log ancestor must be rejected");
            assert_eq!(error.exit_code(), 2);
            assert!(
                !outside.path().join("Logs").exists(),
                "no directory may be created outside the root"
            );
        }
    }

    #[test]
    fn rejects_root_nested_under_real_install_marker() {
        let base = tempfile::tempdir().expect("temp dir");
        std::fs::write(base.path().join("LeagueClient.exe"), b"").expect("marker");
        let nested = base.path().join("nested").join("mock-root");
        std::fs::create_dir_all(&nested).expect("nested root");
        let port = 50_414u16;
        let token = generate_token().expect("token");

        let error = write_lockfile(&nested, port, &token)
            .expect_err("an ancestor LeagueClient.exe must reject the root");
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(!nested.join("lockfile").exists());

        std::fs::remove_file(base.path().join("LeagueClient.exe")).expect("unmark");
        std::fs::write(base.path().join("LeagueClientUx.exe"), b"").expect("ux marker");
        let error = write_lockfile(&nested, port, &token)
            .expect_err("an ancestor foreign LeagueClientUx.exe must reject the root");
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(!nested.join("lockfile").exists());
    }

    #[cfg(unix)]
    #[test]
    fn resolves_or_rejects_root_under_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let base = tempfile::tempdir().expect("temp dir");
        let real = base.path().join("real");
        std::fs::create_dir(&real).expect("real dir");
        let nested = real.join("mock-root");
        std::fs::create_dir(&nested).expect("nested root");
        let linked = base.path().join("linked");
        symlink(&real, &linked).expect("ancestor symlink");
        let requested = linked.join("mock-root");
        let port = 50_414u16;
        let token = generate_token().expect("token");

        match write_lockfile(&requested, port, &token) {
            Ok(discovery) => {
                let canonical_root = std::fs::canonicalize(&requested).expect("canonical root");
                assert_eq!(
                    discovery.path(),
                    canonical_root.join("lockfile").as_path(),
                    "writes must use the canonical root, never the raw input"
                );
                assert!(canonical_root.join("lockfile").exists());
            }
            Err(error) => {
                assert_eq!(
                    error.exit_code(),
                    2,
                    "a rejected ancestor symlink is a usage error: {error}"
                );
                assert!(
                    !nested.join("lockfile").exists(),
                    "no file may be written for a rejected root"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_lockfile_leaf() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("target");
        std::fs::write(&target, b"untouched").expect("target");
        let leaf = root.path().join("lockfile");
        symlink(&target, &leaf).expect("leaf symlink");
        let token = generate_token().expect("token");

        let error = write_lockfile(root.path(), 50_414, &token)
            .expect_err("a symlinked lockfile leaf must be rejected");
        assert_ne!(error.exit_code(), 0, "{error}");
        assert_eq!(
            std::fs::read(&target).expect("target read"),
            b"untouched",
            "the symlink target must not be truncated"
        );
        assert!(std::fs::symlink_metadata(&leaf)
            .expect("leaf metadata")
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_hardlinked_lockfile_leaf() {
        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("target");
        std::fs::write(&target, b"untouched").expect("target");
        let leaf = root.path().join("lockfile");
        // A hardlink is a regular file, so `O_NOFOLLOW` does not stop it.
        std::fs::hard_link(&target, &leaf).expect("leaf hardlink");
        let token = generate_token().expect("token");

        let error = write_lockfile(root.path(), 50_414, &token)
            .expect_err("a hardlinked lockfile leaf must be rejected");
        assert_ne!(error.exit_code(), 0, "{error}");
        assert_eq!(
            std::fs::read(&target).expect("target read"),
            b"untouched",
            "the hardlink target must not be modified"
        );
        assert!(std::fs::metadata(&leaf).expect("leaf metadata").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn ignores_markers_in_world_writable_ancestors() {
        use std::os::unix::fs::PermissionsExt;

        let base = tempfile::tempdir().expect("temp dir");
        let shared = base.path().join("shared");
        std::fs::create_dir(&shared).expect("shared dir");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777))
            .expect("world-writable shared dir");
        std::fs::write(shared.join("LeagueClient.exe"), b"").expect("planted marker");

        let root = shared.join("run");
        std::fs::create_dir(&root).expect("run root");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("private run root");
        let token = generate_token().expect("token");

        {
            let discovery = write_lockfile(&root, 50_414, &token)
                .expect("a marker in a world-writable ancestor must be ignored");
            assert!(discovery.path().exists());
        }

        // A marker inside the user-chosen root is still honored.
        std::fs::write(root.join("LeagueClient.exe"), b"").expect("root marker");
        let error = write_lockfile(&root, 50_414, &token)
            .expect_err("a marker in the chosen root must still be rejected");
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(!root.join("lockfile").exists());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_bare_known_install_prefix_on_windows() {
        let token = generate_token().expect("token");
        let root = std::path::PathBuf::from(r"C:\Riot Games\aramkit-mock-test");
        let error = write_lockfile(&root, 50_414, &token)
            .expect_err("a known Riot Games prefix must be rejected");
        assert_eq!(error.exit_code(), 2, "{error}");
    }

    #[test]
    fn clean_exit_removes_discovery_files() {
        let root = tempfile::tempdir().expect("temp dir");
        let port = 50_414u16;
        let token = generate_token().expect("token");
        // See `lockfile_writer_emits_parseable_line`: the writer reports paths
        // under the canonicalized root, not the raw temp root.
        let canonical_root = std::fs::canonicalize(root.path()).expect("canonical root");
        let lockfile_path = canonical_root.join("lockfile");

        {
            let discovery = write_lockfile(root.path(), port, &token).expect("lockfile");
            assert_eq!(discovery.path(), lockfile_path.as_path());
            assert!(lockfile_path.exists());
        }
        assert!(
            !lockfile_path.exists(),
            "a clean drop removes the lockfile it created"
        );

        let log_path;
        {
            let discovery = write_log(root.path(), port, &token).expect("log");
            log_path = discovery.path().to_path_buf();
            assert!(log_path.exists());
        }
        assert!(
            !log_path.exists(),
            "a clean drop removes the log it created"
        );

        // The temporary root is still usable for another run.
        let discovery = write_lockfile(root.path(), port, &token).expect("second lockfile");
        assert!(discovery.path().exists());
        assert!(root.path().join("lockfile").exists());
    }

    #[test]
    fn shutdown_flag_runs_cleanup() {
        let root = tempfile::tempdir().expect("temp dir");
        let token = generate_token().expect("token");
        let discovery = write_lockfile(root.path(), 50_414, &token).expect("lockfile");
        let path = discovery.path().to_path_buf();

        let flag = Arc::new(AtomicBool::new(false));
        let flipper = {
            let flag = Arc::clone(&flag);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                flag.store(true, Ordering::SeqCst);
            })
        };

        run_until_stopped(&flag, || drop(discovery));
        flipper.join().expect("flipper");

        assert!(
            !path.exists(),
            "the stop flag must run the cleanup callback"
        );
    }
}

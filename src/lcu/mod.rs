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
    use super::creds::{parse_lcu_args, LcuCredentials};
    use super::mock_core::{generate_token, MockOptions, MockServer};
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
}

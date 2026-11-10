use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;

use super::client::{build_client, fetch_champ_select, fetch_champion_catalog, fetch_gameflow};
use super::creds::{discover, LcuCredentials};
use super::{ChampionPage, LcuEvent, Status};
use crate::champion::{champion_url_for_alias, is_mayhem_queue, ChampionCatalog};
use crate::config::WindowBounds;
use crate::platform::ChampionWindow;
use crate::tracker::{ChampionTracker, Trigger};

type Wake = Arc<dyn Fn() + Send + Sync>;

/// Wait this long in `GameStart` before opening the page, so a
/// `FailedToLaunch` transition can still cancel it.
const GAME_START_GRACE: Duration = Duration::from_secs(2);

/// Owns the background thread. Dropping it stops the watcher.
pub struct WatcherHandle {
    stop: Arc<AtomicBool>,
    window: Arc<Mutex<ChampionWindow>>,
}

impl WatcherHandle {
    pub fn spawn(tx: Sender<LcuEvent>, wake: Wake, bounds: Option<WindowBounds>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_tx = tx.clone();
        let window = Arc::new(Mutex::new(ChampionWindow::new(bounds)));
        let thread_window = Arc::clone(&window);
        let spawn_result = std::thread::Builder::new()
            .name("lcu-watcher".to_owned())
            .spawn(move || run(thread_tx, wake, thread_stop, &thread_window));

        if let Err(error) = spawn_result {
            let _ = tx.send(LcuEvent::Log(format!("无法启动后台线程：{error}")));
        }

        Self { stop, window }
    }

    pub fn open_url(&self, url: &str) -> Result<(), String> {
        open_managed_window(&self.window, url)
    }

    pub fn browser_bounds(&self) -> Option<WindowBounds> {
        self.window
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .bounds()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.window
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .close();
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(tx: Sender<LcuEvent>, wake: Wake, stop: Arc<AtomicBool>, window: &Mutex<ChampionWindow>) {
    let mut last_status = None;
    let mut tracker = ChampionTracker::default();
    let mut previous_phase = String::new();
    let mut missing_client_rounds = 0u32;

    emit_status(
        &tx,
        &wake,
        &mut last_status,
        Status::disconnected("请启动英雄联盟客户端"),
    );

    while !stop.load(Ordering::Relaxed) {
        let discovery = discover();

        match discovery.credentials {
            Some(credentials) => {
                missing_client_rounds = 0;
                poll_client(
                    &tx,
                    &wake,
                    &stop,
                    &credentials,
                    &mut last_status,
                    &mut tracker,
                    &mut previous_phase,
                    window,
                );
            }
            None => {
                missing_client_rounds += 1;
                // Do not keep a stale pending pick forever if the client has
                // been gone for a few minutes.
                if missing_client_rounds >= 60 {
                    tracker = ChampionTracker::default();
                    previous_phase.clear();
                }
                // The CN client often runs elevated, so the process is visible
                // while its credentials are still unreadable for a moment.
                let message = if discovery.client_running {
                    "已检测到客户端，正在读取登录凭证…"
                } else {
                    "请启动英雄联盟客户端"
                };
                emit_status(&tx, &wake, &mut last_status, Status::disconnected(message));
            }
        }

        sleep_interruptible(&stop, Duration::from_millis(3000));
    }
}

#[allow(clippy::too_many_arguments)]
fn poll_client(
    tx: &Sender<LcuEvent>,
    wake: &Wake,
    stop: &AtomicBool,
    credentials: &LcuCredentials,
    last_status: &mut Option<Status>,
    tracker: &mut ChampionTracker,
    previous_phase: &mut String,
    window: &Mutex<ChampionWindow>,
) {
    let client = match build_client() {
        Ok(client) => client,
        Err(error) => {
            log_event(tx, wake, format!("{error}"));
            return;
        }
    };

    let mut catalog: Option<ChampionCatalog> = None;
    let mut catalog_error_logged = false;
    let mut connected_announced = false;
    let mut gameflow_errors = 0u32;
    let mut game_start_since: Option<Instant> = None;
    let mut retry_page_after: Option<Instant> = None;
    let mut last_champion_select_error: Option<String> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }

        let session = match fetch_gameflow(&client, credentials) {
            Ok(session) => {
                gameflow_errors = 0;
                session
            }
            Err(error) => {
                gameflow_errors += 1;

                // A refused TCP connection means the client really exited.
                // Timeouts while the port is still open usually mean the
                // client is busy (for example during the champion-select ->
                // loading transition); keep the tracker alive and retry.
                let port_state = port_state(credentials.port);
                let client_gone = matches!(port_state, PortState::Closed);
                let credentials_rejected = error.chain().any(|cause| {
                    cause
                        .downcast_ref::<reqwest::Error>()
                        .and_then(reqwest::Error::status)
                        == Some(reqwest::StatusCode::UNAUTHORIZED)
                });
                if client_gone || credentials_rejected || gameflow_errors >= 20 {
                    if connected_announced {
                        let reason = if client_gone {
                            "与英雄联盟客户端的连接已断开"
                        } else if credentials_rejected {
                            "英雄联盟客户端拒绝了当前连接凭证"
                        } else {
                            "英雄联盟客户端接口持续无响应"
                        };
                        log_event(tx, wake, format!("{reason}，等待重新连接"));
                    }
                    emit_status(
                        tx,
                        wake,
                        last_status,
                        Status::disconnected(if client_gone {
                            "请启动英雄联盟客户端"
                        } else if credentials_rejected {
                            "客户端凭证已失效，正在重新读取"
                        } else {
                            "客户端接口无响应，正在重新连接"
                        }),
                    );
                    return;
                }

                if gameflow_errors == 3 {
                    log_event(
                        tx,
                        wake,
                        format!("英雄联盟客户端接口暂时无响应（{error}），继续重试"),
                    );
                }

                sleep_interruptible(stop, Duration::from_millis(500));
                continue;
            }
        };

        if !connected_announced {
            connected_announced = true;
            log_event(tx, wake, "已连接英雄联盟客户端".to_owned());
        }

        let phase = session.phase.clone();
        let game_id = session.game_id();
        let queue = session.queue();
        let mayhem = if queue.id > 0 || is_mayhem_queue(queue.id, &queue.game_mode) {
            Some(is_mayhem_queue(queue.id, &queue.game_mode))
        } else {
            None
        };

        let mut champion_id = 0;
        let mut champion_name = String::new();
        let mut message = if phase == "ChampSelect" {
            match mayhem {
                Some(false) if queue.id > 0 => {
                    format!("当前队列 {} 不是海克斯大乱斗，已跳过", queue.id)
                }
                Some(false) => "当前模式不是海克斯大乱斗，已跳过".to_owned(),
                None => "正在读取队列信息…".to_owned(),
                Some(true) => String::new(),
            }
        } else {
            String::new()
        };

        if phase == "ChampSelect" {
            game_start_since = None;
            retry_page_after = None;
            let new_session =
                previous_phase.as_str() != "ChampSelect" || tracker.has_different_game(game_id);
            if new_session {
                tracker.begin_champ_select(game_id, mayhem);
            }
            tracker.update_champ_select(game_id, mayhem, None);

            match fetch_champ_select(&client, credentials) {
                Ok(Some(champion_select)) => {
                    last_champion_select_error = None;

                    if let Some(local_champion_id) = champion_select.local_champion_id() {
                        champion_id = local_champion_id;
                        tracker.update_champ_select(game_id, mayhem, Some(local_champion_id));

                        if mayhem != Some(false) {
                            if catalog.is_none() && !catalog_error_logged {
                                refresh_catalog(
                                    &client,
                                    credentials,
                                    &mut catalog,
                                    &mut catalog_error_logged,
                                    tx,
                                    wake,
                                );
                            }
                            champion_name = catalog
                                .as_ref()
                                .and_then(|catalog| catalog.name_of(local_champion_id))
                                .unwrap_or_default()
                                .to_owned();
                            message = if mayhem.is_none() {
                                "已识别英雄，正在读取队列信息…".to_owned()
                            } else if champion_name.is_empty() {
                                "已识别英雄，进入游戏后将打开 ARAMKit 页面".to_owned()
                            } else {
                                format!("已识别英雄 {champion_name}，进入游戏后将打开 ARAMKit 页面")
                            };
                        }
                    } else {
                        tracker.update_champ_select(game_id, mayhem, None);
                        if mayhem != Some(false) {
                            message = "正在选择英雄…".to_owned();
                        }
                    }
                }
                Ok(None) => {
                    // The endpoint disappears for a moment while the client
                    // transitions to the loading screen. Keep the tracker.
                }
                Err(error) => {
                    let error_message = format!("读取英雄选择状态失败：{error}");
                    if last_champion_select_error.as_deref() != Some(error_message.as_str()) {
                        log_event(tx, wake, error_message.clone());
                        last_champion_select_error = Some(error_message);
                    }
                }
            }
        }

        if phase != "ChampSelect" {
            if phase != "GameStart" && phase != "InProgress" {
                retry_page_after = None;
            }
            let waiting_to_retry = retry_page_after.is_some_and(|time| Instant::now() < time);
            let mut trigger = tracker.on_phase(
                if waiting_to_retry && phase == "InProgress" {
                    ""
                } else {
                    &phase
                },
                game_id,
                mayhem,
            );

            if phase == "GameStart" {
                let since = game_start_since.get_or_insert_with(Instant::now);
                if !waiting_to_retry && since.elapsed() >= GAME_START_GRACE {
                    trigger = trigger.or_else(|| tracker.fire_if_ready(game_id));
                }
            } else {
                game_start_since = None;
            }

            if let Some(trigger) = trigger {
                let (opened_id, opened_name, opened_message, opened) = handle_trigger(
                    &trigger,
                    &client,
                    credentials,
                    &mut catalog,
                    &mut catalog_error_logged,
                    tx,
                    wake,
                    window,
                );
                champion_id = opened_id;
                champion_name = opened_name;
                message = opened_message;
                if !opened {
                    tracker.begin_champ_select(trigger.game_id, Some(true));
                    tracker.update_champ_select(
                        trigger.game_id,
                        Some(true),
                        Some(trigger.champion_id),
                    );
                    retry_page_after = Some(Instant::now() + Duration::from_secs(5));
                }
            }
        }

        let status = Status {
            connected: true,
            phase: phase.clone(),
            queue_id: queue.id,
            champion_id,
            champion_name,
            message,
        };
        emit_status(tx, wake, last_status, status);

        *previous_phase = phase;
        sleep_interruptible(stop, Duration::from_millis(700));
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_trigger(
    trigger: &Trigger,
    client: &Client,
    credentials: &LcuCredentials,
    catalog: &mut Option<ChampionCatalog>,
    catalog_error_logged: &mut bool,
    tx: &Sender<LcuEvent>,
    wake: &Wake,
    window: &Mutex<ChampionWindow>,
) -> (i64, String, String, bool) {
    if catalog
        .as_ref()
        .and_then(|catalog| catalog.get(trigger.champion_id))
        .is_none()
    {
        refresh_catalog(client, credentials, catalog, catalog_error_logged, tx, wake);
    }

    let page = open_champion_page(catalog, trigger.champion_id, window);
    let champion_name = page.champion_name.clone();
    let message = match &page.error {
        Some(error) => format!("打开页面失败：{error}"),
        None => format!("已打开 {} 的 ARAMKit 页面", page.champion_name),
    };

    let opened = page.error.is_none();
    let _ = tx.send(LcuEvent::ChampionOpened(page));
    wake();

    (trigger.champion_id, champion_name, message, opened)
}

fn refresh_catalog(
    client: &Client,
    credentials: &LcuCredentials,
    catalog: &mut Option<ChampionCatalog>,
    error_logged: &mut bool,
    tx: &Sender<LcuEvent>,
    wake: &Wake,
) {
    match fetch_champion_catalog(client, credentials) {
        Ok(loaded) => {
            *catalog = Some(loaded);
            *error_logged = false;
        }
        Err(error) => {
            if !*error_logged {
                log_event(tx, wake, format!("无法读取英雄列表：{error}"));
                *error_logged = true;
            }
        }
    }
}

fn open_champion_page(
    catalog: &Option<ChampionCatalog>,
    champion_id: i64,
    window: &Mutex<ChampionWindow>,
) -> ChampionPage {
    let Some(info) = catalog
        .as_ref()
        .and_then(|catalog| catalog.get(champion_id))
    else {
        return ChampionPage {
            champion_id,
            champion_name: format!("英雄 #{champion_id}"),
            url: String::new(),
            error: Some("未读取到该英雄的数据，无法生成 ARAMKit 链接".to_owned()),
        };
    };

    let champion_name = if info.name.is_empty() {
        format!("英雄 #{champion_id}")
    } else {
        info.name.clone()
    };

    let Some(url) = champion_url_for_alias(&info.alias) else {
        return ChampionPage {
            champion_id,
            champion_name,
            url: String::new(),
            error: Some("无法确定英雄别名，无法生成 ARAMKit 链接".to_owned()),
        };
    };

    let error = open_managed_window(window, &url).err();
    ChampionPage {
        champion_id,
        champion_name,
        url,
        error,
    }
}

fn open_managed_window(window: &Mutex<ChampionWindow>, url: &str) -> Result<(), String> {
    window
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .open(url)
}

fn emit_status(
    tx: &Sender<LcuEvent>,
    wake: &Wake,
    last_status: &mut Option<Status>,
    status: Status,
) {
    if last_status.as_ref() == Some(&status) {
        return;
    }

    if tx.send(LcuEvent::Status(status.clone())).is_ok() {
        *last_status = Some(status);
        wake();
    }
}

fn log_event(tx: &Sender<LcuEvent>, wake: &Wake, message: String) {
    if tx.send(LcuEvent::Log(message)).is_ok() {
        wake();
    }
}

fn sleep_interruptible(stop: &AtomicBool, duration: Duration) {
    let deadline = std::time::Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
    }
}

fn port_state(port: u16) -> PortState {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match std::net::TcpStream::connect_timeout(&address, Duration::from_millis(300)) {
        Ok(_) => PortState::Open,
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => PortState::Closed,
        Err(_) => PortState::Unknown,
    }
}

enum PortState {
    Open,
    Closed,
    Unknown,
}

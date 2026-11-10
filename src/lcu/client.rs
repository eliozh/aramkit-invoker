use std::io::Read;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;

use super::creds::LcuCredentials;
use super::model::{ChampSelectSession, GameflowSession};
use crate::champion::{ChampionCatalog, ChampionSummaryEntry};

/// Upper bound for any local API response. The champion summary is ~1 MB.
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;

/// Builds a loopback-only client. The League client serves HTTPS with a
/// self-signed certificate, so certificate verification is intentionally off.
pub fn build_client() -> Result<Client> {
    Client::builder()
        .danger_accept_invalid_certs(true)
        // Never route the local client API through a system proxy.
        .no_proxy()
        // A local client should never redirect us off-loopback.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .connect_timeout(Duration::from_secs(1))
        .build()
        .context("创建 HTTP 客户端失败")
}

pub fn fetch_gameflow(client: &Client, credentials: &LcuCredentials) -> Result<GameflowSession> {
    let response = send(client, credentials, "/lol-gameflow/v1/session")?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(GameflowSession {
            phase: get_json(client, credentials, "/lol-gameflow/v1/gameflow-phase")?,
            ..GameflowSession::default()
        });
    }
    let response = response
        .error_for_status()
        .context("游戏流程接口返回错误")?;
    read_json(response, "游戏流程数据")
}

pub fn fetch_champ_select(
    client: &Client,
    credentials: &LcuCredentials,
) -> Result<Option<ChampSelectSession>> {
    let response = send(client, credentials, "/lol-champ-select/v1/session")?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let response = response
        .error_for_status()
        .context("英雄选择接口返回错误")?;
    let session = read_json(response, "英雄选择数据")?;
    Ok(Some(session))
}

pub fn fetch_champion_catalog(
    client: &Client,
    credentials: &LcuCredentials,
) -> Result<ChampionCatalog> {
    let entries: Vec<ChampionSummaryEntry> = get_json(
        client,
        credentials,
        "/lol-game-data/assets/v1/champion-summary.json",
    )?;
    Ok(ChampionCatalog::from_entries(entries))
}

fn get_json<T: DeserializeOwned>(
    client: &Client,
    credentials: &LcuCredentials,
    path: &str,
) -> Result<T> {
    let response = send(client, credentials, path)?;
    let response = response
        .error_for_status()
        .with_context(|| format!("{path} 返回错误状态"))?;
    read_json(response, path)
}

fn read_json<T: DeserializeOwned>(response: Response, description: &str) -> Result<T> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err(anyhow!("{description} 响应过大"));
    }

    let mut body = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .with_context(|| format!("读取 {description} 响应失败"))?;

    if body.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(anyhow!("{description} 响应过大"));
    }

    serde_json::from_slice(&body).with_context(|| format!("解析 {description} 响应失败"))
}

fn send(client: &Client, credentials: &LcuCredentials, path: &str) -> Result<Response> {
    let url = format!("{}{}", credentials.base_url(), path);
    client
        .get(url)
        .basic_auth("riot", Some(&credentials.token))
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, "aramkit-invoker/0.1")
        .send()
        .with_context(|| format!("请求 {path} 失败"))
}

//! Scenario state and the JSON bodies served by the mock.
//!
//! Everything here is static fixture data: no I/O and no scheduling.

use serde_json::{json, Value};

pub(crate) const GAME_ID: i64 = 424_242;
pub(crate) const MAYHEM_QUEUE_ID: i64 = 2400;
pub(crate) const MAYHEM_GAME_MODE: &str = "KIWI";
pub(crate) const ARAM_QUEUE_ID: i64 = 450;
pub(crate) const ARAM_GAME_MODE: &str = "ARAM";
pub(crate) const DEFAULT_CHAMPION_ID: i64 = 103;
pub(crate) const DEFAULT_LOCAL_CELL_ID: i64 = 1;
pub(crate) const DEFAULT_PHASE: &str = "Lobby";

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

pub(crate) fn gameflow_body(state: &MockState) -> Value {
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

pub(crate) fn champ_select_body(state: &MockState) -> Value {
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

pub(crate) fn champion_summary_body(state: &MockState) -> Value {
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

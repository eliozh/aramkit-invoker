//! Pure timeline model used by the mock server and the CLI.
//!
//! Nothing in this module performs I/O or reads the wall clock: a schedule is
//! evaluated against an elapsed duration so tests never sleep.

use std::time::{Duration, Instant};

use super::fixtures::{
    MockState, ARAM_GAME_MODE, ARAM_QUEUE_ID, DEFAULT_PHASE, MAYHEM_GAME_MODE, MAYHEM_QUEUE_ID,
};

/// One scripted state and how long it is held.
#[derive(Debug, Clone)]
pub struct Step {
    pub state: MockState,
    pub dwell_ms: u64,
}

/// A pure timeline over elapsed time. The final step holds indefinitely.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub steps: Vec<Step>,
}

impl Schedule {
    /// A one-step schedule that holds forever, used by `--phase`.
    pub fn static_state(state: MockState) -> Self {
        Self {
            steps: vec![Step { state, dwell_ms: 0 }],
        }
    }

    /// Returns the state that is active at `elapsed`.
    ///
    /// A step is active for `[start, start + dwell)`; the last step has no
    /// successor and holds for every later instant. This is pure, so tests can
    /// assert boundaries without sleeping.
    pub fn state_at(&self, elapsed: Duration) -> MockState {
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let mut cumulative = 0u64;
        for step in &self.steps {
            cumulative = cumulative.saturating_add(step.dwell_ms);
            if elapsed_ms < cumulative {
                return step.state.clone();
            }
        }
        self.steps
            .last()
            .map(|step| step.state.clone())
            .unwrap_or_default()
    }
}

/// The scripted timelines from ADR-0005.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    Mayhem,
    NonMayhem,
    Dodge,
    FailedToLaunch,
}

impl Scenario {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "mayhem" => Some(Self::Mayhem),
            "non-mayhem" => Some(Self::NonMayhem),
            "dodge" => Some(Self::Dodge),
            "failed-to-launch" => Some(Self::FailedToLaunch),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Mayhem => "mayhem",
            Self::NonMayhem => "non-mayhem",
            Self::Dodge => "dodge",
            Self::FailedToLaunch => "failed-to-launch",
        }
    }

    pub fn queue_id(self) -> i64 {
        match self {
            Self::NonMayhem => ARAM_QUEUE_ID,
            Self::Mayhem | Self::Dodge | Self::FailedToLaunch => MAYHEM_QUEUE_ID,
        }
    }

    pub fn game_mode(self) -> &'static str {
        match self {
            Self::NonMayhem => ARAM_GAME_MODE,
            Self::Mayhem | Self::Dodge | Self::FailedToLaunch => MAYHEM_GAME_MODE,
        }
    }

    /// Builds the scenario timeline; `template` supplies queue, mode, and IDs.
    pub fn schedule(self, step_ms: u64, template: &MockState) -> Schedule {
        let step = |phase: &str, multiplier: u64| Step {
            state: MockState {
                phase: phase.to_owned(),
                ..template.clone()
            },
            dwell_ms: step_ms.saturating_mul(multiplier),
        };

        let steps = match self {
            Self::Mayhem | Self::NonMayhem => vec![
                step(DEFAULT_PHASE, 2),
                step("ChampSelect", 12),
                step("GameStart", 4),
                step("InProgress", 0),
            ],
            Self::Dodge => vec![
                step(DEFAULT_PHASE, 2),
                step("ChampSelect", 12),
                step(DEFAULT_PHASE, 0),
            ],
            Self::FailedToLaunch => vec![
                step(DEFAULT_PHASE, 2),
                step("ChampSelect", 12),
                step("FailedToLaunch", 0),
            ],
        };

        Schedule { steps }
    }
}

/// A schedule evaluated from the server's start instant, read per request.
pub struct ScheduledState {
    pub(crate) schedule: Schedule,
    pub(crate) started: Instant,
}

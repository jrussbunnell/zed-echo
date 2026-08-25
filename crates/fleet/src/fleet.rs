//! Echo's reader for the parallel work Claude Code is already doing.
//!
//! Claude Code runs sessions the user never sees: a supervisor daemon hosts
//! them, gives each its own git worktree, and records what they are doing under
//! `~/.claude`. This crate turns that on-disk record into values Echo can
//! render. It schedules nothing and spawns nothing.
//!
//! See `docs/superpowers/specs/2026-08-22-fleet-and-workflows-design.md`.

pub mod claude_home;

use serde::{Deserialize, Deserializer};

/// What a dispatched session is doing, as the daemon last wrote it down.
///
/// These strings are a private Claude Code implementation detail, so an
/// unrecognized one is carried through as [`FleetState::Unknown`] rather than
/// rejected: a CLI upgrade that adds a state must cost the row its icon, not
/// its existence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FleetState {
    Working,
    /// Waiting on the user — a permission prompt, or a question.
    Blocked,
    Done,
    Failed,
    Stopped,
    Unknown(String),
}

impl FleetState {
    pub fn from_wire(state: &str) -> Self {
        match state.trim() {
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Whether this session still has work in flight.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Working | Self::Blocked)
    }

    /// Whether the session is waiting on the user. These sort to the top of the
    /// fleet, which is the reason the surface exists.
    pub fn needs_attention(&self) -> bool {
        matches!(self, Self::Blocked)
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Working => "Working",
            Self::Blocked => "Needs input",
            Self::Done => "Done",
            Self::Failed => "Failed",
            Self::Stopped => "Stopped",
            Self::Unknown(other) => other,
        }
    }
}

impl Default for FleetState {
    fn default() -> Self {
        Self::Unknown(String::new())
    }
}

impl<'de> Deserialize<'de> for FleetState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Deliberately infallible: a state that arrives as a number, an object,
        // or anything else still has to produce a renderable row.
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match value.as_str() {
            Some(state) => Self::from_wire(state),
            None => Self::Unknown(value.to_string()),
        })
    }
}

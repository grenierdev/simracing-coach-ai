//! MCP server embedded in the program: exposes telemetry and the trigger plan
//! to Claude.
//!
//! Transport: "streamable" HTTP on `127.0.0.1`, ephemeral port, protected by
//! a random Bearer token. The URL and token are passed to the agent in
//! `mcpServers` during `session/new`.
//!
//! Tools:
//! - `get_session_overview()`: laps, delta, time lost per corner (ranked);
//! - `get_corner_summary(corner, laps)`: corner metrics against the reference;
//! - `get_corner_trace(corner, channels, lap)`: resampled traces (≤ 100 points);
//! - `update_plan(triggers, mode)`: replaces or extends the upcoming triggers;
//! - `say(text, when)`: one-off sentence (now, next straight, position);
//! - `set_focus(corners, announcement)`: the corners the driver works on;
//! - `next_focus(reason)`: leaves the current corners for the next finding.

pub mod server;
pub mod tools;

use std::sync::{Arc, Mutex};

use analysis::Analyzer;
use serde::Serialize;
use telemetry::LiveSnapshot;
use triggers::{Plan, SayWhen};

pub use server::{CoachMcp, McpServerHandle, SERVER_NAME};

/// Tool call received, for the UI log.
#[derive(Debug, Clone)]
pub struct ToolCallLog {
    /// Session that called the tool ("conversation", "analysis").
    pub source: String,
    pub name: String,
    pub ok: bool,
    pub millis: f64,
}

/// Request to the focus coaching loop, from Claude (MCP tools) or the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum FocusCommand {
    /// Work on these corners (indices into the track map), and say so.
    Set {
        corners: Vec<usize>,
        announcement: Option<String>,
    },
    /// Leave the current corners for the next finding.
    Next { reason: String },
}

/// Stage of the focus coaching loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FocusPhase {
    /// Automatic planning disabled or unavailable.
    #[default]
    Off,
    /// Waiting for a complete, valid lap to choose what to work on.
    WaitingForLap,
    /// The race engineer is choosing the corners.
    Choosing,
    /// The driver works on the focus corners; each pass is analyzed.
    Practicing,
}

/// One pass through the focus corners.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FocusAttempt {
    pub number: u32,
    pub lap: i32,
    /// Time lost (+) or gained (−) on the reference over the focus corners, s.
    pub loss_s: f64,
    /// Per corner: label and loss, s.
    pub corners: Vec<(String, f64)>,
    pub off_track: bool,
}

/// State of the focus coaching loop, shared with the UI, the MCP tools and
/// the conversation context.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct FocusStatus {
    pub phase: FocusPhase,
    /// Focus corners ("T5").
    pub corners: Vec<String>,
    /// Loss over the focus corners on the lap they were chosen from, s.
    pub baseline_s: Option<f64>,
    pub attempts: Vec<FocusAttempt>,
    /// An attempt is being analyzed by the race engineer.
    pub analyzing: bool,
    /// Corners worked on earlier in the session, with their outcome.
    pub finished: Vec<String>,
}

impl FocusStatus {
    /// Best valid attempt, s.
    pub fn best_s(&self) -> Option<f64> {
        self.attempts
            .iter()
            .filter(|a| !a.off_track)
            .map(|a| a.loss_s)
            .min_by(f64::total_cmp)
    }

    /// One line for the driver context and the screens: "T5, T6: attempt 3,
    /// +0.21 s (best +0.15 s, start +0.30 s)".
    pub fn line(&self) -> Option<String> {
        match self.phase {
            FocusPhase::Off => None,
            FocusPhase::WaitingForLap => {
                Some("waiting for a clean lap to choose the corners".into())
            }
            FocusPhase::Choosing => Some("choosing the corners to work on".into()),
            FocusPhase::Practicing => {
                let corners = self.corners.join(", ");
                let Some(last) = self.attempts.last() else {
                    let base = self
                        .baseline_s
                        .map(|b| format!(" (start {b:+.2} s)"))
                        .unwrap_or_default();
                    return Some(format!("{corners}: first attempt{base}"));
                };
                let mut extra = vec![];
                if let Some(b) = self.best_s() {
                    extra.push(format!("best {b:+.2} s"));
                }
                if let Some(b) = self.baseline_s {
                    extra.push(format!("start {b:+.2} s"));
                }
                Some(format!(
                    "{corners}: attempt {}, {:+.2} s{}{}",
                    last.number,
                    last.loss_s,
                    if last.off_track { " off track" } else { "" },
                    if extra.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", extra.join(", "))
                    }
                ))
            }
        }
    }
}

/// Link between the focus loop and its clients: a command queue and the
/// shared status.
#[derive(Clone)]
pub struct FocusLink {
    pub commands: crossbeam_channel::Sender<FocusCommand>,
    pub status: Arc<Mutex<FocusStatus>>,
}

impl FocusLink {
    pub fn new() -> (Self, crossbeam_channel::Receiver<FocusCommand>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (
            FocusLink {
                commands: tx,
                status: Arc::default(),
            },
            rx,
        )
    }

    pub fn status(&self) -> FocusStatus {
        self.status.lock().unwrap().clone()
    }
}

/// MCP server access to the program state.
pub trait CoachData: Send + Sync + 'static {
    fn snapshot(&self) -> LiveSnapshot;
    fn analyzer(&self) -> Option<Arc<Analyzer>>;
    /// Coach trigger layer (the driver's plan is never modified by Claude).
    fn coach_plan(&self) -> Option<Plan>;
    fn apply_coach_plan(&self, plan: Plan);
    fn say(&self, text: String, when: SayWhen);
    /// Tool call notification (UI log).
    fn tool_called(&self, _name: &str, _ok: bool, _millis: f64) {}
    /// Focus coaching loop, when the analysis session runs.
    fn focus(&self) -> Option<FocusLink> {
        None
    }
}

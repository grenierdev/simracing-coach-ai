//! MCP server embedded in the program: exposes telemetry and the trigger plan
//! to Claude.
//!
//! Transport: "streamable" HTTP on `127.0.0.1`, ephemeral port, protected by
//! a random Bearer token. The URL and token are passed to the agent in
//! `mcpServers` during `session/new`.
//!
//! Tools:
//! - `get_session_overview()`: laps, delta, time lost per corner (ranked),
//!   and the latest pass through each turn;
//! - `get_corner_summary(corner, laps)`: corner metrics against the reference;
//! - `get_corner_trace(corner, channels, lap)`: resampled traces (≤ 100 points);
//! - `update_plan(triggers, mode)`: replaces or extends the upcoming triggers;
//! - `say(text, when)`: one-off sentence (now, next straight, position).

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

/// The passes through one turn, as analyzed after each of them.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct TurnStatus {
    /// "T5".
    pub corner: String,
    /// Passes recorded through the turn.
    pub passes: u32,
    /// Time lost (+) or gained (−) on the reference over the turn's zone, on
    /// the last clean pass, s.
    pub last_s: Option<f64>,
    /// Best clean pass, s.
    pub best_s: Option<f64>,
    /// The last pass waits for the race engineer.
    pub queued: bool,
    /// The race engineer is looking at it now.
    pub analyzing: bool,
}

/// State of the turn-by-turn coaching, shared with the screens, the MCP
/// tools and the conversation context.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct CoachingStatus {
    /// One entry per turn of the track map, in track order.
    pub turns: Vec<TurnStatus>,
}

impl CoachingStatus {
    /// The turns with the most time still to gain (last clean pass), most
    /// costly first.
    pub fn top_gains(&self, n: usize) -> Vec<(String, f64)> {
        let mut v: Vec<(String, f64)> = self
            .turns
            .iter()
            .filter_map(|t| t.last_s.map(|s| (t.corner.clone(), s)))
            .filter(|(_, s)| *s >= 0.05)
            .collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v.truncate(n);
        v
    }

    /// One line for the driver context: "analyzing T4; queued T5, T6; most to
    /// gain: T4 +0.31 s (best +0.12 s), T2 +0.20 s".
    pub fn line(&self) -> Option<String> {
        let names = |f: fn(&TurnStatus) -> bool| -> Vec<String> {
            self.turns
                .iter()
                .filter(|t| f(t))
                .map(|t| t.corner.clone())
                .collect()
        };
        let mut parts = vec![];
        let analyzing = names(|t| t.analyzing);
        if !analyzing.is_empty() {
            parts.push(format!("analyzing {}", analyzing.join(", ")));
        }
        let queued = names(|t| t.queued);
        if !queued.is_empty() {
            parts.push(format!("queued {}", queued.join(", ")));
        }
        let gains: Vec<String> = self
            .top_gains(3)
            .into_iter()
            .map(|(c, s)| {
                let best = self
                    .turns
                    .iter()
                    .find(|t| t.corner == c)
                    .and_then(|t| t.best_s)
                    .filter(|b| (b - s).abs() > 0.005)
                    .map(|b| format!(" (best {b:+.2} s)"))
                    .unwrap_or_default();
                format!("{c} {s:+.2} s{best}")
            })
            .collect();
        if !gains.is_empty() {
            parts.push(format!("most to gain: {}", gains.join(", ")));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Shared coaching state, written by the analysis loop.
pub type CoachingLink = Arc<Mutex<CoachingStatus>>;

/// MCP server access to the program state.
pub trait CoachData: Send + Sync + 'static {
    fn snapshot(&self) -> LiveSnapshot;
    fn analyzer(&self) -> Option<Arc<Analyzer>>;
    /// Coach trigger layer (the driver's plan is never modified by Claude).
    fn coach_plan(&self) -> Option<Plan>;
    fn apply_coach_plan(&self, plan: Plan);
    fn say(&self, text: String, when: SayWhen);
    /// The session may speak to the driver directly (`say` tool). The race
    /// engineer may not: its cues are spoken on the approach to their turn.
    fn can_speak(&self) -> bool {
        true
    }
    /// Tool call notification (UI log).
    fn tool_called(&self, _name: &str, _ok: bool, _millis: f64) {}
    /// Turn-by-turn coaching, when the analysis session runs.
    fn coaching(&self) -> Option<CoachingStatus> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(c: &str, last: Option<f64>, best: Option<f64>) -> TurnStatus {
        TurnStatus {
            corner: c.into(),
            passes: 1,
            last_s: last,
            best_s: best,
            ..Default::default()
        }
    }

    #[test]
    fn line_lists_queue_and_gains() {
        let mut s = CoachingStatus {
            turns: vec![
                turn("T1", Some(0.02), None),
                turn("T2", Some(0.20), Some(0.20)),
                turn("T4", Some(0.31), Some(0.12)),
                turn("T5", None, None),
            ],
        };
        s.turns[2].analyzing = true;
        s.turns[3].queued = true;
        assert_eq!(
            s.top_gains(5),
            vec![("T4".to_string(), 0.31), ("T2".to_string(), 0.20)]
        );
        assert_eq!(
            s.line().unwrap(),
            "analyzing T4; queued T5; most to gain: T4 +0.31 s (best +0.12 s), T2 +0.20 s"
        );
        assert_eq!(CoachingStatus::default().line(), None);
    }
}

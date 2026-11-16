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
//! - `say(text, when)`: one-off sentence (now, next straight, position).

pub mod server;
pub mod tools;

use std::sync::Arc;

use analysis::Analyzer;
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
}

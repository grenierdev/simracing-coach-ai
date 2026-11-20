//! Conversation with the coach: log, application of ACP events, context
//! injected into each message.

use analysis::Analyzer;
use coach::{AgentStatus, CoachEvent, PilotContext, TurnLatency};
use mcp::ToolCallLog;
use telemetry::LiveSnapshot;
use triggers::RuntimeStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    Driver,
    Coach,
    Tool,
    System,
    /// Summary of the analysis session (planning of the next lap).
    Analysis,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatEntry {
    pub who: Who,
    pub text: String,
    pub turn: Option<u64>,
    /// For a tool call: (ACP identifier, short name, duration measured on the MCP side).
    pub tool: Option<(String, String, Option<f64>)>,
}

fn short_tool_name(name: &str) -> String {
    name.rsplit("__").next().unwrap_or(name).to_string()
}

#[derive(Debug, Default)]
pub struct Chat {
    pub entries: Vec<ChatEntry>,
    pub input: String,
    pub status: Option<AgentStatus>,
    pub last_latency: Option<TurnLatency>,
    tools_seen: usize,
}

const MAX_ENTRIES: usize = 200;

impl Chat {
    fn push(&mut self, who: Who, text: String, turn: Option<u64>) {
        if self.entries.len() == MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push(ChatEntry {
            who,
            text,
            turn,
            tool: None,
        });
    }

    pub fn busy(&self) -> bool {
        matches!(self.status, Some(AgentStatus::Busy))
    }

    /// Adds the summary of a new analysis, once per lap.
    pub fn analysis_message(&mut self, title: &str, summary: &str, secs: f64) {
        self.push(
            Who::Analysis,
            format!("{title} ({secs:.0} s): {}", summary.trim()),
            None,
        );
    }

    pub fn system_message(&mut self, text: impl Into<String>) {
        self.push(Who::System, text.into(), None);
    }

    pub fn driver_message(&mut self, text: &str, turn: u64) {
        self.push(Who::Driver, text.trim().to_string(), Some(turn));
    }

    pub fn apply(&mut self, e: CoachEvent) {
        match e {
            CoachEvent::Status(s) => {
                if let AgentStatus::Failed(err) = &s {
                    self.push(Who::System, format!("Agent stopped: {err}"), None);
                }
                self.status = Some(s);
            }
            CoachEvent::Chunk { turn, text } => {
                // Continue the reply of this turn, unless something of the same
                // turn (a tool call) came in between. Lines from elsewhere (the
                // analysis session, the system) do not split the reply.
                let open = self
                    .entries
                    .iter()
                    .rposition(|e| e.turn == Some(turn))
                    .filter(|&i| self.entries[i].who == Who::Coach);
                match open {
                    Some(i) => self.entries[i].text.push_str(&text),
                    None => self.push(Who::Coach, text, Some(turn)),
                }
            }
            CoachEvent::ToolCall { turn, id, name, .. } => {
                // One line per call, in its place in the thread; the duration
                // measured on the MCP server side is attached later (`sync_tools`).
                let known = self.entries.iter().rev().take(20).any(|e| {
                    e.turn == Some(turn) && e.tool.as_ref().is_some_and(|(i, _, _)| *i == id)
                });
                if !known {
                    let short = short_tool_name(&name);
                    self.push(Who::Tool, short.clone(), Some(turn));
                    if let Some(last) = self.entries.last_mut() {
                        last.tool = Some((id, short, None));
                    }
                }
            }
            CoachEvent::Permission { tool, allowed } => {
                if !allowed {
                    self.push(Who::System, format!("Tool denied: {tool}"), None);
                }
            }
            CoachEvent::TurnEnd { latency, .. } => {
                self.last_latency = Some(latency);
            }
            CoachEvent::Error(err) => self.push(Who::System, format!("Error: {err}"), None),
        }
    }

    /// Adds the tool calls received by the MCP server since the last pass.
    pub fn sync_tools(&mut self, log: &[ToolCallLog]) {
        if log.len() < self.tools_seen {
            self.tools_seen = 0;
        }
        for t in &log[self.tools_seen..] {
            let mark = if t.ok { "" } else { " (error)" };
            if t.source == "analysis" {
                // Tools of the analysis session: no ACP id to match, one line each.
                self.push(
                    Who::Tool,
                    format!("analysis · {} · {:.0} ms{mark}", t.name, t.millis),
                    None,
                );
                continue;
            }
            let text = format!("{} · {:.0} ms{mark}", t.name, t.millis);
            let pending = self.entries.iter_mut().rev().take(20).find(|e| {
                e.tool
                    .as_ref()
                    .is_some_and(|(_, n, ms)| *n == t.name && ms.is_none())
            });
            match pending {
                Some(e) => {
                    if let Some(tool) = e.tool.as_mut() {
                        tool.2 = Some(t.millis);
                    }
                    e.text = text;
                }
                None => {
                    self.push(Who::Tool, text, None);
                    if let Some(last) = self.entries.last_mut() {
                        last.tool = Some((String::new(), t.name.clone(), Some(t.millis)));
                    }
                }
            }
        }
        self.tools_seen = log.len();
    }
}

/// Driver context at the moment they speak.
pub fn driver_context(
    snap: &LiveSnapshot,
    analyzer: Option<&Analyzer>,
    coach: &RuntimeStatus,
    focus: Option<String>,
) -> PilotContext {
    let sample = snap.sample;
    let pct = sample.map(|s| s.lap_dist_pct as f64);
    let (mut last_corner, mut next_corner) = (None, None);
    if let (Some(a), Some(p)) = (analyzer, pct) {
        let fwd = |a: f64, b: f64| (b - a).rem_euclid(1.0);
        last_corner = a
            .model
            .corners
            .iter()
            .filter(|c| !c.contains(p))
            .min_by(|x, y| fwd(x.end_pct, p).total_cmp(&fwd(y.end_pct, p)))
            .map(|c| c.label());
        next_corner = a
            .model
            .corners
            .iter()
            .min_by(|x, y| fwd(p, x.start_pct).total_cmp(&fwd(p, y.start_pct)))
            .map(|c| c.label());
        if let Some(c) = a.model.corners.iter().find(|c| c.contains(p)) {
            // Inside a corner: announce the current one.
            next_corner = Some(format!("{} (current)", c.label()));
        }
    }
    let live_delta = match (
        analyzer,
        pct,
        snap.current_lap_elapsed,
        snap.current_lap_flags,
    ) {
        (Some(a), Some(p), Some(el), Some(f)) if f.complete => Some(a.live_delta(p, el)),
        _ => None,
    };
    PilotContext {
        lap: sample.map(|s| s.lap),
        lap_pct: pct,
        last_corner,
        next_corner,
        live_delta,
        recent_cues: coach
            .log
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(|e| e.text.clone())
            .collect(),
        focus,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_of_a_turn_are_merged() {
        let mut c = Chat::default();
        c.driver_message("where am I losing time?", 1);
        c.apply(CoachEvent::Chunk {
            turn: 1,
            text: "Let me look. ".into(),
        });
        c.apply(CoachEvent::Chunk {
            turn: 1,
            text: "T4.".into(),
        });
        c.sync_tools(&[ToolCallLog {
            source: "conversation".into(),
            name: "get_session_overview".into(),
            ok: true,
            millis: 3.2,
        }]);
        c.apply(CoachEvent::Chunk {
            turn: 2,
            text: "Other.".into(),
        });
        let who: Vec<Who> = c.entries.iter().map(|e| e.who).collect();
        assert_eq!(who, vec![Who::Driver, Who::Coach, Who::Tool, Who::Coach]);
        assert_eq!(c.entries[1].text, "Let me look. T4.");
        assert_eq!(c.entries[2].text, "get_session_overview · 3 ms");
        c.sync_tools(&[ToolCallLog {
            source: "conversation".into(),
            name: "get_session_overview".into(),
            ok: true,
            millis: 3.2,
        }]);
        assert_eq!(c.entries.len(), 4, "no duplicate");
    }

    #[test]
    fn analysis_line_does_not_split_a_reply() {
        let mut c = Chat::default();
        c.driver_message("where am I losing time?", 1);
        c.apply(CoachEvent::Chunk {
            turn: 1,
            text: "The ".into(),
        });
        c.analysis_message("Lap 3", "T6: cue added.", 12.0);
        c.apply(CoachEvent::Chunk {
            turn: 1,
            text: "costliest is T6.".into(),
        });
        let texts: Vec<&str> = c.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "where am I losing time?",
                "The costliest is T6.",
                "Lap 3 (12 s): T6: cue added."
            ]
        );
    }

    #[test]
    fn tool_line_stays_between_ack_and_answer() {
        let mut c = Chat::default();
        c.driver_message("and in T4?", 1);
        c.apply(CoachEvent::Chunk {
            turn: 1,
            text: "Let me look. ".into(),
        });
        c.apply(CoachEvent::ToolCall {
            turn: 1,
            id: "t1".into(),
            name: "mcp__coach__get_corner_summary".into(),
            status: "Pending".into(),
        });
        c.apply(CoachEvent::Chunk {
            turn: 1,
            text: "You brake ".into(),
        });
        // The duration arrives after the reply has started: it completes the existing line.
        c.sync_tools(&[ToolCallLog {
            source: "conversation".into(),
            name: "get_corner_summary".into(),
            ok: true,
            millis: 4.0,
        }]);
        c.apply(CoachEvent::Chunk {
            turn: 1,
            text: "too early.".into(),
        });
        c.apply(CoachEvent::ToolCall {
            turn: 1,
            id: "t1".into(),
            name: "mcp__coach__get_corner_summary".into(),
            status: "Completed".into(),
        });
        let texts: Vec<&str> = c.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "and in T4?",
                "Let me look. ",
                "get_corner_summary · 4 ms",
                "You brake too early."
            ]
        );
    }

    #[test]
    fn context_names_corners_around_the_car() {
        use telemetry::synthetic::SyntheticTrack;
        use telemetry::{CornerDetectionParams, Lap, LapFlags, Sample};
        let track = SyntheticTrack::demo();
        let samples = track.simulate_lap(1.0, 0.0, 0.0);
        let lap = Lap {
            number: 1,
            start_time: 0.0,
            lap_time: samples.len() as f64 / 60.0,
            flags: LapFlags {
                complete: true,
                ..Default::default()
            },
            samples,
        };
        let a = Analyzer::new(
            &lap,
            Some(track.length_m()),
            &CornerDetectionParams::default(),
        );
        // Between T3 and T4.
        let between = (a.model.corners[2].end_pct + a.model.corners[3].start_pct) / 2.0;
        let snap = LiveSnapshot {
            sample: Some(Sample {
                lap: 3,
                lap_dist_pct: between as f32,
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = driver_context(&snap, Some(&a), &RuntimeStatus::default(), None);
        assert_eq!(ctx.lap, Some(3));
        assert_eq!(ctx.last_corner.as_deref(), Some("T3"));
        assert_eq!(ctx.next_corner.as_deref(), Some("T4"));
    }
}

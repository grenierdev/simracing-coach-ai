//! Automatic planning of the next lap by the analysis session.
//!
//! After every complete, valid lap, the debrief (session overview and active
//! coach triggers) goes to an ACP session separate from the conversation, with
//! a stronger model. Claude digs into the costly corners with the MCP tools,
//! then replaces the coach trigger layer (`update_plan`). The driver's own plan
//! is never modified. The analysis blocks neither the fast loop nor the
//! conversation; a lap completed during an analysis is skipped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use coach::{AgentStatus, CoachAgent, CoachEvent};
use crossbeam_channel::{Receiver, select};
use telemetry::{Lap, LiveSnapshot, TelemetryEvent};
use triggers::{Plan, TriggerRuntime};

#[derive(Debug, Clone, Copy)]
pub struct PlannerPolicy {
    /// One analysis every N valid laps.
    pub every_n_laps: u32,
}

impl Default for PlannerPolicy {
    fn default() -> Self {
        PlannerPolicy { every_n_laps: 1 }
    }
}

/// Should this lap be analyzed? `valid_since_last`: valid laps since the last
/// analysis, this one included.
pub fn should_analyze(
    lap: &Lap,
    valid_since_last: u32,
    busy: bool,
    policy: &PlannerPolicy,
) -> bool {
    lap.is_valid() && !busy && valid_since_last >= policy.every_n_laps.max(1)
}

fn lap_time(secs: f64) -> String {
    let ms = (secs * 1000.0).round() as u64;
    format!("{}:{:02}.{:03}", ms / 60_000, (ms / 1000) % 60, ms % 1000)
}

/// Message sent to the analysis session.
pub fn analysis_prompt(
    lap: &Lap,
    overview: &serde_json::Value,
    coach_plan: Option<&Plan>,
) -> String {
    let active = match coach_plan {
        Some(p) if !p.triggers.is_empty() => p
            .triggers
            .iter()
            .map(|c| {
                format!(
                    "{} ({}): \"{}\"",
                    c.id.as_deref().unwrap_or("?"),
                    c.corner.as_deref().unwrap_or("position"),
                    c.text
                )
            })
            .collect::<Vec<_>>()
            .join("; "),
        _ => "none".into(),
    };
    format!(
        "[Analysis] Lap {} completed in {}.\n\
         Overview (get_session_overview): {}\n\
         Active coach triggers: {}\n\
         Prepare the triggers for the next lap (update_plan, mode replace, at most three), then summarize in one sentence.",
        lap.number,
        lap_time(lap.lap_time),
        overview,
        active
    )
}

#[derive(Debug, Clone, Default)]
pub struct PlannerStatus {
    pub enabled: bool,
    pub model: String,
    /// Lap being analyzed.
    pub running: Option<i32>,
    /// Last analysis: lap, summary, duration (s).
    pub last: Option<(i32, String, f64)>,
    pub analyses: u32,
    pub error: Option<String>,
}

pub struct Planner {
    status: Arc<Mutex<PlannerStatus>>,
    enabled: Arc<AtomicBool>,
}

pub type SnapshotFn = Arc<dyn Fn() -> LiveSnapshot + Send + Sync>;

impl Planner {
    pub fn spawn(
        agent: Arc<CoachAgent>,
        model: String,
        laps: Receiver<TelemetryEvent>,
        snapshot: SnapshotFn,
        triggers: Arc<TriggerRuntime>,
        policy: PlannerPolicy,
        enabled: bool,
    ) -> Self {
        let status = Arc::new(Mutex::new(PlannerStatus {
            enabled,
            model,
            ..Default::default()
        }));
        let flag = Arc::new(AtomicBool::new(enabled));
        {
            let (status, flag) = (status.clone(), flag.clone());
            std::thread::Builder::new()
                .name("planner".into())
                .spawn(move || run(agent, laps, snapshot, triggers, policy, status, flag))
                .expect("starting the planner");
        }
        Planner {
            status,
            enabled: flag,
        }
    }

    pub fn status(&self) -> PlannerStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        self.status.lock().unwrap().enabled = on;
    }
}

fn run(
    agent: Arc<CoachAgent>,
    laps: Receiver<TelemetryEvent>,
    snapshot: SnapshotFn,
    triggers: Arc<TriggerRuntime>,
    policy: PlannerPolicy,
    status: Arc<Mutex<PlannerStatus>>,
    enabled: Arc<AtomicBool>,
) {
    let events = agent.events().clone();
    let mut valid_since_last: u32 = 0;
    let mut current: Option<(u64, i32, Instant, String)> = None;
    loop {
        select! {
            recv(laps) -> e => match e {
                Ok(TelemetryEvent::LapCompleted(lap)) => {
                    if lap.is_valid() {
                        valid_since_last += 1;
                    }
                    let busy = current.is_some();
                    if !enabled.load(Ordering::Relaxed)
                        || !should_analyze(&lap, valid_since_last, busy, &policy)
                    {
                        continue;
                    }
                    let Some(analyzer) = triggers.analyzer() else {
                        status.lock().unwrap().error = Some("no reference lap".into());
                        continue;
                    };
                    let overview = mcp::tools::session_overview(&snapshot(), Some(&analyzer));
                    let prompt = analysis_prompt(&lap, &overview, triggers.coach_plan().as_ref());
                    let turn = agent.prompt(prompt);
                    valid_since_last = 0;
                    current = Some((turn, lap.number, Instant::now(), String::new()));
                    let mut s = status.lock().unwrap();
                    s.running = Some(lap.number);
                    s.error = None;
                }
                Ok(_) => {}
                Err(_) => break,
            },
            recv(events) -> e => match e {
                Ok(CoachEvent::Chunk { turn, text }) => {
                    if let Some((t, _, _, summary)) = current.as_mut()
                        && *t == turn
                    {
                        summary.push_str(&text);
                    }
                }
                Ok(CoachEvent::TurnEnd { turn, .. }) => {
                    if let Some((t, lap, started, summary)) = current.take() {
                        if t != turn {
                            current = Some((t, lap, started, summary));
                            continue;
                        }
                        // The summary is the last line of the reply.
                        let summary = summary
                            .trim()
                            .rsplit('\n')
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        let mut s = status.lock().unwrap();
                        s.running = None;
                        s.analyses += 1;
                        s.last = Some((lap, summary, started.elapsed().as_secs_f64()));
                    }
                }
                Ok(CoachEvent::Status(AgentStatus::Failed(err))) => {
                    let mut s = status.lock().unwrap();
                    s.running = None;
                    s.error = Some(err);
                    current = None;
                }
                Ok(CoachEvent::Error(err)) => status.lock().unwrap().error = Some(err),
                Ok(_) => {}
                Err(_) => break,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::LapFlags;

    fn lap(valid: bool) -> Lap {
        Lap {
            number: 4,
            start_time: 0.0,
            lap_time: 83.456,
            flags: LapFlags {
                complete: true,
                off_track: if valid { 0 } else { 1 },
                ..Default::default()
            },
            samples: vec![],
        }
    }

    #[test]
    fn policy() {
        let p = PlannerPolicy { every_n_laps: 2 };
        assert!(!should_analyze(&lap(true), 1, false, &p));
        assert!(should_analyze(&lap(true), 2, false, &p));
        assert!(
            !should_analyze(&lap(true), 2, true, &p),
            "analysis already running"
        );
        assert!(!should_analyze(&lap(false), 3, false, &p), "invalid lap");
    }

    #[test]
    fn prompt_lists_overview_and_active_triggers() {
        let plan =
            Plan::parse("[[trigger]]\nid = \"auto-T4\"\ncorner = \"T4\"\ntext = \"Patience.\"")
                .unwrap();
        let p = analysis_prompt(&lap(true), &serde_json::json!({"x": 1}), Some(&plan));
        assert!(p.starts_with("[Analysis] Lap 4 completed in 1:23.456."));
        assert!(p.contains("{\"x\":1}"));
        assert!(p.contains("auto-T4 (T4): \"Patience.\""));
        assert!(
            analysis_prompt(&lap(true), &serde_json::json!({}), None).contains("triggers: none")
        );
    }
}

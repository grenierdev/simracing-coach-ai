//! Focus coaching loop, driven by the analysis session.
//!
//! An analysis takes about 15 s: waiting for the end of the lap meant that the
//! new cues arrived after the first corners of the next lap. The loop works on
//! one or two corners at a time instead, and analyzes each pass through them
//! as soon as the car leaves the last one:
//!
//! 1. **Waiting for a lap**: a complete, valid lap is needed to choose.
//! 2. **Choosing**: the race engineer reads the lap, picks one corner or two
//!    consecutive corners (`set_focus`), tells the driver, and places cues on
//!    them (`update_plan`). The driver can set a save point before them.
//! 3. **Practicing**: every pass through the focus corners is compared to the
//!    reference right after the last corner. The program speaks the time at
//!    once; the race engineer then adjusts the cues for the next attempt while
//!    the driver goes around (or resets). This repeats until the gap closes,
//!    the progress stalls, or someone asks to move on (`next_focus`, from the
//!    driver through the conversation, or the button). Then back to 2, with
//!    the corners already worked on set aside.
//!
//! The driver's own plan is never modified. Nothing here blocks the fast loop
//! or the conversation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use analysis::{Analyzer, CornerComparison, PassCapture};
use coach::{AgentStatus, CoachAgent, CoachEvent};
use crossbeam_channel::{Receiver, select};
use mcp::{FocusAttempt, FocusCommand, FocusLink, FocusPhase, FocusStatus};
use telemetry::{Lap, LiveSnapshot, Sample, TelemetryEvent};
use triggers::plan::PlanSettings;
use triggers::{Plan, SayWhen, TriggerRuntime};
use voice::Language;

/// When to leave the focus corners.
#[derive(Debug, Clone, Copy)]
pub struct PlannerPolicy {
    /// Gap closed: loss over the focus corners at or below this, s.
    pub collapse_s: f64,
    /// Progress stalled: the last `plateau_attempts` valid attempts did not
    /// beat the earlier best by `plateau_gain_s`.
    pub plateau_attempts: usize,
    pub plateau_gain_s: f64,
    /// At most this many attempts on the same corners.
    pub max_attempts: usize,
}

impl Default for PlannerPolicy {
    fn default() -> Self {
        PlannerPolicy {
            collapse_s: 0.05,
            plateau_attempts: 3,
            plateau_gain_s: 0.03,
            max_attempts: 12,
        }
    }
}

/// Why the focus corners were left.
#[derive(Debug, Clone, PartialEq)]
pub enum Exit {
    GapClosed,
    NoProgress,
    MaxAttempts,
    Asked(String),
}

impl Exit {
    fn label(&self) -> String {
        match self {
            Exit::GapClosed => "gap closed".into(),
            Exit::NoProgress => "no more progress".into(),
            Exit::MaxAttempts => "attempt limit".into(),
            Exit::Asked(r) => r.clone(),
        }
    }
}

/// Decides whether to leave the focus corners after the attempts so far
/// (losses in s, `None` for an attempt that went off track).
pub fn verdict(attempts: &[Option<f64>], policy: &PlannerPolicy) -> Option<Exit> {
    let valid: Vec<f64> = attempts.iter().flatten().copied().collect();
    if let Some(&last) = valid.last()
        && attempts.last().is_some_and(|a| a.is_some())
        && last <= policy.collapse_s
    {
        return Some(Exit::GapClosed);
    }
    if attempts.len() >= policy.max_attempts {
        return Some(Exit::MaxAttempts);
    }
    let k = policy.plateau_attempts.max(1);
    if valid.len() > k {
        let best = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
        let (before, recent) = valid.split_at(valid.len() - k);
        if best(recent) > best(before) - policy.plateau_gain_s {
            return Some(Exit::NoProgress);
        }
    }
    None
}

/// Last analysis finished, for the chat and the screens.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisSummary {
    /// Increases with each analysis.
    pub seq: u32,
    /// What was analyzed: "Lap 4", "T5, T6 attempt 3".
    pub title: String,
    pub text: String,
    pub secs: f64,
}

#[derive(Debug, Clone, Default)]
pub struct PlannerStatus {
    pub enabled: bool,
    pub model: String,
    /// What is being analyzed now.
    pub running: Option<String>,
    pub last: Option<AnalysisSummary>,
    pub analyses: u32,
    pub error: Option<String>,
    pub focus: FocusStatus,
}

pub struct Planner {
    status: Arc<Mutex<PlannerStatus>>,
    enabled: Arc<AtomicBool>,
    link: FocusLink,
}

pub type SnapshotFn = Arc<dyn Fn() -> LiveSnapshot + Send + Sync>;

/// What the loop is connected to.
pub struct PlannerDeps {
    pub agent: Arc<CoachAgent>,
    pub model: String,
    /// Hub events (completed laps).
    pub laps: Receiver<TelemetryEvent>,
    /// Every telemetry sample (`TelemetryHub::subscribe_samples`).
    pub samples: Receiver<Sample>,
    pub snapshot: SnapshotFn,
    pub triggers: Arc<TriggerRuntime>,
    /// Shared with the MCP servers and the conversation.
    pub link: FocusLink,
    pub commands: Receiver<FocusCommand>,
    /// Language of the sentences the program speaks itself.
    pub language: Language,
}

impl Planner {
    pub fn spawn(deps: PlannerDeps, policy: PlannerPolicy, enabled: bool) -> Self {
        let status = Arc::new(Mutex::new(PlannerStatus {
            enabled,
            model: deps.model.clone(),
            ..Default::default()
        }));
        let flag = Arc::new(AtomicBool::new(enabled));
        let link = deps.link.clone();
        {
            let (status, flag) = (status.clone(), flag.clone());
            std::thread::Builder::new()
                .name("planner".into())
                .spawn(move || Loop::new(deps, policy, status, flag).run())
                .expect("starting the planner");
        }
        Planner {
            status,
            enabled: flag,
            link,
        }
    }

    pub fn status(&self) -> PlannerStatus {
        let mut s = self.status.lock().unwrap().clone();
        s.focus = self.link.status();
        s
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        self.status.lock().unwrap().enabled = on;
    }

    /// Leaves the focus corners (button on the screens).
    pub fn next_focus(&self) {
        let _ = self.link.commands.send(FocusCommand::Next {
            reason: "the driver asked to move on".into(),
        });
    }
}

struct Attempt {
    number: u32,
    lap: i32,
    corners: Vec<CornerComparison>,
    loss: f64,
    off_track: bool,
}

struct Focus {
    corners: Vec<usize>,
    labels: Vec<String>,
    analyzer: Arc<Analyzer>,
    capture: PassCapture,
    /// Per-corner loss on the lap the corners were chosen from, s.
    baseline: Option<(i32, Vec<f64>)>,
    attempts: Vec<Attempt>,
    /// Attempts already sent to the race engineer.
    analyzed: u32,
}

impl Focus {
    fn baseline_s(&self) -> Option<f64> {
        self.baseline.as_ref().map(|(_, v)| v.iter().sum())
    }
}

enum Phase {
    Off,
    WaitingForLap,
    Choosing,
    Practicing(Box<Focus>),
}

#[derive(Clone)]
enum TurnKind {
    Choose,
    Attempt,
}

struct Turn {
    id: u64,
    kind: TurnKind,
    title: String,
    started: Instant,
    text: String,
}

struct Loop {
    deps: PlannerDeps,
    policy: PlannerPolicy,
    status: Arc<Mutex<PlannerStatus>>,
    enabled: Arc<AtomicBool>,
    phase: Phase,
    turn: Option<Turn>,
    /// A choice to make once the current turn ends.
    choose_pending: bool,
    last_valid: Option<Arc<Lap>>,
    finished: Vec<String>,
    /// Corners worked on, not to be chosen again right away.
    done_labels: Vec<String>,
    seq: u32,
}

impl Loop {
    fn new(
        deps: PlannerDeps,
        policy: PlannerPolicy,
        status: Arc<Mutex<PlannerStatus>>,
        enabled: Arc<AtomicBool>,
    ) -> Self {
        Loop {
            deps,
            policy,
            status,
            enabled,
            phase: Phase::Off,
            turn: None,
            choose_pending: false,
            last_valid: None,
            finished: vec![],
            done_labels: vec![],
            seq: 0,
        }
    }

    fn run(mut self) {
        let laps = self.deps.laps.clone();
        let mut samples = self.deps.samples.clone();
        let mut commands = self.deps.commands.clone();
        let events = self.deps.agent.events().clone();
        self.sync_enabled();
        self.publish();
        loop {
            select! {
                recv(laps) -> e => match e {
                    Ok(TelemetryEvent::LapCompleted(lap)) => {
                        self.sync_enabled();
                        self.on_lap(lap);
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
                recv(samples) -> s => match s {
                    Ok(s) => self.on_sample(&s),
                    // A closed channel is always ready: stop listening to it.
                    Err(_) => samples = crossbeam_channel::never(),
                },
                recv(commands) -> c => match c {
                    Ok(c) => {
                        self.sync_enabled();
                        self.on_command(c);
                    }
                    Err(_) => commands = crossbeam_channel::never(),
                },
                recv(events) -> e => match e {
                    Ok(e) => self.on_agent(e),
                    Err(_) => break,
                },
            }
        }
    }

    fn sync_enabled(&mut self) {
        let on = self.enabled.load(Ordering::Relaxed);
        match (&self.phase, on) {
            (Phase::Off, true) => {
                self.phase = Phase::WaitingForLap;
                self.publish();
            }
            (Phase::Off, false) => {}
            (_, false) => {
                self.phase = Phase::Off;
                self.choose_pending = false;
                self.publish();
            }
            _ => {}
        }
    }

    fn on_lap(&mut self, lap: Arc<Lap>) {
        if !lap.is_valid() {
            return;
        }
        self.last_valid = Some(lap);
        if matches!(self.phase, Phase::WaitingForLap) {
            self.start_choosing();
        }
    }

    fn on_sample(&mut self, s: &Sample) {
        let Phase::Practicing(focus) = &mut self.phase else {
            return;
        };
        let Some(pass) = focus.capture.push(s) else {
            return;
        };
        // A new reference changes the track map: the corners no longer apply.
        if !self
            .deps
            .triggers
            .analyzer()
            .is_some_and(|a| Arc::ptr_eq(&a, &focus.analyzer))
        {
            self.phase = Phase::WaitingForLap;
            self.publish();
            return;
        }
        let Some(cmp) = focus.analyzer.compare_pass(&pass) else {
            return;
        };
        let corners: Vec<CornerComparison> = focus
            .corners
            .iter()
            .map(|&i| cmp.corners[i].clone())
            .collect();
        let loss: f64 = corners.iter().map(|c| c.time_delta).sum();
        let previous_best = focus
            .attempts
            .iter()
            .filter(|a| !a.off_track)
            .map(|a| a.loss)
            .min_by(f64::total_cmp)
            .or(focus.baseline_s());
        let number = focus.attempts.len() as u32 + 1;
        tracing::info!(
            "focus {}: attempt {number}, {loss:+.3} s{}",
            focus.labels.join(", "),
            if pass.off_track { " (off track)" } else { "" }
        );
        focus.attempts.push(Attempt {
            number,
            lap: pass.lap,
            corners,
            loss,
            off_track: pass.off_track,
        });
        // The time, at once: the race engineer's advice follows.
        self.deps.triggers.say(
            attempt_sentence(self.deps.language, loss, previous_best, pass.off_track),
            SayWhen::Now,
        );
        let history: Vec<Option<f64>> = focus
            .attempts
            .iter()
            .map(|a| (!a.off_track).then_some(a.loss))
            .collect();
        if let Some(exit) = verdict(&history, &self.policy) {
            self.leave_focus(exit);
        } else if self.turn.is_none() {
            self.send_attempt();
        }
        self.publish();
    }

    fn on_command(&mut self, c: FocusCommand) {
        if matches!(self.phase, Phase::Off) {
            return;
        }
        match c {
            FocusCommand::Set {
                corners,
                announcement,
            } => self.set_focus(corners, announcement),
            FocusCommand::Next { reason } => {
                if matches!(self.phase, Phase::Practicing(_)) {
                    self.leave_focus(Exit::Asked(reason));
                } else if matches!(self.phase, Phase::WaitingForLap) {
                    self.start_choosing();
                }
            }
        }
        self.publish();
    }

    fn set_focus(&mut self, corners: Vec<usize>, announcement: Option<String>) {
        let Some(analyzer) = self.deps.triggers.analyzer() else {
            self.error("no reference lap");
            return;
        };
        let Some(window) = analyzer.window_for(&corners) else {
            return;
        };
        let labels: Vec<String> = corners
            .iter()
            .filter_map(|&i| analyzer.model.corners.get(i).map(|c| c.label()))
            .collect();
        let baseline = self.last_valid.as_ref().map(|lap| {
            let cmp = analyzer.compare(lap);
            (
                lap.number,
                corners.iter().map(|&i| cmp.corners[i].time_delta).collect(),
            )
        });
        let text = announcement.unwrap_or_else(|| announce(self.deps.language, &labels));
        self.deps.triggers.say(text, SayWhen::NextStraight);
        tracing::info!("focus: {}", labels.join(", "));
        self.phase = Phase::Practicing(Box::new(Focus {
            corners,
            labels,
            capture: PassCapture::new(window),
            analyzer,
            baseline,
            attempts: vec![],
            analyzed: 0,
        }));
    }

    fn leave_focus(&mut self, exit: Exit) {
        let Phase::Practicing(focus) = std::mem::replace(&mut self.phase, Phase::WaitingForLap)
        else {
            return;
        };
        let best = focus
            .attempts
            .iter()
            .filter(|a| !a.off_track)
            .map(|a| a.loss)
            .min_by(f64::total_cmp);
        let from = focus
            .baseline_s()
            .map(|b| format!("{b:+.2} s → "))
            .unwrap_or_default();
        self.finished.push(format!(
            "{}: {from}{} in {} attempts ({})",
            focus.labels.join(", "),
            best.map(|b| format!("{b:+.2} s"))
                .unwrap_or("no clean pass".into()),
            focus.attempts.len(),
            exit.label()
        ));
        self.done_labels.extend(focus.labels.iter().cloned());
        tracing::info!("focus {} left: {}", focus.labels.join(", "), exit.label());
        self.deps.triggers.set_coach_plan(Plan::default());
        self.deps.triggers.say(
            exit_sentence(self.deps.language, &focus.labels, &exit),
            SayWhen::NextStraight,
        );
        self.start_choosing();
    }

    fn start_choosing(&mut self) {
        if self.last_valid.is_none() {
            self.phase = Phase::WaitingForLap;
            return;
        }
        self.phase = Phase::Choosing;
        if self.turn.is_some() {
            self.choose_pending = true;
        } else {
            self.send_choose();
        }
    }

    fn send_choose(&mut self) {
        self.choose_pending = false;
        let Some(lap) = self.last_valid.clone() else {
            self.phase = Phase::WaitingForLap;
            return;
        };
        let Some(analyzer) = self.deps.triggers.analyzer() else {
            self.error("no reference lap");
            self.phase = Phase::WaitingForLap;
            return;
        };
        let overview = mcp::tools::session_overview(&(self.deps.snapshot)(), Some(&analyzer));
        let windows = triggers::speaking_windows(&analyzer, &self.plan_settings());
        let prompt = choose_prompt(
            &lap,
            analyzer.reference.lap_time,
            &overview,
            &self.finished,
            &windows,
        );
        self.prompt(TurnKind::Choose, format!("Lap {}", lap.number), prompt);
    }

    fn send_attempt(&mut self) {
        let settings = self.plan_settings();
        let coach_plan = self.deps.triggers.coach_plan();
        let Phase::Practicing(focus) = &mut self.phase else {
            return;
        };
        let Some(last) = focus.attempts.last() else {
            return;
        };
        if last.number <= focus.analyzed {
            return;
        }
        let windows = triggers::speaking_windows(&focus.analyzer, &settings);
        let prompt = attempt_prompt(focus, &windows, coach_plan.as_ref());
        let title = format!("{} attempt {}", focus.labels.join(", "), last.number);
        focus.analyzed = last.number;
        self.prompt(TurnKind::Attempt, title, prompt);
    }

    fn plan_settings(&self) -> PlanSettings {
        self.deps
            .triggers
            .current_plan()
            .map(|(_, p)| p.settings)
            .unwrap_or_default()
    }

    fn prompt(&mut self, kind: TurnKind, title: String, text: String) {
        let id = self.deps.agent.prompt(text);
        {
            let mut s = self.status.lock().unwrap();
            s.running = Some(title.clone());
            s.error = None;
        }
        self.turn = Some(Turn {
            id,
            kind,
            title,
            started: Instant::now(),
            text: String::new(),
        });
    }

    fn on_agent(&mut self, e: CoachEvent) {
        match e {
            CoachEvent::Chunk { turn, text } => {
                if let Some(t) = self.turn.as_mut()
                    && t.id == turn
                {
                    t.text.push_str(&text);
                }
            }
            CoachEvent::TurnEnd { turn, .. } => {
                if self.turn.as_ref().is_none_or(|t| t.id != turn) {
                    return;
                }
                let t = self.turn.take().unwrap();
                self.seq += 1;
                {
                    let mut s = self.status.lock().unwrap();
                    s.running = None;
                    s.analyses += 1;
                    s.last = Some(AnalysisSummary {
                        seq: self.seq,
                        title: t.title.clone(),
                        text: last_line(&t.text),
                        secs: t.started.elapsed().as_secs_f64(),
                    });
                }
                // The race engineer did not choose: take the most costly
                // corner not worked on yet.
                if matches!(t.kind, TurnKind::Choose) && matches!(self.phase, Phase::Choosing) {
                    self.fallback_choice();
                }
                self.next_turn();
                self.publish();
            }
            CoachEvent::Status(AgentStatus::Failed(err)) => {
                self.turn = None;
                self.error(&err);
            }
            CoachEvent::Error(err) => self.error(&err),
            _ => {}
        }
    }

    /// Sends what waited for the end of the turn.
    fn next_turn(&mut self) {
        if self.turn.is_some() {
            return;
        }
        if self.choose_pending && matches!(self.phase, Phase::Choosing) {
            self.send_choose();
        } else if matches!(self.phase, Phase::Practicing(_)) {
            self.send_attempt();
        }
    }

    fn fallback_choice(&mut self) {
        let (Some(lap), Some(analyzer)) = (self.last_valid.clone(), self.deps.triggers.analyzer())
        else {
            self.phase = Phase::WaitingForLap;
            return;
        };
        let cmp = analyzer.compare(&lap);
        let pick = cmp
            .ranked()
            .into_iter()
            .find(|c| {
                c.time_delta > self.policy.collapse_s && !self.done_labels.contains(&c.label())
            })
            .and_then(|c| {
                analyzer
                    .model
                    .corners
                    .iter()
                    .position(|k| k.label() == c.label())
            });
        match pick {
            Some(i) => self.set_focus(vec![i], None),
            None => self.phase = Phase::WaitingForLap,
        }
    }

    fn error(&mut self, e: &str) {
        let mut s = self.status.lock().unwrap();
        s.running = None;
        s.error = Some(e.to_string());
    }

    /// Shares the state with the screens, the MCP tools and the conversation.
    fn publish(&self) {
        let mut st = FocusStatus {
            finished: self.finished.clone(),
            analyzing: self.turn.is_some(),
            ..Default::default()
        };
        match &self.phase {
            Phase::Off => st.phase = FocusPhase::Off,
            Phase::WaitingForLap => st.phase = FocusPhase::WaitingForLap,
            Phase::Choosing => st.phase = FocusPhase::Choosing,
            Phase::Practicing(f) => {
                st.phase = FocusPhase::Practicing;
                st.corners = f.labels.clone();
                st.baseline_s = f.baseline_s();
                st.attempts = f
                    .attempts
                    .iter()
                    .map(|a| FocusAttempt {
                        number: a.number,
                        lap: a.lap,
                        loss_s: a.loss,
                        corners: a
                            .corners
                            .iter()
                            .map(|c| (c.label(), c.time_delta))
                            .collect(),
                        off_track: a.off_track,
                    })
                    .collect();
            }
        }
        *self.deps.link.status.lock().unwrap() = st;
    }
}

/// The summary is the last non-empty line of the reply.
fn last_line(text: &str) -> String {
    text.trim()
        .rsplit('\n')
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn lap_time(secs: f64) -> String {
    let ms = (secs * 1000.0).round() as u64;
    format!("{}:{:02}.{:03}", ms / 60_000, (ms / 1000) % 60, ms % 1000)
}

fn windows_line(windows: &[(String, Option<f64>)], only: Option<&[String]>) -> String {
    let parts: Vec<String> = windows
        .iter()
        .filter(|(l, _)| only.is_none_or(|o| o.contains(l)))
        .filter_map(|(l, w)| {
            w.map(|w| format!("{l} {w:.1} s (about {} characters)", triggers::max_chars(w)))
        })
        .collect();
    if parts.is_empty() {
        "unknown".into()
    } else {
        parts.join(", ")
    }
}

/// Message asking the race engineer to choose the focus corners.
pub fn choose_prompt(
    lap: &Lap,
    reference_time: f64,
    overview: &serde_json::Value,
    finished: &[String],
    windows: &[(String, Option<f64>)],
) -> String {
    format!(
        "[Focus] Choose the corners to work on.\n\
         Lap {} completed in {} ({:+.3} s on the reference).\n\
         Overview (get_session_overview): {}\n\
         Already worked on this session: {}\n\
         Speaking time on the straight before each braking zone, at reference pace: {}\n\
         Pick one corner, or two consecutive corners when they are linked, where the most time can be gained and that were not worked on yet. Call set_focus with them and a short announcement, then update_plan (mode replace) with at most one cue per focus corner, short enough for its speaking time. Finish with one summary sentence.",
        lap.number,
        lap_time(lap.lap_time),
        lap.lap_time - reference_time,
        overview,
        if finished.is_empty() {
            "nothing yet".into()
        } else {
            finished.join("; ")
        },
        windows_line(windows, None)
    )
}

/// What one corner of a pass did differently from the reference.
pub fn corner_line(c: &CornerComparison) -> String {
    let mut parts = vec![format!("{} {:+.2} s", c.label(), c.time_delta)];
    if let Some(d) = c.brake_point_delta_m {
        parts.push(format!("brake point {d:+.0} m"));
    }
    if c.min_speed_delta.is_finite() {
        parts.push(format!("min speed {:+.0} km/h", c.min_speed_delta * 3.6));
    }
    if let Some(d) = c.throttle_on_delta_m {
        parts.push(format!("throttle on {d:+.0} m"));
    }
    if let Some(d) = c.turn_in_delta_m {
        parts.push(format!("turn-in {d:+.0} m"));
    }
    let coast = c.lap.coasting_s - c.reference.coasting_s;
    if coast.abs() >= 0.05 {
        parts.push(format!("coasting {coast:+.2} s"));
    }
    if let Some(r) = c.min_radius_ratio {
        parts.push(format!("radius x{r:.2}"));
    }
    if let Some(r) = c.steering_peak_ratio {
        parts.push(format!("steering x{r:.2}"));
    }
    if let Some(r) = c.rotation_entry_ratio {
        parts.push(format!("entry rotation x{r:.2}"));
    }
    parts.join(", ")
}

fn attempt_prompt(
    focus: &Focus,
    windows: &[(String, Option<f64>)],
    coach_plan: Option<&Plan>,
) -> String {
    let last = focus.attempts.last().expect("an attempt");
    let fresh: Vec<&Attempt> = focus
        .attempts
        .iter()
        .filter(|a| a.number > focus.analyzed)
        .collect();
    let earlier: Vec<String> = focus
        .attempts
        .iter()
        .filter(|a| a.number < last.number)
        .map(|a| {
            format!(
                "{}: {:+.2} s{}",
                a.number,
                a.loss,
                if a.off_track { " (off track)" } else { "" }
            )
        })
        .collect();
    let detail: Vec<String> = fresh
        .iter()
        .map(|a| {
            format!(
                "Attempt {} (lap {}): {:+.2} s{}. {}",
                a.number,
                a.lap,
                a.loss,
                if a.off_track { ", off track" } else { "" },
                a.corners
                    .iter()
                    .map(corner_line)
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })
        .collect();
    let start = match &focus.baseline {
        Some((lap, v)) => format!("{:+.2} s on lap {lap}", v.iter().sum::<f64>()),
        None => "unknown".into(),
    };
    let active = match coach_plan {
        Some(p) if !p.triggers.is_empty() => p
            .triggers
            .iter()
            .map(|c| format!("{}: \"{}\"", c.id.as_deref().unwrap_or("?"), c.text))
            .collect::<Vec<_>>()
            .join("; "),
        _ => "none".into(),
    };
    format!(
        "[Attempt {} on {}] Pass analyzed right after the last focus corner.\n\
         {}\n\
         Conventions: loss > 0 = slower than the reference; brake point, throttle on and turn-in > 0 = later; min speed < 0 = slower; radius < 1 = tighter line; steering > 1 = more steering.\n\
         Start: {start}. Earlier attempts: {}.\n\
         The driver has already heard the time. Active cues: {active}.\n\
         Speaking time before the focus corners: {}.\n\
         If one correction matters most, say it now in one short sentence (say, when = now). Update the cues for the next attempt only if the advice changes (update_plan, mode replace, at most one cue per focus corner). If these corners have nothing more to give, call next_focus. Finish with one summary sentence.",
        last.number,
        focus.labels.join(", "),
        detail.join("\n"),
        if earlier.is_empty() {
            "none".into()
        } else {
            earlier.join(", ")
        },
        windows_line(windows, Some(&focus.labels)),
    )
}

/// "T5" → "turn 5"; two corners → "turns 5 and 6".
fn spoken_corners(lang: Language, labels: &[String]) -> String {
    let nums: Vec<String> = labels
        .iter()
        .map(|l| l.trim_start_matches(['T', 't']).to_string())
        .collect();
    match (lang, nums.as_slice()) {
        (Language::French, [one]) => format!("virage {one}"),
        (Language::French, many) => format!("virages {}", join_and(many, "et")),
        (_, [one]) => format!("turn {one}"),
        (_, many) => format!("turns {}", join_and(many, "and")),
    }
}

fn join_and(items: &[String], and: &str) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} {and} {last}", init.join(", ")),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Spoken right after a pass: the time against the reference and the trend.
pub fn attempt_sentence(
    lang: Language,
    loss: f64,
    previous_best: Option<f64>,
    off_track: bool,
) -> String {
    let fr = lang == Language::French;
    if off_track {
        return if fr {
            "Sortie de piste, ce passage ne compte pas.".into()
        } else {
            "Off track, that one does not count.".into()
        };
    }
    let tenths = (loss.abs() * 10.0).round() as u32;
    let time = match (fr, tenths, loss > 0.0) {
        (false, 0, _) => "level with the reference".to_string(),
        (false, n, true) => format!("{n} tenth{} off", if n > 1 { "s" } else { "" }),
        (false, n, false) => format!("{n} tenth{} up", if n > 1 { "s" } else { "" }),
        (true, 0, _) => "dans le temps de la référence".to_string(),
        (true, n, true) => format!("{n} dixième{} de retard", if n > 1 { "s" } else { "" }),
        (true, n, false) => format!("{n} dixième{} d'avance", if n > 1 { "s" } else { "" }),
    };
    let trend = match previous_best {
        Some(b) if loss < b - 0.03 => {
            if fr {
                ", meilleur passage"
            } else {
                ", best yet"
            }
        }
        Some(b) if loss > b + 0.1 => {
            if fr {
                ", plus lent"
            } else {
                ", slower"
            }
        }
        _ => "",
    };
    format!("{}{trend}.", capitalize(&time))
}

/// Default announcement of the focus corners.
pub fn announce(lang: Language, labels: &[String]) -> String {
    let c = spoken_corners(lang, labels);
    if lang == Language::French {
        format!("On travaille le {c}.")
    } else {
        format!("Let's work on {c}.")
    }
}

/// Spoken when leaving the focus corners.
pub fn exit_sentence(lang: Language, labels: &[String], exit: &Exit) -> String {
    let c = spoken_corners(lang, labels);
    match (lang == Language::French, exit) {
        (false, Exit::GapClosed) => capitalize(&format!("{c}: gap closed, moving on.")),
        (false, Exit::Asked(_)) => "OK, moving on.".into(),
        (false, _) => format!("That's it for {c} for now."),
        (true, Exit::GapClosed) => capitalize(&format!("{c} : écart comblé, on passe à la suite.")),
        (true, Exit::Asked(_)) => "D'accord, on passe à autre chose.".into(),
        (true, _) => format!("On s'arrête là pour le {c}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_rules() {
        let p = PlannerPolicy::default();
        assert_eq!(verdict(&[Some(0.30), Some(0.20)], &p), None);
        assert_eq!(
            verdict(&[Some(0.30), Some(0.04)], &p),
            Some(Exit::GapClosed)
        );
        assert_eq!(verdict(&[Some(-0.1)], &p), Some(Exit::GapClosed));
        // Off track does not close the gap.
        assert_eq!(verdict(&[Some(0.30), None], &p), None);
        // Best 0.20 before, then three attempts that do not beat 0.17.
        assert_eq!(
            verdict(&[Some(0.20), Some(0.19), Some(0.25), Some(0.18)], &p),
            Some(Exit::NoProgress)
        );
        // Still improving.
        assert_eq!(
            verdict(&[Some(0.30), Some(0.25), Some(0.20), Some(0.15)], &p),
            None
        );
        let many = vec![Some(0.3); 12];
        assert!(verdict(&many, &p).is_some());
    }

    #[test]
    fn sentences() {
        let en = Language::English;
        assert_eq!(
            attempt_sentence(en, 0.21, Some(0.30), false),
            "2 tenths off, best yet."
        );
        assert_eq!(
            attempt_sentence(en, 0.04, None, false),
            "Level with the reference."
        );
        assert_eq!(attempt_sentence(en, -0.12, None, false), "1 tenth up.");
        assert_eq!(
            attempt_sentence(en, 0.5, Some(0.2), false),
            "5 tenths off, slower."
        );
        assert!(attempt_sentence(en, 0.5, None, true).starts_with("Off track"));
        let fr = Language::French;
        assert_eq!(
            attempt_sentence(fr, 0.31, Some(0.4), false),
            "3 dixièmes de retard, meilleur passage."
        );
        let two = vec!["T5".to_string(), "T6".to_string()];
        assert_eq!(announce(en, &two), "Let's work on turns 5 and 6.");
        assert_eq!(announce(fr, &two[..1]), "On travaille le virage 5.");
        assert_eq!(
            exit_sentence(en, &two, &Exit::GapClosed),
            "Turns 5 and 6: gap closed, moving on."
        );
    }

    #[test]
    fn choose_prompt_lists_overview_finished_and_windows() {
        let lap = Lap {
            number: 4,
            start_time: 0.0,
            lap_time: 83.456,
            flags: Default::default(),
            samples: vec![],
        };
        let p = choose_prompt(
            &lap,
            83.0,
            &serde_json::json!({"x": 1}),
            &["T2: +0.30 s → +0.04 s in 3 attempts (gap closed)".into()],
            &[("T1".into(), Some(4.0)), ("T2".into(), None)],
        );
        assert!(p.starts_with("[Focus] Choose the corners to work on."));
        assert!(p.contains("Lap 4 completed in 1:23.456 (+0.456 s"));
        assert!(p.contains("{\"x\":1}"));
        assert!(p.contains("T2: +0.30 s"));
        assert!(p.contains("T1 4.0 s (about 52 characters)"));
        assert!(p.contains("set_focus"));
    }
}

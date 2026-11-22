//! Turn-by-turn coaching, driven by the analysis session.
//!
//! An analysis takes about 15 s, too long to wait for the end of the lap.
//! Every turn is analyzed instead as soon as the car leaves it:
//!
//! 1. each turn has its own capture over its zone ([`analysis::PassCapture`]);
//!    a pass ends when the car leaves the zone, and is compared to the
//!    reference at once;
//! 2. the turn joins a queue, which keeps only its latest pass; when the race
//!    engineer is free, it receives the queued turns (a few at a time) and
//!    updates their cues with `update_plan`;
//! 3. a cue is spoken before its turn on the next pass: on the next lap, or
//!    right away after a reset before the turn.
//!
//! The driver can drive full laps and follow every cue, or reset to a save
//! point and work on one turn or a sequence of turns: resets do not get in
//! the way (a pass can start from the reset point). After a reset, the time of
//! the first turn is spoken as soon as the car leaves it.
//!
//! The driver's own plan is never modified. Nothing here blocks the fast loop
//! or the conversation.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use analysis::{Analyzer, CornerComparison, Pass, PassCapture};
use coach::{AgentStatus, CoachAgent, CoachEvent};
use crossbeam_channel::{Receiver, select};
use mcp::{CoachingLink, CoachingStatus, TurnStatus};
use telemetry::{Sample, TelemetryEvent};
use triggers::{Plan, SayWhen, TriggerRuntime};
use voice::Language;

#[derive(Debug, Clone, Copy)]
pub struct PlannerPolicy {
    /// Turns sent to the race engineer in one message, at most.
    pub max_batch: usize,
}

impl Default for PlannerPolicy {
    fn default() -> Self {
        PlannerPolicy { max_batch: 4 }
    }
}

/// Last analysis finished, for the chat and the screens.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisSummary {
    /// Increases with each analysis.
    pub seq: u32,
    /// What was analyzed: "T4, T5".
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
    pub coaching: CoachingStatus,
}

pub struct Planner {
    status: Arc<Mutex<PlannerStatus>>,
    enabled: Arc<AtomicBool>,
    link: CoachingLink,
}

/// What the loop is connected to.
pub struct PlannerDeps {
    pub agent: Arc<CoachAgent>,
    pub model: String,
    /// Hub events: drained (the turns come from the samples).
    pub laps: Receiver<TelemetryEvent>,
    /// Every telemetry sample (`TelemetryHub::subscribe_samples`).
    pub samples: Receiver<Sample>,
    pub triggers: Arc<TriggerRuntime>,
    /// Shared with the MCP servers, the conversation and the screens.
    pub link: CoachingLink,
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
        s.coaching = self.link.lock().unwrap().clone();
        s
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        self.status.lock().unwrap().enabled = on;
    }
}

/// One pass through a turn, compared to the reference.
struct PassResult {
    number: u32,
    lap: i32,
    corner: CornerComparison,
    off_track: bool,
    after_reset: bool,
}

impl PassResult {
    fn loss(&self) -> f64 {
        self.corner.time_delta
    }
}

struct TurnHistory {
    label: String,
    passes: Vec<PassResult>,
}

impl TurnHistory {
    fn best(&self) -> Option<f64> {
        self.passes
            .iter()
            .filter(|p| !p.off_track)
            .map(PassResult::loss)
            .min_by(f64::total_cmp)
    }

    fn last_clean(&self) -> Option<f64> {
        self.passes
            .iter()
            .rev()
            .find(|p| !p.off_track)
            .map(PassResult::loss)
    }
}

/// Captures and history for the current track map.
struct Track {
    analyzer: Arc<Analyzer>,
    captures: Vec<PassCapture>,
    turns: Vec<TurnHistory>,
}

impl Track {
    fn new(analyzer: Arc<Analyzer>) -> Self {
        Track {
            captures: analyzer.turn_captures(),
            turns: analyzer
                .model
                .corners
                .iter()
                .map(|c| TurnHistory {
                    label: c.label(),
                    passes: vec![],
                })
                .collect(),
            analyzer,
        }
    }
}

struct Turn {
    id: u64,
    corners: Vec<usize>,
    title: String,
    started: Instant,
    text: String,
}

struct Loop {
    deps: PlannerDeps,
    policy: PlannerPolicy,
    status: Arc<Mutex<PlannerStatus>>,
    enabled: Arc<AtomicBool>,
    track: Option<Track>,
    /// Turns waiting for the race engineer, oldest first, each once.
    queue: VecDeque<usize>,
    turn: Option<Turn>,
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
            track: None,
            queue: VecDeque::new(),
            turn: None,
            seq: 0,
        }
    }

    fn run(mut self) {
        let mut laps = self.deps.laps.clone();
        let mut samples = self.deps.samples.clone();
        let events = self.deps.agent.events().clone();
        self.publish();
        loop {
            select! {
                // A closed channel is always ready: stop listening to it.
                recv(laps) -> e => if e.is_err() {
                    laps = crossbeam_channel::never();
                },
                recv(samples) -> s => match s {
                    Ok(s) => self.on_sample(&s),
                    Err(_) => samples = crossbeam_channel::never(),
                },
                recv(events) -> e => match e {
                    Ok(e) => self.on_agent(e),
                    Err(_) => break,
                },
            }
        }
    }

    fn on_sample(&mut self, s: &Sample) {
        // A new reference changes the track map: start over.
        let Some(analyzer) = self.deps.triggers.analyzer() else {
            if self.track.take().is_some() {
                self.queue.clear();
                self.publish();
            }
            return;
        };
        if self
            .track
            .as_ref()
            .is_none_or(|t| !Arc::ptr_eq(&t.analyzer, &analyzer))
        {
            self.track = Some(Track::new(analyzer));
            self.queue.clear();
            self.publish();
        }
        let track = self.track.as_mut().expect("track set above");
        let done: Vec<(usize, Pass)> = track
            .captures
            .iter_mut()
            .enumerate()
            .filter_map(|(i, c)| c.push(s).map(|p| (i, p)))
            .collect();
        for (i, pass) in done {
            self.on_pass(i, pass);
        }
    }

    fn on_pass(&mut self, i: usize, pass: Pass) {
        let Some(track) = self.track.as_mut() else {
            return;
        };
        let Some(cmp) = track.analyzer.compare_pass(&pass) else {
            return;
        };
        let turn = &mut track.turns[i];
        let previous_best = turn.best();
        let result = PassResult {
            number: turn.passes.len() as u32 + 1,
            lap: pass.lap,
            corner: cmp.corners[i].clone(),
            off_track: pass.off_track,
            after_reset: pass.after_reset,
        };
        tracing::info!(
            "{} pass {}: {:+.3} s{}{}",
            turn.label,
            result.number,
            result.loss(),
            if result.after_reset {
                " (after a reset)"
            } else {
                ""
            },
            if result.off_track { " (off track)" } else { "" }
        );
        // Working on a turn from a save point: its time, at once.
        if result.after_reset {
            let sentence = format!(
                "{}. {}",
                capitalize(&spoken_corners(
                    self.deps.language,
                    std::slice::from_ref(&turn.label)
                )),
                attempt_sentence(
                    self.deps.language,
                    result.loss(),
                    previous_best,
                    result.off_track
                )
            );
            self.deps.triggers.say(sentence, SayWhen::Now);
        }
        turn.passes.push(result);
        if self.enabled.load(Ordering::Relaxed) && !self.queue.contains(&i) {
            self.queue.push_back(i);
        }
        if self.turn.is_none() {
            self.send_batch();
        }
        self.publish();
    }

    /// Sends the oldest queued turns to the race engineer.
    fn send_batch(&mut self) {
        if !self.enabled.load(Ordering::Relaxed) {
            self.queue.clear();
            return;
        }
        let Some(track) = self.track.as_ref() else {
            return;
        };
        let n = self.policy.max_batch.max(1).min(self.queue.len());
        if n == 0 {
            return;
        }
        let corners: Vec<usize> = self.queue.drain(..n).collect();
        let settings = self
            .deps
            .triggers
            .current_plan()
            .map(|(_, p)| p.settings)
            .unwrap_or_default();
        let windows = triggers::speaking_windows(&track.analyzer, &settings);
        let prompt = turns_prompt(
            track,
            &corners,
            &windows,
            self.deps.triggers.coach_plan().as_ref(),
            self.deps.language,
        );
        let title = corners
            .iter()
            .map(|&i| track.turns[i].label.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let id = self.deps.agent.prompt(prompt);
        {
            let mut s = self.status.lock().unwrap();
            s.running = Some(title.clone());
            s.error = None;
        }
        self.turn = Some(Turn {
            id,
            corners,
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
                self.send_batch();
                self.publish();
            }
            CoachEvent::Status(AgentStatus::Failed(err)) => {
                self.turn = None;
                self.error(&err);
                self.publish();
            }
            CoachEvent::Error(err) => self.error(&err),
            _ => {}
        }
    }

    fn error(&mut self, e: &str) {
        let mut s = self.status.lock().unwrap();
        s.running = None;
        s.error = Some(e.to_string());
    }

    /// Shares the state with the screens, the MCP tools and the conversation.
    fn publish(&self) {
        let analyzing: &[usize] = self.turn.as_ref().map(|t| &t.corners[..]).unwrap_or(&[]);
        let turns = self
            .track
            .as_ref()
            .map(|t| {
                t.turns
                    .iter()
                    .enumerate()
                    .map(|(i, h)| TurnStatus {
                        corner: h.label.clone(),
                        passes: h.passes.len() as u32,
                        last_s: h.last_clean(),
                        best_s: h.best(),
                        queued: self.queue.contains(&i),
                        analyzing: analyzing.contains(&i),
                    })
                    .collect()
            })
            .unwrap_or_default();
        *self.deps.link.lock().unwrap() = CoachingStatus { turns };
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

/// What one pass through a corner did differently from the reference.
pub fn corner_line(c: &CornerComparison) -> String {
    let mut parts = vec![format!("{:+.2} s", c.time_delta)];
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

/// Message sending the latest pass through each of `corners` to the race
/// engineer.
fn turns_prompt(
    track: &Track,
    corners: &[usize],
    windows: &[(String, Option<f64>)],
    coach_plan: Option<&Plan>,
    language: Language,
) -> String {
    let lang = language.name();
    let lines: Vec<String> = corners
        .iter()
        .filter_map(|&i| {
            let h = &track.turns[i];
            let last = h.passes.last()?;
            let earlier: Vec<String> = h.passes[..h.passes.len() - 1]
                .iter()
                .rev()
                .take(5)
                .rev()
                .map(|p| {
                    format!(
                        "{:+.2}{}",
                        p.loss(),
                        if p.off_track { " (off track)" } else { "" }
                    )
                })
                .collect();
            let best = h
                .best()
                .map(|b| format!(", best {b:+.2} s"))
                .unwrap_or_default();
            let id = format!("auto-{}", h.label);
            let cue = coach_plan
                .and_then(|p| p.triggers.iter().find(|t| t.id.as_deref() == Some(&id)))
                .map(|t| match Language::detect(&t.text) {
                    Some(l) if l != language => {
                        format!("\"{}\" (in {}: rewrite it in {lang})", t.text, l.name())
                    }
                    _ => format!("\"{}\"", t.text),
                })
                .unwrap_or_else(|| "none".into());
            let window = windows
                .iter()
                .find(|(l, _)| *l == h.label)
                .and_then(|(_, w)| *w)
                .map(|w| format!("{w:.1} s (about {} characters)", triggers::max_chars(w)))
                .unwrap_or_else(|| "no braking zone, keep it very short".into());
            Some(format!(
                "{label}, pass {n} (lap {lap}{reset}{off}): {detail}. Earlier passes: {earlier}{best}. Cue now: {cue}. Speaking time before {label}: {window}.",
                label = h.label,
                n = last.number,
                lap = last.lap,
                reset = if last.after_reset {
                    ", after a reset"
                } else {
                    ""
                },
                off = if last.off_track { ", off track" } else { "" },
                detail = corner_line(&last.corner),
                earlier = if earlier.is_empty() {
                    "none".into()
                } else {
                    earlier.join(", ")
                },
            ))
        })
        .collect();
    format!(
        "[Turns] Passes analyzed right after each turn.\n\
         {}\n\
         Conventions: time > 0 = slower than the reference over the turn; brake point, throttle on and turn-in > 0 = later; min speed < 0 = slower; radius < 1 = tighter line; steering > 1 = more steering.\n\
         Language: write every cue in {lang}, whatever the language of the cue in place; a {lang} voice speaks them and cannot pronounce another language.\n\
         For each of these turns, keep its cue if the advice still holds. Otherwise call update_plan once, with mode \"add\" and one cue per turn to change: id \"auto-T5\", corner \"T5\", anchor \"braking\", offset_m -300, text under the given length. The program speaks each cue on the approach to its turn, on the next pass or right after a reset before the turn; you cannot speak to the driver yourself. Finish with one summary line for the log.",
        lines.join("\n"),
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

/// Spoken after a pass: the time against the reference and the trend.
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

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::synthetic::SyntheticTrack;
    use telemetry::{CornerDetectionParams, Lap, LapFlags, TrackSurface};

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
        assert_eq!(
            capitalize(&spoken_corners(en, &["T5".into(), "T6".into()])),
            "Turns 5 and 6"
        );
        assert_eq!(spoken_corners(fr, &["T5".into()]), "virage 5");
    }

    fn lap_from(samples: Vec<Sample>) -> Lap {
        Lap {
            number: 1,
            start_time: samples[0].session_time,
            lap_time: samples.len() as f64 / 60.0,
            flags: LapFlags {
                complete: true,
                ..Default::default()
            },
            samples,
        }
    }

    #[test]
    fn prompt_lists_each_turn_with_its_history_cue_and_room() {
        let track = SyntheticTrack::demo();
        let a = Arc::new(Analyzer::new(
            &lap_from(track.simulate_lap(1.0, 0.0, 0.0)),
            Some(track.length_m()),
            &CornerDetectionParams::default(),
        ));
        let mut t = Track::new(a.clone());
        // Two passes through T4 at a slower pace, the second after a reset.
        for (n, after_reset) in [(1, false), (2, true)] {
            let mut caps = a.turn_captures();
            for mut s in track.simulate_lap(0.95, 0.0, 0.0) {
                s.is_on_track = true;
                s.surface = TrackSurface::OnTrack;
                if let Some(p) = caps[3].push(&s) {
                    let cmp = a.compare_pass(&p).unwrap();
                    t.turns[3].passes.push(PassResult {
                        number: n,
                        lap: 7,
                        corner: cmp.corners[3].clone(),
                        off_track: false,
                        after_reset,
                    });
                }
            }
        }
        assert_eq!(t.turns[3].passes.len(), 2);
        let plan = Plan::parse(
            "[[trigger]]\nid = \"auto-T4\"\ncorner = \"T4\"\ntext = \"Brake later.\"\n",
        )
        .unwrap();
        let windows = vec![("T4".to_string(), Some(4.0))];
        // A French voice: the English cue is to be rewritten.
        let fr = turns_prompt(&t, &[3], &windows, Some(&plan), Language::French);
        assert!(fr.contains("write every cue in French"), "{fr}");
        assert!(
            fr.contains("Cue now: \"Brake later.\" (in English: rewrite it in French)"),
            "{fr}"
        );
        let p = turns_prompt(&t, &[3], &windows, Some(&plan), Language::English);
        assert!(p.starts_with("[Turns] Passes analyzed right after each turn."));
        assert!(p.contains("write every cue in English"), "{p}");
        assert!(p.contains("T4, pass 2 (lap 7, after a reset): +"), "{p}");
        assert!(p.contains("brake point"), "{p}");
        assert!(p.contains("Earlier passes: +"), "{p}");
        assert!(p.contains("Cue now: \"Brake later.\""), "{p}");
        assert!(p.contains("about 52 characters"), "{p}");
        assert!(p.contains("mode \"add\""), "{p}");
    }
}

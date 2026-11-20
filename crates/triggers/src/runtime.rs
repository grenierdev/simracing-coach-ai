//! Coach fast loop: a thread consumes telemetry samples, runs the trigger
//! engine and drives speech.
//!
//! The reference (track map) and the plan can change at any time from the UI;
//! the engine is then rebuilt.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use analysis::Analyzer;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use telemetry::{LapEvent, LapRecorder, Sample};

use crate::engine::{Command, TickContext, TriggerEngine, TriggerView};
use crate::plan::{Plan, TrackContext, Trigger, TriggerSpec, resolve};
use crate::timing::{Fit, TimeProfile, fit_positions};

/// Speech output as seen by the engine. Implemented by the `voice` crate (via
/// the application); a fake in tests.
pub trait SpeechOutput: Send {
    fn say(&mut self, text: &str);
    fn stop(&mut self);
    fn is_speaking(&self) -> bool;
    /// Duration of the sentence once spoken, s (exact if already synthesized, estimated otherwise).
    fn duration(&mut self, text: &str) -> f64;
    /// Prepares sentences in advance (cached synthesis).
    fn prepare(&mut self, _texts: &[String]) {}
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub lap: i32,
    pub pct: f64,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeStatus {
    pub plan_name: Option<String>,
    pub plan_error: Option<String>,
    /// Coach plan resolution error (corner gone after a reference change).
    pub coach_error: Option<String>,
    pub triggers: Vec<TriggerView>,
    /// Last sentences spoken (most recent last).
    pub log: Vec<LogEntry>,
    pub quiet_now: bool,
    pub quiet_zones: usize,
    /// Where each cue is spoken and whether it has time (reference pace).
    pub fits: Vec<Fit>,
}

/// When to speak an injected sentence.
#[derive(Debug, Clone)]
pub enum SayWhen {
    /// As soon as possible (highest priority), respecting quiet zones.
    Now,
    /// On the next stretch where the sentence has time to be spoken.
    NextStraight,
    /// At a position on the lap (corner + anchor + offset, or absolute position).
    At(Box<TriggerSpec>),
}

struct Shared {
    analyzer: Option<Arc<Analyzer>>,
    plan: Option<(String, Plan)>,
    /// Trigger layer managed by Claude (`update_plan`), separate from the driver's plan.
    coach_plan: Option<Plan>,
    dirty: bool,
    injections: Vec<(String, SayWhen)>,
    status: RuntimeStatus,
}

pub struct TriggerRuntime {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

const LOG_LEN: usize = 50;

impl TriggerRuntime {
    pub fn spawn(samples: Receiver<Sample>, speech: Box<dyn SpeechOutput>) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            analyzer: None,
            plan: None,
            coach_plan: None,
            dirty: true,
            injections: vec![],
            status: RuntimeStatus::default(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let shared = shared.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("coach-fast".into())
                .spawn(move || run(samples, speech, shared, stop))
                .expect("starting the trigger thread")
        };
        TriggerRuntime {
            shared,
            stop,
            thread: Some(thread),
        }
    }

    pub fn set_reference(&self, analyzer: Option<Arc<Analyzer>>) {
        let mut s = self.shared.lock().unwrap();
        s.analyzer = analyzer;
        s.dirty = true;
    }

    pub fn set_plan(&self, name: impl Into<String>, plan: Plan) {
        let mut s = self.shared.lock().unwrap();
        s.plan = Some((name.into(), plan));
        s.dirty = true;
    }

    /// Current driver plan (name and content).
    pub fn current_plan(&self) -> Option<(String, Plan)> {
        self.shared.lock().unwrap().plan.clone()
    }

    /// Replaces the coach trigger layer; the driver's plan is left untouched.
    pub fn set_coach_plan(&self, plan: Plan) {
        let mut s = self.shared.lock().unwrap();
        s.coach_plan = Some(plan);
        s.dirty = true;
    }

    pub fn coach_plan(&self) -> Option<Plan> {
        self.shared.lock().unwrap().coach_plan.clone()
    }

    /// Current track map (loaded reference).
    pub fn analyzer(&self) -> Option<Arc<Analyzer>> {
        self.shared.lock().unwrap().analyzer.clone()
    }

    /// Schedules a one-off sentence.
    pub fn say(&self, text: impl Into<String>, when: SayWhen) {
        self.shared
            .lock()
            .unwrap()
            .injections
            .push((text.into(), when));
    }

    pub fn status(&self) -> RuntimeStatus {
        self.shared.lock().unwrap().status.clone()
    }
}

impl Drop for TriggerRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Prefix of coach trigger identifiers.
pub const COACH_PREFIX: &str = "coach:";

fn rebuild(shared: &mut Shared, speech: &mut dyn SpeechOutput) -> Option<(TriggerEngine, f64)> {
    shared.dirty = false;
    // Without a plan, an empty engine still serves injected sentences.
    let empty = (String::new(), Plan::default());
    let (name, plan) = shared.plan.as_ref().unwrap_or(&empty);
    shared.status.plan_name = shared.plan.as_ref().map(|_| name.clone());
    let ctx = shared.analyzer.as_ref().map(|a| TrackContext {
        model: &a.model,
        reference: &a.reference_metrics,
    });
    // The two layers are resolved separately: an error in one does not lose
    // the other.
    let mut triggers = match resolve(plan, ctx.as_ref()) {
        Ok(t) => {
            shared.status.plan_error = None;
            t
        }
        Err(e) => {
            shared.status.plan_error = Some(e.to_string());
            vec![]
        }
    };
    if let Some(coach) = &shared.coach_plan {
        let mut coach = coach.clone();
        coach.settings = plan.settings.clone();
        match resolve(&coach, ctx.as_ref()) {
            Ok(t) => {
                shared.status.coach_error = None;
                triggers.extend(t.into_iter().map(|mut t| {
                    t.id = format!("{COACH_PREFIX}{}", t.id);
                    t
                }));
            }
            Err(e) => shared.status.coach_error = Some(e.to_string()),
        }
    }
    let quiet = ctx
        .as_ref()
        .map(|c| c.quiet_zones(&plan.settings))
        .unwrap_or_default();
    let texts: Vec<String> = triggers.iter().map(|t| t.text.clone()).collect();
    speech.prepare(&texts);
    shared.status.quiet_zones = quiet.len();
    let length = shared
        .analyzer
        .as_ref()
        .map(|a| a.model.length_m)
        .unwrap_or(4000.0);
    // Cues that would not have time to be spoken are moved earlier on their
    // straight; the ones that still do not fit are reported.
    let profile = shared
        .analyzer
        .as_deref()
        .and_then(TimeProfile::from_analyzer);
    shared.status.fits = match &profile {
        Some(p) => {
            let fits = fit_positions(&mut triggers, &quiet, p, length, &mut |t| {
                speech.duration(t)
            });
            for f in &fits {
                if let Some(problem) = f.problem() {
                    tracing::warn!("{problem}");
                } else if f.moved_m > 1.0 {
                    tracing::info!(
                        "cue \"{}\" moved {:.0} m earlier to have time to be spoken",
                        f.id,
                        f.moved_m
                    );
                }
            }
            fits
        }
        None => vec![],
    };
    let mut engine = TriggerEngine::new(triggers, quiet);
    if let Some(p) = profile {
        engine.set_time_profile(p);
    }
    shared.status.triggers = engine.views();
    Some((engine, length))
}

/// One-off trigger matching an injected sentence.
fn injected_trigger(
    text: &str,
    when: &SayWhen,
    analyzer: Option<&Analyzer>,
    counter: &mut u32,
) -> Result<(Trigger, bool), String> {
    *counter += 1;
    let id = format!("say-{counter}");
    let base = |priority: u8, max_delay_pct: f64| Trigger {
        id: id.clone(),
        text: text.trim().to_string(),
        at_pct: f64::NAN,
        corner: None,
        priority,
        repeat: crate::plan::Repeat::Once,
        condition: None,
        expire_after_laps: None,
        max_delay_pct,
    };
    match when {
        SayWhen::Now => Ok((base(9, 0.25), true)),
        SayWhen::NextStraight => Ok((base(6, 0.6), true)),
        SayWhen::At(spec) => {
            let mut spec = (**spec).clone();
            spec.text = text.to_string();
            spec.id = Some(id.clone());
            let plan = Plan {
                triggers: vec![spec],
                ..Default::default()
            };
            let ctx = analyzer.map(|a| TrackContext {
                model: &a.model,
                reference: &a.reference_metrics,
            });
            let mut t = resolve(&plan, ctx.as_ref()).map_err(|e| e.to_string())?;
            Ok((t.remove(0), false))
        }
    }
}

fn run(
    samples: Receiver<Sample>,
    mut speech: Box<dyn SpeechOutput>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
) {
    let mut engine: Option<(TriggerEngine, f64)> = None;
    let mut analyzer: Option<Arc<Analyzer>> = None;
    let mut laps = LapRecorder::default();
    let mut log: VecDeque<LogEntry> = VecDeque::with_capacity(LOG_LEN);
    let mut ticks: u32 = 0;
    let mut injected: u32 = 0;

    while !stop.load(Ordering::Relaxed) {
        {
            let mut s = shared.lock().unwrap();
            if s.dirty {
                analyzer = s.analyzer.clone();
                let mut fresh = rebuild(&mut s, speech.as_mut());
                if let (Some((new, _)), Some((old, _))) = (fresh.as_mut(), engine.as_ref()) {
                    new.carry_state_from(old);
                }
                engine = fresh;
            }
            if let Some((eng, _)) = engine.as_mut() {
                for (text, when) in std::mem::take(&mut s.injections) {
                    match injected_trigger(&text, &when, analyzer.as_deref(), &mut injected) {
                        Ok((t, arm_now)) => {
                            speech.prepare(std::slice::from_ref(&t.text));
                            eng.inject(t, arm_now);
                        }
                        Err(e) => tracing::warn!("injected sentence rejected: {e}"),
                    }
                }
                s.status.triggers = eng.views();
            }
        }
        let sample = match samples.recv_timeout(Duration::from_millis(100)) {
            Ok(x) => x,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(LapEvent::Aborted) = laps.push(sample) {
            // nothing: the live delta becomes unavailable again
        }
        let Some((eng, length)) = engine.as_mut() else {
            continue;
        };
        let live_delta = match (
            &analyzer,
            laps.current_lap_elapsed(),
            laps.current_lap_flags(),
        ) {
            (Some(a), Some(el), Some(f)) if f.complete => {
                Some(a.live_delta(sample.lap_dist_pct as f64, el))
            }
            _ => None,
        };
        let ctx = TickContext {
            live_delta,
            speaking: speech.is_speaking(),
            track_length_m: *length,
        };
        let cmds = eng.tick(&sample, &ctx, &mut |t| speech.duration(t));
        for c in &cmds {
            match c {
                Command::Say { text, .. } => {
                    speech.say(text);
                    if log.len() == LOG_LEN {
                        log.pop_front();
                    }
                    log.push_back(LogEntry {
                        lap: sample.lap,
                        pct: sample.lap_dist_pct as f64,
                        text: text.clone(),
                    });
                }
                Command::Stop { reason } => {
                    tracing::debug!("speech cut off: {reason:?}");
                    speech.stop();
                }
            }
        }
        // Status published ~10 times per second or on every command.
        ticks += 1;
        if !cmds.is_empty() || ticks.is_multiple_of(6) {
            let mut s = shared.lock().unwrap();
            s.status.triggers = eng.views();
            s.status.log = log.iter().cloned().collect();
            s.status.quiet_now = eng.in_quiet_zone(sample.lap_dist_pct as f64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;
    use std::sync::Mutex as StdMutex;
    use telemetry::synthetic::SyntheticTrack;
    use telemetry::{CornerDetectionParams, LapFlags};

    #[derive(Clone, Default)]
    struct FakeSpeech {
        said: Arc<StdMutex<Vec<String>>>,
        prepared: Arc<StdMutex<Vec<String>>>,
    }

    impl SpeechOutput for FakeSpeech {
        fn say(&mut self, text: &str) {
            self.said.lock().unwrap().push(text.to_string());
        }
        fn stop(&mut self) {}
        fn is_speaking(&self) -> bool {
            false
        }
        fn duration(&mut self, _: &str) -> f64 {
            1.5
        }
        fn prepare(&mut self, texts: &[String]) {
            self.prepared.lock().unwrap().extend_from_slice(texts);
        }
    }

    #[test]
    fn runtime_speaks_plan_on_synthetic_laps() {
        let track = SyntheticTrack::demo();
        let ref_samples = track.simulate_lap(1.0, 0.0, 0.0);
        let reference = telemetry::Lap {
            number: 1,
            start_time: 0.0,
            lap_time: ref_samples.len() as f64 / 60.0,
            flags: LapFlags {
                complete: true,
                ..Default::default()
            },
            samples: ref_samples,
        };
        let analyzer = Arc::new(Analyzer::new(
            &reference,
            Some(track.length_m()),
            &CornerDetectionParams::default(),
        ));
        let plan = Plan::parse(
            r#"
            [[trigger]]
            corner = "T4"
            text = "Hairpin, brake hard."
            offset_m = -200

            [[trigger]]
            position = 0.05
            text = "New lap."
            repeat = "once"
            "#,
        )
        .unwrap();

        let (tx, rx) = bounded(100_000);
        let fake = FakeSpeech::default();
        let rt = TriggerRuntime::spawn(rx, Box::new(fake.clone()));
        rt.set_reference(Some(analyzer));
        rt.set_plan("test", plan);
        std::thread::sleep(Duration::from_millis(250));
        // Two consecutive laps (the reference replayed), continuous time.
        let mut t = 0.0;
        for lap in 1..=2 {
            for mut s in track.simulate_lap(0.98, 0.0, 0.0) {
                t += 1.0 / 60.0;
                s.session_time = t;
                s.lap = lap;
                tx.send(s).unwrap();
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        let said = fake.said.lock().unwrap().clone();
        assert_eq!(
            said,
            vec!["New lap.", "Hairpin, brake hard.", "Hairpin, brake hard."]
        );
        assert_eq!(fake.prepared.lock().unwrap().len(), 2);
        let status = rt.status();
        assert!(status.plan_error.is_none());
        assert_eq!(status.log.len(), 3);
        assert!(status.quiet_zones >= 3);
    }

    #[test]
    fn say_now_without_plan_is_spoken() {
        let (tx, rx) = bounded(10_000);
        let fake = FakeSpeech::default();
        let rt = TriggerRuntime::spawn(rx, Box::new(fake.clone()));
        std::thread::sleep(Duration::from_millis(150));
        rt.say("Looking at T5.", SayWhen::Now);
        for i in 0..120 {
            let s = Sample {
                session_time: i as f64 / 60.0,
                lap: 1,
                lap_dist_pct: 0.1 + i as f32 * 0.001,
                speed: 50.0,
                is_on_track: true,
                ..Default::default()
            };
            tx.send(s).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(*fake.said.lock().unwrap(), vec!["Looking at T5."]);
    }

    #[test]
    fn coach_layer_adds_to_user_plan() {
        let track = SyntheticTrack::demo();
        let samples = track.simulate_lap(1.0, 0.0, 0.0);
        let reference = telemetry::Lap {
            number: 1,
            start_time: 0.0,
            lap_time: samples.len() as f64 / 60.0,
            flags: LapFlags {
                complete: true,
                ..Default::default()
            },
            samples,
        };
        let analyzer = Arc::new(Analyzer::new(
            &reference,
            Some(track.length_m()),
            &CornerDetectionParams::default(),
        ));
        let (_tx, rx) = bounded(10);
        let rt = TriggerRuntime::spawn(rx, Box::new(FakeSpeech::default()));
        rt.set_reference(Some(analyzer));
        rt.set_plan(
            "driver",
            Plan::parse("[[trigger]]\nid = \"a\"\nposition = 0.1\ntext = \"Driver.\"").unwrap(),
        );
        rt.set_coach_plan(
            Plan::parse("[[trigger]]\nid = \"t4\"\ncorner = \"T4\"\ntext = \"Coach.\"").unwrap(),
        );
        std::thread::sleep(Duration::from_millis(300));
        let ids: Vec<String> = rt
            .status()
            .triggers
            .iter()
            .map(|v| v.trigger.id.clone())
            .collect();
        assert_eq!(ids, vec!["a", "coach:t4"]);
        assert_eq!(
            rt.current_plan().unwrap().1.triggers.len(),
            1,
            "driver plan intact"
        );

        // Invalid coach plan (unknown corner): the driver's plan stays active.
        rt.set_coach_plan(Plan::parse("[[trigger]]\ncorner = \"T99\"\ntext = \"x\"").unwrap());
        std::thread::sleep(Duration::from_millis(300));
        let st = rt.status();
        assert!(st.coach_error.is_some());
        assert_eq!(st.triggers.len(), 1);
    }

    #[test]
    fn corner_plan_without_reference_reports_error() {
        let (_tx, rx) = bounded(10);
        let rt = TriggerRuntime::spawn(rx, Box::new(FakeSpeech::default()));
        rt.set_plan(
            "p",
            Plan::parse("[[trigger]]\ncorner = \"T1\"\ntext = \"x\"").unwrap(),
        );
        std::thread::sleep(Duration::from_millis(300));
        let st = rt.status();
        assert!(st.plan_error.unwrap().contains("no reference lap"));
    }
}

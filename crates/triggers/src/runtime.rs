//! Boucle rapide du coach : un thread consomme les échantillons de la
//! télémétrie, fait tourner le moteur de consignes et pilote la parole.
//!
//! La référence (carte de la piste) et le plan peuvent changer à tout moment
//! depuis l'interface ; le moteur est alors reconstruit.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use analysis::Analyzer;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use telemetry::{LapEvent, LapRecorder, Sample};

use crate::engine::{Command, TickContext, TriggerEngine, TriggerView};
use crate::plan::{Plan, TrackContext, resolve};

/// Sortie vocale vue par le moteur. Implémentée par le crate `voice` (via
/// l'application) ; un faux en test.
pub trait SpeechOutput: Send {
    fn say(&mut self, text: &str);
    fn stop(&mut self);
    fn is_speaking(&self) -> bool;
    /// Durée de la phrase une fois dite, s (exacte si déjà synthétisée, estimée sinon).
    fn duration(&mut self, text: &str) -> f64;
    /// Prépare les phrases à l'avance (synthèse en cache).
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
    pub triggers: Vec<TriggerView>,
    /// Dernières phrases dites (la plus récente en dernier).
    pub log: Vec<LogEntry>,
    pub quiet_now: bool,
    pub quiet_zones: usize,
}

struct Shared {
    analyzer: Option<Arc<Analyzer>>,
    plan: Option<(String, Plan)>,
    dirty: bool,
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
            dirty: true,
            status: RuntimeStatus::default(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let shared = shared.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("coach-fast".into())
                .spawn(move || run(samples, speech, shared, stop))
                .expect("démarrage du thread des consignes")
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

fn rebuild(shared: &mut Shared, speech: &mut dyn SpeechOutput) -> Option<(TriggerEngine, f64)> {
    shared.dirty = false;
    let (name, plan) = shared.plan.as_ref()?;
    shared.status.plan_name = Some(name.clone());
    let ctx = shared.analyzer.as_ref().map(|a| TrackContext {
        model: &a.model,
        reference: &a.reference_metrics,
    });
    match resolve(plan, ctx.as_ref()) {
        Ok(triggers) => {
            let quiet = ctx
                .as_ref()
                .map(|c| c.quiet_zones(&plan.reglages))
                .unwrap_or_default();
            let texts: Vec<String> = triggers.iter().map(|t| t.text.clone()).collect();
            speech.prepare(&texts);
            shared.status.plan_error = None;
            shared.status.quiet_zones = quiet.len();
            let length = shared
                .analyzer
                .as_ref()
                .map(|a| a.model.length_m)
                .unwrap_or(4000.0);
            let engine = TriggerEngine::new(triggers, quiet);
            shared.status.triggers = engine.views();
            Some((engine, length))
        }
        Err(e) => {
            shared.status.plan_error = Some(e.to_string());
            shared.status.triggers.clear();
            None
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

    while !stop.load(Ordering::Relaxed) {
        {
            let mut s = shared.lock().unwrap();
            if s.dirty {
                analyzer = s.analyzer.clone();
                engine = rebuild(&mut s, speech.as_mut());
            }
        }
        let sample = match samples.recv_timeout(Duration::from_millis(100)) {
            Ok(x) => x,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(LapEvent::Aborted) = laps.push(sample) {
            // rien : le delta live redevient indisponible
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
                    tracing::debug!("parole coupée : {reason:?}");
                    speech.stop();
                }
            }
        }
        // Statut publié ~10 fois par seconde ou à chaque commande.
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
            [[consigne]]
            virage = "T4"
            texte = "Épingle, freine fort."
            decalage_m = -200

            [[consigne]]
            position = 0.05
            texte = "Nouveau tour."
            repetition = "une_fois"
            "#,
        )
        .unwrap();

        let (tx, rx) = bounded(100_000);
        let fake = FakeSpeech::default();
        let rt = TriggerRuntime::spawn(rx, Box::new(fake.clone()));
        rt.set_reference(Some(analyzer));
        rt.set_plan("test", plan);
        std::thread::sleep(Duration::from_millis(250));
        // Deux tours enchaînés (la référence rejouée), temps continu.
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
            vec![
                "Nouveau tour.",
                "Épingle, freine fort.",
                "Épingle, freine fort."
            ]
        );
        assert_eq!(fake.prepared.lock().unwrap().len(), 2);
        let status = rt.status();
        assert!(status.plan_error.is_none());
        assert_eq!(status.log.len(), 3);
        assert!(status.quiet_zones >= 3);
    }

    #[test]
    fn corner_plan_without_reference_reports_error() {
        let (_tx, rx) = bounded(10);
        let rt = TriggerRuntime::spawn(rx, Box::new(FakeSpeech::default()));
        rt.set_plan(
            "p",
            Plan::parse("[[consigne]]\nvirage = \"T1\"\ntexte = \"x\"").unwrap(),
        );
        std::thread::sleep(Duration::from_millis(300));
        let st = rt.status();
        assert!(st.plan_error.unwrap().contains("aucune référence"));
    }
}

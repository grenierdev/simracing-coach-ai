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
use crate::plan::{Plan, TrackContext, Trigger, TriggerSpec, resolve};

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
    /// Erreur de résolution du plan du coach (virage disparu après un changement de référence).
    pub coach_error: Option<String>,
    pub triggers: Vec<TriggerView>,
    /// Dernières phrases dites (la plus récente en dernier).
    pub log: Vec<LogEntry>,
    pub quiet_now: bool,
    pub quiet_zones: usize,
}

/// Quand dire une phrase injectée.
#[derive(Debug, Clone)]
pub enum SayWhen {
    /// Dès que possible (priorité maximale), en respectant les zones de silence.
    Now,
    /// Sur la prochaine portion où la phrase a le temps d'être dite.
    NextStraight,
    /// À une position du tour (virage + ancre + décalage, ou position absolue).
    At(Box<TriggerSpec>),
}

struct Shared {
    analyzer: Option<Arc<Analyzer>>,
    plan: Option<(String, Plan)>,
    /// Couche de consignes gérée par Claude (`update_plan`), distincte du plan du pilote.
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

    /// Plan courant du pilote (nom et contenu).
    pub fn current_plan(&self) -> Option<(String, Plan)> {
        self.shared.lock().unwrap().plan.clone()
    }

    /// Remplace la couche de consignes du coach ; le plan du pilote n'est pas touché.
    pub fn set_coach_plan(&self, plan: Plan) {
        let mut s = self.shared.lock().unwrap();
        s.coach_plan = Some(plan);
        s.dirty = true;
    }

    pub fn coach_plan(&self) -> Option<Plan> {
        self.shared.lock().unwrap().coach_plan.clone()
    }

    /// Carte de la piste courante (référence chargée).
    pub fn analyzer(&self) -> Option<Arc<Analyzer>> {
        self.shared.lock().unwrap().analyzer.clone()
    }

    /// Programme une phrase ponctuelle.
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

/// Préfixe des identifiants des consignes du coach.
pub const COACH_PREFIX: &str = "coach:";

fn rebuild(shared: &mut Shared, speech: &mut dyn SpeechOutput) -> Option<(TriggerEngine, f64)> {
    shared.dirty = false;
    // Sans plan, un moteur vide sert quand même aux phrases injectées.
    let empty = (String::new(), Plan::default());
    let (name, plan) = shared.plan.as_ref().unwrap_or(&empty);
    shared.status.plan_name = shared.plan.as_ref().map(|_| name.clone());
    let ctx = shared.analyzer.as_ref().map(|a| TrackContext {
        model: &a.model,
        reference: &a.reference_metrics,
    });
    // Les deux couches sont résolues séparément : une erreur dans l'une ne
    // fait pas perdre l'autre.
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
    let engine = TriggerEngine::new(triggers, quiet);
    shared.status.triggers = engine.views();
    Some((engine, length))
}

/// Consigne ponctuelle correspondant à une phrase injectée.
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
                        Err(e) => tracing::warn!("phrase injectée refusée : {e}"),
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
            [[trigger]]
            corner = "T4"
            text = "Épingle, freine fort."
            offset_m = -200

            [[trigger]]
            position = 0.05
            text = "Nouveau tour."
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
    fn say_now_without_plan_is_spoken() {
        let (tx, rx) = bounded(10_000);
        let fake = FakeSpeech::default();
        let rt = TriggerRuntime::spawn(rx, Box::new(fake.clone()));
        std::thread::sleep(Duration::from_millis(150));
        rt.say("Je regarde le T5.", SayWhen::Now);
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
        assert_eq!(*fake.said.lock().unwrap(), vec!["Je regarde le T5."]);
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
            "pilote",
            Plan::parse("[[trigger]]\nid = \"a\"\nposition = 0.1\ntext = \"Pilote.\"").unwrap(),
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
            "plan du pilote intact"
        );

        // Plan du coach invalide (virage inconnu) : le plan du pilote reste actif.
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

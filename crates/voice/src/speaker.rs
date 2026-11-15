//! Thread de parole : synthèse (avec cache) puis lecture.
//!
//! Le moteur TTS est construit dans le thread lui-même (chargement du modèle
//! d'environ 1 s) ; les appels depuis la boucle rapide ne bloquent jamais.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::{Sender, unbounded};

use crate::{Audio, AudioSink, Tts, VoiceError, estimate_duration};

#[derive(Debug, Clone, PartialEq, Default)]
pub enum SpeakerState {
    #[default]
    Loading,
    Ready,
    Error(String),
}

#[derive(Debug, Clone, Default)]
pub struct SpeakerStatus {
    pub engine: String,
    pub output: String,
    pub state: SpeakerState,
    /// Précision sur l'état (progression du téléchargement du modèle…).
    pub detail: Option<String>,
    /// Durée de la dernière synthèse, ms (absente si la phrase était en cache).
    pub last_synthesis_ms: Option<f64>,
    /// De la demande à la mise en file audio, ms.
    pub last_latency_ms: Option<f64>,
    pub cached: usize,
}

enum Cmd {
    Prepare(Vec<String>),
    Say(String, Instant),
    Stop,
}

type Cache = Arc<Mutex<HashMap<String, Arc<Audio>>>>;

pub struct Speaker {
    tx: Sender<Cmd>,
    sink: Arc<dyn AudioSink>,
    cache: Cache,
    status: Arc<Mutex<SpeakerStatus>>,
    pending: Arc<AtomicBool>,
}

impl Speaker {
    /// `make_tts` construit le moteur dans le thread de parole ; il reçoit une
    /// fonction pour publier sa progression (téléchargement, chargement).
    pub fn spawn<F>(make_tts: F, sink: Arc<dyn AudioSink>) -> Self
    where
        F: FnOnce(&dyn Fn(String)) -> Result<Box<dyn Tts>, VoiceError> + Send + 'static,
    {
        let (tx, rx) = unbounded::<Cmd>();
        let cache: Cache = Arc::default();
        let status = Arc::new(Mutex::new(SpeakerStatus {
            output: sink.name(),
            ..Default::default()
        }));
        let pending = Arc::new(AtomicBool::new(false));
        {
            let (sink, cache, status, pending) =
                (sink.clone(), cache.clone(), status.clone(), pending.clone());
            std::thread::Builder::new()
                .name("speaker".into())
                .spawn(move || {
                    let report = |msg: String| status.lock().unwrap().detail = Some(msg);
                    let mut tts = match make_tts(&report) {
                        Ok(t) => {
                            let mut s = status.lock().unwrap();
                            s.engine = t.name();
                            s.state = SpeakerState::Ready;
                            s.detail = None;
                            t
                        }
                        Err(e) => {
                            tracing::warn!("TTS indisponible : {e}");
                            status.lock().unwrap().state = SpeakerState::Error(e.to_string());
                            // On continue à vider les commandes pour ne rien bloquer.
                            for cmd in rx {
                                if let Cmd::Say(..) = cmd {
                                    pending.store(false, Ordering::Release);
                                }
                            }
                            return;
                        }
                    };
                    let mut synth = |text: &str| -> Option<(Arc<Audio>, Option<f64>)> {
                        if let Some(a) = cache.lock().unwrap().get(text) {
                            return Some((a.clone(), None));
                        }
                        let t = Instant::now();
                        match tts.synthesize(text) {
                            Ok(a) => {
                                let a = Arc::new(a);
                                let mut c = cache.lock().unwrap();
                                c.insert(text.to_string(), a.clone());
                                status.lock().unwrap().cached = c.len();
                                Some((a, Some(t.elapsed().as_secs_f64() * 1e3)))
                            }
                            Err(e) => {
                                tracing::warn!("synthèse de « {text} » : {e}");
                                None
                            }
                        }
                    };
                    for cmd in rx {
                        match cmd {
                            Cmd::Prepare(texts) => {
                                for t in texts {
                                    synth(&t);
                                }
                            }
                            Cmd::Say(text, asked) => {
                                if let Some((audio, synth_ms)) = synth(&text) {
                                    sink.play(&audio);
                                    let mut s = status.lock().unwrap();
                                    s.last_synthesis_ms = synth_ms;
                                    s.last_latency_ms = Some(asked.elapsed().as_secs_f64() * 1e3);
                                }
                                pending.store(false, Ordering::Release);
                            }
                            Cmd::Stop => sink.stop(),
                        }
                    }
                })
                .expect("démarrage du thread de parole");
        }
        Speaker {
            tx,
            sink,
            cache,
            status,
            pending,
        }
    }

    pub fn say(&self, text: &str) {
        self.pending.store(true, Ordering::Release);
        let _ = self.tx.send(Cmd::Say(text.to_string(), Instant::now()));
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
        self.sink.stop();
    }

    /// Une phrase est demandée ou en cours de lecture.
    pub fn is_speaking(&self) -> bool {
        self.pending.load(Ordering::Acquire) || self.sink.is_playing()
    }

    /// Durée de la phrase : exacte si elle est en cache, estimée sinon.
    pub fn duration(&self, text: &str) -> f64 {
        self.cache
            .lock()
            .unwrap()
            .get(text)
            .map(|a| a.duration_s())
            .unwrap_or_else(|| estimate_duration(text))
    }

    pub fn prepare(&self, texts: &[String]) {
        let _ = self.tx.send(Cmd::Prepare(texts.to_vec()));
    }

    pub fn status(&self) -> SpeakerStatus {
        self.status.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NullSink, SilentTts};
    use std::time::Duration;

    fn wait_until(mut f: impl FnMut() -> bool) {
        for _ in 0..200 {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("délai dépassé");
    }

    #[test]
    fn prepares_says_and_stops() {
        let sp = Speaker::spawn(
            |report| {
                report("chargement".into());
                Ok(Box::new(SilentTts::default()) as Box<dyn Tts>)
            },
            Arc::new(NullSink::default()),
        );
        wait_until(|| sp.status().state == SpeakerState::Ready);
        sp.prepare(&["Bonjour".to_string()]);
        wait_until(|| sp.status().cached == 1);
        assert!((sp.duration("Bonjour") - estimate_duration("Bonjour")).abs() < 0.01);

        sp.say("Bonjour");
        assert!(sp.is_speaking());
        wait_until(|| sp.status().last_latency_ms.is_some());
        assert_eq!(sp.status().last_synthesis_ms, None, "phrase en cache");
        sp.stop();
        wait_until(|| !sp.is_speaking());
    }

    #[test]
    fn engine_failure_is_reported_and_harmless() {
        let sp = Speaker::spawn(
            |_| Err(VoiceError::ModelMissing("piper".into())),
            Arc::new(NullSink::default()),
        );
        wait_until(|| matches!(sp.status().state, SpeakerState::Error(_)));
        sp.say("x");
        wait_until(|| !sp.is_speaking());
    }
}

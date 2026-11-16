//! Speech thread: synthesis (with a cache) then playback.
//!
//! The TTS engine is built inside the thread itself (model loading takes about
//! 1 s); calls from the fast loop never block.

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
    /// Detail on the state (model download progress…).
    pub detail: Option<String>,
    /// Duration of the last synthesis, ms (absent if the sentence was cached).
    pub last_synthesis_ms: Option<f64>,
    /// From the request to queuing the audio, ms.
    pub last_latency_ms: Option<f64>,
    /// Time of the last audio queuing (first sound of a sentence).
    pub last_play_at: Option<Instant>,
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
    /// `make_tts` builds the engine in the speech thread; it receives a function
    /// to report its progress (download, loading).
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
                            tracing::warn!("TTS unavailable: {e}");
                            status.lock().unwrap().state = SpeakerState::Error(e.to_string());
                            // Keep draining commands so nothing blocks.
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
                                tracing::warn!("synthesizing \"{text}\": {e}");
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
                                    s.last_play_at = Some(Instant::now());
                                }
                                pending.store(false, Ordering::Release);
                            }
                            Cmd::Stop => sink.stop(),
                        }
                    }
                })
                .expect("failed to start the speech thread");
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

    /// A sentence is requested or playing.
    pub fn is_speaking(&self) -> bool {
        self.pending.load(Ordering::Acquire) || self.sink.is_playing()
    }

    /// Level of the coach's voice being played, 0..1.
    pub fn level(&self) -> f32 {
        self.sink.level()
    }

    /// Sentence duration: exact if cached, estimated otherwise.
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
        let mut s = self.status.lock().unwrap().clone();
        // The output can be switched while running.
        s.output = self.sink.name();
        s
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
        panic!("timed out");
    }

    #[test]
    fn prepares_says_and_stops() {
        let sp = Speaker::spawn(
            |report| {
                report("loading".into());
                Ok(Box::new(SilentTts::default()) as Box<dyn Tts>)
            },
            Arc::new(NullSink::default()),
        );
        wait_until(|| sp.status().state == SpeakerState::Ready);
        sp.prepare(&["Hello".to_string()]);
        wait_until(|| sp.status().cached == 1);
        assert!((sp.duration("Hello") - estimate_duration("Hello")).abs() < 0.01);

        sp.say("Hello");
        assert!(sp.is_speaking());
        wait_until(|| sp.status().last_latency_ms.is_some());
        assert_eq!(sp.status().last_synthesis_ms, None, "sentence cached");
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

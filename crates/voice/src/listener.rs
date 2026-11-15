//! Thread de reconnaissance vocale : transcrit les enregistrements du push-to-talk.
//!
//! Le moteur (Parakeet, environ 670 Mo) est construit dans le thread : son
//! téléchargement et son chargement ne bloquent rien.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::speaker::SpeakerState;
use crate::{Audio, Stt, VoiceError};

#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub id: u64,
    pub text: String,
    /// Durée de l'enregistrement, s.
    pub audio_s: f64,
    /// Durée de la transcription, ms.
    pub millis: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ListenerStatus {
    pub engine: String,
    pub state: SpeakerState,
    pub detail: Option<String>,
}

pub struct Listener {
    tx: Sender<(u64, Audio)>,
    results: Receiver<Transcript>,
    status: Arc<Mutex<ListenerStatus>>,
}

impl Listener {
    pub fn spawn<F>(make_stt: F) -> Self
    where
        F: FnOnce(&dyn Fn(String)) -> Result<Box<dyn Stt>, VoiceError> + Send + 'static,
    {
        let (tx, rx) = unbounded::<(u64, Audio)>();
        let (rtx, rrx) = unbounded();
        let status: Arc<Mutex<ListenerStatus>> = Arc::default();
        let st = status.clone();
        std::thread::Builder::new()
            .name("stt".into())
            .spawn(move || {
                let report = |msg: String| st.lock().unwrap().detail = Some(msg);
                let mut stt = match make_stt(&report) {
                    Ok(s) => {
                        let mut g = st.lock().unwrap();
                        g.engine = s.name();
                        g.state = SpeakerState::Ready;
                        g.detail = None;
                        s
                    }
                    Err(e) => {
                        tracing::warn!("STT indisponible : {e}");
                        st.lock().unwrap().state = SpeakerState::Error(e.to_string());
                        return;
                    }
                };
                for (id, audio) in rx {
                    // Trop court pour être une phrase (appui accidentel).
                    if audio.duration_s() < 0.25 {
                        continue;
                    }
                    let t = Instant::now();
                    match stt.transcribe(&audio) {
                        Ok(text) => {
                            let _ = rtx.send(Transcript {
                                id,
                                text,
                                audio_s: audio.duration_s(),
                                millis: t.elapsed().as_secs_f64() * 1e3,
                            });
                        }
                        Err(e) => tracing::warn!("transcription : {e}"),
                    }
                }
            })
            .expect("démarrage du thread de reconnaissance vocale");
        Listener {
            tx,
            results: rrx,
            status,
        }
    }

    pub fn transcribe(&self, id: u64, audio: Audio) {
        let _ = self.tx.send((id, audio));
    }

    pub fn results(&self) -> &Receiver<Transcript> {
        &self.results
    }

    pub fn status(&self) -> ListenerStatus {
        self.status.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Echo;
    impl Stt for Echo {
        fn name(&self) -> String {
            "écho".into()
        }
        fn transcribe(&mut self, audio: &Audio) -> Result<String, VoiceError> {
            Ok(format!("{} échantillons", audio.samples.len()))
        }
    }

    #[test]
    fn transcribes_and_skips_accidental_taps() {
        let l = Listener::spawn(|_| Ok(Box::new(Echo) as Box<dyn Stt>));
        let audio = |n: usize| Audio {
            samples: vec![0.0; n],
            sample_rate: 16_000,
        };
        l.transcribe(1, audio(1_000)); // 60 ms : ignoré
        l.transcribe(2, audio(16_000));
        let t = l.results().recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(t.id, 2);
        assert_eq!(t.text, "16000 échantillons");
        assert_eq!(l.status().state, SpeakerState::Ready);
    }
}

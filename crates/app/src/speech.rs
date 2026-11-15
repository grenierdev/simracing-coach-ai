//! Mise en place de la voix : modèle, moteur, sortie audio, et adaptation au
//! moteur de consignes.

use std::sync::Arc;

use voice::{AudioSink, CpalSink, NullSink, SilentTts, Speaker, Tts};

/// Démarre le thread de parole. Le modèle est téléchargé au premier lancement.
pub fn start_speaker(no_voice: bool) -> Arc<Speaker> {
    let sink: Arc<dyn AudioSink> = if no_voice {
        Arc::new(NullSink::default())
    } else {
        match CpalSink::open() {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!("{e} : les consignes seront affichées sans son");
                Arc::new(NullSink::default())
            }
        }
    };
    Arc::new(Speaker::spawn(
        move |report| make_tts(no_voice, report),
        sink,
    ))
}

#[cfg(feature = "voice-sherpa")]
fn make_tts(no_voice: bool, report: &dyn Fn(String)) -> Result<Box<dyn Tts>, voice::VoiceError> {
    use voice::models::{PIPER_FR_SIWIS, ensure, models_dir};
    if no_voice {
        return Ok(Box::new(SilentTts::default()));
    }
    let dir = models_dir();
    let path = ensure(&PIPER_FR_SIWIS, &dir, |done, total| {
        let pct = (done * 100).checked_div(total).unwrap_or(0);
        report(format!("téléchargement de la voix : {pct} %"));
    })?;
    report("chargement de la voix".into());
    Ok(Box::new(voice::sherpa::SherpaTts::piper(&path, 2)?))
}

#[cfg(not(feature = "voice-sherpa"))]
fn make_tts(_no_voice: bool, _report: &dyn Fn(String)) -> Result<Box<dyn Tts>, voice::VoiceError> {
    Ok(Box::new(SilentTts::default()))
}

/// Le moteur de consignes parle à travers ce trait.
pub struct SpeakerOutput(pub Arc<Speaker>);

impl triggers::SpeechOutput for SpeakerOutput {
    fn say(&mut self, text: &str) {
        self.0.say(text);
    }
    fn stop(&mut self) {
        self.0.stop();
    }
    fn is_speaking(&self) -> bool {
        self.0.is_speaking()
    }
    fn duration(&mut self, text: &str) -> f64 {
        self.0.duration(text)
    }
    fn prepare(&mut self, texts: &[String]) {
        self.0.prepare(texts);
    }
}

//! Voice setup: model, engine, audio output, and adapter to the cue engine.

use std::sync::Arc;

use voice::{AudioSink, CpalSink, NullSink, SilentTts, Speaker, Tts};

/// Starts the speech thread. The model is downloaded on first launch.
pub fn start_speaker(no_voice: bool) -> Arc<Speaker> {
    let sink: Arc<dyn AudioSink> = if no_voice {
        Arc::new(NullSink::default())
    } else {
        match CpalSink::open() {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!("{e}: cues will be shown without sound");
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
        report(format!("downloading voice: {pct}%"));
    })?;
    report("loading voice".into());
    Ok(Box::new(voice::sherpa::SherpaTts::piper(&path, 2)?))
}

#[cfg(not(feature = "voice-sherpa"))]
fn make_tts(_no_voice: bool, _report: &dyn Fn(String)) -> Result<Box<dyn Tts>, voice::VoiceError> {
    Ok(Box::new(SilentTts::default()))
}

/// Speech recognition: Parakeet v3, downloaded on first launch (~670 MB).
pub fn start_listener() -> voice::Listener {
    voice::Listener::spawn(make_stt)
}

#[cfg(feature = "voice-sherpa")]
fn make_stt(report: &dyn Fn(String)) -> Result<Box<dyn voice::Stt>, voice::VoiceError> {
    use voice::models::{PARAKEET_V3_INT8, ensure, models_dir};
    let dir = models_dir();
    let path = ensure(&PARAKEET_V3_INT8, &dir, |done, total| {
        let pct = (done * 100).checked_div(total).unwrap_or(0);
        report(format!("downloading speech recognition: {pct}%"));
    })?;
    report("loading speech recognition".into());
    Ok(Box::new(voice::sherpa::SherpaStt::parakeet(&path, 2)?))
}

#[cfg(not(feature = "voice-sherpa"))]
fn make_stt(_report: &dyn Fn(String)) -> Result<Box<dyn voice::Stt>, voice::VoiceError> {
    Err(voice::VoiceError::Engine(
        "speech recognition not compiled in (sherpa feature)".into(),
    ))
}

/// The cue engine speaks through this trait.
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

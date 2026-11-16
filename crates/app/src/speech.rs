//! Voice setup: model, engine, audio output, and adapter to the cue engine.

use std::sync::Arc;

use voice::{AudioSink, CpalSink, NullSink, SilentTts, Speaker, SwitchableSink, Tts};

/// Opens the output `id` (`None`: system default); falls back to the default
/// device, then to no sound. Warnings go to `notes`.
pub fn open_output(id: Option<&str>, notes: &mut Vec<String>) -> Arc<dyn AudioSink> {
    if id.is_some() {
        match CpalSink::open_device(id) {
            Ok(s) => return Arc::new(s),
            Err(e) => notes.push(format!(
                "Coach output unavailable ({e}): using the default device"
            )),
        }
    }
    match CpalSink::open() {
        Ok(s) => Arc::new(s),
        Err(e) => {
            notes.push(format!("No sound: cues are only shown. {e}"));
            Arc::new(NullSink::default())
        }
    }
}

/// Starts the speech thread. The model is downloaded on first launch.
/// Returns the speaker and, unless `no_voice`, its switchable output.
pub fn start_speaker(
    no_voice: bool,
    language: voice::Language,
    output: Option<&str>,
    notes: &mut Vec<String>,
) -> (Arc<Speaker>, Option<Arc<SwitchableSink>>) {
    let switchable = (!no_voice).then(|| Arc::new(SwitchableSink::new(open_output(output, notes))));
    let sink: Arc<dyn AudioSink> = match &switchable {
        Some(s) => s.clone(),
        None => Arc::new(NullSink::default()),
    };
    let speaker = Arc::new(Speaker::spawn(
        move |report| make_tts(no_voice, language, report),
        sink,
    ));
    (speaker, switchable)
}

/// Opens the push-to-talk microphone `id` (`None`: system default), with the
/// same fallback as the output.
pub fn open_microphone(id: Option<&str>, notes: &mut Vec<String>) -> Arc<voice::Recorder> {
    let r = voice::Recorder::new();
    if id.is_some() {
        match r.select(id) {
            Ok(name) => {
                tracing::info!("microphone: {name}");
                return Arc::new(r);
            }
            Err(e) => notes.push(format!(
                "Saved microphone not opened, using the default one. {e}"
            )),
        }
    }
    match r.select(None) {
        Ok(name) => tracing::info!("microphone: {name}"),
        Err(e) => notes.push(format!("Push-to-talk unavailable. {e}")),
    }
    Arc::new(r)
}

#[cfg(feature = "voice-sherpa")]
fn make_tts(
    no_voice: bool,
    language: voice::Language,
    report: &dyn Fn(String),
) -> Result<Box<dyn Tts>, voice::VoiceError> {
    use voice::models::{ensure, models_dir};
    if no_voice {
        return Ok(Box::new(SilentTts::default()));
    }
    let dir = models_dir();
    let path = ensure(language.tts_model(), &dir, |done, total| {
        let pct = (done * 100).checked_div(total).unwrap_or(0);
        report(format!("downloading {} voice: {pct}%", language.name()));
    })?;
    report("loading voice".into());
    Ok(Box::new(voice::sherpa::SherpaTts::piper(&path, 2)?))
}

#[cfg(not(feature = "voice-sherpa"))]
fn make_tts(
    _no_voice: bool,
    _language: voice::Language,
    _report: &dyn Fn(String),
) -> Result<Box<dyn Tts>, voice::VoiceError> {
    Ok(Box::new(SilentTts::default()))
}

/// Speech recognition: Parakeet v3, downloaded on first launch (~670 MB).
/// Multilingual: it detects English and French by itself.
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

//! Coach voice.
//!
//! Engines sit behind two traits, [`Tts`] and [`Stt`], so they can be swapped
//! without touching the rest: sherpa-onnx (Piper, Kokoro, Parakeet) is the
//! first implementation; Windows voices or another engine can plug in.
//!
//! - [`output`]: audio output (cpal, or silent if there is no device);
//! - [`speaker`]: speech thread with a synthesis cache;
//! - [`models`]: model download from Hugging Face;
//! - `sherpa` (feature): sherpa-onnx engines.

pub mod language;
pub mod listener;
pub mod models;
pub mod output;
pub mod recorder;
#[cfg(feature = "sherpa")]
pub mod sherpa;
pub mod speaker;

use thiserror::Error;

pub use language::Language;
pub use listener::{Listener, ListenerStatus, Transcript};
pub use output::{AudioSink, CpalSink, NullSink};
pub use recorder::{Microphone, Recorder};
pub use speaker::{Speaker, SpeakerState, SpeakerStatus};

#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("model not found: {0}")]
    ModelMissing(String),
    #[error("engine failure: {0}")]
    Engine(String),
    #[error("audio output: {0}")]
    Output(String),
    #[error("download: {0}")]
    Download(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Mono audio as floats (-1..1).
#[derive(Debug, Clone, PartialEq)]
pub struct Audio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

impl Audio {
    pub fn duration_s(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.samples.len() as f64 / self.sample_rate as f64
        }
    }
}

/// Speech synthesis.
pub trait Tts {
    /// Display name of the engine and voice.
    fn name(&self) -> String;
    fn synthesize(&mut self, text: &str) -> Result<Audio, VoiceError>;
}

/// Speech recognition on a complete recording (push-to-talk).
pub trait Stt {
    fn name(&self) -> String;
    fn transcribe(&mut self, audio: &Audio) -> Result<String, VoiceError>;
}

/// Estimated duration of a spoken sentence (s), before synthesis. Measured with
/// Piper `fr_FR-siwis`: 15 to 20 characters per second.
pub fn estimate_duration(text: &str) -> f64 {
    0.35 + text.chars().count() as f64 / 16.0
}

/// Silent engine: produces silence of the estimated duration. Used when the
/// voice is disabled or unavailable, and in tests.
pub struct SilentTts {
    pub sample_rate: u32,
}

impl Default for SilentTts {
    fn default() -> Self {
        SilentTts {
            sample_rate: 16_000,
        }
    }
}

impl Tts for SilentTts {
    fn name(&self) -> String {
        "silent".into()
    }

    fn synthesize(&mut self, text: &str) -> Result<Audio, VoiceError> {
        let n = (estimate_duration(text) * self.sample_rate as f64) as usize;
        Ok(Audio {
            samples: vec![0.0; n],
            sample_rate: self.sample_rate,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_matches_measured_piper_rates() {
        // Piper siwis measurements: 35 characters → 2.31 s; 67 characters → 3.38 s.
        let a = estimate_duration("Brake five meters later for turn 5.");
        let b = estimate_duration(
            "You lose two tenths in turn four, get back on the throttle earlier.",
        );
        assert!((a - 2.31).abs() < 0.4, "{a}");
        assert!((b - 3.38).abs() < 1.2, "{b}");
        assert!(b > a);
    }

    #[test]
    fn silent_tts_has_estimated_duration() {
        let mut t = SilentTts::default();
        let a = t.synthesize("Hello").unwrap();
        assert!((a.duration_s() - estimate_duration("Hello")).abs() < 0.01);
    }
}

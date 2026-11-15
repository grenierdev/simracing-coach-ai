//! Voix du coach.
//!
//! Les moteurs sont derrière deux traits, [`Tts`] et [`Stt`], pour pouvoir en
//! changer sans toucher au reste : sherpa-onnx (Piper, Kokoro, Parakeet) est la
//! première implémentation ; les voix Windows ou un autre moteur peuvent s'y
//! brancher.
//!
//! - [`output`] : sortie audio (cpal, ou silencieuse si aucun périphérique) ;
//! - [`speaker`] : thread de parole avec cache de synthèse ;
//! - [`models`] : téléchargement des modèles depuis Hugging Face ;
//! - `sherpa` (feature) : moteurs sherpa-onnx.

pub mod models;
pub mod output;
#[cfg(feature = "sherpa")]
pub mod sherpa;
pub mod speaker;

use thiserror::Error;

pub use output::{AudioSink, CpalSink, NullSink};
pub use speaker::{Speaker, SpeakerState, SpeakerStatus};

#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("modèle introuvable : {0}")]
    ModelMissing(String),
    #[error("échec du moteur : {0}")]
    Engine(String),
    #[error("sortie audio : {0}")]
    Output(String),
    #[error("téléchargement : {0}")]
    Download(String),
    #[error("E/S : {0}")]
    Io(#[from] std::io::Error),
}

/// Audio mono en flottants (-1..1).
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

/// Synthèse vocale.
pub trait Tts {
    /// Nom affiché du moteur et de la voix.
    fn name(&self) -> String;
    fn synthesize(&mut self, text: &str) -> Result<Audio, VoiceError>;
}

/// Reconnaissance vocale sur un enregistrement complet (push-to-talk).
pub trait Stt {
    fn name(&self) -> String;
    fn transcribe(&mut self, audio: &Audio) -> Result<String, VoiceError>;
}

/// Durée estimée d'une phrase dite (s), avant synthèse. Mesuré avec Piper
/// `fr_FR-siwis` : 15 à 20 caractères par seconde.
pub fn estimate_duration(text: &str) -> f64 {
    0.35 + text.chars().count() as f64 / 16.0
}

/// Moteur muet : produit du silence de la durée estimée. Sert quand la voix
/// est désactivée ou indisponible, et dans les tests.
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
        "muet".into()
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
        // Mesures Piper siwis : 35 caractères → 2,31 s ; 67 caractères → 3,38 s.
        let a = estimate_duration("Freine cinq mètres plus tard au T5.");
        let b = estimate_duration(
            "Tu perds deux dixièmes au virage quatre, reprends les gaz plus tôt.",
        );
        assert!((a - 2.31).abs() < 0.4, "{a}");
        assert!((b - 3.38).abs() < 1.2, "{b}");
        assert!(b > a);
    }

    #[test]
    fn silent_tts_has_estimated_duration() {
        let mut t = SilentTts::default();
        let a = t.synthesize("Bonjour").unwrap();
        assert!((a.duration_s() - estimate_duration("Bonjour")).abs() < 0.01);
    }
}

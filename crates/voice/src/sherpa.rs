//! Moteurs sherpa-onnx : Piper (VITS) et Kokoro pour la synthèse, Parakeet
//! pour la reconnaissance.

use std::path::Path;

use sherpa_onnx::{
    GenerationConfig, OfflineRecognizer, OfflineRecognizerConfig, OfflineTts, OfflineTtsConfig,
};

use crate::{Audio, Stt, Tts, VoiceError};

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Premier fichier `.onnx` du dossier.
fn find_onnx(dir: &Path) -> Result<std::path::PathBuf, VoiceError> {
    std::fs::read_dir(dir)
        .map_err(|_| VoiceError::ModelMissing(path_str(dir)))?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "onnx"))
        .ok_or_else(|| VoiceError::ModelMissing(format!("aucun .onnx dans {}", dir.display())))
}

fn require(p: &Path) -> Result<String, VoiceError> {
    if p.exists() {
        Ok(path_str(p))
    } else {
        Err(VoiceError::ModelMissing(path_str(p)))
    }
}

pub struct SherpaTts {
    tts: OfflineTts,
    speaker_id: i32,
    speed: f32,
    label: String,
}

impl SherpaTts {
    /// Voix Piper : dossier contenant `<voix>.onnx`, `tokens.txt` et `espeak-ng-data/`.
    pub fn piper(dir: &Path, threads: i32) -> Result<Self, VoiceError> {
        let model = find_onnx(dir)?;
        let mut cfg = OfflineTtsConfig::default();
        cfg.model.vits.model = Some(path_str(&model));
        cfg.model.vits.tokens = Some(require(&dir.join("tokens.txt"))?);
        cfg.model.vits.data_dir = Some(require(&dir.join("espeak-ng-data"))?);
        cfg.model.num_threads = threads;
        let tts = OfflineTts::create(&cfg)
            .ok_or_else(|| VoiceError::Engine(format!("chargement de {}", model.display())))?;
        let label = model
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(SherpaTts {
            tts,
            speaker_id: 0,
            speed: 1.0,
            label: format!("Piper {label}"),
        })
    }

    /// Kokoro multilingue : `model.onnx`, `voices.bin`, `tokens.txt`,
    /// `espeak-ng-data/`. La voix française `ff_siwis` est le locuteur 30.
    pub fn kokoro(dir: &Path, speaker_id: i32, threads: i32) -> Result<Self, VoiceError> {
        let mut cfg = OfflineTtsConfig::default();
        cfg.model.kokoro.model = Some(require(&dir.join("model.onnx"))?);
        cfg.model.kokoro.voices = Some(require(&dir.join("voices.bin"))?);
        cfg.model.kokoro.tokens = Some(require(&dir.join("tokens.txt"))?);
        cfg.model.kokoro.data_dir = Some(require(&dir.join("espeak-ng-data"))?);
        cfg.model.kokoro.lang = Some("fr".into());
        cfg.model.num_threads = threads;
        let tts = OfflineTts::create(&cfg)
            .ok_or_else(|| VoiceError::Engine("chargement de Kokoro".into()))?;
        Ok(SherpaTts {
            tts,
            speaker_id,
            speed: 1.0,
            label: format!("Kokoro (locuteur {speaker_id})"),
        })
    }

    /// Débit : 1.0 = normal, 1.1 = 10 % plus rapide.
    pub fn with_speed(mut self, speed: f32) -> Self {
        self.speed = speed;
        self
    }
}

impl Tts for SherpaTts {
    fn name(&self) -> String {
        self.label.clone()
    }

    fn synthesize(&mut self, text: &str) -> Result<Audio, VoiceError> {
        let cfg = GenerationConfig {
            sid: self.speaker_id,
            speed: self.speed,
            ..Default::default()
        };
        let audio = self
            .tts
            .generate_with_config(text, &cfg, None::<fn(&[f32], f32) -> bool>)
            .ok_or_else(|| VoiceError::Engine(format!("synthèse de « {text} »")))?;
        Ok(Audio {
            samples: audio.samples().to_vec(),
            sample_rate: audio.sample_rate() as u32,
        })
    }
}

pub struct SherpaStt {
    recognizer: OfflineRecognizer,
}

impl SherpaStt {
    /// Parakeet TDT (transducteur NeMo) : `encoder*.onnx`, `decoder*.onnx`,
    /// `joiner*.onnx`, `tokens.txt`.
    pub fn parakeet(dir: &Path, threads: i32) -> Result<Self, VoiceError> {
        let pick = |prefix: &str| -> Result<String, VoiceError> {
            std::fs::read_dir(dir)
                .map_err(|_| VoiceError::ModelMissing(path_str(dir)))?
                .flatten()
                .map(|e| e.path())
                .find(|p| {
                    p.extension().is_some_and(|x| x == "onnx")
                        && p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
                })
                .map(|p| path_str(&p))
                .ok_or_else(|| {
                    VoiceError::ModelMissing(format!("{prefix}*.onnx dans {}", dir.display()))
                })
        };
        let mut cfg = OfflineRecognizerConfig::default();
        cfg.model_config.transducer.encoder = Some(pick("encoder")?);
        cfg.model_config.transducer.decoder = Some(pick("decoder")?);
        cfg.model_config.transducer.joiner = Some(pick("joiner")?);
        cfg.model_config.tokens = Some(require(&dir.join("tokens.txt"))?);
        cfg.model_config.model_type = Some("nemo_transducer".into());
        cfg.model_config.num_threads = threads;
        let recognizer = OfflineRecognizer::create(&cfg)
            .ok_or_else(|| VoiceError::Engine("chargement de Parakeet".into()))?;
        Ok(SherpaStt { recognizer })
    }
}

impl Stt for SherpaStt {
    fn name(&self) -> String {
        "Parakeet TDT 0.6B v3".into()
    }

    fn transcribe(&mut self, audio: &Audio) -> Result<String, VoiceError> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(audio.sample_rate as i32, &audio.samples);
        self.recognizer.decode(&stream);
        Ok(stream
            .get_result()
            .map(|r| r.text.trim().to_string())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    //! Tests avec les vrais modèles : lancés seulement si `SIMCOACH_MODELS_DIR`
    //! contient Piper siwis et Parakeet v3.
    use super::*;
    use crate::models::{PARAKEET_V3_INT8, PIPER_FR_SIWIS, is_present, models_dir};

    #[test]
    fn piper_then_parakeet_round_trip() {
        if std::env::var_os("SIMCOACH_MODELS_DIR").is_none() {
            eprintln!("SIMCOACH_MODELS_DIR absent : test ignoré");
            return;
        }
        let dir = models_dir();
        assert!(is_present(&PIPER_FR_SIWIS, &dir) && is_present(&PARAKEET_V3_INT8, &dir));
        let mut tts = SherpaTts::piper(&PIPER_FR_SIWIS.path_in(&dir), 2).unwrap();
        let mut stt = SherpaStt::parakeet(&PARAKEET_V3_INT8.path_in(&dir), 2).unwrap();
        let audio = tts.synthesize("Je n'ai plus de rotation au T5.").unwrap();
        assert!(audio.duration_s() > 1.0);
        let text = stt.transcribe(&audio).unwrap();
        assert_eq!(text, "Je n'ai plus de rotation au T5.");
    }
}

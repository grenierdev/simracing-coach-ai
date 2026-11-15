//! Modèles vocaux : emplacement local et téléchargement depuis Hugging Face.
//!
//! Chaque modèle est un dépôt Hugging Face recopié dans
//! `<dossier des modèles>/<nom du dépôt>/`. Un fichier `.complet` marque un
//! téléchargement terminé.
//!
//! Dossier des modèles : `SIMCOACH_MODELS_DIR`, sinon le dossier de données
//! local de l'utilisateur (`%LOCALAPPDATA%\simcoach\data\models` sous Windows).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::VoiceError;

#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    /// Dépôt Hugging Face, `propriétaire/nom`.
    pub repo: &'static str,
    /// Fichiers à garder parmi ceux du dépôt.
    pub keep: fn(&str) -> bool,
}

impl ModelSpec {
    pub fn dir_name(&self) -> &'static str {
        self.repo.rsplit('/').next().unwrap_or(self.repo)
    }

    pub fn path_in(&self, models_dir: &Path) -> PathBuf {
        models_dir.join(self.dir_name())
    }
}

/// Voix française Piper « siwis » (féminine, CC-BY 4.0).
pub const PIPER_FR_SIWIS: ModelSpec = ModelSpec {
    repo: "csukuangfj/vits-piper-fr_FR-siwis-medium",
    keep: |p| !p.ends_with(".py") && !p.ends_with(".sh") && !p.starts_with('.'),
};

/// Reconnaissance vocale Parakeet TDT 0.6B v3, int8 (25 langues dont le français).
pub const PARAKEET_V3_INT8: ModelSpec = ModelSpec {
    repo: "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
    keep: |p| p.ends_with(".onnx") || p == "tokens.txt",
};

const DONE_MARKER: &str = ".complet";

/// Client HTTP : certificats du système et proxy des variables d'environnement.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}

pub fn models_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SIMCOACH_MODELS_DIR") {
        return PathBuf::from(d);
    }
    directories::ProjectDirs::from("", "", "simcoach")
        .map(|p| p.data_local_dir().join("models"))
        .unwrap_or_else(|| PathBuf::from("models"))
}

/// Le modèle est-il présent ? Un dossier sans marqueur mais contenant un
/// `.onnx` est accepté (copie manuelle).
pub fn is_present(spec: &ModelSpec, models_dir: &Path) -> bool {
    let dir = spec.path_in(models_dir);
    dir.join(DONE_MARKER).exists()
        || std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .any(|e| e.path().extension().is_some_and(|x| x == "onnx"))
            })
            .unwrap_or(false)
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    #[serde(default)]
    size: u64,
}

fn encode_path(p: &str) -> String {
    p.split('/')
        .map(|seg| {
            seg.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Télécharge le modèle s'il manque. `progress(octets reçus, octets attendus)`.
pub fn ensure(
    spec: &ModelSpec,
    models_dir: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf, VoiceError> {
    let dir = spec.path_in(models_dir);
    if is_present(spec, models_dir) {
        return Ok(dir);
    }
    let dl = |e: ureq::Error| VoiceError::Download(e.to_string());
    let agent = agent();
    let listing: Vec<Entry> = agent
        .get(&format!(
            "https://huggingface.co/api/models/{}/tree/main?recursive=true",
            spec.repo
        ))
        .call()
        .map_err(dl)?
        .body_mut()
        .read_json()
        .map_err(dl)?;
    let files: Vec<&Entry> = listing
        .iter()
        .filter(|e| e.kind == "file" && (spec.keep)(&e.path))
        .collect();
    let total: u64 = files.iter().map(|e| e.size).sum();
    let mut done = 0u64;
    for e in files {
        let dst = dir.join(&e.path);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dst
            .metadata()
            .is_ok_and(|m| m.len() == e.size && e.size > 0)
        {
            done += e.size;
            progress(done, total);
            continue;
        }
        let url = format!(
            "https://huggingface.co/{}/resolve/main/{}",
            spec.repo,
            encode_path(&e.path)
        );
        let tmp = dst.with_extension("partiel");
        let mut out = std::fs::File::create(&tmp)?;
        let mut body = agent.get(&url).call().map_err(dl)?.into_body();
        let mut reader = body.as_reader();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = std::io::Read::read(&mut reader, &mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            done += n as u64;
            progress(done, total);
        }
        drop(out);
        std::fs::rename(&tmp, &dst)?;
    }
    std::fs::write(dir.join(DONE_MARKER), b"")?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_special_paths() {
        assert_eq!(
            encode_path("espeak-ng-data/voices/!v/Mr serious"),
            "espeak-ng-data/voices/%21v/Mr%20serious"
        );
    }

    #[test]
    fn filters_and_detects_presence() {
        assert!((PIPER_FR_SIWIS.keep)("espeak-ng-data/fr_dict"));
        assert!(!(PIPER_FR_SIWIS.keep)("vits-piper-fr_FR.py"));
        assert!((PARAKEET_V3_INT8.keep)("encoder.int8.onnx"));
        assert!(!(PARAKEET_V3_INT8.keep)("test_wavs/fr.wav"));

        let tmp = std::env::temp_dir().join(format!("simcoach-models-{}", std::process::id()));
        let dir = PIPER_FR_SIWIS.path_in(&tmp);
        assert!(!is_present(&PIPER_FR_SIWIS, &tmp));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fr_FR-siwis-medium.onnx"), b"x").unwrap();
        assert!(is_present(&PIPER_FR_SIWIS, &tmp));
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}

//! Réglages du pilote, enregistrés en TOML dans le dossier de configuration
//! (`%APPDATA%\simcoach\config\settings.toml` sous Windows).

use std::path::PathBuf;

use input::Binding;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Bouton du push-to-talk.
    pub ptt: Option<Binding>,
    /// Les réponses du coach sont dites à voix haute.
    pub speak_replies: bool,
    /// La session d'analyse prépare les consignes du tour suivant.
    pub auto_planning: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ptt: None,
            speak_replies: true,
            auto_planning: true,
        }
    }
}

pub fn settings_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SIMCOACH_SETTINGS") {
        return PathBuf::from(p);
    }
    directories::ProjectDirs::from("", "", "simcoach")
        .map(|p| p.config_dir().join("settings.toml"))
        .unwrap_or_else(|| PathBuf::from("settings.toml"))
}

impl Settings {
    pub fn load() -> Self {
        Self::load_from(&settings_path())
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&settings_path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let path =
            std::env::temp_dir().join(format!("simcoach-settings-{}.toml", std::process::id()));
        assert_eq!(Settings::load_from(&path), Settings::default());
        let s = Settings {
            ptt: Some(Binding::Controller {
                device: "Volant".into(),
                instance: "{1234}".into(),
                button: 7,
            }),
            speak_replies: false,
            auto_planning: false,
        };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), s);
        std::fs::remove_file(&path).unwrap();
    }
}

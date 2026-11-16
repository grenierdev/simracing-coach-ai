//! Driver settings, saved as TOML in the configuration folder
//! (`%APPDATA%\simcoach\config\settings.toml` on Windows).

use std::path::PathBuf;

use input::Binding;
use serde::{Deserialize, Serialize};
use voice::Language;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Push-to-talk button.
    pub ptt: Option<Binding>,
    /// The coach's replies are spoken aloud.
    pub speak_replies: bool,
    /// The analysis session prepares the cues for the next lap.
    pub auto_planning: bool,
    /// Voice language: speech recognition, text-to-speech, coach replies.
    pub language: Language,
    /// Push-to-talk microphone; system default if absent.
    pub microphone: Option<DeviceChoice>,
    /// Output for the coach's voice; system default if absent.
    pub output: Option<DeviceChoice>,
}

/// An audio device saved by id; the name is for display (and for the log
/// when the device is no longer connected).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceChoice {
    pub id: String,
    pub name: String,
}

impl DeviceChoice {
    pub fn id(choice: &Option<DeviceChoice>) -> Option<&str> {
        choice.as_ref().map(|c| c.id.as_str())
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ptt: None,
            speak_replies: true,
            auto_planning: true,
            language: Language::default(),
            microphone: None,
            output: None,
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
                device: "Wheel".into(),
                instance: "{1234}".into(),
                button: 7,
            }),
            speak_replies: false,
            auto_planning: false,
            language: Language::French,
            microphone: Some(DeviceChoice {
                id: "wasapi:{0.0.1.00000000}.{abc}".into(),
                name: "Headset microphone".into(),
            }),
            output: None,
        };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), s);
        std::fs::remove_file(&path).unwrap();
        let text = toml::to_string(&s).unwrap();
        assert!(text.contains("language = \"fr\""), "{text}");
    }
}

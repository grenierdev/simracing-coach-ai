//! Voice language: speech recognition, text-to-speech and the coach's replies.

use serde::{Deserialize, Serialize};

use crate::models::{ModelSpec, PIPER_EN_LJSPEECH, PIPER_FR_SIWIS};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "fr")]
    French,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::English, Language::French];

    /// ISO 639-1 code.
    pub fn code(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::French => "fr",
        }
    }

    /// English name, used in the prompt sent to Claude.
    pub fn name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::French => "French",
        }
    }

    /// Name shown in the UI, in the language itself.
    pub fn native_name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::French => "Français",
        }
    }

    /// Accepts a code ("fr") or a name ("french", "français").
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_lowercase();
        Language::ALL.into_iter().find(|l| {
            s == l.code() || s == l.name().to_lowercase() || s == l.native_name().to_lowercase()
        })
    }

    pub fn next(self) -> Self {
        match self {
            Language::English => Language::French,
            Language::French => Language::English,
        }
    }

    /// Piper voice for this language.
    pub fn tts_model(self) -> &'static ModelSpec {
        match self {
            Language::English => &PIPER_EN_LJSPEECH,
            Language::French => &PIPER_FR_SIWIS,
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.native_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_codes_and_names() {
        assert_eq!(Language::parse("fr"), Some(Language::French));
        assert_eq!(Language::parse("Français"), Some(Language::French));
        assert_eq!(Language::parse("english"), Some(Language::English));
        assert_eq!(Language::parse("de"), None);
        assert_eq!(Language::French.next(), Language::English);
    }

    #[test]
    fn each_language_has_its_own_voice() {
        assert_ne!(
            Language::English.tts_model().repo,
            Language::French.tts_model().repo
        );
    }
}

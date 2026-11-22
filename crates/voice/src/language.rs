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

    /// The language a short sentence is clearly written in, from its common
    /// words (and accents, for French). `None` when unsure: too few known
    /// words, or a real mix. Meant for cues: "Brake ten meters later." or
    /// "Freine dix mètres plus tard."
    pub fn detect(text: &str) -> Option<Language> {
        let (mut en, mut fr) = (0u32, 0u32);
        let words = text
            .split(|c: char| !(c.is_alphabetic() || c == '\'' || c == '’'))
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase);
        for word in words {
            // French elision: "l'apex", "c'est", "jusqu'à".
            let word = match word.split_once(['\'', '’']) {
                Some((head, rest)) if listed(FR_ELISIONS, head) => {
                    fr += 1;
                    rest.to_string()
                }
                _ => word,
            };
            if word.chars().any(|c| FR_ACCENTS.contains(c)) || listed(FR_WORDS, &word) {
                fr += 1;
            } else if listed(EN_WORDS, &word) {
                en += 1;
            }
        }
        if fr >= 2 && fr >= 3 * en {
            Some(Language::French)
        } else if en >= 2 && en >= 3 * fr {
            Some(Language::English)
        } else {
            None
        }
    }
}

const FR_ACCENTS: &str = "éèêëàâçîïôûùœ";
const FR_ELISIONS: &str = "l d c j m n s t qu jusqu lorsqu puisqu";
/// Common words of driving advice found in one language only. English
/// jargon used as is in French ("apex", "trail braking") is left out.
const FR_WORDS: &str = "le la les un une des du de au aux et ou en dans avec sur pour par \
    que qui pas plus moins trop peu bien encore avant tard vite est sont tu ta ton tes il elle \
    ce cette freine freiner frein freinage virage virages gaz ouvre sortie corde garde rapport \
    ligne trajectoire vitesse volant vibreur panneau utilise pousse reste essaie tourne deux \
    trois quatre cinq sept huit neuf dix secondes beaucoup pareil appuie chaque";
const EN_WORDS: &str = "the and or in into with at to of for from than that this is it it's \
    are be not don't you your you're we more less too much little bit good nice very again same \
    before after late later early earlier turn turns brake braking brakes throttle gas lift \
    release keep hold stay push try carry use open exit entry wide tight inside outside line \
    speed faster slower gear steering curb kerb board one two three four five seven eight nine \
    ten meters seconds tenth tenths on off down up";

fn listed(list: &str, word: &str) -> bool {
    list.split_whitespace().any(|w| w == word)
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
    fn detects_the_language_of_cues() {
        use Language::{English, French};
        for (text, lang) in [
            ("Brake ten meters later.", Some(English)),
            ("Same again.", Some(English)),
            ("Turn five, carry more speed to the apex.", Some(English)),
            ("Freine dix mètres plus tard.", Some(French)),
            (
                "Virage cinq, garde plus de vitesse jusqu'à la corde.",
                Some(French),
            ),
            (
                "Freine plus tard, trail braking jusqu'à l'apex.",
                Some(French),
            ),
            ("Lâche le frein plus tôt.", Some(French)),
            ("C'est bien, encore.", Some(French)),
            ("Pareil.", None),
            ("T5.", None),
            ("Brake plus tard.", None),
        ] {
            assert_eq!(Language::detect(text), lang, "{text}");
        }
    }

    #[test]
    fn each_language_has_its_own_voice() {
        assert_ne!(
            Language::English.tts_model().repo,
            Language::French.tts_model().repo
        );
    }
}

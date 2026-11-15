//! Découpage du flux de réponse en phrases, pour les dire au fil de l'eau :
//! la première phrase (l'accusé « Je regarde le T5. ») part dès qu'elle est
//! complète, sans attendre la fin de la réponse.

#[derive(Debug, Default)]
pub struct SentenceSplitter {
    buf: String,
}

fn is_end(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '…' | '\n')
}

impl SentenceSplitter {
    /// Ajoute un morceau ; retourne les phrases complètes.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        let mut out = vec![];
        loop {
            // Fin de phrase : ponctuation finale suivie d'un blanc (« 1.5 » ou
            // « T5.2 » ne coupent pas), ou retour à la ligne.
            let chars: Vec<(usize, char)> = self.buf.char_indices().collect();
            let cut = chars.iter().enumerate().find_map(|(k, &(i, c))| {
                if c == '\n' {
                    return Some(i + c.len_utf8());
                }
                if is_end(c) {
                    let next = chars.get(k + 1).map(|&(_, n)| n);
                    if next.is_some_and(char::is_whitespace) {
                        return Some(i + c.len_utf8());
                    }
                }
                None
            });
            let Some(cut) = cut else { break };
            let sentence = self.buf[..cut].trim().to_string();
            self.buf.drain(..cut);
            if !sentence.is_empty() {
                out.push(sentence);
            }
        }
        out
    }

    /// Fin du tour : le reste, s'il y en a.
    pub fn finish(&mut self) -> Option<String> {
        let rest = self.buf.trim().to_string();
        self.buf.clear();
        (!rest.is_empty()).then_some(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_streamed_sentences() {
        let mut s = SentenceSplitter::default();
        assert!(s.push("Je regarde").is_empty());
        assert_eq!(s.push(" le T5. Tu fr"), vec!["Je regarde le T5."]);
        assert!(s.push("eines 1.5 seconde").is_empty());
        assert_eq!(
            s.push(" trop tôt ! Relâche plus tard"),
            vec!["Tu freines 1.5 seconde trop tôt !"]
        );
        assert_eq!(s.finish().as_deref(), Some("Relâche plus tard"));
        assert_eq!(s.finish(), None);
    }

    #[test]
    fn newline_ends_a_sentence() {
        let mut s = SentenceSplitter::default();
        assert_eq!(s.push("Premier point\nDeuxième"), vec!["Premier point"]);
        assert_eq!(s.finish().as_deref(), Some("Deuxième"));
    }
}

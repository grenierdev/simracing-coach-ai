//! Splits the reply stream into sentences so they can be spoken as they arrive:
//! the first sentence (the acknowledgement "Looking at T5.") goes out as soon
//! as it is complete, without waiting for the end of the reply.

#[derive(Debug, Default)]
pub struct SentenceSplitter {
    buf: String,
}

fn is_end(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '…' | '\n')
}

impl SentenceSplitter {
    /// Adds a chunk; returns the complete sentences.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        let mut out = vec![];
        loop {
            // End of sentence: final punctuation followed by whitespace ("1.5" or
            // "T5.2" do not split), or a line break.
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

    /// End of turn: the remainder, if any.
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
        assert!(s.push("Looking").is_empty());
        assert_eq!(s.push(" at T5. You br"), vec!["Looking at T5."]);
        assert!(s.push("ake 1.5 seconds").is_empty());
        assert_eq!(
            s.push(" too early! Release later"),
            vec!["You brake 1.5 seconds too early!"]
        );
        assert_eq!(s.finish().as_deref(), Some("Release later"));
        assert_eq!(s.finish(), None);
    }

    #[test]
    fn newline_ends_a_sentence() {
        let mut s = SentenceSplitter::default();
        assert_eq!(s.push("First point\nSecond"), vec!["First point"]);
        assert_eq!(s.finish().as_deref(), Some("Second"));
    }
}

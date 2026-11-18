//! What a search matches: a fixed string or a regular expression, with the case, word
//! and prefilter rules applied. This is the one place the rest of the searcher asks
//! "does this line match, and where".

use crate::bytes;
use crate::regex::{Flags, Regex};

pub use crate::regex::Match;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// A plain substring (the `-F` form, or a pattern with no regex syntax).
    Literal,
    Regex,
}

#[derive(Debug)]
pub struct Pattern {
    kind: PatternKind,
    source: String,
    /// The literal bytes, for `Literal` patterns.
    lit: Vec<u8>,
    ci: bool,
    word: bool,
    re: Option<Regex>,
    /// A byte every match must contain, used to skip lines without running the VM.
    required: Option<u8>,
}

impl Pattern {
    /// Compile `text`. `fixed` forces a literal; otherwise regex syntax is honoured and a
    /// pattern that compiles to a plain literal gets the fast path anyway.
    pub fn new(text: &str, fixed: bool, ignore_case: bool, word: bool) -> Result<Pattern, String> {
        if text.is_empty() {
            return Err("empty pattern".to_string());
        }
        if fixed {
            return Ok(Pattern {
                kind: PatternKind::Literal,
                source: text.to_string(),
                lit: text.as_bytes().to_vec(),
                ci: ignore_case,
                word,
                re: None,
                required: ignore_case.then(|| text.as_bytes()[0].to_ascii_lowercase()),
            });
        }
        let flags = Flags {
            case_insensitive: ignore_case,
            ..Default::default()
        };
        let re = Regex::new_with(text, flags)?;
        // A literal whole pattern is the common "grep for a word" case: scan bytes.
        if let Some(lit) = re.literal() {
            if !lit.is_empty() {
                return Ok(Pattern {
                    kind: PatternKind::Literal,
                    source: text.to_string(),
                    lit: lit.to_vec(),
                    ci: ignore_case,
                    word,
                    re: None,
                    required: None,
                });
            }
        }
        let required = re.required_byte();
        Ok(Pattern {
            kind: PatternKind::Regex,
            source: text.to_string(),
            lit: Vec::new(),
            ci: ignore_case,
            word,
            re: Some(re),
            required,
        })
    }

    pub fn kind(&self) -> PatternKind {
        self.kind
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// The literal bytes when the whole pattern is one and matching is case-sensitive.
    pub fn fast_literal(&self) -> Option<&[u8]> {
        (self.kind == PatternKind::Literal && !self.ci).then_some(&self.lit[..])
    }

    /// True when the pattern is a plain literal and the whole line must equal it (`-x`).
    pub fn matches_line(&self, line: &[u8]) -> bool {
        self.kind == PatternKind::Literal
            && !self.ci
            && !self.word
            && self.lit.as_slice() == line
    }

    /// The next acceptable match at or after `from`.
    pub fn find_at(&self, hay: &[u8], from: usize) -> Option<Match> {
        if from > hay.len() {
            return None;
        }
        match self.kind {
            PatternKind::Literal => self.find_literal(hay, from),
            PatternKind::Regex => {
                // Cheap prefilter: a line without the required byte cannot match.
                if let Some(b) = self.required {
                    if bytes::find_byte(hay, b, from).is_none() {
                        return None;
                    }
                }
                let re = self.re.as_ref()?;
                let mut at = from;
                while at <= hay.len() {
                    let m = re.find_at(hay, at)?;
                    if !self.word || bytes::at_word_boundary(hay, m.start, m.end) {
                        return Some(m);
                    }
                    at = m.start + 1;
                }
                None
            }
        }
    }

    fn find_literal(&self, hay: &[u8], from: usize) -> Option<Match> {
        if !self.word && !self.ci {
            let i = bytes::memmem_from(hay, &self.lit, from)?;
            return Some(Match {
                start: i,
                end: i + self.lit.len(),
            });
        }
        let mut at = from;
        while at <= hay.len() {
            let i = if self.ci {
                bytes::memmem_ci_from(hay, &self.lit, at)?
            } else {
                bytes::memmem_from(hay, &self.lit, at)?
            };
            let m = Match {
                start: i,
                end: i + self.lit.len(),
            };
            if !self.word || bytes::at_word_boundary(hay, m.start, m.end) {
                return Some(m);
            }
            at = i + 1;
        }
        None
    }

    /// Does the line match? For `-v` the caller inverts the answer.
    pub fn is_match(&self, hay: &[u8]) -> bool {
        self.find_at(hay, 0).is_some()
    }

    /// Every non-overlapping match on the line, for `-o` and for colouring.
    pub fn find_all(&self, hay: &[u8], limit: usize) -> Vec<Match> {
        let mut out = Vec::new();
        let mut at = 0;
        while out.len() < limit {
            let Some(m) = self.find_at(hay, at) else { break };
            let next = if m.end > m.start { m.end } else { m.start + 1 };
            out.push(m);
            if next > hay.len() {
                break;
            }
            at = next;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Pattern {
        Pattern::new(s, false, false, false).unwrap()
    }

    #[test]
    fn literal_patterns_use_the_fast_path() {
        let x = p("needle");
        assert_eq!(x.kind(), PatternKind::Literal);
        assert_eq!(x.fast_literal(), Some(&b"needle"[..]));
        assert!(x.is_match(b"a needle in a haystack"));
        assert!(!x.is_match(b"needles are sharp"));
        let fixed = Pattern::new("a.c", true, false, false).unwrap();
        assert!(fixed.is_match(b"xa.cy"));
        assert!(!p("a.c").is_match(b"abc"), "regex dot");
    }

    #[test]
    fn case_and_word_rules() {
        let ci = Pattern::new("Needle", false, true, false).unwrap();
        assert!(ci.is_match(b"a NEEDLE b"));
        assert_eq!(ci.fast_literal(), None, "case folding needs the slow path");
        let w = Pattern::new("cat", false, false, true).unwrap();
        assert!(w.is_match(b"a cat."));
        assert!(!w.is_match(b"concatenate"));
        assert!(!w.is_match(b"cats"));
        let wré = Pattern::new(r"ca[a-z]", false, false, true).unwrap();
        assert!(wré.is_match(b"the cab here"));
        assert!(!wré.is_match(b"scab"));
    }

    #[test]
    fn regex_patterns_with_prefilter() {
        let r = p(r"error:\s+\d+");
        assert_eq!(r.kind(), PatternKind::Regex);
        assert!(r.is_match(b"error:   500"));
        assert!(!r.is_match(b"error: none"));
        assert_eq!(
            p("x").find_all(b"x x x", 10).len(),
            3,
            "non-overlapping iteration"
        );
        assert_eq!(
            p("aa").find_all(b"aaaa", 10).len(),
            2,
            "non-overlapping pairs"
        );
    }

    #[test]
    fn errors_and_edges() {
        assert!(Pattern::new("", false, false, false).is_err());
        assert!(Pattern::new("(unclosed", false, false, false).is_err());
        let x = p("z");
        assert_eq!(x.find_at(b"abc", 0), None);
        assert_eq!(x.find_at(b"abc", 10), None);
        assert_eq!(x.find_at(b"abz", 3), None);
        assert_eq!(x.find_at(b"abz", 2).map(|m| m.start), Some(2));
    }
}

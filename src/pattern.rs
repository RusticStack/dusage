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
        // A literal whole pattern is the common "grep for a word" case: scan bytes. The
        // literal fast path is disabled inside the regex when `-i` folds, so the checker
        // parses the pattern once more without folding to see whether it is plain.
        let plain = if ignore_case {
            Regex::new_with(text, Flags::default())
                .ok()
                .and_then(|r| r.literal().map(<[u8]>::to_vec))
        } else {
            re.literal().map(<[u8]>::to_vec)
        };
        if let Some(lit) = plain {
            if !lit.is_empty() {
                let required = rarest_byte(&lit);
                return Ok(Pattern {
                    kind: PatternKind::Literal,
                    source: text.to_string(),
                    lit,
                    ci: ignore_case,
                    word,
                    re: None,
                    required,
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
        self.kind == PatternKind::Literal && !self.ci && !self.word && self.lit.as_slice() == line
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
                    bytes::find_byte(hay, b, from)?;
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
        // A non-ASCII needle under `-i` is folded per character, so `-i` matches `É` for `é`
        // the way `rg` and `grep` do. Lines that are not valid UTF-8 keep the ASCII fold.
        if self.ci && !self.lit.is_ascii() {
            if let Some(m) = self.find_literal_unicode(hay, from) {
                return Some(m);
            }
        }
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

    /// Unicode case folding for a literal needle: the line and the needle are lowered with
    /// `char::to_lowercase`, and a byte map carries folded offsets back to the original line.
    fn find_literal_unicode(&self, hay: &[u8], from: usize) -> Option<Match> {
        let text = std::str::from_utf8(hay).ok()?;
        let (folded, map) = fold_with_map(text);
        let needle = self.source.to_lowercase();
        let mut at = map.partition_point(|&off| off < from);
        while at <= folded.len() {
            let i = bytes::memmem_from(folded.as_bytes(), needle.as_bytes(), at)?;
            let end_folded = i + needle.len();
            let start = *map.get(i)?;
            let end = map.get(end_folded).copied().unwrap_or(text.len());
            let m = Match { start, end };
            if !self.word || bytes::at_word_boundary(hay, m.start, m.end) {
                return Some(m);
            }
            at = i + 1;
            if at > folded.len() {
                break;
            }
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
            let Some(m) = self.find_at(hay, at) else {
                break;
            };
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

/// The least frequent byte of a needle, by a plain frequency table of English-ish text. It is
/// only a filter, so a wrong guess costs a false positive, never a missed match.
fn rarest_byte(lit: &[u8]) -> Option<u8> {
    // Rough probabilities: letters in the middle of a word are common, digits and symbols are
    // not, whitespace sits in between.
    fn weight(b: u8) -> u32 {
        match b {
            b'e' | b't' | b'a' | b'o' | b'i' | b'n' | b's' | b'r' => 100,
            b' ' | b'h' | b'l' | b'd' | b'c' | b'u' | b'm' | b'f' | b'p' | b'g' | b'y' => 60,
            b'w' | b'b' | b'v' | b'k' | b'x' | b'j' | b'q' | b'z' => 12,
            b'0'..=b'9' => 20,
            b'\n' | b'\t' | b'\r' => 5,
            _ => 30,
        }
    }
    lit.iter().copied().min_by_key(|b| weight(*b))
}

/// Lowercase `s` and return, for every byte of the folded form, the byte offset of the
/// original character it came from. Folding can change lengths (`İ` lowers to two chars), so
/// the map is what keeps spans pointing at the line the user sees.
fn fold_with_map(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    for (idx, ch) in s.char_indices() {
        for lower in ch.to_lowercase() {
            out.push(lower);
            for _ in 0..lower.len_utf8() {
                map.push(idx);
            }
        }
    }
    (out, map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Pattern {
        Pattern::new(s, false, false, false).unwrap()
    }

    #[test]
    fn unicode_case_folding_for_literals() {
        let ci = Pattern::new("café", false, true, false).unwrap();
        assert!(ci.is_match("un CAFÉ noir".as_bytes()), "folds non-ASCII");
        assert_eq!(
            ci.find_at("un CAFÉ noir".as_bytes(), 0),
            Some(Match { start: 3, end: 8 })
        );
        let word = Pattern::new("café", false, true, true).unwrap();
        assert!(!word.is_match("xcafé".as_bytes()), "-w rejects a prefix");
        assert!(word.is_match("le café.".as_bytes()));
        let plain = Pattern::new("caf", false, false, true).unwrap();
        assert!(
            !plain.is_match("un café".as_bytes()),
            "-w does not match inside a non-ASCII word"
        );
        let bad = Pattern::new("naïve", false, true, false).unwrap();
        assert!(
            !bad.is_match(b"na\xefve"),
            "invalid UTF-8 falls back to bytes"
        );
    }

    #[test]
    fn literal_patterns_use_the_fast_path() {
        let x = p("needle");
        assert_eq!(x.kind(), PatternKind::Literal);
        assert_eq!(x.fast_literal(), Some(&b"needle"[..]));
        assert!(x.is_match(b"a needle in a haystack"));
        assert!(
            x.is_match(b"needles are sharp"),
            "a literal is a substring match"
        );
        let fixed = Pattern::new("a.c", true, false, false).unwrap();
        assert!(fixed.is_match(b"xa.cy"));
        assert!(!fixed.is_match(b"abc"), "-F is literal");
        assert!(
            p("a.c").is_match(b"abc"),
            "without -F the dot is a wildcard"
        );
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

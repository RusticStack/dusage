//! What a search matches: a fixed string or a regular expression, with the case, word
//! and prefilter rules applied. This is the one place the rest of the searcher asks
//! "does this line match, and where".

use crate::bytes;
use crate::pcre::Pcre;
use crate::regex::{Flags, Regex};

pub use crate::regex::Match;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// A plain substring (the `-F` form, or a pattern with no regex syntax).
    Literal,
    /// The Pike VM: everything without lookaround or backreferences.
    Regex,
    /// A backtracking matcher, used when the pattern needs lookaround or backreferences.
    Pcre,
}

#[derive(Debug)]
pub struct Pattern {
    kind: PatternKind,
    source: String,
    /// The literal bytes, for `Literal` patterns.
    lit: Vec<u8>,
    /// The same bytes with the scan plan already built: which byte to look for and the skip
    /// table. `dusage search` asks this per line, so nothing about the needle is recomputed.
    finder: Option<bytes::Finder>,
    ci: bool,
    word: bool,
    re: Option<Regex>,
    /// The backtracking matcher, when the pattern is beyond the Pike VM.
    pcre: Option<Pcre>,
    /// A byte every match must contain, used to skip lines without running the VM.
    required: Option<u8>,
}

impl Pattern {
    /// Compile `text`. `fixed` forces a literal; otherwise regex syntax is honoured and a
    /// pattern that compiles to a plain literal gets the fast path anyway.
    pub fn new(text: &str, fixed: bool, ignore_case: bool, word: bool) -> Result<Pattern, String> {
        Pattern::with_engine(text, fixed, ignore_case, word, false)
    }

    /// Like [`Pattern::new`], with `pcre` forcing the backtracking engine the way `-P` does.
    pub fn with_engine(
        text: &str,
        fixed: bool,
        ignore_case: bool,
        word: bool,
        pcre: bool,
    ) -> Result<Pattern, String> {
        if text.is_empty() {
            return Err("empty pattern".to_string());
        }
        if pcre && !fixed {
            return Ok(Pattern {
                kind: PatternKind::Pcre,
                source: text.to_string(),
                lit: Vec::new(),
                finder: None,
                ci: ignore_case,
                word,
                re: None,
                pcre: Some(Pcre::new(text)?),
                required: None,
            });
        }
        if fixed {
            return Ok(Pattern {
                kind: PatternKind::Literal,
                source: text.to_string(),
                lit: text.as_bytes().to_vec(),
                finder: Some(bytes::Finder::new(text.as_bytes(), ignore_case)),
                ci: ignore_case,
                word,
                re: None,
                pcre: None,
                required: None,
            });
        }
        let flags = Flags {
            case_insensitive: ignore_case,
            ..Default::default()
        };
        let re = match Regex::new_with(text, flags) {
            Ok(re) => re,
            // Lookaround and backreferences are beyond the Pike VM; a bounded backtracking
            // engine covers exactly those, so a pattern that needs them still works.
            Err(err) => match Pcre::new(text) {
                Ok(pcre) => {
                    return Ok(Pattern {
                        kind: PatternKind::Pcre,
                        source: text.to_string(),
                        lit: Vec::new(),
                        finder: None,
                        ci: ignore_case,
                        word,
                        re: None,
                        pcre: Some(pcre),
                        required: None,
                    })
                }
                Err(_) => return Err(err),
            },
        };
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
                return Ok(Pattern {
                    kind: PatternKind::Literal,
                    source: text.to_string(),
                    finder: Some(bytes::Finder::new(&lit, ignore_case)),
                    lit,
                    ci: ignore_case,
                    word,
                    re: None,
                    pcre: None,
                    required: None,
                });
            }
        }
        let required = re.required_byte();
        Ok(Pattern {
            kind: PatternKind::Regex,
            source: text.to_string(),
            lit: Vec::new(),
            finder: None,
            ci: ignore_case,
            word,
            re: Some(re),
            pcre: None,
            required,
        })
    }

    /// The byte scanner for the block path: a plain literal whose matching rules the byte
    /// scanner reproduces exactly. A literal that needs Unicode folding takes the line path,
    /// because the scanner folds ASCII only.
    pub fn block_finder(&self) -> Option<&bytes::Finder> {
        if self.ci && !self.lit.is_ascii() {
            return None;
        }
        self.finder.as_ref()
    }

    /// True when this pattern can be matched against a whole block: no line-relative
    /// assertion, no word requirement, and a matcher whose semantics cover the block.
    pub fn block_safe(&self) -> bool {
        if self.word {
            return false;
        }
        match self.kind {
            PatternKind::Literal => self.block_finder().is_some(),
            PatternKind::Regex => self.block_re().is_some(),
            PatternKind::Pcre => false,
        }
    }

    /// The compiled regex when a block scan may use it: no line-relative assertion (or `^`
    /// and `$` would mean the block's ends) and no instruction that permits a newline (or a
    /// match could straddle a line and could not be attributed to one).
    pub fn block_re(&self) -> Option<&Regex> {
        let re = self.re.as_ref()?;
        // A pattern that needs Unicode semantics is answered by the character program on
        // text that is not ASCII, which a byte-level block walk cannot stand in for.
        if !re.assertion_free() || re.can_match_newline() || re.is_unicode() {
            return None;
        }
        Some(re)
    }

    /// The end of the first match at or after `from`, from the forward automaton alone.
    pub fn block_accept_end(&self, hay: &[u8], from: usize) -> Option<usize> {
        self.block_re()?.accept_end(hay, from)
    }

    /// The next match at or after `from`, for the block scanner.
    pub fn block_match(&self, hay: &[u8], from: usize) -> Option<Match> {
        self.find_at(hay, from)
    }

    /// True for `-w`: a match must sit on word boundaries.
    pub fn is_word(&self) -> bool {
        self.word
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
            PatternKind::Pcre => {
                let pcre = self.pcre.as_ref()?;
                let mut at = from;
                while at <= hay.len() {
                    let m = pcre.find_at(hay, at)?;
                    if !self.word || bytes::at_word_boundary(hay, m.start, m.end) {
                        return Some(Match {
                            start: m.start,
                            end: m.end,
                        });
                    }
                    at = m.start + 1;
                }
                None
            }
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
        let finder = self.finder.as_ref()?;
        if !self.word {
            let i = finder.find_from(hay, from)?;
            return Some(Match {
                start: i,
                end: i + self.lit.len(),
            });
        }
        let mut at = from;
        while at <= hay.len() {
            let i = finder.find_from(hay, at)?;
            let m = Match {
                start: i,
                end: i + self.lit.len(),
            };
            if bytes::at_word_boundary(hay, m.start, m.end) {
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
    /// How many capturing groups the pattern has, which bounds what `-r` may refer to.
    pub fn group_count(&self) -> usize {
        match self.kind {
            PatternKind::Regex => self.re.as_ref().map_or(0, |re| re.group_count()),
            PatternKind::Literal | PatternKind::Pcre => 0,
        }
    }

    /// The capture spans of the match at or after `from`: index 0 is the whole match. The
    /// backtracking engine does not record groups, so it answers with the whole match only;
    /// `dusage search` refuses a `-r` group reference in that case rather than guessing.
    pub fn captures_at(&self, hay: &[u8], from: usize) -> Option<Vec<Option<(usize, usize)>>> {
        match self.kind {
            PatternKind::Regex => self.re.as_ref()?.captures_at(hay, from),
            PatternKind::Literal | PatternKind::Pcre => {
                let m = self.find_at(hay, from)?;
                Some(vec![Some((m.start, m.end))])
            }
        }
    }

    /// True when the pattern runs on the backtracking engine.
    pub fn is_pcre(&self) -> bool {
        self.kind == PatternKind::Pcre
    }

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

    /// `-i` folds every atom, so the pattern has no run to jump between and the scan steps the
    /// bytes: a repeated part must still match however many times it repeats.
    #[test]
    fn a_folded_repetition_matches_every_length() {
        let ci = Pattern::new("ne+dle", false, true, false).unwrap();
        assert_eq!(
            ci.find_at(b"NEEEEEDLE", 0),
            Some(Match { start: 0, end: 9 })
        );
        assert_eq!(
            ci.find_at(b"a Neeeedle b", 0),
            Some(Match { start: 2, end: 10 })
        );
        assert!(ci.is_match(b"neeeeedle"));
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
        assert_eq!(
            ci.kind(),
            PatternKind::Literal,
            "a folded literal still scans bytes, not the regex VM"
        );
        assert!(
            ci.finder.is_some(),
            "the plan is built with the pattern, not per line"
        );
        assert_eq!(
            Pattern::new("nEeDlE", false, true, false)
                .unwrap()
                .find_at(b"a NeEdLe b", 0),
            Some(Match { start: 2, end: 8 })
        );
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

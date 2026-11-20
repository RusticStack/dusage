//! A small, linear-time regular-expression engine (std only).
//!
//! Enough of the syntax to power a grep/rg replacement, compiled to a Thompson NFA and
//! run two ways: a lazily built DFA that walks bytes at one table lookup each, and the
//! Pike VM it is built from, which answers everything the DFA cannot. No backtracking, so
//! `(a+)+$` cannot blow up either way.
//!
//! # Syntax
//!
//! ```text
//! literal bytes        any UTF-8 text; a character is matched as its UTF-8 bytes
//! .                    any byte except \n      (?s) makes it any byte
//! *  +  ?  {n}  {n,}  {n,m}                     greedy, or lazy with a trailing ?
//! |                    alternation, leftmost-first (Perl) preference
//! ( ... )  (?: ... )   capturing and non-capturing groups (captures are not reported)
//! [abc]  [^abc]  [a-z]  [\d_]  [\]]            character classes, ASCII or not
//! [\p{L}a]  [^\p{Nd}]                          Unicode classes, negated or not
//! ^  $  \A  \z  \b  \B  anchors and word boundaries
//! \d \D \w \W \s \S    digit / word / space classes and their negations
//! \p{L} \p{Lu} \p{Ll} \p{N} \p{Nd} \p{P} \p{Z} \p{S} \p{C}
//!                      Unicode general categories (see `crate::unicode`)
//! [[:alpha:]] [^[:space:]]  POSIX bracket expressions inside a class
//! \n \r \t \f \v \0 \xHH   escapes; \. matches a literal dot
//! (?i) (?s) (?m) (?-i) (?ism)   inline flags, at the pattern start or a group start
//! ```
//!
//! # Semantics
//!
//! Two execution paths share one VM. The byte path runs the program over the haystack
//! bytes, so `.` matches one byte of a multi-byte character and `\d`, `\w`, `\s` and
//! `\b` are ASCII. The character path runs the same VM over the characters of the line,
//! so `.` matches one code point, `\d` is `\p{Nd}`, `\w` and `\b` are Unicode, and
//! `[[:alpha:]]` matches `é`. A pattern needs the character path when it names something
//! only Unicode can answer: a character outside ASCII, a `\p{..}` class, a POSIX class,
//! a `\d`-style shorthand, `\b`, or `-i` folding over a non-ASCII character.
//!
//! Which path runs is decided per line, not per pattern. An ASCII line is answered by the
//! byte program, which is exact for it and costs what it always did; only a line holding
//! a non-ASCII byte is decoded and given to the character program. That keeps the engine's
//! speed on ASCII text while `\w+` matches `café` and `\d` matches `٣`. A pattern that is
//! a plain literal never reaches either path: it is searched for as bytes, as it always
//! was.
//!
//! `^` and `$` anchor at the ends of the haystack, or at line boundaries with
//! `(?m)`/`multi_line`. `$` matches only at the end of the haystack (or before a `\n`
//! when multi-line), never before a trailing newline. `-i` folds with the simple case
//! folding of `crate::unicode`, so `Σ`, `σ` and `ς` are one character to it. Alternation
//! and repetition are leftmost-first, not POSIX longest. Lookaround, backreferences and
//! property names outside the categories `crate::unicode` holds are rejected with an error
//! naming the offset.
//!
//! A line that is not valid UTF-8 has no characters to be right about, so it is matched
//! byte-wise, exactly as before: the byte meanings apply to binary and Latin-1 content,
//! which is also what keeps a binary file from turning into millions of replacement
//! characters.

use crate::unicode::{self, CharClass};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Compile-time flags, also settable inline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// Fold ASCII letters.
    pub case_insensitive: bool,
    /// `^` and `$` match at line boundaries.
    pub multi_line: bool,
    /// `.` matches any byte, including `\n`.
    pub dot_matches_newline: bool,
}

/// A match: byte offsets into the haystack, `start..end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub start: usize,
    pub end: usize,
}

impl Match {
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// Longest repetition bound accepted, so a pattern cannot explode the program.
const MAX_REPEAT: u32 = 1000;

/// Hands out the pattern ids a thread's DFA cache is keyed on.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    /// How many searches this thread settled from line windows, so a test can prove the path
    /// it compares against a full decode is the one that ran. Thread-local because the tests
    /// run in parallel, and a shared count would be another test's.
    static LINE_WINDOWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_line_window() {
    LINE_WINDOWS.with(|n| n.set(n.get() + 1));
}

#[cfg(test)]
fn line_windows() -> u64 {
    LINE_WINDOWS.with(|n| n.get())
}

/// 256-bit set of bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ByteSet([u64; 4]);

impl ByteSet {
    fn empty() -> Self {
        ByteSet([0; 4])
    }

    fn full() -> Self {
        ByteSet([u64::MAX; 4])
    }

    fn add(&mut self, b: u8) {
        self.0[(b >> 6) as usize] |= 1 << (b & 63);
    }

    fn add_range(&mut self, a: u8, b: u8) {
        for x in a..=b {
            self.add(x);
        }
    }

    fn contains(&self, b: u8) -> bool {
        self.0[(b >> 6) as usize] & (1 << (b & 63)) != 0
    }

    fn negate(&mut self) {
        for w in &mut self.0 {
            *w = !*w;
        }
    }

    /// Fold ASCII case: for each letter in the set, add its counterpart.
    fn fold_ascii(&mut self) {
        for c in b'a'..=b'z' {
            if self.contains(c) {
                self.add(c.to_ascii_uppercase());
            }
            if self.contains(c.to_ascii_uppercase()) {
                self.add(c);
            }
        }
    }

    /// The single byte in the set, if it holds exactly one.
    fn sole(&self) -> Option<u8> {
        let mut found = None;
        for b in 0..=255u8 {
            if self.contains(b) {
                if found.is_some() {
                    return None;
                }
                found = Some(b);
            }
        }
        found
    }

    fn is_empty(&self) -> bool {
        self.0 == [0; 4]
    }
}

impl std::fmt::Debug for ByteSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        for b in 0..=255u8 {
            if self.contains(b) {
                parts.push(escape_byte(b));
            }
        }
        write!(f, "[{}]", parts.concat())
    }
}

fn escape_byte(b: u8) -> String {
    match b {
        b'\n' => "\\n".to_string(),
        b'\r' => "\\r".to_string(),
        b'\t' => "\\t".to_string(),
        0x20..=0x7e => (b as char).to_string(),
        _ => format!("\\x{b:02x}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Assert {
    StartText,
    EndText,
    StartLine,
    EndLine,
    WordBoundary,
    NotWordBoundary,
}

/// One instruction of a program. `S` is the symbol set it consumes: a `ByteSet` on the
/// byte path, a `CharClass` on the character path.
#[derive(Debug)]
enum Inst<S> {
    /// Consume one symbol from a set.
    Set(S),
    /// Continue at one of two program counters, preferring the first.
    Split(usize, usize),
    Jmp(usize),
    Assert(Assert),
    Match,
    /// Record the current position in a capture slot: pairs of slots bracket a group, and
    /// only the capture program carries these.
    Save(u32),
}

/// A set of symbols an instruction consumes.
trait Symbols {
    type Sym: Copy;
    fn permits(&self, s: Self::Sym) -> bool;
}

impl Symbols for ByteSet {
    type Sym = u8;

    fn permits(&self, b: u8) -> bool {
        self.contains(b)
    }
}

impl Symbols for CharClass {
    type Sym = char;

    fn permits(&self, c: char) -> bool {
        self.contains(c)
    }
}

/// The haystack the VM walks: bytes, or the characters decoded from them.
trait Hay {
    type Sym: Copy;
    fn sym(&self, at: usize) -> Option<Self::Sym>;
    fn assert_holds(&self, a: Assert, at: usize) -> bool;
}

impl Hay for [u8] {
    type Sym = u8;

    fn sym(&self, at: usize) -> Option<u8> {
        self.get(at).copied()
    }

    fn assert_holds(&self, a: Assert, at: usize) -> bool {
        let is_word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
        let before = if at == 0 { None } else { self.get(at - 1) };
        let after = self.get(at);
        match a {
            Assert::StartText => at == 0,
            Assert::EndText => at == self.len(),
            Assert::StartLine => at == 0 || self.get(at - 1) == Some(&b'\n'),
            Assert::EndLine => at == self.len() || self.get(at) == Some(&b'\n'),
            Assert::WordBoundary => is_word(before) != is_word(after),
            Assert::NotWordBoundary => is_word(before) == is_word(after),
        }
    }
}

/// The character program's assertions, answered from `chars` alone. `starts_text` and
/// `ends_text` say whether `chars` begins and ends the haystack: `\A` and `\z` are the only
/// assertions whose answer needs characters the slice does not hold.
fn char_assert(chars: &[char], starts_text: bool, ends_text: bool, a: Assert, at: usize) -> bool {
    let is_word = |c: Option<char>| c.is_some_and(unicode::is_word);
    // Past either end of the slice sits the newline that ended the line, or nothing at all:
    // both are non-word and both end a line, so nothing below has to tell them apart.
    let before = if at == 0 {
        None
    } else {
        chars.get(at - 1).copied()
    };
    let after = chars.get(at).copied();
    match a {
        Assert::StartText => starts_text && at == 0,
        Assert::EndText => ends_text && at == chars.len(),
        Assert::StartLine => at == 0 || before == Some('\n'),
        Assert::EndLine => at == chars.len() || after == Some('\n'),
        Assert::WordBoundary => is_word(before) != is_word(after),
        Assert::NotWordBoundary => is_word(before) == is_word(after),
    }
}

impl Hay for [char] {
    type Sym = char;

    fn sym(&self, at: usize) -> Option<char> {
        self.get(at).copied()
    }

    fn assert_holds(&self, a: Assert, at: usize) -> bool {
        char_assert(self, true, true, a, at)
    }
}

/// One line of the text read as characters, so a match that cannot cross a newline can be
/// resolved without decoding any other line.
struct LineHay<'a> {
    chars: &'a [char],
    starts_text: bool,
    ends_text: bool,
}

impl Hay for LineHay<'_> {
    type Sym = char;

    fn sym(&self, at: usize) -> Option<char> {
        self.chars.get(at).copied()
    }

    fn assert_holds(&self, a: Assert, at: usize) -> bool {
        char_assert(self.chars, self.starts_text, self.ends_text, a, at)
    }
}

/// AST node produced by the parser.
enum Node {
    Empty,
    /// One ASCII byte of the pattern.
    Byte(u8),
    /// One byte class, matched byte by byte.
    Set(ByteSet),
    /// One character outside ASCII, matched as one code point.
    Char(char),
    /// A code-point class: `\p{..}`, `\d\w\s` in their Unicode meaning, a class with a
    /// non-ASCII member, a negated class when the pattern needs Unicode semantics.
    CharSet(CharClass),
    /// `.`: one byte, or one code point, depending on the path.
    Dot(bool),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>, bool),
    Assert(Assert),
    /// A capturing group, by index: a `Save` pair when the program is built for captures,
    /// and transparent otherwise.
    Group(u32, Box<Node>),
}

/// A haystack read as text: the character program to run over it, its characters, and the
/// byte offset each character starts at, with a sentinel holding the text's length.
#[derive(Clone, Copy)]
struct Text<'a> {
    prog: &'a [Inst<CharClass>],
    chars: &'a [char],
    bytes: &'a [usize],
}

/// A compiled pattern.
#[derive(Debug)]
pub struct Regex {
    /// The byte program, always present: it runs on ASCII haystacks, where it is exact
    /// for a pattern that needs Unicode semantics elsewhere.
    prog: Vec<Inst<ByteSet>>,
    /// The character program, present when the pattern needs Unicode semantics.
    cprog: Option<Vec<Inst<CharClass>>>,
    flags: Flags,
    /// How many capturing groups the pattern has.
    groups: usize,
    /// The byte and character programs with `Save` instructions, built only when the pattern
    /// has groups: they exist for `-r`, and nothing on the search path touches them.
    cap_bytes: Option<Vec<Inst<ByteSet>>>,
    cap_chars: Option<Vec<Inst<CharClass>>>,
    /// The whole pattern, when it is a case-sensitive literal.
    lit: Option<Vec<u8>>,
    /// A byte every match must contain, for prefiltering.
    required: Option<u8>,
    /// A substring every match must contain, with its skip plan built once so the per-line
    /// check costs a scan and nothing else.
    run: Option<(Vec<u8>, crate::bytes::Finder)>,
    /// The runs a block scan jumps between, when a byte walk can rely on them: the pattern
    /// holds no assertion, cannot match a newline, and cannot match the empty string.
    cand: Option<Candidates>,
    /// The run every match begins with, when the pattern has one and cannot match empty. An
    /// automaton in its start state may jump to the next occurrence of it, since a match
    /// cannot begin anywhere else, and a four-byte run is far rarer than the byte the
    /// automaton would otherwise leave the start state on.
    start_run: Option<crate::simd::Prefilter>,
    /// No `^`, `$`, `\b` or `\B` anywhere in the pattern.
    assertion_free: bool,
    /// The assertion context bits the byte program reads, which is what decides how many
    /// assertion contexts a DFA state needs.
    ctx_mask: u8,
    /// Distinguishes this pattern from every other one in a thread's DFA cache.
    id: u64,
}

impl Regex {
    pub fn new(pattern: &str) -> Result<Regex, String> {
        Regex::new_with(pattern, Flags::default())
    }

    pub fn new_with(pattern: &str, flags: Flags) -> Result<Regex, String> {
        let parsed = Parser::parse(pattern, flags, false)?;
        // The search programs and every analysis see a group-free tree; the capture program
        // is built from `parsed.node`, which still has them.
        let node = strip_groups(&parsed.node);
        let mut prog = Vec::new();
        compile(&node, &mut prog, byte_symbols, false);
        prog.push(Inst::Match);
        // An inline `(?i)` anywhere in the pattern rules out the literal fast path, and so
        // does a capturing group: a whole-pattern literal has no groups to report, so `-r`
        // could not refer to one.
        let lit = match parsed.groups {
            0 => literal_of(&node, parsed.folded),
            _ => None,
        };
        let required = required_byte(&node);
        // A literal's rarest byte is the best prefilter the parser can give us cheaply;
        // for everything else use the single mandatory byte of the leftmost atom.
        let required = match &lit {
            Some(bytes) => rarest_byte(bytes).or(required),
            None => required,
        };
        // A pattern that needs Unicode semantics is parsed again in character mode:
        // `\d` is `\p{Nd}`, `[^a-z]` excludes every non-ASCII character, `.` is one code
        // point. The byte program above keeps the byte meanings, which are what an ASCII
        // haystack can tell apart. A pure literal never reaches the VM at all.
        let cprog = match parsed.unicode && lit.is_none() {
            true => {
                let chars = Parser::parse(pattern, flags, true)?.node;
                let mut cprog = Vec::new();
                compile(&chars, &mut cprog, char_symbols, false);
                cprog.push(Inst::Match);
                Some(cprog)
            }
            false => None,
        };
        // The capture programs keep the group boundaries, which every other program drops.
        let (cap_bytes, cap_chars) = if parsed.groups > 0 {
            let mut cap = Vec::new();
            compile(&parsed.node, &mut cap, byte_symbols, true);
            cap.push(Inst::Match);
            let chars = match parsed.unicode {
                true => {
                    let tree = Parser::parse(pattern, flags, true)?.node;
                    let mut c = Vec::new();
                    compile(&tree, &mut c, char_symbols, true);
                    c.push(Inst::Match);
                    Some(c)
                }
                false => None,
            };
            (Some(cap), chars)
        } else {
            (None, None)
        };
        // A folded pattern's bytes in the tree are pre-folded and no longer literal.
        let run = match parsed.folded {
            true => None,
            false => required_run(&node).filter(|run| run.len() >= 2).map(|run| {
                let finder = crate::bytes::Finder::new(&run, false);
                (run, finder)
            }),
        };
        // A candidate scan walks a slice of the haystack and works from newline bounds, so it
        // only stands in for the plain byte walk when the pattern reads no assertion, cannot
        // match across a line, and every match holds one of the runs.
        let cand = match parsed.folded {
            true => None,
            false => match candidates_are_sound(&node, &prog) {
                true => candidate_runs(&node).and_then(Candidates::new),
                false => None,
            },
        };
        let start_run = match parsed.folded || closure(&prog, &[], 0).1 {
            true => None,
            false => anchor_run(&node)
                .filter(|run| run.len() >= MIN_RUN)
                .and_then(|run| crate::simd::Prefilter::new(vec![run])),
        };
        let ctx_mask = ctx_mask(&prog);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        Ok(Regex {
            prog,
            cprog,
            groups: parsed.groups,
            cap_bytes,
            cap_chars,
            flags,
            lit,
            required,
            run,
            cand,
            start_run,
            ctx_mask,
            id,
            assertion_free: !has_assertion(&node),
        })
    }

    pub fn flags(&self) -> Flags {
        self.flags
    }

    /// How many capturing groups the pattern has.
    pub fn group_count(&self) -> usize {
        self.groups
    }

    /// The capture spans of the leftmost-first match at or after `start`: index 0 is the
    /// whole match, and a group that did not take part in it is `None`. Built for `-r`, so
    /// it runs the capture program and nothing else.
    pub fn captures_at(&self, hay: &[u8], start: usize) -> Option<Vec<Option<(usize, usize)>>> {
        if start > hay.len() {
            return None;
        }
        let nslots = 2 * (self.groups + 1);
        // Same program choice as `find_at`: text that is not ASCII goes through the character
        // program, so a group around `é` is bounded by the character and not by one byte.
        if let (Some(cap), Some(_)) = (&self.cap_chars, &self.cprog) {
            if !hay.is_ascii() {
                if let Ok(text) = std::str::from_utf8(hay) {
                    let (chars, bytes) = decode(text);
                    let ch_start = bytes.partition_point(|&at| at < start);
                    // The character program owns this haystack, exactly as it does in
                    // `find_at`: its no-match is the answer, not a reason to ask the byte
                    // program, whose idea of `\w` and `\b` is a different one.
                    let (_, slots) = run_capture(cap, &chars[..], ch_start, nslots)?;
                    return Some(spans_from_slots(&slots, |i| bytes[i]));
                }
            }
        }
        let Some(cap) = self.cap_bytes.as_ref() else {
            let m = self.find_at(hay, start)?;
            return Some(vec![Some((m.start, m.end))]);
        };
        let (_, slots) = run_capture(cap, hay, start, nslots)?;
        Some(spans_from_slots(&slots, |i| i))
    }

    /// True when some instruction in the program permits a newline, which means a match could
    /// straddle a line and a block scan could not attribute it to one line.
    pub fn can_match_newline(&self) -> bool {
        self.prog.iter().any(|inst| match inst {
            Inst::Set(set) => set.permits(b'\n'),
            _ => false,
        })
    }

    /// The position just past the first match ending at or after `from`, worked out by the
    /// forward automaton alone. A block scan uses it to find the lines worth looking at, and
    /// leaves the exact span to a per-line search.
    pub fn accept_end(&self, hay: &[u8], from: usize) -> Option<usize> {
        if from > hay.len() {
            return None;
        }
        if let Some(lit) = &self.lit {
            let i = crate::bytes::memmem_from(hay, lit, from)?;
            return Some(i + lit.len());
        }
        // With runs to jump between, the automaton only walks where a match could be; the
        // bytes between two candidates are skipped at the speed of a vector compare.
        if let Some(cand) = &self.cand {
            if let Some(found) = self.candidate_end(cand, hay, from) {
                return found;
            }
        }
        if let Some((_, finder)) = &self.run {
            finder.find_from(hay, from)?;
        }
        match with_dfa(self, |c| {
            c.forward_end(&self.prog, hay, from, self.anchor())
        }) {
            Some(Some(Some(end))) => Some(end),
            // The automaton proved there is no match; that is an answer, not a fallback.
            Some(Some(None)) => None,
            _ => self.find_at(hay, from).map(|m| m.end),
        }
    }

    /// The end of a match found by jumping between candidate runs. `None` is the automaton
    /// giving up (the state cap), which leaves the answer to the slow path; `Some(None)` is a
    /// proof that no match exists.
    fn candidate_end(&self, cand: &Candidates, hay: &[u8], from: usize) -> Option<Option<usize>> {
        let mut at = from;
        while let Some((p, i)) = cand.scan.find_any(hay, at) {
            let (run, place) = &cand.runs[i];
            let end = p + run.len();
            match place {
                Place::Whole => return Some(Some(end)),
                // The match starts where the run does: an anchored walk from there is exactly
                // the match, and stops at its end.
                Place::Start => match with_dfa(self, |c| {
                    c.forward_end(&self.prog, &hay[p..], 0, self.anchor())
                }) {
                    Some(Some(Some(end))) => return Some(Some(p + end)),
                    Some(Some(None)) => at = p + 1,
                    _ => return None,
                },
                // Nothing is known about where the run sits, so the line holding it is the
                // least the automaton can be asked to walk. A line with no accept in it holds
                // no match, because no match may cross the newline that ends it. The walk still
                // begins at `from`: a match starting before the caller's offset is not its to
                // report, exactly as in the plain byte walk.
                Place::Any => {
                    let (ls, le) = (line_start(hay, p).max(from), line_end(hay, p));
                    match with_dfa(self, |c| {
                        c.forward_end(&self.prog, &hay[ls..le], 0, self.anchor())
                    }) {
                        Some(Some(Some(end))) => return Some(Some(ls + end)),
                        Some(Some(None)) => at = le + 1,
                        _ => return None,
                    }
                }
            }
        }
        Some(None)
    }

    /// The leftmost-first match found by jumping between candidate runs, the span form of
    /// [`Self::candidate_end`]. Every match holds one of the runs, so the first match can only
    /// lie on the line that holds the first candidate at or after `from`: the automaton walks
    /// that line and never the bytes between two candidates. `None` is the automaton giving up
    /// (the state cap), which leaves the answer to the slow path; `Some(None)` is a proof that
    /// no match exists.
    fn candidate_find(&self, cand: &Candidates, hay: &[u8], from: usize) -> Option<Option<Match>> {
        let mut at = from;
        // A walk that found nothing in `[p, line_end)` also rules a match out for every
        // candidate after `p` in that line, since a walk from one of them would cover a span
        // already walked: `clear_to` is how far the last such proof reaches.
        let mut clear_to = 0usize;
        while let Some((p, i)) = cand.scan.find_any(hay, at) {
            let (run, place) = &cand.runs[i];
            match place {
                // The run is the whole match, so the earliest one starts the earliest match.
                Place::Whole => {
                    return Some(Some(Match {
                        start: p,
                        end: p + run.len(),
                    }))
                }
                // Every match begins with the run, so a match begins here or nowhere, and the
                // line is as far as it can run: the walk over the line resolves the span, and
                // the start state never re-enters before the match, so that span is the
                // leftmost-first one.
                Place::Start => {
                    let le = line_end(hay, p);
                    if p >= clear_to {
                        match with_dfa(self, |c| c.find(&self.prog, &hay[p..le], 0, self.anchor()))
                        {
                            Some(Some(Some(m))) => {
                                return Some(Some(Match {
                                    start: p + m.start,
                                    end: p + m.end,
                                }))
                            }
                            Some(Some(None)) => clear_to = le,
                            _ => return None,
                        }
                    }
                    at = p + 1;
                }
                // Nothing is known about where the run sits, so the line holding it is the
                // least the automaton can be asked to walk: a match cannot cross the newline
                // that ends that line, so a line with no match in it holds none, and the walk
                // over the line ends on the leftmost-first match for the VM to resolve. The
                // walk runs on the line rather than on the whole haystack so that the span is
                // resolved from where the automaton last ran out of threads, not from the
                // start of the haystack.
                Place::Any => {
                    let ls = line_start(hay, p).max(from);
                    let le = line_end(hay, p);
                    match with_dfa(self, |c| c.find(&self.prog, &hay[ls..le], 0, self.anchor())) {
                        Some(Some(Some(m))) => {
                            return Some(Some(Match {
                                start: ls + m.start,
                                end: ls + m.end,
                            }))
                        }
                        Some(Some(None)) => at = le + 1,
                        _ => return None,
                    }
                }
            }
        }
        Some(None)
    }

    /// True when the pattern uses no line-relative assertion, so a whole block can be
    /// matched at once: `^` and `$` would otherwise mean "start and end of the block".
    pub fn assertion_free(&self) -> bool {
        self.assertion_free
    }

    /// True when the pattern needs Unicode semantics and so runs a character program on
    /// haystacks that are not pure ASCII.
    pub fn is_unicode(&self) -> bool {
        self.cprog.is_some()
    }

    /// A substring every match must contain, or `None` when the pattern gives no hint.
    pub fn required_literal(&self) -> Option<&[u8]> {
        self.run.as_ref().map(|(bytes, _)| bytes.as_slice())
    }

    /// A byte every match must contain, or `None` when the pattern gives no hint.
    pub fn required_byte(&self) -> Option<u8> {
        self.required
    }

    /// The pattern as plain bytes when it is a case-sensitive literal.
    pub fn literal(&self) -> Option<&[u8]> {
        self.lit.as_deref()
    }

    /// Leftmost-first match anywhere in `hay`.
    pub fn find(&self, hay: &[u8]) -> Option<Match> {
        self.find_at(hay, 0)
    }

    /// Leftmost-first match starting at or after `start`.
    pub fn find_at(&self, hay: &[u8], start: usize) -> Option<Match> {
        if start > hay.len() {
            return None;
        }
        // An ASCII line and a line that is not UTF-8 are both answered by the byte
        // program: the first is exact for it, and the second has no characters to
        // speak of, so it keeps the byte semantics it had before this engine learned
        // any. Only text is decoded.
        if let Some(cprog) = &self.cprog {
            if !hay.is_ascii() {
                if let Ok(text) = std::str::from_utf8(hay) {
                    return self.run_chars(cprog, text, start);
                }
            }
        }
        self.find_bytes_at(hay, start)
    }

    /// The byte program's answer at `start`, which is exact for an ASCII haystack and the
    /// only answer a haystack that is not UTF-8 has.
    fn find_bytes_at(&self, hay: &[u8], start: usize) -> Option<Match> {
        if start > hay.len() {
            return None;
        }
        if let Some(lit) = &self.lit {
            let i = crate::bytes::memmem_from(hay, lit, start)?;
            return Some(Match {
                start: i,
                end: i + lit.len(),
            });
        }
        // With runs to jump between, the answer comes from walking the lines they sit on
        // rather than every byte between them.
        if let Some(cand) = &self.cand {
            if let Some(found) = self.candidate_find(cand, hay, start) {
                return found;
            }
        }
        // A haystack without a run of bytes the pattern cannot match without cannot match it
        // at all, and proving that is a scan rather than an automaton walk.
        if let Some((_, finder)) = &self.run {
            finder.find_from(hay, start)?;
        }
        match with_dfa(self, |c| c.find(&self.prog, hay, start, self.anchor())) {
            // The DFA answered; its `None` is a real answer, not a fallback.
            Some(Some(found)) => found,
            _ => self.run(&self.prog, hay, start),
        }
    }

    /// Every non-overlapping match in one haystack, for a search that sees a whole file at
    /// once (`-U`). `accept` is the caller's rule on top of the pattern — the word
    /// boundaries `-w` asks for — and a rejected match retries one symbol later, the way
    /// `find_at` is asked again.
    ///
    /// `find_at` decides per call whether the haystack is text, which for a file is a scan
    /// of it per match, and when it is text a decode of it per match. Whether a haystack is
    /// text is a property of the haystack, so here it is decided once and decoded once.
    pub fn find_all_in(
        &self,
        hay: &[u8],
        limit: usize,
        accept: impl Fn(&Match) -> bool,
    ) -> Vec<Match> {
        // Only a haystack that is text this engine decodes can need the character program.
        // A file with no non-ASCII byte is answered by the byte program, which is exact for
        // it; a file that is not UTF-8 has no characters to decode.
        let Some(cprog) = self.cprog.as_ref().filter(|_| !hay.is_ascii()) else {
            return self.find_all_parts(hay, None, limit, &accept);
        };
        // A mandatory literal the file does not hold rules out a match under either
        // semantics, so such a file never needs decoding: UTF-8 spells no ASCII byte inside a
        // multi-byte character, and spells a non-ASCII one only one way. Proving that is a
        // scan, and a scan is what the decode would cost and more.
        let holds_mandatory = match &self.run {
            Some((_, finder)) => finder.find_from(hay, 0).is_some(),
            None => match self.required {
                Some(b) => crate::bytes::find_byte(hay, b, 0).is_some(),
                None => true,
            },
        };
        if !holds_mandatory {
            return Vec::new();
        }
        match std::str::from_utf8(hay) {
            Ok(text) => {
                // A text file whose pattern has a mandatory ASCII literal and cannot consume
                // a newline: every match lies on the line that holds one of the literal's
                // occurrences, so those lines are the only ones worth decoding.
                if let Some((run, finder)) = &self.run {
                    if run.is_ascii() && !cprog_can_match_newline(cprog) {
                        return self.find_all_lines(cprog, finder, hay, limit, &accept);
                    }
                }
                let (chars, bytes) = decode(text);
                let text = Text {
                    prog: cprog,
                    chars: &chars,
                    bytes: &bytes,
                };
                self.find_all_parts(hay, Some(text), limit, &accept)
            }
            Err(_) => self.find_all_parts(hay, None, limit, &accept),
        }
    }

    /// Every match in a text haystack, decoding one line at a time instead of the haystack.
    ///
    /// Every match holds the mandatory ASCII literal, and the caller has established that no
    /// match can consume a newline, so a match lies on the line that holds one of the
    /// literal's occurrences: that line is the window, and the line bounds are the ones
    /// `candidate_find` already works from on the byte path. The character program walks the
    /// line from where the caller's offset falls in it and never the lines between two
    /// candidates, so a file is answered without the characters of every line it holds.
    ///
    /// The windows are lines and not byte ranges for a reason: a newline is one ASCII byte, so
    /// no multi-byte character can straddle the bound of a window, and no match — which the
    /// caller has established cannot cross a newline — can be cut by one.
    fn find_all_lines(
        &self,
        cprog: &[Inst<CharClass>],
        finder: &crate::bytes::Finder,
        hay: &[u8],
        limit: usize,
        accept: &impl Fn(&Match) -> bool,
    ) -> Vec<Match> {
        #[cfg(test)]
        note_line_window();
        let mut out = Vec::new();
        // The byte offset the next match may start at, and where the literal scan resumes.
        let mut from = 0;
        let mut scan = 0;
        // The line the buffers hold, so two candidates on one line decode it once.
        let mut held = (usize::MAX, usize::MAX);
        let mut chars = Vec::new();
        let mut bytes = Vec::new();
        while out.len() < limit {
            let Some(q) = finder.find_from(hay, scan) else {
                break;
            };
            let (ls, le) = (line_start(hay, q), line_end(hay, q));
            if held != (ls, le) {
                let line = std::str::from_utf8(&hay[ls..le]).expect("a line of text is text");
                decode_into(line, &mut chars, &mut bytes);
                held = (ls, le);
            }
            let start = bytes.partition_point(|&at| ls + at < from);
            let window = LineHay {
                chars: &chars,
                starts_text: ls == 0,
                ends_text: le == hay.len(),
            };
            let Some(m) = self.run(cprog, &window, start) else {
                // No match can begin in this line at or after `from`, so the next one, if
                // there is one, holds a candidate on a later line.
                scan = le + 1;
                continue;
            };
            let span = Match {
                start: ls + bytes[m.start],
                end: ls + bytes[m.end],
            };
            let accepted = accept(&span);
            if accepted {
                out.push(span);
            }
            // A character offset one past the end of the line is the newline, which no match
            // can begin with, so `bytes`' sentinel stands for it.
            from = match accepted && m.end > m.start {
                true => ls + bytes[m.end],
                false => ls + bytes[m.start + 1],
            };
            scan = from;
            if from > hay.len() {
                break;
            }
        }
        out
    }

    /// The same search with the ASCII shortcut taken away: text is decoded and its character
    /// program run even when every byte of it is ASCII. The equivalence the ASCII shortcut
    /// rests on is that this answers exactly what the byte program answers.
    #[cfg(test)]
    fn find_all_decoded(
        &self,
        hay: &[u8],
        limit: usize,
        accept: impl Fn(&Match) -> bool,
    ) -> Vec<Match> {
        let (chars, bytes) = match std::str::from_utf8(hay) {
            Ok(text) => decode(text),
            Err(_) => return self.find_all_parts(hay, None, limit, &accept),
        };
        let text = self.cprog.as_ref().map(|prog| Text {
            prog,
            chars: &chars,
            bytes: &bytes,
        });
        self.find_all_parts(hay, text, limit, &accept)
    }

    /// The match loop shared by both paths. `text` is the same haystack read as characters,
    /// when it is text; without it the byte offsets are the haystack's own.
    fn find_all_parts(
        &self,
        hay: &[u8],
        text: Option<Text<'_>>,
        limit: usize,
        accept: &impl Fn(&Match) -> bool,
    ) -> Vec<Match> {
        let mut out = Vec::new();
        let mut at = 0;
        while out.len() < limit {
            // `at`, `m.start` and `m.end` are character offsets when the haystack was
            // decoded and byte offsets otherwise; `len` is the haystack in the same unit.
            let (m, len) = match text {
                Some(Text { prog, chars, .. }) => match self.run(prog, chars, at) {
                    Some(m) => (m, chars.len()),
                    None => break,
                },
                None => match self.find_bytes_at(hay, at) {
                    Some(m) => (m, hay.len()),
                    None => break,
                },
            };
            let span = match text {
                Some(Text { bytes, .. }) => Match {
                    start: bytes[m.start],
                    end: bytes[m.end],
                },
                None => m,
            };
            if accept(&span) {
                out.push(span);
                at = if m.end > m.start { m.end } else { m.start + 1 };
            } else {
                at = m.start + 1;
            }
            if at > len {
                break;
            }
        }
        out
    }

    /// The run every match begins with, for an automaton to jump between.
    fn anchor(&self) -> Option<&crate::simd::Prefilter> {
        self.start_run.as_ref()
    }

    /// True when the pattern matches anywhere in `hay`.
    pub fn is_match(&self, hay: &[u8]) -> bool {
        if let Some(lit) = &self.lit {
            return crate::bytes::memmem(hay, lit).is_some();
        }
        if let Some((_, finder)) = &self.run {
            if finder.find_from(hay, 0).is_none() {
                return false;
            }
        }
        if self.cprog.is_none() || hay.is_ascii() {
            if let Some(Some(found)) =
                with_dfa(self, |c| c.is_match(&self.prog, hay, 0, self.anchor()))
            {
                return found;
            }
        }
        self.find_at(hay, 0).is_some()
    }

    /// Pike VM: leftmost-first semantics, linear time in the haystack length. Returns
    /// symbol offsets, which are byte offsets on the byte path and character offsets on
    /// the character path.
    fn run<H, S>(&self, prog: &[Inst<S>], hay: &H, start: usize) -> Option<Match>
    where
        H: Hay + ?Sized,
        S: Symbols<Sym = H::Sym>,
    {
        run_vm(prog, hay, start, false)
    }

    /// The character path: decode the line once, run the character program over it, and
    /// report the match as byte offsets.
    fn run_chars(&self, cprog: &[Inst<CharClass>], text: &str, from: usize) -> Option<Match> {
        let (chars, bytes) = decode(text);
        // `bytes[i]` is the offset of character `i`; the sentinel at the end makes
        // `partition_point` land on the first character at or after `from`.
        let start = bytes.partition_point(|&at| at < from);
        let m = self.run(cprog, &chars[..], start)?;
        Some(Match {
            start: bytes[m.start],
            end: bytes[m.end],
        })
    }
}

/// Decode `text` into characters and, beside them, the byte offset each one starts at
/// plus one sentinel holding `text.len()`. The sentinel is what turns a character offset
/// from the VM back into a byte offset into the line.
fn decode(text: &str) -> (Vec<char>, Vec<usize>) {
    let mut chars = Vec::new();
    let mut bytes = Vec::new();
    decode_into(text, &mut chars, &mut bytes);
    (chars, bytes)
}

/// [`decode`] into buffers a caller reuses, so a search decodes one line at a time without
/// allocating for every line it looks at.
fn decode_into(text: &str, chars: &mut Vec<char>, bytes: &mut Vec<usize>) {
    chars.clear();
    bytes.clear();
    chars.reserve(text.len());
    bytes.reserve(text.len() + 1);
    for (at, c) in text.char_indices() {
        bytes.push(at);
        chars.push(c);
    }
    bytes.push(text.len());
}

/// Turn a slot array into capture spans, mapping each position through `to_offset`.
fn spans_from_slots(
    slots: &[usize],
    to_offset: impl Fn(usize) -> usize,
) -> Vec<Option<(usize, usize)>> {
    slots
        .chunks(2)
        .map(|pair| match (pair.first(), pair.get(1)) {
            (Some(&a), Some(&b)) if a != usize::MAX && b != usize::MAX => {
                Some((to_offset(a), to_offset(b)))
            }
            _ => None,
        })
        .collect()
}

/// One thread of the capture VM: the search VM's `(pc, start)` pair plus the slot array that
/// the `Save` instructions have written so far.
struct CapThread {
    pc: usize,
    start: usize,
    slots: Vec<usize>,
}

struct CapPending {
    best: Option<(Match, Vec<usize>)>,
    cut: bool,
}

/// The Pike VM again, over a program that carries `Save` instructions. It exists for `-r`,
/// where a handful of matched lines need to know where each group began and ended, so it
/// buys correctness with a slot array per thread and leaves the search path untouched.
fn run_capture<H, S>(
    prog: &[Inst<S>],
    hay: &H,
    start: usize,
    nslots: usize,
) -> Option<(Match, Vec<usize>)>
where
    H: Hay + ?Sized,
    S: Symbols<Sym = H::Sym>,
{
    let mut clist: Vec<CapThread> = Vec::new();
    let mut nlist: Vec<CapThread> = Vec::new();
    let mut cl_marks = vec![false; prog.len()];
    let mut nl_marks = vec![false; prog.len()];
    let mut st = CapPending {
        best: None,
        cut: false,
    };
    let mut sp = start;
    loop {
        st.cut = false;
        if st.best.is_none() {
            let slots = vec![usize::MAX; nslots];
            add_cap_thread(
                prog,
                &mut clist,
                &mut cl_marks,
                0,
                sp,
                slots,
                hay,
                sp,
                &mut st,
            );
            // As in `run_vm_scratch`: the queued threads outrank the match this seed made.
            st.cut = false;
        }
        let Some(sym) = hay.sym(sp) else {
            break;
        };
        for t in clist.drain(..) {
            if let Inst::Set(set) = &prog[t.pc] {
                if set.permits(sym) {
                    add_cap_thread(
                        prog,
                        &mut nlist,
                        &mut nl_marks,
                        t.pc + 1,
                        t.start,
                        t.slots,
                        hay,
                        sp + 1,
                        &mut st,
                    );
                }
            }
        }
        std::mem::swap(&mut clist, &mut nlist);
        std::mem::swap(&mut cl_marks, &mut nl_marks);
        nl_marks.fill(false);
        nlist.clear();
        sp += 1;
        if st.best.is_some() && clist.is_empty() {
            break;
        }
    }
    st.best
}

/// Epsilon closure over a capture program, keeping the slots each path has written.
#[allow(clippy::too_many_arguments)]
fn add_cap_thread<H, S>(
    prog: &[Inst<S>],
    list: &mut Vec<CapThread>,
    visited: &mut [bool],
    mut pc: usize,
    start: usize,
    mut slots: Vec<usize>,
    hay: &H,
    sp: usize,
    st: &mut CapPending,
) where
    H: Hay + ?Sized,
    S: Symbols<Sym = H::Sym>,
{
    if st.cut || visited[pc] {
        return;
    }
    visited[pc] = true;
    loop {
        if st.cut {
            return;
        }
        match &prog[pc] {
            Inst::Jmp(to) => {
                if visited[*to] {
                    return;
                }
                visited[*to] = true;
                pc = *to;
            }
            Inst::Split(a, b) => {
                add_cap_thread(prog, list, visited, *a, start, slots.clone(), hay, sp, st);
                if visited[*b] {
                    return;
                }
                visited[*b] = true;
                pc = *b;
            }
            Inst::Assert(a) => {
                if !hay.assert_holds(*a, sp) {
                    return;
                }
                if visited[pc + 1] {
                    return;
                }
                visited[pc + 1] = true;
                pc += 1;
            }
            Inst::Save(slot) => {
                slots[*slot as usize] = sp;
                if visited[pc + 1] {
                    return;
                }
                visited[pc + 1] = true;
                pc += 1;
            }
            Inst::Set(_) => {
                list.push(CapThread { pc, start, slots });
                return;
            }
            Inst::Match => {
                let m = Match { start, end: sp };
                slots[0] = start;
                slots[1] = sp;
                if st.best.as_ref().is_none_or(|(b, _)| m.start <= b.start) {
                    st.best = Some((m, slots.clone()));
                }
                st.cut = true;
                return;
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Thread {
    pc: usize,
    start: usize,
}

/// Match state for one step of the VM.
#[derive(Default)]
struct Pending {
    best: Option<Match>,
    /// A match was recorded in one epsilon closure of this step: the alternatives that
    /// closure would add after it are lower priority and are dropped. Threads already
    /// queued when the closure started outrank it and are untouched.
    cut: bool,
}

/// Buffers the VM reuses across calls, so a search over a million lines does not allocate
/// four of them per line.
#[derive(Default)]
struct Scratch {
    clist: Vec<Thread>,
    nlist: Vec<Thread>,
    cl_marks: Vec<bool>,
    nl_marks: Vec<bool>,
}

impl Scratch {
    const fn new() -> Scratch {
        Scratch {
            clist: Vec::new(),
            nlist: Vec::new(),
            cl_marks: Vec::new(),
            nl_marks: Vec::new(),
        }
    }
}

thread_local! {
    static VM_SCRATCH: RefCell<Scratch> = const { RefCell::new(Scratch::new()) };
}

/// The Pike VM over `prog`: leftmost-first semantics, linear time in the haystack length.
/// `anchored` starts a thread only at `start`, for the DFA path that already knows where
/// the match begins.
fn run_vm<H, S>(prog: &[Inst<S>], hay: &H, start: usize, anchored: bool) -> Option<Match>
where
    H: Hay + ?Sized,
    S: Symbols<Sym = H::Sym>,
{
    VM_SCRATCH.with(|cell| {
        let mut scratch = cell.borrow_mut();
        let scratch = &mut *scratch;
        scratch.clist.clear();
        scratch.nlist.clear();
        scratch.cl_marks.clear();
        scratch.cl_marks.resize(prog.len(), false);
        scratch.nl_marks.clear();
        scratch.nl_marks.resize(prog.len(), false);
        run_vm_scratch(scratch, prog, hay, start, anchored)
    })
}

fn run_vm_scratch<H, S>(
    scratch: &mut Scratch,
    prog: &[Inst<S>],
    hay: &H,
    start: usize,
    anchored: bool,
) -> Option<Match>
where
    H: Hay + ?Sized,
    S: Symbols<Sym = H::Sym>,
{
    let Scratch {
        clist,
        nlist,
        cl_marks,
        nl_marks,
    } = scratch;
    let mut st = Pending::default();
    let mut sp = start;
    loop {
        // A fresh attempt starts at every position until something has matched; it is
        // lower priority than the threads carried over from earlier positions.
        st.cut = false;
        if st.best.is_none() && !(anchored && sp > start) {
            add_thread(prog, clist, cl_marks, 0, sp, hay, sp, &mut st);
            // That closure reaching `Match` only outranks what follows *it*: the threads
            // already queued here were seeded at earlier positions and so have a higher
            // priority, and their byte must still be offered to them. Clearing the cut
            // here is what lets a greedy repeat outgrow the empty match it can also make.
            st.cut = false;
        }
        let Some(sym) = hay.sym(sp) else {
            // Past the last symbol: the closures above have recorded any match that
            // ends here, and no thread can advance further.
            break;
        };
        for t in clist.drain(..) {
            if let Inst::Set(set) = &prog[t.pc] {
                if set.permits(sym) {
                    add_thread(
                        prog,
                        nlist,
                        nl_marks,
                        t.pc + 1,
                        t.start,
                        hay,
                        sp + 1,
                        &mut st,
                    );
                }
            }
        }
        std::mem::swap(clist, nlist);
        std::mem::swap(cl_marks, nl_marks);
        nl_marks.fill(false);
        sp += 1;
        if st.best.is_some() && clist.is_empty() {
            break;
        }
    }
    st.best
}

/// Epsilon closure: follow Split/Jmp/Assert from `pc`, recording matches.
#[allow(clippy::too_many_arguments)]
fn add_thread<H, S>(
    prog: &[Inst<S>],
    list: &mut Vec<Thread>,
    visited: &mut [bool],
    mut pc: usize,
    start: usize,
    hay: &H,
    sp: usize,
    st: &mut Pending,
) where
    H: Hay + ?Sized,
    S: Symbols<Sym = H::Sym>,
{
    if st.cut || visited[pc] {
        return;
    }
    visited[pc] = true;
    loop {
        if st.cut {
            return;
        }
        match &prog[pc] {
            Inst::Jmp(to) => {
                if visited[*to] {
                    return;
                }
                visited[*to] = true;
                pc = *to;
            }
            Inst::Split(a, b) => {
                add_thread(prog, list, visited, *a, start, hay, sp, st);
                if visited[*b] {
                    return;
                }
                visited[*b] = true;
                pc = *b;
            }
            Inst::Assert(a) => {
                if !hay.assert_holds(*a, sp) {
                    return;
                }
                if visited[pc + 1] {
                    return;
                }
                visited[pc + 1] = true;
                pc += 1;
            }
            Inst::Save(_) => {
                if visited[pc + 1] {
                    return;
                }
                visited[pc + 1] = true;
                pc += 1;
            }
            Inst::Set(_) => {
                list.push(Thread { pc, start });
                return;
            }
            Inst::Match => {
                // Leftmost-first: a match with a smaller start beats everything; with
                // the same start the thread that reaches Match later has the higher
                // priority (lower-priority ones were cut off here), so it overwrites.
                let m = Match { start, end: sp };
                if st.best.as_ref().is_none_or(|b| m.start <= b.start) {
                    st.best = Some(m);
                }
                // Everything after this thread in this step is lower priority and can
                // no longer produce a better match; a thread that stays alive from
                // earlier has higher priority and may still overwrite this match.
                st.cut = true;
                return;
            }
        }
    }
}

/// Compile `n` into `prog`, asking `symbols` what each consuming atom matches.
/// Build the program for `n`. `caps` asks for `Save` instructions around every capturing
/// group, which only the capture VM reads: the search program leaves them out, so the hot
/// path carries no writes it never uses.
fn compile<S>(n: &Node, prog: &mut Vec<Inst<S>>, symbols: fn(&Node) -> S, caps: bool) -> usize {
    match n {
        Node::Empty => {}
        Node::Byte(_) | Node::Set(_) | Node::Char(_) | Node::CharSet(_) | Node::Dot(_) => {
            prog.push(Inst::Set(symbols(n)));
        }
        Node::Concat(parts) => {
            for p in parts {
                compile(p, prog, symbols, caps);
            }
        }
        Node::Alt(branches) => {
            // split a | split b | ... | last
            let mut jumps = Vec::new();
            for (i, b) in branches.iter().enumerate() {
                if i + 1 == branches.len() {
                    compile(b, prog, symbols, caps);
                } else {
                    let split = prog.len();
                    prog.push(Inst::Split(0, 0));
                    compile(b, prog, symbols, caps);
                    jumps.push(prog.len());
                    prog.push(Inst::Jmp(0));
                    let next = prog.len();
                    if let Inst::Split(a, _) = &mut prog[split] {
                        *a = split + 1;
                        prog[split] = Inst::Split(*a, next);
                    }
                }
            }
            let end = prog.len();
            for j in jumps {
                prog[j] = Inst::Jmp(end);
            }
        }
        Node::Repeat(inner, min, max, greedy) => {
            for _ in 0..*min {
                compile(inner, prog, symbols, caps);
            }
            match max {
                None => {
                    // (inner)* : L: split(body, out); body; jmp L
                    let split = prog.len();
                    prog.push(Inst::Split(0, 0));
                    let body = prog.len();
                    compile(inner, prog, symbols, caps);
                    prog.push(Inst::Jmp(split));
                    let out = prog.len();
                    prog[split] = if *greedy {
                        Inst::Split(body, out)
                    } else {
                        Inst::Split(out, body)
                    };
                }
                Some(max) => {
                    // (max - min) optional copies, chained so each can stop early.
                    let mut splits = Vec::new();
                    for _ in *min..*max {
                        let split = prog.len();
                        prog.push(Inst::Split(0, 0));
                        splits.push(split);
                        compile(inner, prog, symbols, caps);
                    }
                    let out = prog.len();
                    for split in splits {
                        let body = split + 1;
                        prog[split] = if *greedy {
                            Inst::Split(body, out)
                        } else {
                            Inst::Split(out, body)
                        };
                    }
                }
            }
        }
        Node::Assert(a) => prog.push(Inst::Assert(*a)),
        Node::Group(i, inner) => {
            if caps {
                prog.push(Inst::Save(2 * *i));
            }
            compile(inner, prog, symbols, caps);
            if caps {
                prog.push(Inst::Save(2 * *i + 1));
            }
        }
    }
    prog.len()
}

/// What an atom consumes on the byte path. A pattern that needs Unicode semantics gets
/// its ASCII projection here: an ASCII haystack holds no other byte, so this program is
/// exact for it and much cheaper than decoding the line.
fn byte_symbols(n: &Node) -> ByteSet {
    match n {
        Node::Byte(b) => {
            let mut set = ByteSet::empty();
            set.add(*b);
            set
        }
        Node::Set(s) => *s,
        Node::Char(c) if c.is_ascii() => {
            let mut set = ByteSet::empty();
            set.add(*c as u8);
            set
        }
        Node::Char(_) => ByteSet::empty(),
        Node::CharSet(cc) => project(cc),
        Node::Dot(true) => ByteSet::full(),
        Node::Dot(false) => ByteSet(full_without_newline()),
        _ => ByteSet::empty(),
    }
}

/// What an atom consumes on the character path: one code point.
fn char_symbols(n: &Node) -> CharClass {
    let mut class = CharClass::new();
    match n {
        Node::Byte(b) => class.add(*b as char),
        Node::Set(_) => {
            let set = byte_symbols(n);
            for b in 0..=0x7fu8 {
                if set.contains(b) {
                    class.add(b as char);
                }
            }
        }
        Node::Char(c) => class.add(*c),
        Node::CharSet(cc) => return cc.clone(),
        Node::Dot(matches_newline) => return unicode::dot(*matches_newline),
        _ => {}
    }
    class
}

/// The ASCII bytes a code-point class can match, which is what a byte program needs.
fn project(cc: &CharClass) -> ByteSet {
    let mut set = ByteSet::empty();
    for b in 0..=0x7fu8 {
        if cc.contains(b as char) {
            set.add(b);
        }
    }
    set
}

/// The whole pattern as bytes when it is a case-sensitive literal.
fn literal_of(n: &Node, ci: bool) -> Option<Vec<u8>> {
    if ci {
        return None;
    }
    match n {
        Node::Empty => Some(Vec::new()),
        Node::Byte(b) => Some(vec![*b]),
        Node::Char(c) => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        Node::Concat(parts) => {
            let mut out = Vec::new();
            for p in parts {
                out.extend(literal_of(p, ci)?);
            }
            Some(out)
        }
        _ => None,
    }
}

/// True when any assertion (`^`, `$`, `\b`, `\B`, `\A`, `\z`) appears in the tree.
fn has_assertion(n: &Node) -> bool {
    match n {
        Node::Assert(_) => true,
        Node::Concat(parts) | Node::Alt(parts) => parts.iter().any(has_assertion),
        Node::Repeat(inner, ..) => has_assertion(inner),
        _ => false,
    }
}

/// The single byte every match must contain, when the parser can prove one cheaply.
fn required_byte(n: &Node) -> Option<u8> {
    match n {
        Node::Byte(b) => Some(*b),
        Node::Set(s) => s.sole(),
        Node::Char(c) => Some(first_byte(*c)),
        Node::CharSet(cc) => cc.sole().map(first_byte),
        // Every part of a concatenation must match, so the first one with a mandatory byte
        // of its own is a mandatory byte of the whole.
        Node::Concat(parts) => parts.iter().find_map(required_byte),
        // A branch with no mandatory byte leaves the alternation without one.
        Node::Alt(branches) => {
            let mut it = branches.iter().map(required_byte);
            let first = it.next().flatten()?;
            it.all(|b| b == Some(first)).then_some(first)
        }
        // An optional repetition matches zero times, and then its byte never appears.
        Node::Repeat(inner, min, _, _) => (*min > 0).then(|| required_byte(inner)).flatten(),
        Node::Group(_, inner) => required_byte(inner),
        Node::Assert(_) | Node::Dot(_) | Node::Empty => None,
    }
}

/// The whole match of `n`, when it is a fixed string of bytes: `foo`, `\.go`, `a{3}`.
fn pure_literal(n: &Node) -> Option<Vec<u8>> {
    match n {
        Node::Byte(b) => Some(vec![*b]),
        // A class of one byte matches exactly that byte.
        Node::Set(set) => set.sole().map(|b| vec![b]),
        Node::Char(c) => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        Node::Concat(parts) => {
            let mut out = Vec::new();
            for part in parts {
                out.extend_from_slice(&pure_literal(part)?);
            }
            Some(out)
        }
        Node::Empty => Some(Vec::new()),
        _ => None,
    }
}

/// The longest run of consecutive bytes every match of `n` must contain.
fn required_run(n: &Node) -> Option<Vec<u8>> {
    if let Some(lit) = pure_literal(n) {
        return (!lit.is_empty()).then_some(lit);
    }
    match n {
        // Literal neighbours join into one run; anything else contributes only a run of its
        // own, since where inside its own match that run sits is not known here.
        Node::Concat(parts) => {
            let mut best: Vec<u8> = Vec::new();
            let mut run: Vec<u8> = Vec::new();
            for part in parts {
                match pure_literal(part) {
                    Some(bytes) => run.extend_from_slice(&bytes),
                    None => {
                        if run.len() > best.len() {
                            best = std::mem::take(&mut run);
                        } else {
                            run.clear();
                        }
                        if let Some(bytes) = required_run(part) {
                            if bytes.len() > best.len() {
                                best = bytes;
                            }
                        }
                    }
                }
            }
            if run.len() > best.len() {
                best = run;
            }
            (!best.is_empty()).then_some(best)
        }
        // Every branch must hold the run, so it has to survive in all of them.
        Node::Alt(branches) => {
            let runs: Vec<Vec<u8>> = branches.iter().map(required_run).collect::<Option<_>>()?;
            let (first, rest) = runs.split_first()?;
            Some(common_run(first, rest))
        }
        // The first `min` iterations of a repetition are mandatory and sit next to each
        // other, so a literal inner turns them into one run.
        Node::Repeat(inner, min, _, _) if *min > 0 => match pure_literal(inner) {
            Some(lit) if *min <= 16 => Some(lit.repeat(*min as usize)),
            _ => required_run(inner),
        },
        _ => None,
    }
}

/// Where in a match the run a candidate scan looks for sits, which is what decides how little
/// the automaton has to walk to confirm a candidate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// The run is the whole match.
    Whole,
    /// Every match begins with it: an anchored forward walk from its first byte is the match.
    Start,
    /// Every match contains it somewhere, so the line holding it has to be walked.
    Any,
}

/// The runs a prefiltered scan jumps between, how each of them sits in a match, and the scan
/// itself.
#[derive(Debug)]
struct Candidates {
    runs: Vec<(Vec<u8>, Place)>,
    scan: crate::simd::Prefilter,
}

impl Candidates {
    fn new(runs: Vec<(Vec<u8>, Place)>) -> Option<Candidates> {
        let needles: Vec<Vec<u8>> = runs.iter().map(|(run, _)| run.clone()).collect();
        let scan = crate::simd::Prefilter::new(needles.clone())?;
        // The scan answers an index into its needles, which are sorted longest first; the runs
        // are kept in that same order.
        let mut sorted = Vec::with_capacity(runs.len());
        for needle in scan.needles() {
            let (_, place) = runs.iter().find(|(run, _)| run == needle)?;
            sorted.push((needle.clone(), *place));
        }
        Some(Candidates { runs: sorted, scan })
    }
}

/// The runs a prefiltered scan may look for: one per alternation branch when every branch has a
/// run of its own, and otherwise the single run every match must contain.
fn candidate_runs(n: &Node) -> Option<Vec<(Vec<u8>, Place)>> {
    if let Node::Alt(branches) = n {
        if branches.len() > 1 {
            let per_branch: Option<Vec<_>> = branches.iter().map(single_run).collect();
            if let Some(runs) = per_branch.filter(|r| r.len() <= crate::simd::MAX_PREFILTER) {
                let mut out: Vec<(Vec<u8>, Place)> = Vec::with_capacity(runs.len());
                for run in runs.into_iter().flatten() {
                    if !out.iter().any(|(seen, _)| *seen == run.0) {
                        out.push(run);
                    }
                }
                if !out.is_empty() {
                    return Some(out);
                }
            }
        }
    }
    single_run(n)
}

/// The one run that best stands in for `n`, and where it sits. A run the match begins with is
/// worth more than one it merely contains — it costs a walk over the match instead of over its
/// line — so an anchored run wins a tie and loses to a longer one, which is rarer to find.
fn single_run(n: &Node) -> Option<Vec<(Vec<u8>, Place)>> {
    if let Some(lit) = pure_literal(n) {
        return (lit.len() >= MIN_RUN).then_some(vec![(lit, Place::Whole)]);
    }
    let anchored = anchor_run(n).filter(|run| run.len() >= MIN_RUN);
    let anywhere = required_run(n).filter(|run| run.len() >= MIN_RUN);
    match (anchored, anywhere) {
        (Some(run), None) => Some(vec![(run, Place::Start)]),
        (Some(start), Some(run)) if start.len() >= run.len() => Some(vec![(start, Place::Start)]),
        (_, Some(run)) => Some(vec![(run, Place::Any)]),
        (None, None) => None,
    }
}

/// The longest literal run every match of `n` begins with. Unlike [`required_run`], which may
/// pick a run from the middle of the pattern, this one is anchored: it is what lets a candidate
/// be confirmed by walking only the match rather than the line it sits on.
fn anchor_run(n: &Node) -> Option<Vec<u8>> {
    if let Some(lit) = pure_literal(n) {
        return (!lit.is_empty()).then_some(lit);
    }
    match n {
        Node::Concat(parts) => {
            let mut run = Vec::new();
            for part in parts {
                match pure_literal(part) {
                    Some(bytes) => run.extend_from_slice(&bytes),
                    // A part that is not a fixed string can consume past the run it begins
                    // with — `e+` is one `e` or ten — so nothing after it joins this run. Its
                    // own run still begins every match, so the run ends there.
                    None => {
                        if let Some(inner) = anchor_run(part) {
                            run.extend_from_slice(&inner);
                        }
                        return (!run.is_empty()).then_some(run);
                    }
                }
            }
            (!run.is_empty()).then_some(run)
        }
        // Every iteration is complete, so a repetition that runs at least once begins with its
        // own run. A fixed count repeats it; an open one keeps one copy.
        Node::Repeat(inner, min, _, _) if *min > 0 => {
            let run = anchor_run(inner)?;
            Some(match *min {
                1 => run,
                n => run.repeat(n.min(16) as usize),
            })
        }
        Node::Group(_, inner) => anchor_run(inner),
        _ => None,
    }
}

/// The first byte of the line holding `at`.
fn line_start(hay: &[u8], at: usize) -> usize {
    crate::bytes::rfind_byte(hay, b'\n', at).map_or(0, |i| i + 1)
}

/// One past the last byte of the line holding `at`, newline excluded.
fn line_end(hay: &[u8], at: usize) -> usize {
    crate::bytes::find_byte(hay, b'\n', at).unwrap_or(hay.len())
}

/// True when a candidate scan may replace the plain byte walk of a haystack: the pattern reads
/// no assertion (or a slice would answer for a different text), permits no newline (or a match
/// could straddle the line bounds it works from), and cannot match empty (or a matching line
/// would hold no run at all).
fn candidates_are_sound(n: &Node, prog: &[Inst<ByteSet>]) -> bool {
    !has_assertion(n)
        && !prog
            .iter()
            .any(|inst| matches!(inst, Inst::Set(set) if set.permits(b'\n')))
        && !closure(prog, &[], 0).1
}

/// True when some instruction of the character program consumes a newline, which means a match
/// could straddle a line and one line would not hold it whole.
fn cprog_can_match_newline(cprog: &[Inst<CharClass>]) -> bool {
    cprog
        .iter()
        .any(|inst| matches!(inst, Inst::Set(set) if set.contains('\n')))
}

/// Runs shorter than this are the byte prefilter's job: a one-byte scan is no cheaper than the
/// automaton step it would replace.
const MIN_RUN: usize = 2;

/// The longest substring of `first` that every haystack in `rest` also contains.
fn common_run(first: &[u8], rest: &[Vec<u8>]) -> Vec<u8> {
    for len in (1..=first.len()).rev() {
        for at in 0..=first.len() - len {
            let cand = &first[at..at + len];
            if rest
                .iter()
                .all(|hay| crate::bytes::memmem(hay, cand).is_some())
            {
                return cand.to_vec();
            }
        }
    }
    Vec::new()
}

/// The first byte of `c` in UTF-8: every haystack holding `c` holds this byte.
fn first_byte(c: char) -> u8 {
    let mut buf = [0u8; 4];
    c.encode_utf8(&mut buf).as_bytes()[0]
}
/// The least frequent byte of a literal, a better prefilter than the first one.
fn rarest_byte(lit: &[u8]) -> Option<u8> {
    if lit.is_empty() {
        return None;
    }
    let mut best = lit[0];
    let mut best_score = u32::MAX;
    for &b in lit {
        let score = lit.iter().filter(|x| **x == b).count() as u32;
        if score < best_score {
            best_score = score;
            best = b;
        }
    }
    Some(best)
}

// ---------------------------------------------------------------------------------------
// Lazy DFA
//
// The VM pays for every alternative at every byte, and it restarts a thread at every
// position on the chance that a match begins there. This is the usual cure: subset
// construction over the same byte program, done lazily — a state is built the first time a
// scan needs it and memoised by its set of program counters — so a byte costs one indexed
// table lookup instead of an epsilon closure.
//
// A state is a set of pcs plus the assertion context of the position it sits at. That second
// half is what keeps `^`, `$`, `\b` and `\B` exact without a second engine for them: the six
// facts an assertion can ask about a position — is it the text start or end, does a line start
// or end here, is the byte before it or the byte at it a word byte — are six bits, and states
// differing only in them are different states. Only the bits a pattern's assertions actually
// read are kept, so a pattern that reads none — most patterns — has one context and pays
// nothing for this.
//
// A DFA finds where a match *ends*, not where it starts: leftmost-first preference is a
// property of thread priority, which subset construction throws away (`ab|a` and `a|ab`
// accept the same language and end differently on `ab`). So a search is two steps:
//
//   1. walk the forward DFA to the first position where a match ends. The start state is
//      re-entered at every position, so an empty set — the state the walk is in when no
//      thread from an earlier start survives — is what "nothing is alive" looks like, and the
//      last position the walk passed in it is a position no match can begin before;
//   2. run the Pike VM from there. Its answer is the leftmost-first one by definition, and
//      it costs the bytes from that position to the end of the match rather than the whole
//      haystack — which on a whole-file `-U` search is the difference between one pass over
//      the file and one pass per match.
//
// The walk also jumps while it sits in a start state: to the next occurrence of the run every
// match begins with when the pattern has one, and otherwise to the next byte that leaves the
// start state. Every position a jump passes over begins no match, so this is free and it is
// what keeps a search over a large buffer close to the cost of the prefilter scan itself.
//
// Lines that need the character program never get here: they stay on the VM, which is exact
// for code points and is not where the bytes are.
//
// A pattern that reads no assertions has one context, and for those a state's transitions are
// also kept densely by byte (`Dfa::dense`): the walk is then a single indexed load per byte
// rather than a class lookup feeding a table lookup. Those rows are capped too, and a state
// without one falls back to the class-keyed table, so nothing here is ever wrong, only slower.
//
// Every state is built on demand and the cache is capped (`MAX_DFA_STATES`); a pattern that
// blows the cap abandons the DFA for good and the VM answers from then on, so the worst case
// is the engine this one replaces, never something worse.

/// Cap on the states, and so the 1 KiB transition tables, one pattern may build.
const MAX_DFA_STATES: usize = 10_000;
/// Cap on the states that get a dense stride-256 row, and so the 1 MiB a table may take.
/// Past it the walk falls back to the class-keyed table for the states that have no row.
const MAX_DENSE_STATES: usize = 1_024;
/// Byte ranges a scan may skip over, when the start state escapes on few enough bytes: four
/// keep the word-at-a-time test one masked word wide.
const MAX_SKIP_RANGES: usize = 4;
/// Bytes a skip set may hold before skipping is judged not worth the range test — a set this
/// wide makes up most of what a line is.
const MAX_SKIP_BYTES: u16 = 48;
/// Bytes a dense walk's skip set may hold before stepping through them beats scanning for
/// them. The scan runs at about a byte per 0.19 ns and one indexed load at about 0.2 ns, so
/// only a start state that escapes on a couple of bytes is worth jumping to.
const MAX_DENSE_SKIP_BYTES: usize = 4;
/// Patterns one thread keeps a DFA for. A `search` run holds one; a test that also builds a
/// second pattern for comparison holds two.
const MAX_CACHED_DFAS: usize = 2;

/// `at == 0`.
const CTX_TEXT_START: u8 = 1 << 0;
/// `at == 0 || hay[at - 1] == b'\n'`: `^` holds.
const CTX_LINE_START: u8 = 1 << 1;
/// `at > 0` and `hay[at - 1]` is a word byte.
const CTX_WORD_BEFORE: u8 = 1 << 2;
/// `at == hay.len()`.
const CTX_TEXT_END: u8 = 1 << 3;
/// `at == hay.len() || hay[at] == b'\n'`: `$` holds.
const CTX_LINE_END: u8 = 1 << 4;
/// `at < hay.len()` and `hay[at]` is a word byte.
const CTX_WORD_AT: u8 = 1 << 5;
/// Number of distinct assertion contexts before folding.
const CTX_VALUES: usize = 64;

/// A word byte, in the byte program's ASCII sense (the character program has its own).
#[inline]
fn word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// What the assertions can ask about a position, given the byte before it and the byte at
/// it. A `None` byte is "no such position": the text start or end.
#[inline]
fn ctx_of(before: Option<u8>, at: Option<u8>) -> u8 {
    let mut ctx = match before {
        None => CTX_TEXT_START | CTX_LINE_START,
        Some(b'\n') => CTX_LINE_START,
        Some(b) if word_byte(b) => CTX_WORD_BEFORE,
        Some(_) => 0,
    };
    match at {
        None => ctx |= CTX_TEXT_END | CTX_LINE_END,
        Some(b'\n') => ctx |= CTX_LINE_END,
        Some(b) if word_byte(b) => ctx |= CTX_WORD_AT,
        Some(_) => {}
    }
    ctx
}

/// The context bits the assertions in `prog` read; a program with none needs no contexts.
fn ctx_mask(prog: &[Inst<ByteSet>]) -> u8 {
    let mut mask = 0;
    for inst in prog {
        if let Inst::Assert(a) = inst {
            mask |= match a {
                Assert::StartText => CTX_TEXT_START,
                Assert::EndText => CTX_TEXT_END,
                Assert::StartLine => CTX_LINE_START,
                Assert::EndLine => CTX_LINE_END,
                Assert::WordBoundary | Assert::NotWordBoundary => CTX_WORD_BEFORE | CTX_WORD_AT,
            };
        }
    }
    mask
}

/// Whether `a` holds in a raw context, matching `Hay::assert_holds` for bytes.
#[inline]
fn ctx_holds(a: Assert, ctx: u8) -> bool {
    let before = ctx & CTX_WORD_BEFORE != 0;
    let at = ctx & CTX_WORD_AT != 0;
    match a {
        Assert::StartText => ctx & CTX_TEXT_START != 0,
        Assert::EndText => ctx & CTX_TEXT_END != 0,
        Assert::StartLine => ctx & CTX_LINE_START != 0,
        Assert::EndLine => ctx & CTX_LINE_END != 0,
        Assert::WordBoundary => before != at,
        Assert::NotWordBoundary => before == at,
    }
}

/// The contexts a pattern has to tell apart, densely numbered: how many there are, the
/// index each raw context folds to, and one raw context per index for closures to read.
fn ctx_space(mask: u8) -> (usize, Box<[u8; CTX_VALUES]>, Vec<u8>) {
    let bits: Vec<u8> = (0..6).map(|b| 1u8 << b).filter(|b| mask & b != 0).collect();
    let nctx = 1usize << bits.len();
    let mut fold = Box::new([0u8; CTX_VALUES]);
    let mut raw = vec![0u8; nctx];
    for value in 0..CTX_VALUES {
        let mut dense = 0u8;
        for (i, bit) in bits.iter().enumerate() {
            if value as u8 & bit != 0 {
                dense |= 1 << i;
            }
        }
        fold[value] = dense;
        raw[dense as usize] = value as u8;
    }
    (nctx, fold, raw)
}

/// The dense context bits each byte contributes as the byte before a position and as the
/// byte at it. The two split cleanly because `ctx_of` is an OR of the two, and the mask's
/// "before" bits all precede its "at" bits in the dense numbering.
fn ctx_tables(fold: &[u8; CTX_VALUES]) -> ([u8; 257], [u8; 257]) {
    let neutral = Some(b' ');
    let mut before = [0u8; 257];
    let mut cur = [0u8; 257];
    for b in 0..257usize {
        let byte = if b == 256 { None } else { Some(b as u8) };
        before[b] = fold[ctx_of(byte, neutral) as usize];
        cur[b] = fold[ctx_of(neutral, byte) as usize];
    }
    (before, cur)
}

/// Group the 256 bytes into the classes no instruction of `prog` can tell apart, and pick one
/// byte to stand for each: a class moves where any of its bytes moves.
fn byte_classes(prog: &[Inst<ByteSet>]) -> (Box<[u8; 256]>, Vec<u8>) {
    let mut classes: Vec<Vec<u8>> = vec![(0..=255u8).collect()];
    for inst in prog {
        let Inst::Set(set) = inst else { continue };
        let mut split = Vec::with_capacity(classes.len() + 1);
        for class in &classes {
            let (inside, outside): (Vec<u8>, Vec<u8>) =
                class.iter().partition(|b| set.contains(**b));
            if !inside.is_empty() {
                split.push(inside);
            }
            if !outside.is_empty() {
                split.push(outside);
            }
        }
        classes = split;
    }
    let mut class_of = Box::new([0u8; 256]);
    let mut reps = Vec::with_capacity(classes.len());
    for (i, class) in classes.iter().enumerate() {
        reps.push(class[0]);
        for &b in class {
            class_of[b as usize] = i as u8;
        }
    }
    (class_of, reps)
}

/// A lazily built DFA over a byte program, with its transition tables packed flat.
struct Dfa {
    /// The pc sets the states are keyed on. Entry 0 is the empty set: because the start pc
    /// is re-entered at every position, the empty set *is* the start state.
    sets: Vec<Box<[u32]>>,
    index: HashMap<Vec<u32>, u32>,
    /// `trans[slot * nclass + class[byte]]` is where the byte leads, already scaled by the
    /// context count so that a scan adds the next context instead of multiplying. Bytes that no
    /// instruction in the program can tell apart share a class, so the table stays small enough
    /// to sit in cache and a state is built once per class rather than once per byte.
    trans: Vec<u32>,
    /// The class of each byte, and one byte standing for each class.
    class: Box<[u8; 256]>,
    reps: Vec<u8>,
    nclass: usize,
    /// `dense[state][byte]` for a program with one context: the same transitions as `trans`
    /// with the class lookup already resolved, so the walk is one indexed load per byte
    /// instead of two dependent ones. A row exists for the first `MAX_DENSE_STATES` states
    /// and none for the rest, which keep the class table; the two agree where both exist.
    dense: Vec<[u32; 256]>,
    /// Two flags per state: a match ends in it, and nothing at all is alive in it. A state
    /// that has not been built yet holds `STOP_UNBUILT`, which is what sends the walk to
    /// `prepare` without a second table read first.
    stop: Vec<u8>,
    built_count: usize,
    /// Dense context count, and the raw context each index stands for.
    nctx: usize,
    raw: Vec<u8>,
}

/// A match ends in this state.
const STOP_ACCEPT: u8 = 1;
/// Nothing is alive in this state. For a pattern that may only begin at the text start,
/// reaching one means the rest of the haystack is hopeless.
const STOP_SINK: u8 = 2;
/// This slot's transitions have not been built yet.
const STOP_UNBUILT: u8 = u8::MAX;

impl Dfa {
    fn new(nctx: usize, raw: Vec<u8>, prog: &[Inst<ByteSet>]) -> Dfa {
        let mut index = HashMap::new();
        index.insert(Vec::new(), 0u32);
        let (class, reps) = byte_classes(prog);
        let nclass = reps.len();
        Dfa {
            sets: vec![Box::new([])],
            index,
            trans: vec![0; nctx * nclass],
            class,
            reps,
            nclass,
            dense: if nctx == 1 {
                vec![[0; 256]]
            } else {
                Vec::new()
            },
            stop: vec![STOP_UNBUILT; nctx],
            built_count: 0,
            nctx,
            raw,
        }
    }

    /// The id of `set`, adding it to the state space if it is new.
    fn intern(&mut self, set: &[u32]) -> u32 {
        if let Some(&id) = self.index.get(set) {
            return id;
        }
        let id = self.sets.len() as u32;
        self.sets.push(set.to_vec().into_boxed_slice());
        self.index.insert(set.to_vec(), id);
        self.trans
            .resize(self.trans.len() + self.nctx * self.nclass, 0);
        if self.nctx == 1 && self.dense.len() < MAX_DENSE_STATES {
            self.dense.push([0; 256]);
        }
        self.stop.resize(self.stop.len() + self.nctx, STOP_UNBUILT);
        id
    }

    /// Build the transitions of one state on first use. `false` means the state cap is hit.
    fn prepare(&mut self, prog: &[Inst<ByteSet>], slot: usize) -> bool {
        if self.stop[slot] != STOP_UNBUILT {
            return true;
        }
        if self.built_count == MAX_DFA_STATES {
            return false;
        }
        let pid = slot / self.nctx;
        let (pcs, accept) = closure(prog, &self.sets[pid], self.raw[slot % self.nctx]);
        let sink = pcs.is_empty() && !accept;
        self.stop[slot] =
            (if accept { STOP_ACCEPT } else { 0 }) | (if sink { STOP_SINK } else { 0 });
        let mut into = Vec::with_capacity(pcs.len());
        let reps = std::mem::take(&mut self.reps);
        for (c, &rep) in reps.iter().enumerate() {
            into.clear();
            for &pc in &pcs {
                if let Inst::Set(set) = &prog[pc as usize] {
                    if set.contains(rep) {
                        into.push(pc + 1);
                    }
                }
            }
            // `pcs` ascends, so the target set is already sorted and unique.
            if !into.is_empty() {
                self.trans[slot * self.nclass + c] = self.intern(&into) * self.nctx as u32;
            }
        }
        if let Some(row) = self.dense.get_mut(pid) {
            for (b, into) in row.iter_mut().enumerate() {
                *into = self.trans[slot * self.nclass + self.class[b] as usize];
            }
        }
        self.reps = reps;
        self.built_count += 1;
        true
    }

    /// The byte ranges that leave the start state, in every context, when skipping the
    /// others is worthwhile. Empty when a start state accepts, when too much of the byte
    /// range escapes, or when it escapes above ASCII (the skip test is an ASCII word test).
    fn skip_set(&mut self, prog: &[Inst<ByteSet>]) -> Vec<(u8, u8)> {
        let mut escape = [false; 256];
        for slot in 0..self.nctx {
            if !self.prepare(prog, slot) || self.stop[slot] & STOP_ACCEPT != 0 {
                return Vec::new();
            }
            let base = slot * self.nclass;
            for (b, e) in escape.iter_mut().enumerate() {
                *e |= self.trans[base + self.class[b] as usize] != 0;
            }
        }
        if escape[0x7f..].iter().any(|&e| e) {
            return Vec::new();
        }
        let mut ranges: Vec<(u8, u8)> = Vec::new();
        let mut count = 0u16;
        let mut b = 0usize;
        while b < 0x7f {
            if !escape[b] {
                b += 1;
                continue;
            }
            let lo = b;
            while b + 1 < 0x7f && escape[b + 1] {
                b += 1;
            }
            count += (b - lo) as u16 + 1;
            ranges.push((lo as u8, b as u8));
            if ranges.len() > MAX_SKIP_RANGES || count > MAX_SKIP_BYTES {
                return Vec::new();
            }
            b += 1;
        }
        ranges
    }
}

/// The epsilon closure of `entry` plus the start pc — re-entered at every position, so that
/// a scan finds a match beginning anywhere — under the assertion context `ctx`. Returns the
/// pcs that consume a byte, ascending, and whether a match ends here.
fn closure(prog: &[Inst<ByteSet>], entry: &[u32], ctx: u8) -> (Vec<u32>, bool) {
    let mut seen = vec![false; prog.len()];
    let mut stack: Vec<u32> = Vec::with_capacity(prog.len());
    stack.push(0);
    stack.extend(entry.iter().rev().copied());
    let mut pcs = Vec::new();
    let mut accept = false;
    while let Some(pc) = stack.pop() {
        let at = pc as usize;
        if seen[at] {
            continue;
        }
        seen[at] = true;
        match &prog[at] {
            Inst::Set(_) => pcs.push(pc),
            Inst::Jmp(to) => stack.push(*to as u32),
            Inst::Split(a, b) => {
                stack.push(*b as u32);
                stack.push(*a as u32);
            }
            Inst::Assert(a) => {
                if ctx_holds(*a, ctx) {
                    stack.push(pc + 1);
                }
            }
            Inst::Match => accept = true,
            Inst::Save(_) => stack.push(pc + 1),
        }
    }
    pcs.sort_unstable();
    (pcs, accept)
}

/// The same tree with capturing groups rubbed out. Groups change nothing about which text
/// matches, so every analysis and the search programs work on the stripped tree; only the
/// capture program, which has to know where each group starts and ends, keeps them.
fn strip_groups(n: &Node) -> Node {
    match n {
        Node::Group(_, inner) => strip_groups(inner),
        Node::Concat(parts) => Node::Concat(parts.iter().map(strip_groups).collect()),
        Node::Alt(branches) => Node::Alt(branches.iter().map(strip_groups).collect()),
        Node::Repeat(inner, min, max, greedy) => {
            Node::Repeat(Box::new(strip_groups(inner)), *min, *max, *greedy)
        }
        Node::Empty => Node::Empty,
        Node::Byte(b) => Node::Byte(*b),
        Node::Set(s) => Node::Set(*s),
        Node::Char(c) => Node::Char(*c),
        Node::CharSet(cc) => Node::CharSet(cc.clone()),
        Node::Dot(m) => Node::Dot(*m),
        Node::Assert(a) => Node::Assert(*a),
    }
}

/// One thread's DFA for one pattern. Patterns are shared by every worker thread and a
/// `Regex` is immutable, so the states a scan discovers have to be cached per thread.
struct DfaCache {
    id: u64,
    fwd: Dfa,
    /// The dense context bits a byte contributes as the byte before a position and as the
    /// byte at it; index 256 is "no byte there". Adding the two gives the position's context
    /// index without a branch.
    before_bits: [u8; 257],
    cur_bits: [u8; 257],
    /// Bytes that leave the start state, for a walk to jump over: `None` until the first scan
    /// works them out. A dense walk reads it too, but only for a set narrow enough that the
    /// scan costs less than the loads it replaces.
    skip: Option<Vec<(u8, u8)>>,
    /// Nothing can begin anywhere but at the start of the haystack, so a forward state with
    /// nothing alive ends the search.
    start_only: bool,
    /// The DFA cannot answer this pattern (state cap): the VM takes over for good.
    off: bool,
}

thread_local! {
    static DFAS: RefCell<(Vec<DfaCache>, u64, usize)> =
        const { RefCell::new((Vec::new(), 0, 0)) };
}

/// Run `f` on this thread's DFA for `re`, building it on first use.
fn with_dfa<T>(re: &Regex, f: impl FnOnce(&mut DfaCache) -> T) -> Option<T> {
    DFAS.with(|cell| {
        let mut state = cell.borrow_mut();
        // One pattern is looked up thousands of times in a search, so the last answer is kept:
        // matching the id alone is enough to reuse it.
        let i = if state.1 == re.id && state.2 < state.0.len() {
            state.2
        } else {
            let i = match state.0.iter().position(|c| c.id == re.id) {
                Some(i) => i,
                None => {
                    if state.0.len() == MAX_CACHED_DFAS {
                        state.0.remove(0);
                    }
                    state.0.push(DfaCache::new(re));
                    state.0.len() - 1
                }
            };
            state.1 = re.id;
            state.2 = i;
            i
        };
        Some(f(&mut state.0[i]))
    })
}

impl DfaCache {
    fn new(re: &Regex) -> DfaCache {
        let (nctx, fold, raw) = ctx_space(re.ctx_mask);
        let (before_bits, cur_bits) = ctx_tables(&fold);
        DfaCache {
            id: re.id,
            fwd: Dfa::new(nctx, raw, &re.prog),
            before_bits,
            cur_bits,
            skip: None,
            start_only: starts_only_at_text_start(&re.prog),
            off: false,
        }
    }

    /// The context index at the position after the byte `before`, with `at` at it.
    #[inline]
    fn slot(&self, base: u32, before: Option<u8>, at: Option<u8>) -> usize {
        let index = |b: Option<u8>| b.map_or(256, usize::from);
        base as usize + self.before_bits[index(before)] as usize + self.cur_bits[index(at)] as usize
    }

    /// The leftmost-first match at or after `start`. The inner `None` is "no match"; the
    /// outer one is "the DFA gave up", and the caller falls back to the VM.
    fn find(
        &mut self,
        prog: &[Inst<ByteSet>],
        hay: &[u8],
        start: usize,
        run: Option<&crate::simd::Prefilter>,
    ) -> Option<Option<Match>> {
        if self.off {
            return None;
        }
        // Nothing can begin past the text start, so a later start has no answer to give.
        if self.start_only && start > 0 {
            return Some(None);
        }
        // `floor` comes back as the last position the walk found nothing alive at: no match
        // can begin before it, so the span is resolved from there rather than from the start
        // of the haystack.
        let mut floor = start;
        let Some(end) = self.forward(prog, hay, start, run, &mut floor) else {
            self.off = true;
            return None;
        };
        if end.is_none() {
            return Some(None);
        }
        // The walk narrowed the match to one that cannot have begun before `floor`; the VM
        // answers the exact span from there, and its answer is the leftmost-first one by
        // definition. On a line-at-a-time search `floor` is the line's first byte and this is
        // the whole line; on a whole-buffer search it is wherever the automaton last ran out
        // of threads, which is what keeps the work off the size of the buffer.
        Some(run_vm(prog, hay, floor, false))
    }

    /// The end of the first match, from the forward automaton only.
    fn forward_end(
        &mut self,
        prog: &[Inst<ByteSet>],
        hay: &[u8],
        start: usize,
        run: Option<&crate::simd::Prefilter>,
    ) -> Option<Option<usize>> {
        if self.off {
            return None;
        }
        if self.start_only && start > 0 {
            return Some(None);
        }
        let mut floor = start;
        let end = self.forward(prog, hay, start, run, &mut floor);
        if end.is_none() {
            self.off = true;
        }
        end
    }

    /// True when the pattern matches anywhere in `hay`.
    fn is_match(
        &mut self,
        prog: &[Inst<ByteSet>],
        hay: &[u8],
        start: usize,
        run: Option<&crate::simd::Prefilter>,
    ) -> Option<bool> {
        if self.off {
            return None;
        }
        let mut floor = start;
        match self.forward(prog, hay, start, run, &mut floor) {
            Some(found) => Some(found.is_some()),
            None => {
                self.off = true;
                None
            }
        }
    }

    /// Walk the forward DFA to the first position where a match ends, leaving in `floor` the
    /// last position the walk passed with nothing alive: the start state is re-entered at
    /// every position, so an empty set there says no thread from an earlier start survives,
    /// and no match can begin before it.
    fn forward(
        &mut self,
        prog: &[Inst<ByteSet>],
        hay: &[u8],
        start: usize,
        run: Option<&crate::simd::Prefilter>,
        floor: &mut usize,
    ) -> Option<Option<usize>> {
        let hopeless = self.start_only;
        // Past the end there is nothing to consume and nothing to find.
        if start > hay.len() {
            return Some(None);
        }
        if self.skip.is_none() {
            self.skip = Some(self.fwd.skip_set(prog));
        }
        if self.fwd.nctx == 1 {
            return self.forward_dense(prog, hay, start, run, hopeless, floor);
        }
        let slot = self.slot(
            0,
            start.checked_sub(1).map(|i| hay[i]),
            hay.get(start).copied(),
        );
        self.forward_slots(prog, hay, start, run, slot, hopeless, floor)
    }

    /// The forward walk over a program with one context: a state's row is fetched once and
    /// every byte in it is one indexed load, until the byte that leaves the state.
    #[allow(clippy::too_many_arguments)]
    fn forward_dense(
        &mut self,
        prog: &[Inst<ByteSet>],
        hay: &[u8],
        start: usize,
        run: Option<&crate::simd::Prefilter>,
        hopeless: bool,
        floor: &mut usize,
    ) -> Option<Option<usize>> {
        let mut at = start;
        let mut state = 0u32;
        let skip = self.skip.as_ref().filter(|ranges| {
            !ranges.is_empty()
                && ranges
                    .iter()
                    .map(|&(lo, hi)| (hi - lo) as usize + 1)
                    .sum::<usize>()
                    <= MAX_DENSE_SKIP_BYTES
        });
        let mut jumped = false;
        'states: loop {
            let mut stop = self.fwd.stop[state as usize];
            if stop == STOP_UNBUILT {
                if !self.fwd.prepare(prog, state as usize) {
                    return None;
                }
                stop = self.fwd.stop[state as usize];
            }
            if stop & STOP_ACCEPT != 0 {
                return Some(Some(at));
            }
            if stop & STOP_SINK != 0 && hopeless {
                return Some(None);
            }
            if state == 0 {
                // Every byte the jump passes over leaves the state empty, so it is a run of
                // positions a match cannot begin within. A position just jumped to is where
                // the walk starts, not another start state to look ahead from.
                *floor = at;
                if !hopeless && !jumped {
                    match skip_to(hay, at, run, skip.map_or(&[][..], Vec::as_slice)) {
                        Ahead::Jump(next) => {
                            at = next;
                            jumped = true;
                            continue 'states;
                        }
                        Ahead::Step => {}
                        Ahead::Nothing => return Some(None),
                    }
                }
            }
            jumped = false;
            let Some(row) = self.fwd.dense.get(state as usize) else {
                return self.forward_slots(prog, hay, at, run, state as usize, hopeless, floor);
            };
            loop {
                let Some(&byte) = hay.get(at) else {
                    return Some(None);
                };
                at += 1;
                let next = row[byte as usize];
                if next != state {
                    state = next;
                    continue 'states;
                }
            }
        }
    }

    /// The forward walk over the class-keyed table, for a program with several contexts and
    /// for the states past the dense cap.
    #[allow(clippy::too_many_arguments)]
    fn forward_slots(
        &mut self,
        prog: &[Inst<ByteSet>],
        hay: &[u8],
        start: usize,
        run: Option<&crate::simd::Prefilter>,
        mut slot: usize,
        hopeless: bool,
        floor: &mut usize,
    ) -> Option<Option<usize>> {
        let mut at = start;
        let mut jumped = false;
        loop {
            if !self.fwd.prepare(prog, slot) {
                return None;
            }
            let stop = self.fwd.stop[slot];
            if stop & STOP_ACCEPT != 0 {
                return Some(Some(at));
            }
            if stop & STOP_SINK != 0 && hopeless {
                return Some(None);
            }
            let Some(byte) = hay.get(at).copied() else {
                return Some(None);
            };
            // In a start state only a byte that leaves it can begin a match: jump to the
            // next one instead of stepping through the rest. Pointless when nothing may
            // begin past the text start: the next position is already a dead end.
            if slot < self.fwd.nctx {
                *floor = at;
            }
            if !hopeless && !jumped && slot < self.fwd.nctx {
                let ranges = self.skip.as_ref().map_or(&[][..], Vec::as_slice);
                match skip_to(hay, at, run, ranges) {
                    Ahead::Jump(next) => {
                        slot = self.slot(0, hay.get(next - 1).copied(), hay.get(next).copied());
                        at = next;
                        jumped = true;
                        continue;
                    }
                    Ahead::Step => {}
                    Ahead::Nothing => return Some(None),
                }
            }
            jumped = false;
            let next =
                self.fwd.trans[slot * self.fwd.nclass + self.fwd.class[byte as usize] as usize];
            at += 1;
            slot = self.slot(next, Some(byte), hay.get(at).copied());
        }
    }
}

/// What an automaton in its start state found ahead of it.
enum Ahead {
    /// A position a match could begin at, after bytes no match can begin within.
    Jump(usize),
    /// The start state here has bytes to step through as usual.
    Step,
    /// No position ahead has anything that could begin a match.
    Nothing,
}

/// Look ahead from a start state, which stands for "nothing is alive but the start itself":
/// the next occurrence of the run every match begins with is the next position worth walking,
/// and failing that a run, the next byte that leaves the start state. Either way every
/// position the jump passes over begins no match.
fn skip_to(
    hay: &[u8],
    at: usize,
    run: Option<&crate::simd::Prefilter>,
    ranges: &[(u8, u8)],
) -> Ahead {
    if let Some(finder) = run {
        return match finder.find_from(hay, at) {
            Some(next) if next > at => Ahead::Jump(next),
            Some(_) => Ahead::Step,
            None => Ahead::Nothing,
        };
    }
    let next = match ranges {
        [] => return Ahead::Step,
        // One byte is the fast one-byte scan; ranges are the slow fallback.
        [(lo, hi)] if lo == hi => crate::bytes::find_byte(hay, *lo, at),
        ranges => crate::bytes::find_in_ranges(hay, ranges, at),
    };
    match next {
        Some(next) if next > at => Ahead::Jump(next),
        Some(_) => Ahead::Step,
        None => Ahead::Nothing,
    }
}

/// True when no match can begin at a position that is not the start of the haystack, which
/// is what lets a forward walk stop as soon as nothing is alive.
fn starts_only_at_text_start(prog: &[Inst<ByteSet>]) -> bool {
    (0..CTX_VALUES).all(|raw| {
        raw as u8 & CTX_TEXT_START != 0 || {
            let (pcs, accept) = closure(prog, &[], raw as u8);
            pcs.is_empty() && !accept
        }
    })
}

// ---------------------------------------------------------------------------------------
// Parser

struct Parser<'a> {
    src: &'a [u8],
    at: usize,
    flags: Flags,
    depth: usize,
    /// Case-insensitive matching was enabled somewhere, inline flags included.
    folded: bool,
    /// Build code-point classes and note that the pattern needs Unicode semantics. The
    /// first parse runs with this off to find out whether a second one is needed at all.
    code_points: bool,
    /// The pattern names something only Unicode can answer: a `\p{..}` class, a
    /// shorthand (`\d`, `\w`, `\s`, `\b`), a POSIX class, a character outside ASCII,
    /// or `-i` over one.
    needs_unicode: bool,
    /// The index the next capturing group takes. Group 0 is the whole match, which the VM
    /// fills in from the match itself, so counting starts at 1.
    groups: usize,
}

/// What one parse produced.
struct Parsed {
    node: Node,
    /// Case-insensitive matching was on somewhere in the pattern.
    folded: bool,
    /// The pattern needs Unicode semantics, so it also needs a character program.
    unicode: bool,
    /// How many capturing groups the pattern has.
    groups: usize,
}

impl<'a> Parser<'a> {
    /// Parse `pattern`. With `code_points` off, `\d`, `\w`, `\s`, POSIX classes and
    /// negated classes keep their byte meanings and the result says whether the pattern
    /// needs a second, Unicode parse.
    fn parse(pattern: &'a str, flags: Flags, code_points: bool) -> Result<Parsed, String> {
        let mut p = Parser {
            src: pattern.as_bytes(),
            at: 0,
            flags,
            depth: 0,
            folded: flags.case_insensitive,
            code_points,
            needs_unicode: false,
            groups: 1,
        };
        let node = p.parse_alternation()?;
        if p.at < p.src.len() {
            return Err(p.err_at("unexpected character", p.at));
        }
        Ok(Parsed {
            node,
            folded: p.folded,
            unicode: p.needs_unicode,
            groups: p.groups - 1,
        })
    }
    fn err_at(&self, msg: &str, at: usize) -> String {
        format!(
            "{msg} at offset {at} in pattern {:?}",
            String::from_utf8_lossy(self.src)
        )
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.at).copied()
    }

    fn parse_alternation(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some(b'|') {
            self.at += 1;
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn parse_concat(&mut self) -> Result<Node, String> {
        let mut parts: Vec<Node> = Vec::new();
        loop {
            match self.peek() {
                None | Some(b'|') | Some(b')') => break,
                _ => {}
            }
            let atom = self.parse_atom()?;
            let atom = self.parse_quantifier(atom)?;
            parts.push(atom);
        }
        Ok(match parts.len() {
            0 => Node::Empty,
            1 => parts.pop().unwrap(),
            _ => Node::Concat(parts),
        })
    }

    fn parse_quantifier(&mut self, atom: Node) -> Result<Node, String> {
        let (min, max) = match self.peek() {
            Some(b'*') => {
                self.at += 1;
                (0, None)
            }
            Some(b'+') => {
                self.at += 1;
                (1, None)
            }
            Some(b'?') => {
                self.at += 1;
                (0, Some(1))
            }
            Some(b'{') => match self.parse_braces()? {
                Some(bounds) => bounds,
                None => return Ok(atom),
            },
            _ => return Ok(atom),
        };
        if matches!(atom, Node::Assert(_)) {
            return Err(self.err_at(
                "nothing to repeat: an assertion cannot be quantified",
                self.at,
            ));
        }
        let lazy = if self.peek() == Some(b'?') {
            self.at += 1;
            true
        } else {
            false
        };
        Ok(Node::Repeat(Box::new(atom), min, max, !lazy))
    }

    /// `{n}`, `{n,}` or `{n,m}`; `Ok(None)` when the brace is not a quantifier (a literal).
    fn parse_braces(&mut self) -> Result<Option<(u32, Option<u32>)>, String> {
        let save = self.at;
        self.at += 1;
        let mut min = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                min.push(c as char);
                self.at += 1;
            } else {
                break;
            }
        }
        if min.is_empty() {
            self.at = save;
            return Ok(None);
        }
        let mut max: Option<u32> = None;
        let mut has_comma = false;
        if self.peek() == Some(b',') {
            has_comma = true;
            self.at += 1;
            let mut digits = String::new();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    digits.push(c as char);
                    self.at += 1;
                } else {
                    break;
                }
            }
            if !digits.is_empty() {
                max = Some(
                    digits
                        .parse()
                        .map_err(|_| self.err_at("bad repetition bound", save))?,
                );
            }
        }
        if self.peek() != Some(b'}') {
            self.at = save;
            return Ok(None);
        }
        self.at += 1;
        let min: u32 = min
            .parse()
            .map_err(|_| self.err_at("bad repetition bound", save))?;
        // `{n}` is exactly n, not "n or more": only a comma makes the upper bound open.
        if !has_comma {
            max = Some(min);
        }
        if min > MAX_REPEAT || max.is_some_and(|m| m > MAX_REPEAT) {
            return Err(self.err_at("repetition bound above 1000", save));
        }
        if max.is_some_and(|m| m < min) {
            return Err(self.err_at("repetition range is empty (max < min)", save));
        }
        Ok(Some((min, max)))
    }

    fn parse_atom(&mut self) -> Result<Node, String> {
        let start = self.at;
        let c = self
            .peek()
            .ok_or_else(|| self.err_at("unexpected end", self.at))?;
        match c {
            b'(' => self.parse_group(),
            b'[' => self.parse_class(),
            b'.' => {
                self.at += 1;
                Ok(Node::Dot(self.flags.dot_matches_newline))
            }
            b'*' | b'+' | b'?' => Err(self.err_at("nothing to repeat", start)),
            b'{' => {
                // `{n,m}` here has nothing to repeat; anything else is a literal brace.
                match self.parse_braces()? {
                    Some(_) => Err(self.err_at("nothing to repeat", start)),
                    None => {
                        self.at += 1;
                        Ok(Node::Byte(b'{'))
                    }
                }
            }
            b')' => Err(self.err_at("unmatched ')'", start)),
            b'^' => {
                self.at += 1;
                Ok(Node::Assert(if self.flags.multi_line {
                    Assert::StartLine
                } else {
                    Assert::StartText
                }))
            }
            b'$' => {
                self.at += 1;
                Ok(Node::Assert(if self.flags.multi_line {
                    Assert::EndLine
                } else {
                    Assert::EndText
                }))
            }
            b'\\' => self.parse_escape(),
            _ => {
                let ch = self.next_char()?;
                if ch.is_ascii() {
                    return Ok(self.literal_node(ch as u8));
                }
                // A character outside ASCII is one code point on the character path, so
                // the pattern needs Unicode semantics. Under `-i` it becomes the class of
                // the characters it folds together with.
                self.needs_unicode = true;
                if self.flags.case_insensitive {
                    let mut class = CharClass::new();
                    class.add(ch);
                    class.fold();
                    Ok(Node::CharSet(class))
                } else {
                    Ok(Node::Char(ch))
                }
            }
        }
    }

    /// One literal byte, case-folded here when `(?i)` is in force.
    fn literal_node(&self, b: u8) -> Node {
        if self.flags.case_insensitive {
            let mut set = ByteSet::empty();
            set.add(b);
            set.fold_ascii();
            Node::Set(set)
        } else {
            Node::Byte(b)
        }
    }

    fn parse_group(&mut self) -> Result<Node, String> {
        let open = self.at;
        self.at += 1;
        self.depth += 1;
        if self.depth > 64 {
            return Err(self.err_at("groups nested more than 64 deep", open));
        }
        let saved = self.flags;
        let mut capturing = true;
        if self.peek() == Some(b'?') {
            match self.src.get(self.at + 1).copied() {
                Some(b':') => {
                    capturing = false;
                    self.at += 2;
                }
                Some(b'=') | Some(b'!') | Some(b'<') | Some(b'>') | Some(b'#') => {
                    return Err(self.err_at(
                        "lookaround, named groups and comments are not supported",
                        self.at,
                    ))
                }
                Some(b'i') | Some(b'm') | Some(b's') | Some(b'-') => {
                    if self.parse_inline_flags()? {
                        // `(?flags:` scopes the flags to the group body.
                        let body = self.parse_alternation()?;
                        if self.peek() != Some(b')') {
                            return Err(self.err_at("unterminated group", open));
                        }
                        self.at += 1;
                        self.flags = saved;
                        self.depth -= 1;
                        return Ok(body);
                    }
                    // Bare `(?flags)` applies to the rest of the enclosing pattern.
                    self.depth -= 1;
                    return Ok(Node::Empty);
                }
                _ => return Err(self.err_at("unknown (? group", self.at)),
            }
        }
        // A capturing group takes the next index; `(?:` is transparent.
        let index = if capturing {
            let i = self.groups as u32;
            self.groups += 1;
            Some(i)
        } else {
            None
        };
        let body = self.parse_alternation()?;
        if self.peek() != Some(b')') {
            return Err(self.err_at("unterminated group", open));
        }
        self.at += 1;
        self.flags = saved;
        self.depth -= 1;
        Ok(match index {
            Some(i) => Node::Group(i, Box::new(body)),
            None => body,
        })
    }

    /// `(?i)`, `(?ism)`, `(?-i)` at a group or pattern start. Returns true when the
    /// flags are scoped with `(?flags:` and a group body follows.
    fn parse_inline_flags(&mut self) -> Result<bool, String> {
        let start = self.at;
        self.at += 1; // '?'
        let mut negate = false;
        let mut seen = false;
        let scoped;
        loop {
            match self.peek() {
                Some(b'-') => {
                    negate = true;
                    self.at += 1;
                }
                Some(b'i') => {
                    self.flags.case_insensitive = !negate;
                    self.folded = true;
                    seen = true;
                    self.at += 1;
                }
                Some(b'm') => {
                    self.flags.multi_line = !negate;
                    seen = true;
                    self.at += 1;
                }
                Some(b's') => {
                    self.flags.dot_matches_newline = !negate;
                    seen = true;
                    self.at += 1;
                }
                Some(b')') => {
                    self.at += 1;
                    scoped = false;
                    break;
                }
                Some(b':') => {
                    self.at += 1;
                    scoped = true;
                    break;
                }
                _ => return Err(self.err_at("bad inline flags", start)),
            }
        }
        if !seen && !negate {
            return Err(self.err_at("empty inline flags", start));
        }
        Ok(scoped)
    }

    fn parse_escape(&mut self) -> Result<Node, String> {
        let start = self.at;
        self.at += 1;
        let c = self
            .peek()
            .ok_or_else(|| self.err_at("trailing backslash", start))?;
        let ci = self.flags.case_insensitive;
        let single = move |b: u8| {
            let mut s = ByteSet::empty();
            s.add(b);
            if ci {
                s.fold_ascii();
            }
            Node::Set(s)
        };
        match c {
            b'n' => {
                self.at += 1;
                Ok(single(b'\n'))
            }
            b'r' => {
                self.at += 1;
                Ok(single(b'\r'))
            }
            b't' => {
                self.at += 1;
                Ok(single(b'\t'))
            }
            b'f' => {
                self.at += 1;
                Ok(single(0x0c))
            }
            b'v' => {
                self.at += 1;
                Ok(single(0x0b))
            }
            b'0' => {
                self.at += 1;
                Ok(single(0))
            }
            b'x' => {
                self.at += 1;
                let h = self
                    .src
                    .get(self.at..self.at + 2)
                    .ok_or_else(|| self.err_at("\\x needs exactly two hex digits", start))?;
                let hex = std::str::from_utf8(h).map_err(|_| self.err_at("bad \\x", start))?;
                let b = u8::from_str_radix(hex, 16)
                    .map_err(|_| self.err_at("\\x needs exactly two hex digits", start))?;
                self.at += 2;
                Ok(single(b))
            }
            b'd' | b'D' | b'w' | b'W' | b's' | b'S' => {
                self.at += 1;
                self.needs_unicode = true;
                let negate = c.is_ascii_uppercase();
                if self.code_points {
                    let mut class = match c.to_ascii_lowercase() {
                        b'd' => unicode::digit(),
                        b'w' => unicode::word(),
                        _ => unicode::space(),
                    };
                    if negate {
                        class.negate();
                    }
                    return Ok(Node::CharSet(class));
                }
                Ok(Node::Set(perl_class(c.to_ascii_lowercase(), negate)))
            }
            b'A' => {
                self.at += 1;
                Ok(Node::Assert(Assert::StartText))
            }
            b'z' => {
                self.at += 1;
                Ok(Node::Assert(Assert::EndText))
            }
            b'b' => {
                self.at += 1;
                self.needs_unicode = true;
                Ok(Node::Assert(Assert::WordBoundary))
            }
            b'B' => {
                self.at += 1;
                self.needs_unicode = true;
                Ok(Node::Assert(Assert::NotWordBoundary))
            }
            b'p' | b'P' => {
                self.at += 1;
                self.needs_unicode = true;
                let mut class = self.property_class(start)?;
                if self.flags.case_insensitive {
                    class.fold();
                }
                if c == b'P' {
                    class.negate();
                }
                Ok(Node::CharSet(class))
            }
            b'1'..=b'9' => Err(self.err_at("backreferences are not supported", start)),
            c if c.is_ascii_alphanumeric() => Err(self.err_at("unknown escape", start)),
            c => {
                self.at += 1;
                Ok(single(c))
            }
        }
    }

    /// `\p{Name}` or `\p{N}`: a Unicode general category, with a leading `^` negating.
    fn property_class(&mut self, start: usize) -> Result<CharClass, String> {
        let name = if self.peek() == Some(b'{') {
            self.at += 1;
            let rest = &self.src[self.at..];
            let len = rest
                .iter()
                .position(|&b| b == b'}')
                .ok_or_else(|| self.err_at("unterminated \\p{...}", start))?;
            let name = std::str::from_utf8(&rest[..len]).unwrap_or("");
            self.at += len + 1;
            name.to_string()
        } else {
            match self.peek() {
                Some(c) if c.is_ascii_alphabetic() => {
                    self.at += 1;
                    (c as char).to_string()
                }
                _ => return Err(self.err_at("\\p needs a property name", start)),
            }
        };
        unicode::property(&name)
            .ok_or_else(|| self.err_at(&format!("unknown Unicode property {name:?}"), start))
    }

    /// `[:name:]` inside a class: the POSIX bracket expressions grep users write, in
    /// their Unicode meaning, which the byte program then narrows to ASCII.
    fn posix_class(&mut self) -> Result<CharClass, String> {
        let open = self.at;
        let rest = &self.src[self.at..];
        let end = rest.windows(2).position(|w| w == b":]");
        let Some(end) = end else {
            return Err(self.err_at("unterminated POSIX class", open));
        };
        let name = std::str::from_utf8(&rest[2..end]).unwrap_or("");
        self.at += end + 2;
        unicode::posix(name)
            .ok_or_else(|| self.err_at(&format!("unknown POSIX class {name:?}"), open))
    }

    fn parse_class(&mut self) -> Result<Node, String> {
        let open = self.at;
        self.at += 1;
        let mut negated = false;
        if matches!(self.peek(), Some(b'^') | Some(b'!')) {
            negated = true;
            self.at += 1;
        }
        let mut bytes = ByteSet::empty();
        let mut chars = CharClass::new();
        // A member that only Unicode can answer: a POSIX class, a `\d`-style escape, or a
        // character outside ASCII. Such a class is compiled as a code point set.
        let mut wide = false;
        let mut first = true;
        loop {
            let Some(c) = self.peek() else {
                return Err(self.err_at("unterminated character class", open));
            };
            if c == b']' && !first {
                self.at += 1;
                break;
            }
            first = false;
            // One member: a literal character, a byte escape, or a whole class.
            let lo: Option<(char, Option<u8>)> =
                if c == b'[' && self.src.get(self.at + 1) == Some(&b':') {
                    let class = self.posix_class()?;
                    self.needs_unicode = true;
                    wide = true;
                    or_set(&mut bytes, &project(&class));
                    chars.union(&class);
                    None
                } else if c == b'\\' {
                    let (item, end) = self.class_escape()?;
                    self.at = end;
                    match item {
                        ClassItem::Byte(b) => Some((b as char, Some(b))),
                        ClassItem::Class(class) => {
                            wide = true;
                            or_set(&mut bytes, &project(&class));
                            chars.union(&class);
                            None
                        }
                    }
                } else {
                    let ch = self.next_char()?;
                    if !ch.is_ascii() {
                        self.needs_unicode = true;
                        wide = true;
                    }
                    Some((ch, ch.is_ascii().then_some(ch as u8)))
                };
            let Some((lo, lo_byte)) = lo else { continue };
            if self.peek() != Some(b'-') || self.src.get(self.at + 1) == Some(&b']') {
                chars.add(lo);
                if let Some(b) = lo_byte {
                    bytes.add(b);
                }
                continue;
            }
            self.at += 1;
            let hi = if self.peek() == Some(b'\\') {
                let (item, end) = self.class_escape()?;
                self.at = end;
                match item {
                    ClassItem::Byte(b) => (b as char, Some(b)),
                    // A set at the end of a range is not a range: keep the low end and
                    // the set as separate members.
                    ClassItem::Class(class) => {
                        chars.add(lo);
                        if let Some(b) = lo_byte {
                            bytes.add(b);
                        }
                        wide = true;
                        or_set(&mut bytes, &project(&class));
                        chars.union(&class);
                        continue;
                    }
                }
            } else {
                let h = self.next_char()?;
                if !h.is_ascii() {
                    self.needs_unicode = true;
                    wide = true;
                }
                (h, h.is_ascii().then_some(h as u8))
            };
            if hi.0 < lo {
                return Err(self.err_at("character class range is backwards", self.at));
            }
            chars.add_range(lo, hi.0);
            if let (Some(lo_byte), Some(hi_byte)) = (lo_byte, hi.1) {
                bytes.add_range(lo_byte, hi_byte);
            }
        }
        if !self.code_points && !wide {
            if self.flags.case_insensitive {
                bytes.fold_ascii();
            }
            if negated {
                bytes.negate();
            }
            if bytes.is_empty() {
                return Err(self.err_at("empty character class", open));
            }
            return Ok(Node::Set(bytes));
        }
        if self.flags.case_insensitive {
            chars.fold();
        }
        if negated {
            chars.negate();
        }
        if chars.is_empty() {
            return Err(self.err_at("empty character class", open));
        }
        Ok(Node::CharSet(chars))
    }

    /// Decode and consume one character, or fail with an offset.
    fn next_char(&mut self) -> Result<char, String> {
        let s = std::str::from_utf8(&self.src[self.at..])
            .map_err(|_| self.err_at("invalid UTF-8 in pattern", self.at))?;
        let ch = s
            .chars()
            .next()
            .ok_or_else(|| self.err_at("unexpected end", self.at))?;
        self.at += ch.len_utf8();
        Ok(ch)
    }

    fn class_escape(&mut self) -> Result<(ClassItem, usize), String> {
        let start = self.at;
        self.at += 1;
        let c = self
            .peek()
            .ok_or_else(|| self.err_at("trailing backslash", start))?;
        let single = |b: u8| (ClassItem::Byte(b), start + 2);
        match c {
            b'n' => Ok(single(b'\n')),
            b'r' => Ok(single(b'\r')),
            b't' => Ok(single(b'\t')),
            b'f' => Ok(single(0x0c)),
            b'v' => Ok(single(0x0b)),
            b'0' => Ok(single(0)),
            b'x' => {
                let h = self
                    .src
                    .get(start + 2..start + 4)
                    .ok_or_else(|| self.err_at("\\x needs two hex digits", start))?;
                let hex = std::str::from_utf8(h).map_err(|_| self.err_at("bad \\x", start))?;
                let b = u8::from_str_radix(hex, 16)
                    .map_err(|_| self.err_at("\\x needs two hex digits", start))?;
                Ok((ClassItem::Byte(b), start + 4))
            }
            b'd' | b'w' | b's' => {
                self.needs_unicode = true;
                let mut class = match c {
                    b'd' => unicode::digit(),
                    b'w' => unicode::word(),
                    _ => unicode::space(),
                };
                if self.flags.case_insensitive {
                    class.fold();
                }
                Ok((ClassItem::Class(class), start + 2))
            }
            b'D' | b'W' | b'S' => {
                self.needs_unicode = true;
                let mut class = match c.to_ascii_lowercase() {
                    b'd' => unicode::digit(),
                    b'w' => unicode::word(),
                    _ => unicode::space(),
                };
                if self.flags.case_insensitive {
                    class.fold();
                }
                class.negate();
                Ok((ClassItem::Class(class), start + 2))
            }
            b'p' | b'P' => {
                self.needs_unicode = true;
                self.at = start + 2;
                let mut class = self.property_class(start)?;
                if self.flags.case_insensitive {
                    class.fold();
                }
                if c == b'P' {
                    class.negate();
                }
                Ok((ClassItem::Class(class), self.at))
            }
            c if c.is_ascii_alphanumeric() => Err(self.err_at("unknown escape in class", start)),
            c => Ok(single(c)),
        }
    }
}

/// One member of a character class, as `class_escape` reports it.
enum ClassItem {
    /// A single byte: `\n`, `\xHH`, or a `\]`-style escape.
    Byte(u8),
    /// A whole class of code points: `\d`, `\w`, `\s`, `\p{..}`.
    Class(CharClass),
}

fn or_set(into: &mut ByteSet, from: &ByteSet) {
    for i in 0..4 {
        into.0[i] |= from.0[i];
    }
}

fn perl_class(c: u8, negate: bool) -> ByteSet {
    let mut s = ByteSet::empty();
    match c {
        b'd' => s.add_range(b'0', b'9'),
        b'w' => {
            s.add_range(b'0', b'9');
            s.add_range(b'a', b'z');
            s.add_range(b'A', b'Z');
            s.add(b'_');
        }
        _ => {
            for b in [b' ', b'\t', b'\n', b'\r', 0x0b, 0x0c] {
                s.add(b);
            }
        }
    }
    if negate {
        s.negate();
    }
    s
}

/// All bytes except `\n`, as the four words of the bitmap.
fn full_without_newline() -> [u64; 4] {
    let mut s = ByteSet::full();
    let mut nl = ByteSet::empty();
    nl.add(b'\n');
    s.0[0] &= !nl.0[0];
    s.0[1] &= !nl.0[1];
    s.0[2] &= !nl.0[2];
    s.0[3] &= !nl.0[3];
    s.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pat: &str, hay: &str) -> Option<(usize, usize)> {
        Regex::new(pat)
            .unwrap_or_else(|e| panic!("{pat:?}: {e}"))
            .find(hay.as_bytes())
            .map(|m| (m.start, m.end))
    }

    fn matched(pat: &str, hay: &str) -> bool {
        Regex::new(pat).unwrap().is_match(hay.as_bytes())
    }

    #[test]
    fn literals_and_dot() {
        assert_eq!(m("needle", "a needle here"), Some((2, 8)));
        assert_eq!(m("needle", "none"), None);
        assert_eq!(m("a.c", "abc"), Some((0, 3)));
        assert_eq!(m("a.c", "a\nc"), None, "dot excludes newline");
        assert_eq!(
            Regex::new_with(
                "a.c",
                Flags {
                    dot_matches_newline: true,
                    ..Flags::default()
                }
            )
            .unwrap()
            .find(b"a\nc")
            .map(|m| (m.start, m.end)),
            Some((0, 3))
        );
        assert_eq!(m("", "abc"), Some((0, 0)), "empty pattern matches at 0");
        assert_eq!(m("x", ""), None);
    }

    #[test]
    fn quantifiers() {
        assert_eq!(m("ab*", "abbbc"), Some((0, 4)));
        assert_eq!(m("ab+", "ac"), None);
        assert_eq!(m("ab?c", "ac"), Some((0, 2)));
        assert_eq!(m("a{2,3}", "aaaa"), Some((0, 3)), "greedy takes the most");
        assert_eq!(m("a{2,3}?", "aaaa"), Some((0, 2)), "lazy takes the least");
        assert_eq!(m("^a{3}$", "aaa"), Some((0, 3)));
        assert_eq!(m("^a{4}$", "aaa"), None);
        assert_eq!(m("a{2,}", "aaaaa"), Some((0, 5)));
        assert!(
            Regex::new("a{1001}").is_err(),
            "bound over 1000 is rejected"
        );
        assert!(Regex::new("a{3,2}").is_err());
    }

    #[test]
    fn alternation_is_leftmost_first() {
        assert_eq!(m("a|ab", "ab"), Some((0, 1)));
        assert_eq!(m("ab|a", "ab"), Some((0, 2)));
        assert_eq!(m("(cat|dog)s", "hotdogs"), Some((3, 7)));
        assert_eq!(m("x|y|z", "zzz"), Some((0, 1)));
    }

    #[test]
    fn classes() {
        assert_eq!(m("[a-c]+", "xxabcxx"), Some((2, 5)));
        assert_eq!(m("[^a-c]+", "abxyzab"), Some((2, 5)));
        assert_eq!(m("[\\d]+", "ab123"), Some((2, 5)));
        assert_eq!(m("[\\w-]+", "ok-name"), Some((0, 7)));
        assert_eq!(m("[]]", "a]"), Some((1, 2)));
        assert_eq!(m("[!a]", "b"), Some((0, 1)));
        assert_eq!(m("[a\\-z]+", "a-z"), Some((0, 3)));
        assert!(Regex::new("[abc").is_err());
        assert!(Regex::new("[]").is_err());
        assert!(Regex::new("[z-a]").is_err());
    }

    #[test]
    fn perl_classes_and_escapes() {
        assert!(matched(r"\d+", "abc123"));
        assert!(!matched(r"\d+", "abc"));
        assert!(matched(r"\D", "abc"));
        assert!(matched(r"\w+", "a_1"));
        assert!(!matched(r"^\w+$", "a b"));
        assert!(matched(r"\s", "a b"));
        assert!(matched(r"\S+", "ab"));
        assert!(matched(r"\x41", "A"));
        assert!(matched(r"a\.c", "a.c"));
        assert!(!matched(r"a\.c", "abc"));
        assert!(matched(r"a\nb", "a\nb"));
        assert!(Regex::new(r"\p{L}").unwrap().is_match("é".as_bytes()));
        assert!(Regex::new(r"\1").is_err());
        assert!(Regex::new(r"\q").is_err());
    }

    #[test]
    fn anchors_and_boundaries() {
        assert!(matched("^abc", "abcdef"));
        assert!(!matched("^bc", "abcdef"));
        assert!(matched("def$", "abcdef"));
        assert!(!matched("de$", "abcdef"));
        assert!(matched(r"\bcat\b", "a cat."));
        assert!(!matched(r"\bcat\b", "concatenate"));
        assert!(matched(r"\Bcat", "concatenate"));
        assert_eq!(m("^", "abc"), Some((0, 0)));
        assert_eq!(m("$", "abc"), Some((3, 3)));
        // multi-line anchors
        let re = Regex::new_with(
            "^b",
            Flags {
                multi_line: true,
                ..Flags::default()
            },
        )
        .unwrap();
        assert_eq!(re.find(b"a\nb\nc").map(|m| m.start), Some(2));
        let re = Regex::new_with("^b", Flags::default()).unwrap();
        assert!(re.find(b"a\nb\nc").is_none());
    }

    #[test]
    fn case_insensitive() {
        let re = Regex::new_with(
            "needle",
            Flags {
                case_insensitive: true,
                ..Flags::default()
            },
        )
        .unwrap();
        assert!(re.is_match(b"a NEEDLE b"));
        assert!(re.is_match(b"Needle"));
        assert!(
            re.literal().is_none(),
            "folding disables the literal fast path"
        );
        let re = Regex::new_with(
            "[a-c]+",
            Flags {
                case_insensitive: true,
                ..Flags::default()
            },
        )
        .unwrap();
        assert!(re.is_match(b"ABC"));
        assert!(Regex::new("(?i)abc").unwrap().is_match(b"ABC"));
        assert!(Regex::new("(?i)abc").is_ok());
        assert!(Regex::new("(?i)abc").unwrap().is_match(b"xxABCxx"));
    }

    #[test]
    fn find_at_iteration() {
        let re = Regex::new("ab").unwrap();
        let hay = b"ab xx ab ab";
        let mut starts = Vec::new();
        let mut at = 0;
        while let Some(m) = re.find_at(hay, at) {
            starts.push(m.start);
            at = m.end.max(m.start + 1);
        }
        assert_eq!(starts, [0, 6, 9]);
        assert_eq!(re.find_at(hay, 7).map(|m| m.start), Some(9));
        assert_eq!(re.find_at(hay, hay.len() + 5), None);
        // Zero-width patterns advance one byte at a time.
        let re = Regex::new("").unwrap();
        let mut count = 0;
        let mut at = 0;
        while let Some(m) = re.find_at(b"abc", at) {
            count += 1;
            at = m.start + 1;
        }
        assert_eq!(count, 4, "positions 0..=3");
    }

    #[test]
    fn groups_and_nesting() {
        assert_eq!(m("(ab)+", "ababab"), Some((0, 6)));
        assert_eq!(m("(?:ab|cd)+", "xabcdx"), Some((1, 5)));
        assert_eq!(m("a(b(c)d)e", "abcde"), Some((0, 5)));
        assert_eq!(m("((a|b)c)+", "acbc"), Some((0, 4)));
        assert!(Regex::new("(unclosed").is_err());
        assert!(Regex::new("(?:)").is_ok());
        assert!(Regex::new(")").is_err());
        assert!(Regex::new("(?=x)").is_err());
    }

    #[test]
    fn nothing_to_repeat_and_other_errors() {
        for bad in [
            "*a", "+a", "?a", "(|", "a)", "a**", "^*", "$+", "{2}", "a{2,1}",
        ] {
            assert!(Regex::new(bad).is_err(), "{bad:?} should not compile");
        }
        // Braces that are not a repetition are literal text, and `|` with empty sides
        // compiles to the empty pattern, as in rg.
        assert!(Regex::new("a{").unwrap().is_match(b"a{"));
        assert!(Regex::new("|").unwrap().is_match(b""));
        assert!(Regex::new("a{,3}").unwrap().is_match(b"a{,3}"));
    }

    #[test]
    fn byte_classes_only_merge_bytes_no_instruction_tells_apart() {
        let re = Regex::new("[a-f0-9]+x").unwrap();
        let (class, reps) = byte_classes(&re.prog);
        assert_ne!(
            class[b'a' as usize], class[b'g' as usize],
            "in the class vs not"
        );
        assert_eq!(
            class[b'z' as usize], class[b'y' as usize],
            "neither is named"
        );
        assert!(reps.len() < 256, "the table is smaller than one per byte");
        // Every byte moves where its class's representative moves, for every set.
        for inst in &re.prog {
            let Inst::Set(set) = inst else { continue };
            for b in 0..=255u8 {
                let rep = reps[class[b as usize] as usize];
                assert_eq!(
                    set.contains(b),
                    set.contains(rep),
                    "byte {b} in class {rep}"
                );
            }
        }
    }

    #[test]
    fn required_byte_and_literal_hints() {
        assert_eq!(
            Regex::new("needle").unwrap().literal(),
            Some(&b"needle"[..])
        );
        assert_eq!(Regex::new("nee+dle").unwrap().literal(), None);
        assert_eq!(Regex::new("abc|abd").unwrap().required_byte(), Some(b'a'));
        assert_eq!(Regex::new(r"\d+").unwrap().required_byte(), None);
        assert_eq!(Regex::new("z.*").unwrap().required_byte(), Some(b'z'));
        assert_eq!(Regex::new("x").unwrap().required_byte(), Some(b'x'));
        // A byte an optional part might never match is not a prefilter.
        assert_eq!(Regex::new("a?b").unwrap().required_byte(), Some(b'b'));
        assert_eq!(Regex::new("(?:x)?z").unwrap().required_byte(), Some(b'z'));
        assert_eq!(Regex::new("x?").unwrap().required_byte(), None);
        // An alternation needs every branch to hold it, an empty one included.
        assert_eq!(Regex::new("a|").unwrap().required_byte(), None);
        assert_eq!(Regex::new("a?b|c?d").unwrap().required_byte(), None);
        assert_eq!(Regex::new("ab|ac").unwrap().required_byte(), Some(b'a'));
        // A concatenation may use a part after an assertion.
        assert_eq!(Regex::new(r"\bvar\b").unwrap().required_byte(), Some(b'v'));
        assert_eq!(
            Regex::new(r"[A-Z][a-z]*\.go").unwrap().required_byte(),
            Some(b'.')
        );
    }

    /// The prefilter must never reject a line the engine would match.
    #[test]
    fn required_literal_is_the_longest_mandatory_run() {
        let run = |pat: &str| {
            Regex::new(pat)
                .unwrap()
                .required_literal()
                .map(<[u8]>::to_vec)
        };
        assert_eq!(run(r"[A-Z][a-zA-Z0-9_]*\.go").as_deref(), Some(&b".go"[..]));
        assert_eq!(run("a[A-Z]qqqq[A-Z]").as_deref(), Some(&b"qqqq"[..]));
        assert_eq!(run("foo(bar|baz)").as_deref(), Some(&b"foo"[..]));
        // An alternation only keeps a run every branch holds.
        assert_eq!(run("(foo|foobar)z").as_deref(), Some(&b"foo"[..]));
        // One byte is the byte prefilter's job, not the run's.
        assert_eq!(run("ab|ac"), None);
        assert_eq!(run("a{3}b").as_deref(), Some(&b"aaa"[..]));
        assert_eq!(run("(?:ab)?cd").as_deref(), Some(&b"cd"[..]));
        assert_eq!(run(r"\d+"), None);
        assert_eq!(run("x?"), None);
        assert_eq!(run("[a-z][0-9]"), None);
        // A folded pattern holds no literal bytes to look for.
        assert_eq!(run("(?i)needle"), None);
    }

    /// Lines the block scan reports, and the same walk done with the exact per-line search.
    /// The block path only has to name the right lines, in order, so that is what is compared:
    /// a match may be found by a different route, never on a line the line-at-a-time scan does
    /// not report, and never missing one.
    fn block_lines(re: &Regex, hay: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some(end) = re.accept_end(hay, pos) {
            let at = end.saturating_sub(1).max(pos);
            let line = line_of(hay, at);
            if out.last() != Some(&line) {
                out.push(line);
            }
            pos = line_end(hay, at) + 1;
            if pos > hay.len() {
                break;
            }
        }
        out
    }

    fn line_lines(re: &Regex, hay: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some(m) = re.find_at(hay, pos) {
            let line = line_of(hay, m.start);
            if out.last() != Some(&line) {
                out.push(line);
            }
            pos = line_end(hay, m.start) + 1;
            if pos > hay.len() {
                break;
            }
        }
        out
    }

    fn line_of(hay: &[u8], at: usize) -> usize {
        hay[..at].iter().filter(|&&b| b == b'\n').count()
    }

    /// The candidate scan is the block entry point now, so it has to answer exactly what the
    /// line scan answers, at every offset and across line boundaries.
    #[test]
    fn the_candidate_scan_agrees_with_the_line_scan() {
        let pats = [
            r"[A-Z][a-zA-Z0-9_]*\.go",
            r"func [a-z]+\(",
            r"GO[A-Z]+_[a-z]+",
            r"ab|cd",
            r"Node|Parser",
            r"x?abc",
            r"a{3}b",
            r"[a-z]+_[a-z]+",
            r"Xq[a-z]*Zzy",
        ];
        let hays = [
            "package main\n\nfunc visitNode(n *node) {}\n// see Parser.go\nvar x_y = 1\n",
            "\n\nNode.go\nParser\nNodeParser.go\n",
            "no candidates here at all\njust words\n",
            "abcabc abcd ab\nXZzy\naaaab a{3}b\n",
            "Ǝé Node.go\nplain\n",
        ];
        for pat in pats {
            let re = Regex::new(pat).unwrap();
            for hay in hays {
                let hay = hay.as_bytes();
                assert_eq!(
                    block_lines(&re, hay),
                    line_lines(&re, hay),
                    "{pat:?} on {:?}",
                    String::from_utf8_lossy(hay)
                );
                // Every offset has to agree on whether a match is there at all.
                for from in 0..=hay.len() {
                    assert_eq!(
                        re.accept_end(hay, from).is_some(),
                        re.find_at(hay, from).is_some(),
                        "{pat:?} from {from} on {:?}",
                        String::from_utf8_lossy(hay)
                    );
                }
            }
        }
    }

    #[test]
    fn required_literal_is_never_a_false_negative() {
        let pats = [
            r"[A-Z][a-zA-Z0-9_]*\.go",
            "a[A-Z]qqqq[A-Z]",
            "foo(bar|baz)",
            "(foo|foobar)z",
            "a{3}b",
            r"\bvar\b",
            "a*b",
            "(a|b)c?",
            r"\d{2,4}",
            "ab+",
            "(?s)a.b",
        ];
        let hays = [
            "Foo.go",
            "aBqqqqC",
            "foobar",
            "foobaz",
            "foobarz",
            "aaab",
            "var x",
            "aaab",
            "12",
            "abbb",
            "a\nb",
            "\u{e9}\u{e9}\u{e9}",
        ];
        for pat in pats {
            let re = Regex::new(pat).unwrap();
            for hay in hays {
                let found = re.is_match(hay.as_bytes());
                assert_eq!(
                    found,
                    vm_find(&re, hay.as_bytes(), 0).is_some(),
                    "{pat:?} on {hay:?}"
                );
                if let Some(run) = re.required_literal() {
                    assert!(
                        !found || crate::bytes::memmem(hay.as_bytes(), run).is_some(),
                        "{pat:?} matched {hay:?} without its run {run:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn required_byte_is_never_a_false_negative() {
        let pats = [
            "a?b",
            "(?:x)?z",
            "a|",
            "a?b|c?d",
            "x?",
            r"\bvar\b",
            r"[A-Z][a-z]*\.go",
            "a*b",
            "(a|b)c?",
            "",
            r"(?i)needle",
            r"\d{2,4}",
        ];
        let hays = [
            "b", "z", "abc", "var", "Go.go", "", "x", "c", "abcd", "needle", "12",
        ];
        for pat in pats {
            let re = Regex::new(pat).unwrap();
            for hay in hays {
                let hits = re.is_match(hay.as_bytes());
                if let Some(b) = re.required_byte() {
                    assert!(
                        !hits || hay.as_bytes().contains(&b),
                        "{pat:?} matched {hay:?} without its required byte {b:?}"
                    );
                }
            }
        }
    }

    /// `find_at` with the DFA and its prefilter in front, against the VM answering alone: the
    /// automaton leads a whole-buffer search now — it jumps between the run every match begins
    /// with and resolves the span from the last position it found nothing alive at — so the
    /// sequence it produces has to be the VM's sequence, span for span.
    #[test]
    fn the_whole_buffer_scan_agrees_with_the_vm() {
        // Matches of every shape the mode has to handle: dense ones, ones that cross lines,
        // a run that occurs more often than it matches, and one nothing matches.
        let mut hay = Vec::new();
        for i in 0..4000 {
            hay.extend_from_slice(b"func (c *Config) Handle(p *Reader) Error {\n");
            if i % 7 == 0 {
                hay.extend_from_slice(b"\treturn errors.New(\"short buffer\")\n}\n\n");
            }
            if i % 11 == 0 {
                hay.extend_from_slice(b"start aaaa\nbbbb\ncccc end\n");
            }
        }
        for pat in [
            r"func .{0,40}Error",
            r"(?s)start.*?end",
            r"type [A-Z]\w* struct",
            r"errors\.New",
            r"[Z]{6}[a-z]{9}",
            r"b+",
            r"a*",
        ] {
            let re = Regex::new(pat).unwrap();
            let mut at = 0;
            while at <= hay.len() {
                let got = re.find_at(&hay, at);
                let want = run_vm(&re.prog, &hay[..], at, false);
                assert_eq!(got, want, "{pat} at {at}");
                match got {
                    Some(m) => at = m.end.max(m.start + 1),
                    None => break,
                }
            }
        }
    }

    /// A run that every match begins with lets the start state jump over the bytes between its
    /// occurrences. A buffer with no occurrence at all is answered without walking it, and the
    /// empty haystack keeps the empty match a nullable pattern has there.
    #[test]
    fn the_anchored_jump_skips_only_bytes_no_match_can_begin_with() {
        let hay = b"ff ff func x Error, ff func y Error, ff ff".repeat(200);
        let re = Regex::new(r"func .{0,40}Error").unwrap();
        assert_eq!(re.find(&hay), Some(Match { start: 6, end: 35 }));
        // The run ends where a part that is not a fixed string begins: `ne+dle` starts with
        // "ne" and not with "nedle", so a match whose `e+` is longer than one byte is still
        // found where the run itself is not.
        let open = Regex::new("ne+dle").unwrap();
        assert_eq!(open.find(b"neeeeedle"), Some(Match { start: 0, end: 9 }));
        assert_eq!(
            open.find_at(b"a neeeeedle", 1),
            Some(Match { start: 2, end: 11 })
        );
        let none = b"ff".repeat(100_000);
        assert_eq!(Regex::new(r"func .*Error").unwrap().find(&none), None);
        let star = Regex::new(r"b*").unwrap();
        assert_eq!(star.find(b""), Some(Match { start: 0, end: 0 }));
        assert_eq!(star.find(b"a"), Some(Match { start: 0, end: 0 }));
        // A pattern that can match empty gets no run to jump between: the empty match at the
        // start is still the answer.
        assert_eq!(star.find(b"abb"), Some(Match { start: 0, end: 0 }));
    }

    #[test]
    fn utf8_characters_match_as_bytes() {
        assert_eq!(m("é", "café"), Some((3, 5)));
        assert!(matched("日本", "a日本語b"));
        assert_eq!(m("[é]", "aé"), Some((1, 3)));
    }

    fn ci(pat: &str, hay: &str) -> Option<(usize, usize)> {
        Regex::new_with(
            pat,
            Flags {
                case_insensitive: true,
                ..Flags::default()
            },
        )
        .unwrap_or_else(|e| panic!("{pat:?}: {e}"))
        .find(hay.as_bytes())
        .map(|m| (m.start, m.end))
    }

    #[test]
    fn ascii_patterns_stay_on_the_byte_path() {
        for pat in ["needle", "[a-z]+", "^a.c$", r"\x41+", "é"] {
            // `é` alone is a literal, so it keeps the memmem fast path instead.
            assert!(!Regex::new(pat).unwrap().is_unicode(), "{pat:?}");
        }
        for pat in [
            r"\d",
            r"\w",
            r"\s",
            r"\b",
            r"[\d]",
            r"[^\w]",
            "[[:alpha:]]",
            r"\p{L}",
            r"[\p{Lu}_]",
            "[é]",
            "[à-ÿ]",
        ] {
            assert!(Regex::new(pat).unwrap().is_unicode(), "{pat:?}");
        }
        // `-i` over ASCII text is still the byte path.
        assert!(!Regex::new_with(
            "needle",
            Flags {
                case_insensitive: true,
                ..Flags::default()
            }
        )
        .unwrap()
        .is_unicode());
    }

    /// Files a whole-file (`-U`) search meets: ASCII, text with non-ASCII in it, non-ASCII
    /// next to the ASCII word characters a boundary is decided by, and bytes that are not
    /// text at all.
    fn mixed_corpus() -> Vec<(&'static str, &'static [u8])> {
        vec![
            ("ascii", b"type Widget struct {\n\tName string\n}\n"),
            (
                "non-ASCII in a comment",
                "// the 64-byte \u{3bc} value (omitted)\ntype Widget struct {\n\tName string\n}\n"
                    .as_bytes(),
            ),
            (
                "accented type name",
                "type Caf\u{e9} struct {\n}\ncaf\u{e9} au lait\n".as_bytes(),
            ),
            (
                "non-ASCII but no literal",
                "na\u{ef}vet\u{e9} \u{fc}n\u{ef}code\n\u{65e5}\u{672c}\u{8a9e}\n".as_bytes(),
            ),
            ("adjacent to ASCII", "\u{e9}abc type Widget struct\nabc\u{e9}\n".as_bytes()),
            ("greek", "\u{3a3}\u{399}\u{3a3}\u{3a5}\u{3a6}\u{39f}\u{3a3} \u{3c3}\u{3af}\u{3c3}\u{3c5}\u{3c6}\u{3bf}\u{3c2}\n".as_bytes()),
            ("combining mark", "e\u{301} type Widget struct\n".as_bytes()),
            ("arabic-indic digits", "\u{663}\u{664}\u{665} 123 type Widget struct\n".as_bytes()),
            ("punctuation only", "a\u{2014}b type Widget struct \u{ab}\u{bb}\n".as_bytes()),
            ("empty", b""),
            ("blank lines", b"type A struct\n\ntype B struct\n"),
            ("not text", &[b'c', 0xff, b'a', 0xfe, b'f', 0xc3]),
            ("not text with the literal", b"type \xff Widget struct\n"),
            // The literal's line begins with a multi-byte character, so the byte the line
            // starts at is not the byte the candidate starts at.
            (
                "literal against a multi-byte character",
                "\u{65e5}\u{672c}type Widget struct\n\u{3a3}\u{3c3}type Other struct\n"
                    .as_bytes(),
            ),
            // A multi-byte character on the last place of a line and the first place of the
            // next, so both ends of a window sit against a character wider than a byte.
            (
                "multi-byte on both sides of a line bound",
                "\u{4e00}\u{4e8c}\u{4e09}\ntype A struct \u{3c2}\n\u{663}\u{301}type B struct\n"
                    .as_bytes(),
            ),
            // Two candidates on one line, and a match that begins left of its own candidate.
            (
                "two candidates on one line",
                b"xx type A struct type B struct\ntype C struct\n",
            ),
            (
                "match begins left of the candidate",
                "type Caf\u{e9} struct\n\u{e9}\u{e9} type \u{3bc}x struct\n".as_bytes(),
            ),
            // A line holding the literal without a match for it, between two lines that do.
            (
                "literal in a comment between matches",
                "type A struct\n// type [ is an escape\n\u{e9}type B struct\n".as_bytes(),
            ),
            // The literal at the very end of the file, with and without a trailing newline.
            ("literal at the end, no newline", "x\n\u{e9} type A struct".as_bytes()),
            ("literal at the end, newline", "x\n\u{e9} type A struct\n".as_bytes()),
        ]
    }

    /// Every construct whose Unicode meaning differs from its byte meaning: `\w`, `\d`,
    /// `\s`, `\b`, `.`, a class with a non-ASCII member, a negated class, quantifiers,
    /// anchors, and case folding.
    const UNICODE_PATTERNS: &[(&str, bool)] = &[
        (r"type \w+ struct", false),
        (r"type \w+ struct", true),
        (r"\w+", false),
        (r"\b\w+\b", false),
        (r"\b", false),
        (r"\B", false),
        (r"\d+", false),
        (r"\w{2,4}", false),
        (r"\w+? struct", false),
        (r"a.b", false),
        (r".{3}", false),
        ("(?s).", false),
        ("(?m)^\\w+", false),
        (r"\w+$", false),
        (r"\p{L}+", false),
        ("[\\w\u{e9}-]+", false),
        ("[^a-z\n]+", false),
        (r"type\s+\w+\s+struct", false),
        ("\u{e9}+", false),
        ("\u{3a3}+", true),
        ("\u{3c3}+", true),
        ("\u{e9}\\b", false),
        ("(?:ab|\u{e9})c", false),
        (r"\w+ \w+ \w+", false),
        ("\u{2014}|\u{3bc}", false),
        // Mandatory ASCII literal plus a program that cannot consume a newline: the line
        // windows hold every match whole.
        (r"(?s)type \w+ struct", false),
        (r"(?s)type.*?struct", false),
        (r"\w+ struct", false),
        (r"(?m)^type \w+ struct", false),
        (r"^type \w+ struct", false),
        (r"type \w+ struct$", false),
        (r"\btype \w+ struct\b", false),
        (r"[^\n]*type \w+ struct", false),
        (r"(?s)type \w+ .*struct", false),
        (r"é*type \w+ struct", false),
        (r"type \w+ struct|struct \w+ type", false),
    ];

    /// The rule `-w` adds on top of a pattern's own answer.
    fn word_rule(hay: &'static [u8], word: bool) -> impl Fn(&Match) -> bool {
        move |m: &Match| !word || crate::bytes::at_word_boundary(hay, m.start, m.end)
    }

    /// The claim the whole-file fast path rests on: for a pattern that needs Unicode
    /// semantics, the byte program's answer on a file with no non-ASCII byte is exactly the
    /// character program's answer on the decoded file. If that were false anywhere, `-U`
    /// would report a different match, a different `-c` count and a different exit code
    /// depending only on which path a file happened to take.
    #[test]
    fn the_ascii_shortcut_answers_what_decoding_always_did() {
        let mut decoded_patterns = 0;
        for (pat, ci) in UNICODE_PATTERNS {
            let flags = Flags {
                case_insensitive: *ci,
                ..Flags::default()
            };
            let re = Regex::new_with(pat, flags).unwrap_or_else(|e| panic!("{pat:?}: {e}"));
            if re.is_unicode() {
                decoded_patterns += 1;
            }
            for (name, hay) in mixed_corpus() {
                for word in [false, true] {
                    let fast = re.find_all_in(hay, usize::MAX, word_rule(hay, word));
                    let slow = re.find_all_decoded(hay, usize::MAX, word_rule(hay, word));
                    assert_eq!(fast, slow, "{pat:?} (-i {ci}, -w {word}) on {name:?}");
                    // Equal spans are equal `-c` counts, and `-c` is what decides the exit
                    // code: 0 for a file that matched, 1 for one that did not.
                    assert_eq!(
                        fast.len(),
                        slow.len(),
                        "{pat:?} (-i {ci}, -w {word}) on {name:?}: match count"
                    );
                    assert_eq!(
                        fast.is_empty(),
                        slow.is_empty(),
                        "{pat:?} (-i {ci}, -w {word}) on {name:?}: exit code"
                    );
                }
            }
        }
        assert!(
            decoded_patterns >= 12,
            "the pattern list must exercise the character program"
        );
    }

    /// The claim the line windows rest on: for a pattern with a mandatory ASCII literal that
    /// cannot consume a newline, decoding only the lines the literal puts a candidate on
    /// reports exactly what decoding the whole file reports — same spans, same count, same
    /// order. The corpus puts non-ASCII text against ASCII word characters, CJK, combining
    /// marks, Greek final sigma and Arabic-Indic digits, keeps invalid UTF-8, and holds
    /// multi-byte characters against both ends of a window.
    #[test]
    fn the_line_windows_answer_what_decoding_always_did() {
        let before = line_windows();
        let mut windowed = 0;
        for pat in [
            r"type \w+ struct",
            r"\w+ struct",
            r"(?m)^type \w+ struct",
            r"type \w+ struct$",
            r"\btype \w+ struct\b",
            r"[^\n]*type \w+ struct",
        ] {
            let re = Regex::new(pat).unwrap_or_else(|e| panic!("{pat:?}: {e}"));
            assert!(re.is_unicode(), "{pat:?} needs the character program");
            assert!(re.required_literal().is_some(), "{pat:?} has a literal");
            for (name, hay) in mixed_corpus() {
                for word in [false, true] {
                    let seen = line_windows();
                    let fast = re.find_all_in(hay, usize::MAX, word_rule(hay, word));
                    if line_windows() > seen {
                        windowed += 1;
                    }
                    let slow = re.find_all_decoded(hay, usize::MAX, word_rule(hay, word));
                    assert_eq!(fast, slow, "{pat:?} (-w {word}) on {name:?}");
                }
            }
        }
        assert_eq!(
            line_windows() - before,
            windowed,
            "every windowed search above is one this test counted"
        );
        assert!(
            windowed >= 60,
            "the window path must be the path under test, {windowed} searches took it"
        );
    }

    /// A pattern that can consume a newline keeps decoding the file: a line holds only part of
    /// such a match, so settling one from a line would report it cut in two.
    #[test]
    fn a_match_across_lines_is_not_settled_from_one_line() {
        let re = Regex::new(r"(?s)type \w+\nstruct").unwrap();
        assert!(re.is_unicode());
        assert!(re.required_literal().is_some());
        let hay = "// \u{e9}\ntype Caf\u{e9}\nstruct {\n}\ntype Other struct\n".as_bytes();
        let before = line_windows();
        let got = re.find_all_in(hay, usize::MAX, |_| true);
        assert_eq!(line_windows(), before, "no windows");
        assert_eq!(got, re.find_all_decoded(hay, usize::MAX, |_| true));
        assert_eq!(got.len(), 1);
        assert_eq!(&hay[got[0].start..got[0].end], b"type Caf\xc3\xa9\nstruct");
    }

    /// A window is a line, and a newline is one ASCII byte, so no character can be cut by a
    /// window bound: a line of a thousand multi-byte characters puts the literal's candidate
    /// kilobytes away from the bound, which a fixed-width window would have to cut in half.
    #[test]
    fn a_wide_character_is_never_cut_by_a_window() {
        let mut hay = "\u{65e5}".repeat(1000).into_bytes();
        hay.extend_from_slice("type Caf\u{e9} struct\n\u{4e00}type B struct\n".as_bytes());
        let re = Regex::new(r"type \w+ struct").unwrap();
        let before = line_windows();
        let got = re.find_all_in(&hay, usize::MAX, |_| true);
        assert!(line_windows() > before, "the window path answered this");
        assert_eq!(got, re.find_all_decoded(&hay, usize::MAX, |_| true));
        assert_eq!(got.len(), 2);
        assert_eq!(
            &hay[got[0].start..got[0].end],
            "type Caf\u{e9} struct".as_bytes()
        );
    }

    /// A pattern with no mandatory literal puts no bound on where a match can be, so the file
    /// is decoded whole, as it was before the windows existed.
    #[test]
    fn a_pattern_without_a_mandatory_literal_decodes_the_file() {
        let re = Regex::new(r"\w+").unwrap();
        assert!(re.required_literal().is_none());
        let hay = "caf\u{e9} au lait\n\u{65e5}\u{672c}\u{8a9e}\n".as_bytes();
        let before = line_windows();
        let got = re.find_all_in(hay, usize::MAX, |_| true);
        assert_eq!(line_windows(), before, "no windows");
        assert_eq!(got, re.find_all_decoded(hay, usize::MAX, |_| true));
        assert_eq!(got.len(), 4);
    }

    /// The same equivalence over generated documents, which put fragments that matter to a
    /// window bound wherever a document builder can put them: against a line start, against a
    /// line end, inside the literal's own line, and beside an invalid byte. A match cannot be
    /// cut by a window, so a document that would show one cut is the point.
    #[test]
    fn the_line_windows_agree_over_generated_documents() {
        const FRAGMENTS: &[&str] = &[
            "type ",
            "type A struct",
            " struct",
            "caf\u{e9}",
            "\u{65e5}\u{672c}\u{8a9e}",
            "e\u{301}",
            "\u{3a3}\u{3c3}\u{3c2}",
            "\u{663}\u{664}\u{665}",
            "na\u{ef}ve",
            "\n",
            "\n\n",
            " ",
            "_x",
            "struct ",
            "\u{4e00}",
            "3",
        ];
        let patterns = [
            (r"type \w+ struct", false),
            (r"type \w+ struct", true),
            (r"\w+ struct", false),
            (r"(?m)^type \w+ struct", false),
            (r"type \w+ struct$", false),
            (r"\btype \w+ struct\b", false),
            (r".*type \w+ struct", false),
            (r"(?s)type \w+ struct", false),
            (r"\w+", false),
            (r"\b\w+\b", false),
        ];
        let compiled: Vec<Regex> = patterns
            .iter()
            .map(|(pat, ci)| {
                Regex::new_with(
                    pat,
                    Flags {
                        case_insensitive: *ci,
                        ..Flags::default()
                    },
                )
                .unwrap_or_else(|e| panic!("{pat:?}: {e}"))
            })
            .collect();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut windowed = 0;
        let mut decoded = 0;
        for round in 0..240 {
            let mut hay = Vec::new();
            for _ in 0..=next() % 14 {
                hay.extend_from_slice(
                    FRAGMENTS[(next() % FRAGMENTS.len() as u64) as usize].as_bytes(),
                );
            }
            // One document in five is not text at all, which is the reading a file of binary
            // content gets, and one in five more holds a single invalid byte in the middle of
            // text, which the whole-file check must still see.
            match round % 5 {
                1 => hay.insert(hay.len() / 2, 0xff),
                3 => hay.insert(0, 0xfe),
                _ => {}
            }
            for (re, word) in compiled.iter().flat_map(|re| [(re, false), (re, true)]) {
                let accept =
                    |m: &Match| !word || crate::bytes::at_word_boundary(&hay, m.start, m.end);
                let seen = line_windows();
                let fast = re.find_all_in(&hay, usize::MAX, accept);
                match line_windows() > seen {
                    true => windowed += 1,
                    false => decoded += 1,
                }
                let slow = re.find_all_decoded(&hay, usize::MAX, accept);
                assert_eq!(
                    fast,
                    slow,
                    "{:?} (-w {word}) on {:?}",
                    re.literal(),
                    String::from_utf8_lossy(&hay)
                );
            }
        }
        assert!(windowed >= 240, "{windowed} windowed searches");
        assert!(decoded >= 240, "{decoded} decoded searches");
    }

    /// A mandatory literal the file does not hold rules out a match under either reading, so
    /// the file is never decoded — the shortcut is a proof, not a heuristic.
    #[test]
    fn a_file_without_the_mandatory_literal_is_answered_without_decoding() {
        let re = Regex::new(r"type \w+ struct").unwrap();
        assert!(re.is_unicode());
        assert!(re.required_literal().is_some());
        for hay in ["caf\u{e9} au lait\n", "\u{65e5}\u{672c}\u{8a9e} \u{2014}\n"] {
            let hay = hay.as_bytes();
            assert!(re.find_all_in(hay, usize::MAX, |_| true).is_empty());
            assert!(re.find(hay).is_none(), "the per-call path agrees");
        }
    }

    /// And a file that holds it is decoded and matched with the character program's idea of
    /// a word character, which is the whole reason the mode exists.
    #[test]
    fn a_file_that_holds_the_literal_is_decoded_and_matched() {
        let re = Regex::new(r"type \w+ struct").unwrap();
        let hay = "// \u{3bc}\ntype Caf\u{e9} struct {\n}\n".as_bytes();
        let got = re.find_all_in(hay, usize::MAX, |_| true);
        assert_eq!(got.len(), 1);
        assert_eq!(
            &hay[got[0].start..got[0].end],
            "type Caf\u{e9} struct".as_bytes()
        );
    }

    #[test]
    fn word_class_is_unicode() {
        assert_eq!(m(r"\w+", "café"), Some((0, 5)), "é is one word character");
        assert_eq!(m(r"^\w+$", "café"), Some((0, 5)));
        assert_eq!(m(r"\w+", "Привет!"), Some((0, 12)));
        assert_eq!(m(r"\w+", "日本語"), Some((0, 9)));
        assert_eq!(m(r"\w+", "٣٤٥"), Some((0, 6)), "Arabic-Indic digits");
        assert_eq!(m(r"^\W+$", " ,.! "), Some((0, 5)));
        assert_eq!(
            m(r"\w+", "a_b 12"),
            Some((0, 3)),
            "ASCII text is unaffected"
        );
    }

    #[test]
    fn digit_class_is_unicode() {
        assert_eq!(m(r"\d+", "no ٣٤ digits"), Some((3, 7)));
        assert_eq!(
            m(r"\d+", "abc123"),
            Some((3, 6)),
            "ASCII digits still match"
        );
        assert_eq!(m(r"^\D+$", "café"), Some((0, 5)));
        assert_eq!(m(r"\p{Nd}+", "٤٢"), Some((0, 4)));
    }

    #[test]
    fn unicode_general_categories() {
        for (pat, hay) in [
            (r"^\p{L}+$", "Ελληνικά"),
            (r"^\p{L}+$", "Привет"),
            (r"^\p{L}+$", "日本語"),
            (r"^\p{L}+$", "café"),
        ] {
            assert_eq!(m(pat, hay), Some((0, hay.len())), "{pat:?} on {hay:?}");
        }
        assert_eq!(m(r"^\p{Lu}+$", "ΕΛΛΗΝΙΚΆ"), Some((0, 16)));
        assert_eq!(m(r"^\p{Ll}+$", "ελληνικά"), Some((0, 16)));
        assert_eq!(m(r"^\p{Lu}\p{Ll}+$", "Abc"), Some((0, 3)));
        assert_eq!(m(r"^\p{P}+$", "«»—…"), Some((0, 10)));
        assert_eq!(m(r"^\p{S}+$", "€₽+"), Some((0, 7)));
        assert_eq!(m(r"^\p{Z}$", "\u{2003}"), Some((0, 3)));
        assert_eq!(m(r"^\p{C}+$", "\u{1}\u{200b}"), Some((0, 4)));
        assert!(Regex::new(r"\p{Bogus}").is_err());
        assert!(Regex::new(r"\p{").is_err());
    }

    #[test]
    fn case_folding_reaches_non_ascii() {
        assert_eq!(ci("CAF[É]", "café"), Some((0, 5)));
        assert_eq!(ci("café", "CAFÉ"), Some((0, 5)));
        assert_eq!(ci("É", "é"), Some((0, 2)));
        assert_eq!(ci("é", "É"), Some((0, 2)));
        assert_eq!(ci("Σ", "ς"), Some((0, 2)), "final sigma folds to sigma");
        assert_eq!(ci("σ+", "Σςσ"), Some((0, 6)));
        assert_eq!(ci("ПРИВЕТ", "привет"), Some((0, 12)));
        assert_eq!(ci("[É]", "é"), Some((0, 2)));
        assert_eq!(ci("é+", "ÉÉ"), Some((0, 4)), "a folded quantifier");
        assert_eq!(ci(r"[\p{Lu}]+", "abc"), Some((0, 3)), "`-i` folds a class");
        assert_eq!(ci("(?i)é", "É"), Some((0, 2)));
    }

    #[test]
    fn classes_above_ascii() {
        assert_eq!(m("[à-ÿ]+", "café"), Some((3, 5)), "a range above 0x7f");
        assert_eq!(m("[é]+", "café"), Some((3, 5)));
        assert_eq!(m("^[^é]+", "café"), Some((0, 3)), "a negated Unicode class");
        assert_eq!(m(r"^[^\p{L}]+$", " 12 "), Some((0, 4)));
        assert_eq!(m(r"^[\p{L}\d]+$", "abc٣"), Some((0, 5)));
        assert_eq!(m("[α-ω]+", "αβγ x"), Some((0, 6)));
        assert_eq!(m("[;é]", "bé"), Some((1, 3)));
        assert!(
            Regex::new("[ÿ-à]").is_err(),
            "a backwards range still fails"
        );
    }

    #[test]
    fn posix_classes_are_unicode() {
        assert_eq!(m("[[:alpha:]]+", "café 1"), Some((0, 5)));
        assert_eq!(m("[[:digit:]]+", "x٣"), Some((1, 3)));
        assert_eq!(m("[[:space:]]+$", "ab\u{2003}"), Some((2, 5)));
        assert_eq!(m("[[:word:]]+", "naïve"), Some((0, 6)));
        assert_eq!(m("[[:upper:]]+", "ÉÈ"), Some((0, 4)));
        assert_eq!(m("[[:punct:]]+", "hi, there"), Some((2, 3)));
        assert!(Regex::new("[[:bogus:]]").is_err());
    }

    #[test]
    fn matches_after_a_multibyte_character_report_byte_offsets() {
        assert_eq!(m(r"\d+", "é 42"), Some((3, 5)));
        assert_eq!(m(r"\w+", "é abc"), Some((0, 2)), "the first word is the é");
        assert_eq!(m(r"\d+", "日本語 42"), Some((10, 12)));
        assert_eq!(m(r"\p{L}+", "…é"), Some((3, 5)));
        let re = Regex::new(r"\w+").unwrap();
        let hay = "é αβ 中".as_bytes();
        let mut all = Vec::new();
        let mut at = 0;
        while let Some(m) = re.find_at(hay, at) {
            all.push((m.start, m.end));
            at = m.end;
        }
        assert_eq!(all, [(0, 2), (3, 7), (8, 11)]);
    }

    #[test]
    fn dot_is_one_character_on_the_character_path() {
        assert_eq!(m(r"\p{Lu}.", "Aé"), Some((0, 3)));
        assert_eq!(m(r".\p{Lu}", "éA"), Some((0, 3)));
        // An ASCII-only pattern keeps the byte meaning, as documented for `^caf.$`.
        assert_eq!(m("^caf.$", "café"), None);
    }

    #[test]
    fn a_line_that_is_not_utf8_keeps_the_byte_meanings() {
        for pat in [r"\w+", r"\p{L}+"] {
            let re = Regex::new(pat).unwrap();
            assert_eq!(
                re.find(b"na\xefve").map(|m| (m.start, m.end)),
                Some((0, 2)),
                "{pat:?}: bytes, not lossy characters"
            );
        }
    }

    #[test]
    fn no_catastrophic_backtracking() {
        let re = Regex::new("(a+)+$").unwrap();
        let mut hay = vec![b'a'; 40_000];
        hay.push(b'b');
        let t = std::time::Instant::now();
        assert!(!re.is_match(&hay));
        let took = t.elapsed();
        assert!(took.as_secs_f64() < 2.0, "took {took:?}");
    }

    #[test]
    fn posix_bracket_expressions() {
        let alpha = Regex::new("[[:alpha:]]+").unwrap();
        assert_eq!(alpha.find(b"abc123"), Some(Match { start: 0, end: 3 }));
        let digit = Regex::new("[[:digit:]]+").unwrap();
        assert_eq!(digit.find(b"abc123"), Some(Match { start: 3, end: 6 }));
        let mixed = Regex::new("[[:alpha:][:digit:]]+").unwrap();
        assert_eq!(mixed.find(b"abc123"), Some(Match { start: 0, end: 6 }));
        let negated = Regex::new("[^[:space:]]+").unwrap();
        assert_eq!(negated.find(b"  ab\tc"), Some(Match { start: 2, end: 4 }));
        let word = Regex::new("[[:word:]]+").unwrap();
        assert_eq!(word.find(b" x_1 "), Some(Match { start: 1, end: 4 }));
        assert!(Regex::new("[[:bogus:]]").is_err());
        assert!(
            Regex::new("[[:alpha:]").is_err(),
            "an unterminated class fails"
        );
    }

    /// The pre-DFA answer: what `find_at` does once the DFA is out of the picture.
    fn vm_find(re: &Regex, hay: &[u8], start: usize) -> Option<Match> {
        if let Some(lit) = &re.lit {
            let i = crate::bytes::memmem_from(hay, lit, start)?;
            return Some(Match {
                start: i,
                end: i + lit.len(),
            });
        }
        if let Some(cprog) = &re.cprog {
            if !hay.is_ascii() {
                if let Ok(text) = std::str::from_utf8(hay) {
                    return re.run_chars(cprog, text, start);
                }
            }
        }
        run_vm(&re.prog, hay, start, false)
    }

    #[test]
    fn dfa_agrees_with_the_vm() {
        let hays: [&[u8]; 12] = [
            b"",
            b"x",
            b"package main\nvar x, y int\nfunc f() {}\n",
            br"a cat. concatenate cat",
            b"caf\xc3\xa9 memmove memMOVE na\xc3\xafve",
            b"abc\xff\xfe abc",
            b"line one\r\nline two\r\n",
            b"func a() {} func bb(1) {} Func cc(",
            b"import \"internal/go.go\" and \"a.rs\"",
            b"2026-11-18 19:33:07 build 42 of 1000\n",
            b"\n\n\n",
            b"var\tx, var  y, vary,",
        ];
        let pats = [
            "[A-Z][a-zA-Z0-9_]*\\.go",
            r"func \w+\(",
            r"^package \w+",
            r"\bvar\b \w+,",
            "[0-9]{2,4}",
            "(?i)memmove",
            r"\.(rs|toml)$",
            "abc|b",
            "bc|abc",
            "a|ba",
            "cat|ca",
            "(?:ab|a)*c",
            "a*",
            "",
            "^",
            "$",
            r"\b",
            r"\B",
            "^a{2,3}?$",
            "[^a-c]+",
            r"\w+@\w+",
            "x|y|z",
            "(a+)+$",
            r"\bcat\b",
            r"^\s*$",
            "nomatchanywhere",
            r"go\.go|a\.rs",
            "[[:digit:]]{3}",
        ];
        for pat in pats {
            let re = Regex::new(pat).unwrap_or_else(|e| panic!("{pat:?}: {e}"));
            for hay in hays {
                for at in 0..=hay.len() {
                    assert_eq!(
                        re.find_at(hay, at),
                        vm_find(&re, hay, at),
                        "{pat:?} on {:?} from {at}",
                        String::from_utf8_lossy(hay)
                    );
                }
                assert_eq!(
                    re.is_match(hay),
                    vm_find(&re, hay, 0).is_some(),
                    "{pat:?} is_match on {:?}",
                    String::from_utf8_lossy(hay)
                );
            }
        }
    }

    /// A generated corpus for the candidate scan: patterns from a small grammar, haystacks over a
    /// small alphabet, every answer compared span for span with the Pike VM. The scan is the one
    /// path that decides for itself how much of the haystack the automaton has to walk, so a
    /// pattern it reads wrong costs a match rather than time — and the grammar is drawn to reach
    /// it: a mandatory run it can jump between, an optional assertion branch, and a newline in
    /// the alphabet for the line bound it works from.
    #[test]
    fn the_candidate_scan_agrees_with_the_vm_on_generated_patterns() {
        // A tiny LCG, so the corpus is deterministic and needs no dependency.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut pick = move |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n
        };
        // A run of at least two bytes is what the scan jumps between, so every pattern starts
        // with one. `+` and `{2,3}` cannot match empty and `?` and `*` can, so the first atom is
        // never the one that makes the pattern able to match nothing — which is what a candidate
        // scan needs to be sound at all.
        let runs: [&str; 6] = ["ab", "b.", "aa", "A.", "_a", "ab\\.ba"];
        let mandatory: [&str; 3] = ["", "+", "{2,3}"];
        let atoms: [&str; 9] = ["a", "b", "A", ".", "[ab]", "[a-z]", "[A-Z]", "[0-9]", "0"];
        // A class that permits a newline, or an assertion, makes a candidate scan unsound, so
        // both are drawn rarely: they are here for the patterns that must not reach the scan.
        let others: [&str; 4] = ["[^b]", "\\s", "\\b", "^"];
        let quantifiers: [&str; 5] = ["", "+", "{2,3}", "?", "*"];
        let mut with_candidates = 0;
        let mut checked = 0;
        for _ in 0..500 {
            let alternation = pick(4) == 0;
            let mut pattern = String::new();
            if alternation {
                pattern.push_str("(?:");
            }
            for i in 0..1 + pick(3) {
                if i > 0 && alternation && pick(3) == 0 {
                    pattern.push('|');
                }
                let atom = match (i, pick(8)) {
                    (0, _) => runs[pick(runs.len())].to_string(),
                    (_, 0..=1) => runs[pick(runs.len())].to_string(),
                    (_, 7) => others[pick(others.len())].to_string(),
                    _ => atoms[pick(atoms.len())].to_string(),
                };
                pattern.push_str(&atom);
                pattern.push_str(if i == 0 {
                    mandatory[pick(mandatory.len())]
                } else {
                    quantifiers[pick(quantifiers.len())]
                });
            }
            if alternation {
                pattern.push(')');
            }
            let case_insensitive = pick(2) == 0;
            let flags = Flags {
                case_insensitive,
                ..Flags::default()
            };
            let Ok(re) = Regex::new_with(&pattern, flags) else {
                continue;
            };
            if re.cand.is_some() {
                with_candidates += 1;
            }
            for _ in 0..6 {
                let len = pick(40);
                let alphabet = b"abA._\n";
                let hay: Vec<u8> = (0..len).map(|_| alphabet[pick(alphabet.len())]).collect();
                for at in 0..=hay.len() {
                    assert_eq!(
                        re.find_at(&hay, at),
                        vm_find(&re, &hay, at),
                        "{pattern:?} (i={case_insensitive}) on {:?} from {at}",
                        String::from_utf8_lossy(&hay)
                    );
                    assert_eq!(
                        re.is_match(&hay[at..]),
                        vm_find(&re, &hay[at..], 0).is_some(),
                        "{pattern:?} is_match on {:?} from {at}",
                        String::from_utf8_lossy(&hay[at..])
                    );
                }
                checked += 1;
            }
        }
        // The corpus has to reach the path under test, not just agree with the VM by never
        // taking it.
        assert!(
            with_candidates > 120,
            "only {with_candidates} generated patterns had a candidate run"
        );
        assert!(checked > 2000, "only {checked} haystacks were compared");
    }

    #[test]
    fn dfa_agrees_with_the_vm_on_random_haystacks() {
        // A tiny LCG, so the corpus is deterministic and needs no dependency.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut byte = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u8
        };
        let alphabet = b"ab_ 01\n\t.xzAZ\xc3\xa9\xff";
        let pats = [
            r"\bvar\b",
            r"\Bab",
            "a.*b",
            "(?:ab|_)+",
            r"^[a-z]+$",
            r"\b\w+\b",
            "(?i)AZ",
            "[^a-z]+",
            "a{2,3}b?",
            r"\s\d",
            "x|y",
            r"[0-9]{2,4}",
            r"\.\w+$",
            "(?:a|b)*",
            r"\b",
        ];
        for _ in 0..120 {
            let len = (byte() % 24) as usize;
            let hay: Vec<u8> = (0..len)
                .map(|_| alphabet[(byte() as usize) % alphabet.len()])
                .collect();
            for pat in pats {
                let re = Regex::new(pat).unwrap();
                for at in 0..=hay.len() {
                    assert_eq!(
                        re.find_at(&hay, at),
                        vm_find(&re, &hay, at),
                        "{pat:?} on {:?} from {at}",
                        String::from_utf8_lossy(&hay)
                    );
                }
            }
        }
    }

    #[test]
    fn dfa_respects_multiline_flags() {
        let re = Regex::new_with(
            r"^b\w+$",
            Flags {
                multi_line: true,
                ..Flags::default()
            },
        )
        .unwrap();
        let hay = b"abc\nbcd\nx\n";
        assert_eq!(re.find(hay).map(|m| (m.start, m.end)), Some((4, 7)));
        assert_eq!(re.find_at(hay, 4), Some(Match { start: 4, end: 7 }));
        // `^` only holds at a line start, so nothing can begin at 5.
        assert_eq!(re.find_at(hay, 5), None);
        for at in 0..=hay.len() {
            assert_eq!(re.find_at(hay, at), vm_find(&re, hay, at), "from {at}");
        }
    }

    #[test]
    fn dfa_takes_the_leftmost_start_not_the_earliest_end() {
        // `b` ends first in "abc", but `abc` starts first and leftmost-first prefers it.
        assert_eq!(m("abc|b", "abc"), Some((0, 3)));
        assert_eq!(m("bc|abc", "abc"), Some((0, 3)));
        assert_eq!(m("a|ba", "ba"), Some((0, 2)));
        assert_eq!(m("ab|b", "ab"), Some((0, 2)));
        assert_eq!(m("c|abc", "xabc"), Some((1, 4)));
    }

    #[test]
    fn a_greedy_repeat_outgrows_the_empty_match_at_the_same_start() {
        // Both readings start at 0: the repeat consuming the line, and the empty match the
        // same repeat can make without consuming anything. Greedy prefers the first, so the
        // threads queued at a position must get their byte before an empty match counts.
        for (pat, hay, want) in [
            (".*", "abc", (0, 3)),
            ("a*", "aaa", (0, 3)),
            ("x*", "xa", (0, 1)),
            ("[a-z]*", "abc1", (0, 3)),
            (r"\w*", "abc def", (0, 3)),
            ("a*b", "aab", (0, 3)),
        ] {
            assert_eq!(m(pat, hay), Some(want), "{pat:?} on {hay:?}");
        }
        // The capture program runs the same step and must agree.
        let re = Regex::new("(.*)").unwrap();
        assert_eq!(
            re.captures_at(b"abc", 0),
            Some(vec![Some((0, 3)), Some((0, 3))]),
            "group 1 spans exactly the whole match"
        );
        // A lazy repeat still prefers the empty match, and the search resumes there.
        assert_eq!(m(".*?", "abc"), Some((0, 0)));
    }

    #[test]
    fn text_anchors_do_not_move_the_start_the_reverse_walk_finds() {
        // `.*\z` runs from 4 to the end of the haystack; the `\A` branch matches empty at 5.
        // Leftmost-first prefers the match that starts first, and the reverse walk has to
        // see `\z` where it holds — at the text end — to report that start at all.
        let re = Regex::new(r".*\z|\A").unwrap();
        assert_eq!(
            re.find_at(b"bc1bb", 4).map(|m| (m.start, m.end)),
            Some((4, 5))
        );
        let re = Regex::new(r"a{1,3}(\z|[^abc]{1,3}|[^abc]*){2}|(\A.+)").unwrap();
        assert_eq!(
            re.find(b"0\tabbX").map(|m| (m.start, m.end)),
            Some((0, 6)),
            "the (\\A.+) branch matches at offset 0"
        );
        // A match that ends at the text end keeps the DFA: the reverse walk answers, so the
        // cache is not abandoned for the rest of the thread.
        let re = Regex::new("abc$").unwrap();
        assert_eq!(re.find(b"xabc"), Some(Match { start: 1, end: 4 }));
        assert_eq!(re.find(b"xabc\n"), None);
        with_dfa(&re, |c| assert!(!c.off, "the reverse walk found the start")).unwrap();
    }

    /// The run a pattern begins with ends at the first part that is not a fixed string: `ne+dle`
    /// begins with "ne", and the `e+` after it may be one `e` or ten. A run computed through the
    /// repetition — "nedle" — is a string no match of `neeeeedle` contains, so a scan that jumps
    /// between its occurrences finds nothing at all.
    #[test]
    fn a_run_ends_where_a_repeated_part_begins() {
        let re = Regex::new("ne+dle").unwrap();
        assert_eq!(re.find(b"neeeeedle"), Some(Match { start: 0, end: 9 }));
        assert_eq!(
            re.find(b"a\nneeeeedle\nb"),
            Some(Match { start: 2, end: 11 }),
            "a match whose run starts a line is found like any other"
        );
        assert_eq!(re.find(b"nee dle"), None);
        // A run after a repetition of its own, and a pattern with no anchored run at all.
        assert_eq!(
            Regex::new("x+ne+dle").unwrap().find(b"xxneeeeedle"),
            Some(Match { start: 0, end: 11 })
        );
        assert_eq!(
            Regex::new(".*ne+dle").unwrap().find(b"ab neeeeedle"),
            Some(Match { start: 0, end: 12 })
        );
    }

    #[test]
    fn dfa_path_is_used() {
        let re = Regex::new("[0-9]+z").unwrap();
        assert!(re.literal().is_none());
        assert_eq!(re.find(b"ab12z"), Some(Match { start: 2, end: 5 }));
        let built = with_dfa(&re, |c| c.fwd.built_count).unwrap();
        assert!(built > 0, "the DFA built states for the scan");
    }

    #[test]
    fn dfa_gives_up_cleanly() {
        // Both ways the DFA can refuse: an abandoned cache, and the state cap.
        let re = Regex::new("(ab|cd)+e").unwrap();
        assert_eq!(re.find(b"xabcdabe"), Some(Match { start: 1, end: 8 }));
        with_dfa(&re, |c| c.off = true).unwrap();
        assert_eq!(re.find(b"xabcdabe"), Some(Match { start: 1, end: 8 }));
        assert!(!re.is_match(b"xabcdab"));

        let re = Regex::new("q[0-9]+z").unwrap();
        with_dfa(&re, |c| c.fwd.built_count = MAX_DFA_STATES).unwrap();
        assert_eq!(re.find(b"..q12z.."), Some(Match { start: 2, end: 6 }));
        assert_eq!(re.find(b"..q12y.."), None);
    }

    #[test]
    fn corpus_agrees_with_a_reference() {
        // Independent reference: expand the pattern's literal runs and require them in
        // order, anchored where the pattern is anchored. Catches ordering regressions.
        fn reference(pat: &str, hay: &[u8]) -> bool {
            let anchored = pat.starts_with('^');
            let anchored_end = pat.ends_with('$');
            let body = pat.trim_start_matches('^').trim_end_matches('$');
            let parts: Vec<&str> = body.split(".*").filter(|p| !p.is_empty()).collect();
            if parts.is_empty() {
                return true;
            }
            let mut at = 0usize;
            for (i, p) in parts.iter().enumerate() {
                let pb = p.as_bytes();
                let found = match crate::bytes::memmem_from(hay, pb, at) {
                    Some(x) => x,
                    None => return false,
                };
                if i == 0 && anchored && found != 0 {
                    return false;
                }
                at = found + pb.len();
            }
            if anchored_end {
                let last = parts.last().unwrap().as_bytes();
                return hay.ends_with(last) || at == hay.len();
            }
            true
        }
        let haystacks: [&[u8]; 5] = [
            b"a needle in a haystack",
            b"needle",
            b"",
            b"aaa bbb ccc",
            b"line one\nline two\nneedle here",
        ];
        for pat in [
            "needle",
            "^needle",
            "needle$",
            "a.*needle",
            ".*needle.*",
            "aaa.*ccc",
            "line.*two",
        ] {
            let re = Regex::new(pat).unwrap();
            for hay in haystacks {
                assert_eq!(re.is_match(hay), reference(pat, hay), "{pat:?} on {hay:?}");
            }
        }
    }
}

#[cfg(test)]
mod block_probe_tests {
    use super::*;

    #[test]
    fn the_forward_probe_answers_past_the_end() {
        // A block scan can ask about a position one past the last byte; the automaton has no
        // answer to give, and must say so rather than index out of bounds.
        let re = Regex::new("alpha|needle").unwrap();
        assert_eq!(re.accept_end(b"needle here", 0), Some(6));
        assert_eq!(re.accept_end(b"needle here", 7), None);
        assert_eq!(
            re.accept_end(b"needle here", b"needle here".len() + 1),
            None
        );
        assert_eq!(re.accept_end(b"", 0), None);
    }
}

#[cfg(test)]
mod bounded_repeat_tests {
    use super::*;

    fn spans(pat: &str, hay: &str) -> Vec<(usize, usize)> {
        let re = Regex::new(pat).unwrap();
        let mut out = Vec::new();
        let mut at = 0;
        while let Some(m) = re.find_at(hay.as_bytes(), at) {
            out.push((m.start, m.end));
            at = if m.end > m.start { m.end } else { m.end + 1 };
            if at > hay.len() {
                break;
            }
        }
        out
    }

    #[test]
    fn an_exact_repeat_stops_at_the_bound() {
        assert_eq!(spans("a{2}", "aaa"), vec![(0, 2)]);
        assert_eq!(spans("ab{2}c", "abbbc"), vec![]);
        assert_eq!(spans("ab{2}c", "abbc"), vec![(0, 4)]);
        // Leftmost-first: the longest allowed at 0 wins, and the search resumes past it.
        assert_eq!(spans("a{2,3}", "aaaa"), vec![(0, 3)]);
        assert_eq!(spans("a{2,}", "aaaa"), vec![(0, 4)]);
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    fn caps(pat: &str, hay: &str) -> Vec<Option<(usize, usize)>> {
        let re = Regex::new(pat).unwrap();
        re.captures_at(hay.as_bytes(), 0).unwrap()
    }

    #[test]
    fn groups_report_their_spans() {
        // The whole match is group 0, and each group follows in pattern order.
        assert_eq!(
            caps(r"(\w+)@(\w+)", "mail me at bob@host now"),
            vec![Some((11, 19)), Some((11, 14)), Some((15, 19))]
        );
        // A group that did not take part is absent, not empty.
        let got = caps(r"(a)|(b)", "zzb");
        assert_eq!(got[0], Some((2, 3)));
        assert_eq!(got[1], None);
        assert_eq!(got[2], Some((2, 3)));
        // A repeated group keeps its last iteration, as every engine does.
        assert_eq!(caps(r"(ab)+", "ababab")[1], Some((4, 6)));
        // Alternation, nesting and an empty group all report something usable.
        assert_eq!(
            caps(r"((\d{4})-(\d{2}))", "on 2026-11"),
            vec![Some((3, 10)), Some((3, 10)), Some((3, 7)), Some((8, 10))]
        );
        assert_eq!(caps(r"x()y", "xy")[1], Some((1, 1)));
    }

    #[test]
    fn captures_over_characters_are_bounded_by_bytes() {
        // The character program produces character offsets; they must come back as byte
        // offsets, or a replacement would slice a code point in half.
        let got = caps(r"(\p{L}+)—(\p{L}+)", "é—ü");
        assert_eq!(got[0], Some((0, "é—ü".len())));
        assert_eq!(got[1], Some((0, "é".len())));
        assert_eq!(got[2], Some(("é—".len(), "é—ü".len())));
    }

    #[test]
    fn a_character_program_miss_is_not_a_byte_program_question() {
        // `é` is one word character, so both positions of this one-character haystack are
        // word boundaries and `\B` cannot match. Its bytes `c3 a9` are both non-word, so the
        // byte program would report a non-boundary at 0; the character path's answer is
        // final, exactly as it is in `find_at`.
        let re = Regex::new(r"(\B)").unwrap();
        assert!(
            re.is_unicode(),
            "the character program is the one that answers"
        );
        assert_eq!(re.find("é".as_bytes()), None);
        assert_eq!(re.captures_at("é".as_bytes(), 0), None);
        // Offsets inside a character are read the way `find_at` reads them too, so the two
        // entry points agree on every offset of a text haystack.
        let hay = "aé".as_bytes();
        for at in 0..=hay.len() {
            assert_eq!(
                re.captures_at(hay, at).map(|c| c[0]),
                re.find_at(hay, at).map(|m| Some((m.start, m.end))),
                "captures_at and find_at from {at}"
            );
        }
    }

    #[test]
    fn a_pattern_without_groups_has_group_zero_only() {
        let re = Regex::new("needle").unwrap();
        assert_eq!(re.group_count(), 0);
        assert_eq!(
            re.captures_at(b"a needle here", 0).unwrap(),
            vec![Some((2, 8))]
        );
        assert!(re.captures_at(b"nothing", 0).is_none());
    }
}

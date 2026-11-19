//! A bounded backtracking matcher for the constructs the Pike VM cannot express.
//!
//! [`crate::regex`] is a Thompson NFA run as a Pike VM: linear time, but with no lookaround
//! and no backreferences. grep and ripgrep expose exactly those behind `-P`, so this module
//! implements that subset by backtracking. Backtracking is where pathological patterns blow
//! up, so every call runs under a step budget and reports exhaustion as an error instead of
//! a wrong answer.
//!
//! This engine is a fallback: callers reach it only when a pattern uses a construct the Pike
//! VM rejects, so its cost never touches the ordinary path. `Pcre::new` does not consult the
//! other engine and nothing here runs unless a caller built a `Pcre`.
//!
//! # Syntax
//!
//! ```text
//! literal bytes              any UTF-8 text; a character is matched as its UTF-8 bytes
//! .                          any byte except \n      (?s) makes it any byte
//! *  +  ?  {n}  {n,}  {n,m}  greedy, or lazy with a trailing ?
//! |                          alternation, leftmost-first (Perl) order
//! ( ... )  (?: ... )         capturing and non-capturing groups
//! (?<name> ... )  (?P<name> ... )   named capturing groups
//! (?= ... )  (?! ... )       lookahead, positive and negative
//! (?<= ... )  (?<! ... )     lookbehind, positive and negative
//! \1 .. \9  \k<name>  (?P=name)   backreferences; a fold of the captured bytes under (?i)
//! (?> ... )  a*+  a++  a?+  a{n,m}+   atomic group and possessive quantifiers
//! (?# ... )                  a comment
//! [abc]  [^abc]  [a-z]       byte classes, ranges, negation
//! [[:alpha:]] [^[:space:]]   POSIX bracket expressions inside a class
//! \d \D \w \W \s \S          ASCII digit / word / space classes and their negations
//! \b \B \A \z                word boundaries and text anchors
//! ^  $                       line anchors          (?m) makes them per-line anchors
//! \n \r \t \f \v \0 \xHH     escapes; any other \ + punctuation is that literal byte
//! (?i) (?s) (?m) (?-i) ...   inline flags, bare or scoped as (?i: ... )
//! ```
//!
//! Everything else is rejected at compile time with a message naming the offset: Unicode
//! properties (`\p{..}`), conditionals, subroutine calls, branch reset groups, and
//! `\Z`, `\G`, `\K`, `\R`, `\X`.
//!
//! An atomic group runs its body anchored at the current position, keeps the first match it
//! finds and never offers the outer search a shorter one. A possessive quantifier is exactly
//! that around the ordinary greedy or lazy loop, which is how PCRE defines it.
//!
//! # Semantics
//!
//! Matching is byte-oriented, like [`crate::regex`]: the haystack is `&[u8]`, `.` consumes one
//! byte, and a non-ASCII literal contributes the bytes of its UTF-8 encoding. Case folding is
//! ASCII-only, and a backreference folds the bytes it compares when `(?i)` is in force. `$`
//! matches at the end of the haystack or, with `(?m)`, before a `\n`; `\z` matches only at the
//! very end. An inline flag applies from its position to the end of the enclosing group.
//!
//! Alternation and repetition are leftmost-first, not POSIX longest: the search takes the
//! first start offset that can match at all, and within it the alternation and greedy or lazy
//! quantifiers decide as Perl does.
//!
//! A backreference to a group that has not been set by this point fails the match, as in
//! PCRE.
//!
//! ## Lookbehind
//!
//! A lookbehind body must have a bounded width of at most [`MAX_LOOKBEHIND`] bytes; a body
//! with unbounded width (`(?<=a*)`) or a backreference in it (`(?<=\1)`) is a compile error,
//! because the engine could not know where to start looking. A fixed-width body is tried
//! once. A variable-width body (`(?<=a|abc)`, `(?<=a{1,3})`) is tried at every start offset in
//! `pos - max ..= pos - min`, longest first, which is the greedy reading of the same pattern.
//! The body is matched against the haystack up to `pos`, so it must end exactly at `pos`.
//! Captures made inside a lookbehind are kept on success.
//!
//! ## Step budget
//!
//! Backtracking can be exponential, so one `Pcre` carries a step budget, set by
//! [`Pcre::with_budget`] and defaulting to [`DEFAULT_STEP_BUDGET`]. The budget counts VM
//! instructions executed, including those burned inside lookaround bodies, and it covers a
//! whole call: every start position tried by one [`Pcre::try_find_at`] shares it. Skipping
//! a start position whose first byte cannot match is a byte test outside the count, so
//! scanning a long haystack with a cheap pattern costs one test per position rather than one
//! step.
//!
//! When the budget runs out the call returns [`BudgetExceeded`], never `Ok(None)`: an
//! exhausted budget is an unknown answer, and reporting it as "no match" would be a wrong
//! one. [`Pcre::find_at`] keeps the `Option<Match>` shape for callers that only want to know
//! the bytes; it collapses exhaustion into `None` and is documented as unable to tell the two
//! apart, so anything that reports to a user uses `try_find_at`.

use std::collections::HashMap;
use std::fmt;

/// Steps one call to [`Pcre::try_find_at`] may execute before giving up.
pub const DEFAULT_STEP_BUDGET: u64 = 1_000_000;

/// Widest lookbehind body accepted, in bytes.
pub const MAX_LOOKBEHIND: usize = 1024;

/// Longest repetition bound accepted, so a pattern cannot explode the program.
const MAX_REPEAT: u32 = 1000;

/// Deepest group or lookaround nesting accepted.
const MAX_DEPTH: usize = 32;

/// Instruction cap across the main program and every lookaround body.
const MAX_PROGRAM: usize = 200_000;

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

/// The step budget ran out: the answer is unknown, not "no match".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetExceeded {
    /// Instructions executed when the budget ran out.
    pub steps: u64,
    /// The configured limit.
    pub budget: u64,
}

impl fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pattern exceeded its step budget ({} steps, limit {})",
            self.steps, self.budget
        )
    }
}

impl std::error::Error for BudgetExceeded {}

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

    fn is_empty(&self) -> bool {
        self.0 == [0; 4]
    }
}

impl fmt::Debug for ByteSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = String::new();
        for b in 0..=255u8 {
            if self.contains(b) {
                if (0x20..=0x7e).contains(&b) && b != b'\\' && b != b']' {
                    parts.push(b as char);
                } else {
                    parts.push_str(&format!("\\x{b:02x}"));
                }
            }
        }
        write!(f, "[{parts}]")
    }
}

fn or_set(into: &mut ByteSet, from: &ByteSet) {
    for i in 0..4 {
        into.0[i] |= from.0[i];
    }
}

fn perl_class(c: u8) -> ByteSet {
    let mut s = ByteSet::empty();
    match c.to_ascii_lowercase() {
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
    if c.is_ascii_uppercase() {
        s.negate();
    }
    s
}

fn all_but_newline() -> ByteSet {
    let mut s = ByteSet::full();
    let mut nl = ByteSet::empty();
    nl.add(b'\n');
    for i in 0..4 {
        s.0[i] &= !nl.0[i];
    }
    s
}

fn is_word(b: Option<u8>) -> bool {
    b.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Compile-time flags, also settable inline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Flags {
    case_insensitive: bool,
    multi_line: bool,
    dot_matches_newline: bool,
}

/// Zero-width assertions on the haystack position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Assert {
    StartText,
    EndText,
    StartLine,
    EndLine,
    WordBoundary,
    NotWordBoundary,
}

impl Assert {
    fn holds(self, hay: &[u8], at: usize) -> bool {
        match self {
            Assert::StartText => at == 0,
            Assert::EndText => at == hay.len(),
            Assert::StartLine => at == 0 || hay[at - 1] == b'\n',
            Assert::EndLine => at == hay.len() || hay[at] == b'\n',
            Assert::WordBoundary => {
                is_word(at.checked_sub(1).map(|i| hay[i])) != is_word(hay.get(at).copied())
            }
            Assert::NotWordBoundary => {
                is_word(at.checked_sub(1).map(|i| hay[i])) == is_word(hay.get(at).copied())
            }
        }
    }
}

/// The four lookaround kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LookKind {
    Ahead,
    AheadNot,
    Behind,
    BehindNot,
}

impl LookKind {
    fn behind(self) -> bool {
        matches!(self, LookKind::Behind | LookKind::BehindNot)
    }

    fn negated(self) -> bool {
        matches!(self, LookKind::AheadNot | LookKind::BehindNot)
    }
}

/// A lookaround instruction: its kind, body program and, for lookbehind, its width window.
#[derive(Clone, Copy, Debug)]
struct LookSpec {
    kind: LookKind,
    prog: usize,
    min: usize,
    max: usize,
}

// ---------------------------------------------------------------------------------------
// AST

enum Node {
    Empty,
    Set(ByteSet),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>, bool),
    Assert(Assert),
    Group(Box<Node>, usize),
    Look(LookKind, Box<Node>),
    /// A body that commits to its own first match: the outer search cannot backtrack into it.
    /// This is `(?>...)`, and it is also how a possessive quantifier is expressed.
    Atomic(Box<Node>),
    BackRef(usize, bool),
}

/// `(min, max)` byte width of a node; `max` is `None` when the node can consume unboundedly.
fn width(n: &Node) -> (u64, Option<u64>) {
    match n {
        Node::Empty | Node::Assert(_) | Node::Look(..) => (0, Some(0)),
        Node::Set(_) => (1, Some(1)),
        Node::Concat(parts) => {
            let mut min = 0u64;
            let mut max = Some(0u64);
            for p in parts {
                let (lo, hi) = width(p);
                min = min.saturating_add(lo);
                max = match (max, hi) {
                    (Some(a), Some(b)) => Some(a.saturating_add(b)),
                    _ => None,
                };
            }
            (min, max)
        }
        Node::Alt(branches) => {
            let mut min = u64::MAX;
            let mut max = Some(0u64);
            for b in branches {
                let (lo, hi) = width(b);
                min = min.min(lo);
                max = match (max, hi) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    _ => None,
                };
            }
            (if min == u64::MAX { 0 } else { min }, max)
        }
        Node::Repeat(inner, lo, hi, _) => {
            let (ilo, ihi) = width(inner);
            let min = ilo.saturating_mul(u64::from(*lo));
            let max = match ihi {
                Some(0) => Some(min),
                Some(b) => hi.map(|m| b.saturating_mul(u64::from(m))),
                None => None,
            };
            (min, max)
        }
        Node::Group(body, _) | Node::Atomic(body) => width(body),
        Node::BackRef(..) => (0, None),
    }
}

/// The possible first bytes of a node and whether it can match empty.
fn first_set(n: &Node) -> (ByteSet, bool) {
    match n {
        Node::Empty | Node::Assert(_) | Node::Look(..) | Node::BackRef(..) => {
            (ByteSet::empty(), true)
        }
        Node::Atomic(body) => first_set(body),
        Node::Set(s) => (*s, false),
        Node::Concat(parts) => {
            let mut acc = ByteSet::empty();
            for p in parts {
                let (s, nullable) = first_set(p);
                or_set(&mut acc, &s);
                if !nullable {
                    return (acc, false);
                }
            }
            (acc, true)
        }
        Node::Alt(branches) => {
            let mut acc = ByteSet::empty();
            let mut nullable = false;
            for b in branches {
                let (s, n2) = first_set(b);
                or_set(&mut acc, &s);
                nullable |= n2;
            }
            (acc, nullable)
        }
        Node::Group(body, _) => first_set(body),
        Node::Repeat(inner, min, _, _) => {
            let (s, nullable) = first_set(inner);
            (s, *min == 0 || nullable)
        }
    }
}

// ---------------------------------------------------------------------------------------
// Compiler

#[derive(Clone, Copy, Debug)]
enum Inst {
    /// Consume one byte from a set.
    Set(ByteSet),
    /// Continue at one of two program counters, preferring the first.
    Split(usize, usize),
    Jmp(usize),
    /// Record a capture endpoint: even slots open a group, odd slots close it.
    Save(usize),
    /// Record the position in a loop register.
    Mark(usize),
    /// Loop back edge: `if sp != regs[reg]` the iteration consumed something, so go to
    /// `again`; otherwise the loop body matched empty and the loop must end at `out`.
    Progress(usize, usize, usize),
    Assert(Assert),
    Look(LookSpec),
    /// Run a body program anchored here and continue at its own end, with no alternative
    /// left to the outer backtracking: `(?>...)` and the possessive quantifiers.
    Atomic(usize),
    /// Match the bytes captured by `group`, folding ASCII when the pattern asked for it.
    BackRef {
        group: usize,
        fold: bool,
    },
    Match,
}

struct Compiler {
    progs: Vec<Vec<Inst>>,
    regs: usize,
}

impl Compiler {
    fn emit(&mut self, prog: usize, node: &Node) -> Result<(), String> {
        match node {
            Node::Empty => {}
            Node::Set(s) => self.progs[prog].push(Inst::Set(*s)),
            Node::Concat(parts) => {
                for p in parts {
                    self.emit(prog, p)?;
                }
            }
            Node::Alt(branches) => {
                let mut jumps = Vec::new();
                for (i, b) in branches.iter().enumerate() {
                    if i + 1 == branches.len() {
                        self.emit(prog, b)?;
                    } else {
                        let split = self.progs[prog].len();
                        self.progs[prog].push(Inst::Split(0, 0));
                        let body = self.progs[prog].len();
                        self.emit(prog, b)?;
                        jumps.push(self.progs[prog].len());
                        self.progs[prog].push(Inst::Jmp(0));
                        self.progs[prog][split] = Inst::Split(body, self.progs[prog].len());
                    }
                }
                let end = self.progs[prog].len();
                for j in jumps {
                    self.progs[prog][j] = Inst::Jmp(end);
                }
            }
            Node::Repeat(inner, min, max, greedy) => {
                for _ in 0..*min {
                    self.emit(prog, inner)?;
                }
                match max {
                    None => {
                        let reg = self.regs;
                        self.regs += 1;
                        let split = self.progs[prog].len();
                        self.progs[prog].push(Inst::Split(0, 0));
                        let body = self.progs[prog].len();
                        self.progs[prog].push(Inst::Mark(reg));
                        self.emit(prog, inner)?;
                        let back = self.progs[prog].len();
                        self.progs[prog].push(Inst::Progress(reg, split, 0));
                        let out = self.progs[prog].len();
                        self.progs[prog][split] = if *greedy {
                            Inst::Split(body, out)
                        } else {
                            Inst::Split(out, body)
                        };
                        self.progs[prog][back] = Inst::Progress(reg, split, out);
                    }
                    Some(max) => {
                        let mut splits = Vec::new();
                        for _ in *min..*max {
                            let split = self.progs[prog].len();
                            self.progs[prog].push(Inst::Split(0, 0));
                            splits.push(split);
                            self.emit(prog, inner)?;
                        }
                        let out = self.progs[prog].len();
                        for split in splits {
                            self.progs[prog][split] = if *greedy {
                                Inst::Split(split + 1, out)
                            } else {
                                Inst::Split(out, split + 1)
                            };
                        }
                    }
                }
            }
            Node::Assert(a) => self.progs[prog].push(Inst::Assert(*a)),
            Node::Group(body, g) => {
                self.progs[prog].push(Inst::Save(2 * g));
                self.emit(prog, body)?;
                self.progs[prog].push(Inst::Save(2 * g + 1));
            }
            Node::Look(kind, body) => {
                // The parser has already rejected an unbounded lookbehind body, so the
                // width is known here for every kind that needs it.
                let (min, max) = if kind.behind() {
                    let (lo, hi) = width(body);
                    (lo as usize, hi.unwrap_or(lo) as usize)
                } else {
                    (0, 0)
                };
                let sub = self.progs.len();
                self.progs.push(Vec::new());
                self.emit(sub, body)?;
                self.progs[sub].push(Inst::Match);
                self.progs[prog].push(Inst::Look(LookSpec {
                    kind: *kind,
                    prog: sub,
                    min,
                    max,
                }));
            }
            Node::Atomic(body) => {
                let sub = self.progs.len();
                self.progs.push(Vec::new());
                self.emit(sub, body)?;
                self.progs[sub].push(Inst::Match);
                self.progs[prog].push(Inst::Atomic(sub));
            }
            Node::BackRef(group, fold) => self.progs[prog].push(Inst::BackRef {
                group: *group,
                fold: *fold,
            }),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Parser

struct Parser<'a> {
    src: &'a [u8],
    at: usize,
    flags: Flags,
    depth: usize,
    groups: usize,
    names: HashMap<String, usize>,
    /// A bare `(?i)` in the group just parsed set flags that outlive it.
    persist: bool,
}

enum ClassAtom {
    Byte(u8),
    Set(ByteSet),
}

impl Parser<'_> {
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
        Ok(if branches.len() == 1 {
            branches.pop().unwrap()
        } else {
            Node::Alt(branches)
        })
    }

    fn parse_concat(&mut self) -> Result<Node, String> {
        let mut parts: Vec<Node> = Vec::new();
        loop {
            match self.peek() {
                None | Some(b'|') | Some(b')') => break,
                _ => {}
            }
            let atom = self.parse_atom()?;
            parts.push(self.parse_quantifier(atom)?);
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
        if matches!(atom, Node::Assert(_) | Node::Look(..)) {
            return Err(self.err_at(
                "nothing to repeat: an assertion cannot be quantified",
                self.at - 1,
            ));
        }
        let greedy = if self.peek() == Some(b'?') {
            self.at += 1;
            false
        } else {
            true
        };
        let repeat = Node::Repeat(Box::new(atom), min, max, greedy);
        if self.peek() == Some(b'+') {
            self.at += 1;
            return Ok(Node::Atomic(Box::new(repeat)));
        }
        Ok(repeat)
    }

    /// `{n}`, `{n,}` or `{n,m}`; `Ok(None)` when the brace is not a quantifier (a literal).
    fn parse_braces(&mut self) -> Result<Option<(u32, Option<u32>)>, String> {
        let save = self.at;
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
        if digits.is_empty() {
            self.at = save;
            return Ok(None);
        }
        let mut max: Option<u32> = None;
        let mut open = false;
        if self.peek() == Some(b',') {
            open = true;
            self.at += 1;
            digits.clear();
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
        let mut head = String::new();
        for c in &self.src[save + 1..] {
            if c.is_ascii_digit() {
                head.push(*c as char);
            } else {
                break;
            }
        }
        let min: u32 = head
            .parse()
            .map_err(|_| self.err_at("bad repetition bound", save))?;
        if !open {
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
                Ok(Node::Set(if self.flags.dot_matches_newline {
                    ByteSet::full()
                } else {
                    all_but_newline()
                }))
            }
            b'*' | b'+' | b'?' => Err(self.err_at("nothing to repeat", start)),
            b'{' => match self.parse_braces()? {
                Some(_) => Err(self.err_at("nothing to repeat", start)),
                None => {
                    self.at += 1;
                    Ok(self.literal(b'{'))
                }
            },
            b')' => Err(self.err_at("unmatched ')'", start)),
            b']' => Err(self.err_at("unmatched ']'", start)),
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
                let mut buf = [0u8; 4];
                let bytes = ch.encode_utf8(&mut buf).as_bytes().to_vec();
                if bytes.len() == 1 {
                    Ok(self.literal(bytes[0]))
                } else {
                    Ok(Node::Concat(
                        bytes.iter().map(|b| Node::Set(single(*b))).collect(),
                    ))
                }
            }
        }
    }

    /// One literal byte, folded here when `(?i)` is in force.
    fn literal(&self, b: u8) -> Node {
        if self.flags.case_insensitive {
            Node::Set(single_folded(b))
        } else {
            Node::Set(single(b))
        }
    }

    fn parse_group(&mut self) -> Result<Node, String> {
        let open = self.at;
        self.at += 1;
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err_at(&format!("groups nested more than {MAX_DEPTH} deep"), open));
        }
        let saved = self.flags;
        self.persist = false;
        let body = self.parse_group_body(open);
        // A bare `(?i)` applies to the rest of this group, so this group must not restore;
        // it ends the mark either way, making the enclosing group restore its own flags.
        if !self.persist {
            self.flags = saved;
        }
        self.persist = false;
        self.depth -= 1;
        body
    }

    fn parse_group_body(&mut self, open: usize) -> Result<Node, String> {
        if self.peek() != Some(b'?') {
            self.groups += 1;
            let g = self.groups;
            let body = self.parse_alternation()?;
            self.expect_close(open)?;
            return Ok(Node::Group(Box::new(body), g));
        }
        match self.src.get(self.at + 1).copied() {
            Some(b':') => {
                self.at += 2;
                let body = self.parse_alternation()?;
                self.expect_close(open)?;
                Ok(body)
            }
            Some(b'=') => self.lookaround(open, LookKind::Ahead, 2),
            Some(b'!') => self.lookaround(open, LookKind::AheadNot, 2),
            Some(b'<') => match self.src.get(self.at + 2).copied() {
                Some(b'=') => self.lookaround(open, LookKind::Behind, 3),
                Some(b'!') => self.lookaround(open, LookKind::BehindNot, 3),
                Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                    let name_at = self.at + 2;
                    self.named_group(open, name_at)
                }
                _ => Err(self.err_at("bad (?< group", self.at)),
            },
            Some(b'P') => match self.src.get(self.at + 2).copied() {
                Some(b'<') => {
                    let name_at = self.at + 3;
                    match self.src.get(name_at).copied() {
                        Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                            self.named_group(open, name_at)
                        }
                        _ => Err(self.err_at("named group needs a name", name_at)),
                    }
                }
                Some(b'=') => {
                    let name_at = self.at + 3;
                    let mut at = name_at;
                    while let Some(c) = self.src.get(at) {
                        if c.is_ascii_alphanumeric() || *c == b'_' {
                            at += 1;
                        } else {
                            break;
                        }
                    }
                    if self.src.get(at) != Some(&b')') {
                        return Err(self.err_at("(?P=name) is not terminated by ')'", name_at));
                    }
                    let name = String::from_utf8_lossy(&self.src[name_at..at]).into_owned();
                    let Some(&g) = self.names.get(&name) else {
                        return Err(self.err_at(
                            &format!("backreference to undefined group {name:?}"),
                            name_at,
                        ));
                    };
                    self.at = at + 1;
                    Ok(Node::BackRef(g, self.flags.case_insensitive))
                }
                _ => Err(self.err_at("unknown (?P group", self.at)),
            },
            Some(b'i') | Some(b'm') | Some(b's') | Some(b'-') => {
                if self.parse_inline_flags()? {
                    let body = self.parse_alternation()?;
                    self.expect_close(open)?;
                    Ok(body)
                } else {
                    self.persist = true;
                    Ok(Node::Empty)
                }
            }
            Some(b'#') => {
                self.at += 2;
                while let Some(c) = self.src.get(self.at).copied() {
                    self.at += 1;
                    if c == b')' {
                        return Ok(Node::Empty);
                    }
                }
                Err(self.err_at("unterminated (?#...) comment", self.at))
            }
            Some(b'>') => {
                self.at += 2;
                let body = self.parse_alternation()?;
                self.expect_close(open)?;
                Ok(Node::Atomic(Box::new(body)))
            }
            Some(b'|') => {
                Err(self.err_at("branch reset groups (?|...) are not supported", self.at))
            }
            Some(b'(') => Err(self.err_at("conditional groups (?(...) are not supported", self.at)),
            Some(b'R') | Some(b'&') => {
                Err(self.err_at("subroutine calls are not supported", self.at))
            }
            Some(c) if c.is_ascii_digit() => {
                Err(self.err_at("subroutine calls are not supported", self.at))
            }
            _ => Err(self.err_at("unknown (? group", self.at)),
        }
    }

    fn expect_close(&mut self, open: usize) -> Result<(), String> {
        if self.peek() != Some(b')') {
            return Err(self.err_at("unterminated group", open));
        }
        self.at += 1;
        Ok(())
    }

    fn lookaround(&mut self, open: usize, kind: LookKind, skip: usize) -> Result<Node, String> {
        self.at += skip;
        let body = self.parse_alternation()?;
        self.expect_close(open)?;
        if kind.behind() {
            let (min, max) = width(&body);
            let Some(max) = max else {
                return Err(self.err_at(
                    "lookbehind body has no bounded length (a backreference has none)",
                    open,
                ));
            };
            if max > MAX_LOOKBEHIND as u64 || min > MAX_LOOKBEHIND as u64 {
                return Err(self.err_at(
                    &format!("lookbehind body is longer than {MAX_LOOKBEHIND} bytes"),
                    open,
                ));
            }
        }
        Ok(Node::Look(kind, Box::new(body)))
    }

    fn named_group(&mut self, open: usize, name_at: usize) -> Result<Node, String> {
        let mut at = name_at;
        while let Some(c) = self.src.get(at) {
            if c.is_ascii_alphanumeric() || *c == b'_' {
                at += 1;
            } else {
                break;
            }
        }
        if at == name_at {
            return Err(self.err_at("named group needs a name", name_at));
        }
        if self.src.get(at) != Some(&b'>') {
            return Err(self.err_at("unterminated group name (want '>')", name_at));
        }
        let name = String::from_utf8_lossy(&self.src[name_at..at]).into_owned();
        if self.names.contains_key(&name) {
            return Err(self.err_at(&format!("duplicate group name {name:?}"), name_at));
        }
        self.at = at + 1;
        self.groups += 1;
        let g = self.groups;
        self.names.insert(name, g);
        let body = self.parse_alternation()?;
        self.expect_close(open)?;
        Ok(Node::Group(Box::new(body), g))
    }

    /// `(?i)`, `(?ism)`, `(?-i)` at a group or pattern start. Returns true when the
    /// flags are scoped with `(?flags:` and a group body follows.
    fn parse_inline_flags(&mut self) -> Result<bool, String> {
        let start = self.at;
        self.at += 1; // '?'
        let mut negate = false;
        let mut seen = false;
        loop {
            match self.peek() {
                Some(b'-') => {
                    negate = true;
                    self.at += 1;
                }
                Some(b'i') => {
                    self.flags.case_insensitive = !negate;
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
                    if !seen && !negate {
                        return Err(self.err_at("empty inline flags", start));
                    }
                    return Ok(false);
                }
                Some(b':') => {
                    self.at += 1;
                    if !seen && !negate {
                        return Err(self.err_at("empty inline flags", start));
                    }
                    return Ok(true);
                }
                _ => return Err(self.err_at("bad inline flags", start)),
            }
        }
    }

    fn parse_escape(&mut self) -> Result<Node, String> {
        let start = self.at;
        self.at += 1;
        let c = self
            .peek()
            .ok_or_else(|| self.err_at("trailing backslash", start))?;
        match c {
            b'1'..=b'9' => {
                let mut n = 0usize;
                while let Some(d) = self.peek() {
                    if d.is_ascii_digit() {
                        n = n * 10 + usize::from(d - b'0');
                        self.at += 1;
                    } else {
                        break;
                    }
                }
                if n > 9 {
                    return Err(self.err_at("only \\1-\\9 backreferences are supported", start));
                }
                if n == 0 || n > self.groups {
                    return Err(self.err_at(
                        &format!("backreference \\{n} refers to a group that is not defined"),
                        start,
                    ));
                }
                Ok(Node::BackRef(n, self.flags.case_insensitive))
            }
            b'k' => {
                if self.src.get(self.at + 1) != Some(&b'<') {
                    return Err(self.err_at("\\k needs a group name as \\k<name>", start));
                }
                let name_at = self.at + 2;
                let mut at = name_at;
                while let Some(d) = self.src.get(at) {
                    if d.is_ascii_alphanumeric() || *d == b'_' {
                        at += 1;
                    } else {
                        break;
                    }
                }
                if self.src.get(at) != Some(&b'>') {
                    return Err(self.err_at("\\k<name> is not terminated by '>'", name_at));
                }
                let name = String::from_utf8_lossy(&self.src[name_at..at]).into_owned();
                let Some(&g) = self.names.get(&name) else {
                    return Err(
                        self.err_at(&format!("backreference to undefined group {name:?}"), start)
                    );
                };
                self.at = at + 1;
                Ok(Node::BackRef(g, self.flags.case_insensitive))
            }
            b'd' | b'D' | b'w' | b'W' | b's' | b'S' => {
                self.at += 1;
                Ok(Node::Set(perl_class(c)))
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
                Ok(Node::Assert(Assert::WordBoundary))
            }
            b'B' => {
                self.at += 1;
                Ok(Node::Assert(Assert::NotWordBoundary))
            }
            b'n' | b'r' | b't' | b'f' | b'v' | b'0' => {
                self.at += 1;
                Ok(self.literal(match c {
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'f' => 0x0c,
                    b'v' => 0x0b,
                    _ => 0,
                }))
            }
            b'x' => {
                let h = self
                    .src
                    .get(self.at + 1..self.at + 3)
                    .ok_or_else(|| self.err_at("\\x needs exactly two hex digits", start))?;
                let hex = std::str::from_utf8(h).map_err(|_| self.err_at("bad \\x", start))?;
                let b = u8::from_str_radix(hex, 16)
                    .map_err(|_| self.err_at("\\x needs exactly two hex digits", start))?;
                self.at += 3;
                Ok(self.literal(b))
            }
            b'p' | b'P' => Err(self.err_at(
                "Unicode property classes (\\p{..}) are not supported",
                start,
            )),
            b'Z' => Err(self.err_at("\\Z is not supported; use \\z", start)),
            b'G' | b'K' | b'R' | b'X' => Err(self.err_at(
                &format!("\\{} is not supported", char::from(c).to_ascii_uppercase()),
                start,
            )),
            c if c.is_ascii_alphanumeric() => Err(self.err_at("unknown escape", start)),
            c => {
                self.at += 1;
                Ok(self.literal(c))
            }
        }
    }

    /// `[:name:]` inside a class: the POSIX bracket expressions grep users write.
    fn posix_class(&mut self) -> Result<ByteSet, String> {
        let open = self.at;
        let rest = &self.src[self.at..];
        let Some(end) = rest.windows(2).position(|w| w == b":]") else {
            return Err(self.err_at("unterminated POSIX class", open));
        };
        let name = std::str::from_utf8(&rest[2..end]).unwrap_or("");
        self.at += end + 2;
        let mut set = ByteSet::empty();
        match name {
            "alnum" => {
                set.add_range(b'0', b'9');
                set.add_range(b'A', b'Z');
                set.add_range(b'a', b'z');
            }
            "alpha" => {
                set.add_range(b'A', b'Z');
                set.add_range(b'a', b'z');
            }
            "ascii" => set.add_range(0, 0x7f),
            "blank" => {
                set.add(b' ');
                set.add(b'\t');
            }
            "cntrl" => {
                set.add_range(0, 0x1f);
                set.add(0x7f);
            }
            "digit" => set.add_range(b'0', b'9'),
            "graph" => set.add_range(0x21, 0x7e),
            "lower" => set.add_range(b'a', b'z'),
            "print" => set.add_range(0x20, 0x7e),
            "punct" => {
                set.add_range(0x21, 0x2f);
                set.add_range(0x3a, 0x40);
                set.add_range(0x5b, 0x60);
                set.add_range(0x7b, 0x7e);
            }
            "space" => {
                set.add(b' ');
                set.add_range(0x09, 0x0d);
            }
            "upper" => set.add_range(b'A', b'Z'),
            "word" => {
                set.add_range(b'0', b'9');
                set.add_range(b'A', b'Z');
                set.add_range(b'a', b'z');
                set.add(b'_');
            }
            "xdigit" => {
                set.add_range(b'0', b'9');
                set.add_range(b'A', b'F');
                set.add_range(b'a', b'f');
            }
            _ => return Err(self.err_at(&format!("unknown POSIX class {name:?}"), open)),
        }
        Ok(set)
    }

    fn parse_class(&mut self) -> Result<Node, String> {
        let open = self.at;
        self.at += 1;
        let negated = if self.peek() == Some(b'^') {
            self.at += 1;
            true
        } else {
            false
        };
        let mut set = ByteSet::empty();
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
            match self.class_atom()? {
                ClassAtom::Set(s) => or_set(&mut set, &s),
                ClassAtom::Byte(lo) => {
                    if self.peek() == Some(b'-') && self.src.get(self.at + 1) != Some(&b']') {
                        self.at += 1;
                        match self.class_atom()? {
                            ClassAtom::Byte(hi) if hi >= lo => set.add_range(lo, hi),
                            ClassAtom::Byte(_) => {
                                return Err(
                                    self.err_at("character class range is backwards", self.at)
                                )
                            }
                            ClassAtom::Set(_) => {
                                return Err(self.err_at(
                                    "a character class cannot be a range endpoint",
                                    self.at,
                                ))
                            }
                        }
                    } else {
                        set.add(lo);
                    }
                }
            }
        }
        if set.is_empty() {
            return Err(self.err_at("empty character class", open));
        }
        if self.flags.case_insensitive {
            set.fold_ascii();
        }
        if negated {
            set.negate();
        }
        Ok(Node::Set(set))
    }

    fn class_atom(&mut self) -> Result<ClassAtom, String> {
        let start = self.at;
        let Some(c) = self.peek() else {
            return Err(self.err_at("unterminated character class", start));
        };
        if c == b'[' && self.src.get(start + 1) == Some(&b':') {
            return Ok(ClassAtom::Set(self.posix_class()?));
        }
        if c == b'\\' {
            self.at += 1;
            let e = self
                .peek()
                .ok_or_else(|| self.err_at("trailing backslash", start))?;
            self.at += 1;
            let b = match e {
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'f' => 0x0c,
                b'v' => 0x0b,
                b'0' => 0,
                // PCRE reads \b inside a class as backspace.
                b'b' => 0x08,
                b'd' | b'D' | b'w' | b'W' | b's' | b'S' => {
                    return Ok(ClassAtom::Set(perl_class(e)))
                }
                b'x' => {
                    let h = self
                        .src
                        .get(start + 2..start + 4)
                        .ok_or_else(|| self.err_at("\\x needs exactly two hex digits", start))?;
                    let hex = std::str::from_utf8(h).map_err(|_| self.err_at("bad \\x", start))?;
                    let b = u8::from_str_radix(hex, 16)
                        .map_err(|_| self.err_at("\\x needs exactly two hex digits", start))?;
                    self.at = start + 4;
                    b
                }
                e if e.is_ascii_alphanumeric() => {
                    return Err(self.err_at("unknown escape in class", start))
                }
                e => e,
            };
            return Ok(ClassAtom::Byte(b));
        }
        let ch = self.next_char()?;
        if !ch.is_ascii() {
            return Err(self.err_at(
                "non-ASCII class members are not supported; write the UTF-8 bytes as \\xHH",
                start,
            ));
        }
        Ok(ClassAtom::Byte(ch as u8))
    }

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
}

fn single(b: u8) -> ByteSet {
    let mut s = ByteSet::empty();
    s.add(b);
    s
}

fn single_folded(b: u8) -> ByteSet {
    let mut s = single(b);
    s.fold_ascii();
    s
}

// ---------------------------------------------------------------------------------------
// Matcher

/// A compiled pattern.
#[derive(Debug)]
pub struct Pcre {
    /// Program 0 is the whole pattern; every lookaround body is its own program.
    progs: Vec<Vec<Inst>>,
    /// Loop registers, one per unbounded quantifier.
    regs: usize,
    /// Capturing groups, so `group + 1` capture slots.
    groups: usize,
    budget: u64,
    /// Bytes a match can start with; `None` when the pattern can match empty and every
    /// position must be tried.
    first: Option<ByteSet>,
}

impl Pcre {
    /// Compile `pattern` with the default step budget.
    pub fn new(pattern: &str) -> Result<Pcre, String> {
        Pcre::with_budget(pattern, DEFAULT_STEP_BUDGET)
    }

    /// Compile `pattern` with an explicit step budget for each call.
    pub fn with_budget(pattern: &str, budget: u64) -> Result<Pcre, String> {
        let mut p = Parser {
            src: pattern.as_bytes(),
            at: 0,
            flags: Flags::default(),
            depth: 0,
            groups: 0,
            names: HashMap::new(),
            persist: false,
        };
        let node = p.parse_alternation()?;
        if p.at < p.src.len() {
            return Err(p.err_at("unexpected character", p.at));
        }
        let mut c = Compiler {
            progs: vec![Vec::new()],
            regs: 0,
        };
        c.emit(0, &node)?;
        c.progs[0].push(Inst::Match);
        let total: usize = c.progs.iter().map(|prog| prog.len()).sum();
        if total > MAX_PROGRAM {
            return Err(format!(
                "pattern compiles to {total} instructions, above the {MAX_PROGRAM} limit"
            ));
        }
        let (first, nullable) = first_set(&node);
        Ok(Pcre {
            progs: c.progs,
            regs: c.regs,
            groups: p.groups,
            budget,
            first: if nullable { None } else { Some(first) },
        })
    }

    /// The budget one call may spend.
    pub fn step_budget(&self) -> u64 {
        self.budget
    }

    /// Leftmost-first match anywhere in `hay`, treating budget exhaustion as no match.
    ///
    /// Use [`Pcre::try_find_at`] when the difference matters, which is anything that
    /// reports to a user.
    pub fn find(&self, hay: &[u8]) -> Option<Match> {
        self.find_at(hay, 0)
    }

    /// Leftmost-first match starting at or after `from`, treating budget exhaustion as no
    /// match. `from` past the end of `hay` yields `None`.
    pub fn find_at(&self, hay: &[u8], from: usize) -> Option<Match> {
        self.try_find_at(hay, from).ok().flatten()
    }

    /// Leftmost-first match anywhere in `hay`, with budget exhaustion reported.
    pub fn try_find(&self, hay: &[u8]) -> Result<Option<Match>, BudgetExceeded> {
        self.try_find_at(hay, 0)
    }

    /// Leftmost-first match at or after `from`, with budget exhaustion reported.
    ///
    /// The search tries every start offset from `from` in order and stops at the first that
    /// matches, so the match is the leftmost one and its extent is fixed by alternation and
    /// quantifier order inside the pattern. One budget covers the whole call.
    pub fn try_find_at(&self, hay: &[u8], from: usize) -> Result<Option<Match>, BudgetExceeded> {
        if from > hay.len() {
            return Ok(None);
        }
        let mut m = Machine {
            progs: &self.progs,
            hay,
            starts: vec![None; self.groups + 1],
            ends: vec![None; self.groups + 1],
            regs: vec![0; self.regs],
            undo: Vec::new(),
            steps: 0,
            budget: self.budget,
        };
        for start in from..=hay.len() {
            if let Some(set) = &self.first {
                match hay.get(start) {
                    Some(b) if set.contains(*b) => {}
                    _ => continue,
                }
            }
            match m.run(0, start, hay.len(), None) {
                RunResult::Match(end) => return Ok(Some(Match { start, end })),
                RunResult::NoMatch => {}
                RunResult::Budget => {
                    return Err(BudgetExceeded {
                        steps: m.steps,
                        budget: self.budget,
                    })
                }
            }
        }
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunResult {
    Match(usize),
    NoMatch,
    Budget,
}

enum Undo {
    Start(usize, Option<usize>),
    End(usize, Option<usize>),
    Reg(usize, usize),
}

#[derive(Clone, Copy)]
struct State {
    pc: usize,
    sp: usize,
    undo: usize,
}

struct Machine<'a> {
    progs: &'a [Vec<Inst>],
    hay: &'a [u8],
    starts: Vec<Option<usize>>,
    ends: Vec<Option<usize>>,
    regs: Vec<usize>,
    undo: Vec<Undo>,
    steps: u64,
    budget: u64,
}

impl Machine<'_> {
    fn set_start(&mut self, g: usize, v: Option<usize>) {
        self.undo.push(Undo::Start(g, self.starts[g]));
        self.starts[g] = v;
    }

    fn set_end(&mut self, g: usize, v: Option<usize>) {
        self.undo.push(Undo::End(g, self.ends[g]));
        self.ends[g] = v;
    }

    fn set_reg(&mut self, r: usize, v: usize) {
        self.undo.push(Undo::Reg(r, self.regs[r]));
        self.regs[r] = v;
    }

    fn rewind(&mut self, to: usize) {
        while self.undo.len() > to {
            match self.undo.pop() {
                Some(Undo::Start(g, v)) => self.starts[g] = v,
                Some(Undo::End(g, v)) => self.ends[g] = v,
                Some(Undo::Reg(r, v)) => self.regs[r] = v,
                None => break,
            }
        }
    }

    /// Backtracking VM: run `prog` from `from`, consuming no further than `lim`, and stop at
    /// the first success in priority order. `end_at` demands the match end exactly there,
    /// which is how a lookbehind body is held to its width. On success the captures the run
    /// made stay live; on failure or exhaustion they are rolled back.
    fn run(&mut self, prog: usize, from: usize, lim: usize, end_at: Option<usize>) -> RunResult {
        let base = self.undo.len();
        let mut stack: Vec<State> = Vec::new();
        let mut pc = 0usize;
        let mut sp = from;
        loop {
            self.steps += 1;
            if self.steps > self.budget {
                self.rewind(base);
                return RunResult::Budget;
            }
            let inst = self.progs[prog][pc];
            let mut next = true;
            match inst {
                Inst::Set(set) => {
                    if sp < lim && set.contains(self.hay[sp]) {
                        sp += 1;
                        pc += 1;
                    } else {
                        next = false;
                    }
                }
                Inst::Split(a, b) => {
                    stack.push(State {
                        pc: b,
                        sp,
                        undo: self.undo.len(),
                    });
                    pc = a;
                }
                Inst::Jmp(t) => pc = t,
                Inst::Save(slot) => {
                    let g = slot / 2;
                    if slot % 2 == 0 {
                        self.set_start(g, Some(sp));
                        self.set_end(g, None);
                    } else {
                        self.set_end(g, Some(sp));
                    }
                    pc += 1;
                }
                Inst::Mark(r) => {
                    self.set_reg(r, sp);
                    pc += 1;
                }
                Inst::Progress(r, again, out) => {
                    pc = if self.regs[r] == sp { out } else { again };
                }
                Inst::Assert(a) => {
                    if a.holds(self.hay, sp) {
                        pc += 1;
                    } else {
                        next = false;
                    }
                }
                Inst::Look(spec) => match self.check_look(spec, sp) {
                    Ok(true) => pc += 1,
                    Ok(false) => next = false,
                    Err(()) => {
                        self.rewind(base);
                        return RunResult::Budget;
                    }
                },
                Inst::Atomic(sub) => match self.run(sub, sp, lim, None) {
                    RunResult::Match(end) => {
                        sp = end;
                        pc += 1;
                    }
                    RunResult::NoMatch => next = false,
                    RunResult::Budget => {
                        self.rewind(base);
                        return RunResult::Budget;
                    }
                },
                Inst::BackRef { group, fold } => {
                    match (self.starts[group], self.ends[group]) {
                        (Some(s), Some(e)) if e >= s => {
                            let text = &self.hay[s..e];
                            if sp + text.len() <= lim && eq_at(self.hay, sp, text, fold) {
                                sp += text.len();
                                pc += 1;
                            } else {
                                next = false;
                            }
                        }
                        // A group that has not been set fails the match, as in PCRE.
                        _ => next = false,
                    }
                }
                Inst::Match => {
                    if end_at.is_none_or(|want| want == sp) {
                        return RunResult::Match(sp);
                    }
                    next = false;
                }
            }
            if !next {
                match stack.pop() {
                    Some(s) => {
                        self.rewind(s.undo);
                        pc = s.pc;
                        sp = s.sp;
                    }
                    None => {
                        self.rewind(base);
                        return RunResult::NoMatch;
                    }
                }
            }
        }
    }

    /// Evaluate one lookaround at `sp`, returning whether the assertion holds.
    fn check_look(&mut self, spec: LookSpec, sp: usize) -> Result<bool, ()> {
        if !spec.kind.behind() {
            let hit = self.sub_matched(spec.prog, sp, self.hay.len(), None)?;
            return Ok(hit != spec.kind.negated());
        }
        if spec.min > sp {
            // No candidate start offset at or after 0: the body cannot have matched here.
            return Ok(spec.kind.negated());
        }
        let lo = sp.saturating_sub(spec.max);
        let hi = sp - spec.min;
        for start in lo..=hi {
            if self.sub_matched(spec.prog, start, sp, Some(sp))? {
                return Ok(!spec.kind.negated());
            }
        }
        Ok(spec.kind.negated())
    }

    fn sub_matched(
        &mut self,
        prog: usize,
        from: usize,
        lim: usize,
        end_at: Option<usize>,
    ) -> Result<bool, ()> {
        match self.run(prog, from, lim, end_at) {
            RunResult::Match(_) => Ok(true),
            RunResult::NoMatch => Ok(false),
            RunResult::Budget => Err(()),
        }
    }
}

fn eq_at(hay: &[u8], at: usize, text: &[u8], fold: bool) -> bool {
    let got = &hay[at..at + text.len()];
    if fold {
        got.eq_ignore_ascii_case(text)
    } else {
        got == text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pat: &str, hay: &str) -> Option<(usize, usize)> {
        Pcre::new(pat)
            .unwrap_or_else(|e| panic!("{pat:?}: {e}"))
            .find(hay.as_bytes())
            .map(|m| (m.start, m.end))
    }

    fn none(pat: &str, hay: &str) {
        assert_eq!(m(pat, hay), None, "{pat:?} should not match {hay:?}");
    }

    fn err(pat: &str) -> String {
        match Pcre::new(pat) {
            Ok(_) => panic!("{pat:?} should not compile"),
            Err(e) => e,
        }
    }

    #[test]
    fn literals() {
        assert_eq!(m("needle", "a needle here"), Some((2, 8)));
        none("needle", "none");
        assert_eq!(m("", "abc"), Some((0, 0)));
        assert_eq!(m("caf\u{e9}", "le caf\u{e9} noir"), Some((3, 8)));
        none("x", "");
    }

    #[test]
    fn dot_and_dot_all() {
        assert_eq!(m("a.c", "abc"), Some((0, 3)));
        none("a.c", "a\nc");
        assert_eq!(m("(?s)a.c", "a\nc"), Some((0, 3)));
    }

    #[test]
    fn perl_classes() {
        assert_eq!(m(r"\d+", "ab 123 cd"), Some((3, 6)));
        assert_eq!(m(r"\D+", "12ab34"), Some((2, 4)));
        assert_eq!(m(r"\w+", "  foo_bar1 "), Some((2, 10)));
        none(r"\W", "abc");
        assert_eq!(m(r"a\sb", "a\tb"), Some((0, 3)));
        none(r"a\Sb", "a b");
        assert_eq!(m(r"a\Sb", "axb"), Some((0, 3)));
    }

    #[test]
    fn word_boundaries() {
        assert_eq!(m(r"\bcat\b", "the cat sat"), Some((4, 7)));
        none(r"\bcat\b", "concatenate");
        assert_eq!(m(r"\Bcat", "concatenate"), Some((3, 6)));
        none(r"\Bcat", "cat here");
    }

    #[test]
    fn text_anchors() {
        assert_eq!(m(r"\Aabc", "abcdef"), Some((0, 3)));
        none(r"\Aabc", "xabc");
        assert_eq!(m(r"abc\z", "xabc"), Some((1, 4)));
        none(r"abc\z", "abcx");
        // `$` at the end of the haystack only, without (?m).
        none(r"abc$", "abc\n");
    }

    #[test]
    fn line_anchors_with_multiline() {
        assert_eq!(m("(?m)^b", "a\nb\nc"), Some((2, 3)));
        assert_eq!(m("(?m)a$", "a\nb"), Some((0, 1)));
        none("(?m)^b", "a b");
    }

    #[test]
    fn classes() {
        assert_eq!(m("[0-9]{2,3}", "x 1234"), Some((2, 5)));
        assert_eq!(m("[^a-z]+", "abXY"), Some((2, 4)));
        assert_eq!(m(r"[\d_]+", "ab_12 "), Some((2, 5)));
        assert_eq!(m(r"[\]\[]", "x]y"), Some((1, 2)));
        assert_eq!(m("[]]", "x]y"), Some((1, 2)));
        assert_eq!(
            m("[a-c-e]", "xb"),
            Some((1, 2)),
            "a dash after a range is a literal"
        );
    }

    #[test]
    fn posix_classes() {
        assert_eq!(m("[[:alpha:]]+", "12 ab"), Some((3, 5)));
        assert_eq!(m("[[:digit:]]{3}", "ab123"), Some((2, 5)));
        assert_eq!(m("[^[:space:]]+", "  hi  "), Some((2, 4)));
        assert_eq!(m("[[:xdigit:]]+", "zzbeef"), Some((2, 6)));
        assert!(err("[[:bogus:]]").contains("unknown POSIX class"));
    }

    #[test]
    fn groups_and_alternation() {
        assert_eq!(m("(?:ab)+", "ababx"), Some((0, 4)));
        assert_eq!(m("(a|b)c", "zzbc"), Some((2, 4)));
        assert_eq!(m("a|ab", "ab"), Some((0, 1)), "leftmost-first picks a");
        assert_eq!(m("ab|a", "ab"), Some((0, 2)), "leftmost-first picks ab");
    }

    #[test]
    fn quantifiers() {
        assert_eq!(m("ab*", "abbbc"), Some((0, 4)));
        assert_eq!(m("ab+", "ac"), None);
        assert_eq!(m("ab?c", "ac"), Some((0, 2)));
        assert_eq!(m("a{2,3}", "aaaa"), Some((0, 3)));
        assert_eq!(m("a{2,}", "aaaaa"), Some((0, 5)));
        assert_eq!(m("a{2}", "aaa"), Some((0, 2)));
        assert_eq!(m("a{0}b", "b"), Some((0, 1)));
    }

    #[test]
    fn lazy_quantifiers() {
        assert_eq!(m("a{2,3}?", "aaaa"), Some((0, 2)));
        assert_eq!(m("<.+?>", "<a><b>"), Some((0, 3)));
        assert_eq!(m("<.+>", "<a><b>"), Some((0, 6)));
        assert_eq!(m("a+?b", "aaab"), Some((0, 4)));
    }

    #[test]
    fn empty_loop_terminates() {
        assert_eq!(m("(a*)*b", "aaab"), Some((0, 4)));
        assert_eq!(m("(?:)*x", "x"), Some((0, 1)));
        assert_eq!(m("(a?)*b", "b"), Some((0, 1)));
    }

    #[test]
    fn inline_flags() {
        assert_eq!(m("(?i)NEEDLE", "a needle"), Some((2, 8)));
        assert_eq!(
            m("(?i:NEEDLE)x", "needle"),
            None,
            "scoped flags end at the group"
        );
        assert_eq!(m("(?i:NEEDLE)", "needle"), Some((0, 6)));
        assert_eq!(m("a(?i)b", "aB"), Some((0, 2)));
        none("a(?i)b", "AB");
        assert_eq!(m("(?i)[a-c]+", "xABC"), Some((1, 4)), "classes fold too");
    }

    #[test]
    fn lookahead_positive() {
        assert_eq!(m("foo(?=bar)", "foobar"), Some((0, 3)));
        none("foo(?=bar)", "foobaz");
        assert_eq!(m("(?=abc)a", "xabc"), Some((1, 2)));
    }

    #[test]
    fn lookahead_negative() {
        assert_eq!(m("foo(?!bar)", "foobaz"), Some((0, 3)));
        none("foo(?!bar)", "foobar");
        assert_eq!(m(r"\d+(?!\d)", "12ab"), Some((0, 2)));
    }

    #[test]
    fn lookbehind_fixed() {
        assert_eq!(m("(?<=ab)c", "abc"), Some((2, 3)));
        none("(?<=ab)c", "xbc");
        assert_eq!(m(r"(?<=\d\d)x", "12x"), Some((2, 3)));
        none(r"(?<=\d\d)x", "1x");
        assert_eq!(m("(?<=\\Aab)c", "abc"), Some((2, 3)));
    }

    #[test]
    fn lookbehind_at_start_fails() {
        none("(?<=a)b", "b");
        assert_eq!(m("(?<!a)b", "b"), Some((0, 1)));
    }

    #[test]
    fn lookbehind_negative() {
        assert_eq!(m("(?<!ab)c", "xc"), Some((1, 2)));
        none("(?<!ab)c", "abc");
        assert_eq!(m("(?<!\\d)x", "ax"), Some((1, 2)));
    }

    #[test]
    fn lookbehind_variable_length() {
        // Alternation of widths 1 and 3, matched longest-first.
        assert_eq!(m("(?<=a|abc)d", "abcd"), Some((3, 4)));
        assert_eq!(m("(?<=a|abc)d", "xad"), Some((2, 3)));
        assert_eq!(m("(?<=a{1,3})z", "aaaz"), Some((3, 4)));
        // Greedy prefer the longest lookbehind body: captures inside it survive.
        assert_eq!(m("(?<=a{1,3})b", "aaab"), Some((3, 4)));
    }

    #[test]
    fn nested_lookaround() {
        assert_eq!(m("a(?=b(?!c))", "abd"), Some((0, 1)));
        none("a(?=b(?!c))", "abc");
        assert_eq!(m("(?=a(?=bc))ab", "abc"), Some((0, 2)));
        assert_eq!(m("(?<=a(?=b))b", "ab"), Some((1, 2)));
    }

    #[test]
    fn lookaround_with_captures_inside() {
        // A capture set inside a positive lookahead stays available to a later backref.
        assert_eq!(m("(?=(a+))\\1", "aaaa"), Some((0, 4)));
        assert_eq!(m("(?=(ab))a", "ab"), Some((0, 1)));
        none("(?=(ab))b", "ab");
    }

    #[test]
    fn backreference_equality() {
        assert_eq!(m(r"(\w+) \1", "hi hi"), Some((0, 5)));
        none(r"(\w+) \1", "hi ho");
        assert_eq!(m(r"(\w+) \1", "xx hi hi yy"), Some((3, 8)));
        assert_eq!(m(r"(a)(b)\2\1", "abba"), Some((0, 4)));
        none(r"(a)(b)\2\1", "abab");
    }

    #[test]
    fn backreference_unset_fails() {
        none(r"(a)|b\1", "b");
        assert_eq!(m(r"(a)|b\1", "aa"), Some((0, 1)));
    }

    #[test]
    fn backreference_folds_under_case_insensitive() {
        assert_eq!(m(r"(?i)(\w+) \1", "Hi hI"), Some((0, 5)));
        none(r"(\w+) \1", "Hi hI");
    }

    #[test]
    fn named_groups_and_named_backreferences() {
        assert_eq!(m(r"(?<word>\w+) \k<word>", "go go"), Some((0, 5)));
        assert_eq!(m(r"(?P<w>\w+)-\k<w>", "x-y"), None);
        assert_eq!(m(r"(?P<w>\w+)-\k<w>", "ab-ab"), Some((0, 5)));
        none(r"(?<w>\w+)\s\k<w>", "ab cd");
    }

    #[test]
    fn named_group_counts_as_a_group() {
        // A named group is capture 1, so `\1` can refer to it.
        assert_eq!(m(r"(?<w>\w+) \1", "ok ok"), Some((0, 5)));
    }

    #[test]
    fn find_at_honours_the_offset() {
        let re = Pcre::new("ab").unwrap();
        let hay = b"ab ab ab";
        assert_eq!(re.find_at(hay, 0), Some(Match { start: 0, end: 2 }));
        assert_eq!(re.find_at(hay, 1), Some(Match { start: 3, end: 5 }));
        assert_eq!(re.find_at(hay, 4), Some(Match { start: 6, end: 8 }));
        assert_eq!(re.find_at(hay, 8), None);
        assert_eq!(re.find_at(hay, 99), None, "past the end is no match");
    }

    #[test]
    fn empty_match_positions() {
        let re = Pcre::new("(?=b)").unwrap();
        assert_eq!(re.find_at(b"ab", 0), Some(Match { start: 1, end: 1 }));
        assert_eq!(re.find_at(b"ab", 2), None);
    }

    #[test]
    fn leftmost_start_wins_over_longer_later_match() {
        // The second match is longer but starts later, so leftmost-first takes the first.
        assert_eq!(m("b+|ab+", "abbb"), Some((0, 4)));
        assert_eq!(m("a|ab+", "abbb"), Some((0, 1)));
    }

    #[test]
    fn atomic_groups_commit_to_their_first_match() {
        // `a|ab` prefers `a`, so the atomic group commits to it and `c` has nothing to
        // consume; the ordinary group backtracks into `ab` and matches.
        none(r"(?>a|ab)c", "abc");
        assert_eq!(m(r"(a|ab)c", "abc"), Some((0, 3)));
        // A possessive star cannot give a byte back, so the trailing `a` is unreachable.
        none(r"a*+a", "aaa");
        assert_eq!(m(r"a*a", "aaa"), Some((0, 3)));
        // A possessive run leaves nothing for the digit that follows it, and the search
        // cannot recover by retrying later: every start runs out the same way.
        none(r"\d++\d", "123");
        assert_eq!(m(r"\d++x\d", "123x4"), Some((0, 5)));
        assert_eq!(m(r"a?+b", "ab"), Some((0, 2)));
        assert_eq!(m(r"a{2,3}+a", "aaa"), None);
        assert_eq!(m(r"a{2,3}+a", "aaaa"), Some((0, 4)));
    }

    #[test]
    fn atomic_groups_keep_captures() {
        // The capture made inside the committed body stays visible to a backreference.
        assert_eq!(m(r"(?>(a+))b\1", "aaabaaa"), Some((0, 7)));
        none(r"(?>(a+))b\1", "aaab");
        assert_eq!(m(r"(?>(a+))c", "aaab"), None);
    }

    #[test]
    fn atomic_groups_stop_a_catastrophe() {
        // `(a+)+$` against a non-matching tail is exponential without the atomic commit;
        // with one it is linear, so a small budget is enough.
        let re = Pcre::with_budget(r"(?>(a+))+$", 100_000).unwrap();
        assert_eq!(re.try_find(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaab"), Ok(None));
    }

    #[test]
    fn comments_are_ignored() {
        assert_eq!(m("a(?#this is a comment)b", "ab"), Some((0, 2)));
        assert_eq!(
            m("(?#whole pattern is a comment)", "anything"),
            Some((0, 0))
        );
        assert_eq!(m("a(?#)b", "ab"), Some((0, 2)));
        assert!(err("a(?#unterminated").contains("comment"));
    }

    #[test]
    fn python_named_backreferences() {
        assert_eq!(m(r"(?P<w>\w+) (?P=w)", "go go"), Some((0, 5)));
        none(r"(?P<w>\w+) (?P=w)", "go up");
        assert_eq!(m(r"(?i)(?P<w>ab) (?P=w)", "ab AB"), Some((0, 5)));
        assert!(err(r"(?P=nope)x").contains("undefined group"));
        assert!(err(r"(?P<w>a)(?P=w").contains("terminated"));
    }

    #[test]
    fn budget_error_is_distinguishable() {
        let re = Pcre::with_budget("(a+)+$", 20_000).unwrap();
        let hay = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaab";
        let got = re.try_find_at(hay, 0);
        assert!(got.is_err(), "expected a budget error, got {got:?}");
        let e = got.unwrap_err();
        assert_eq!(e.budget, 20_000);
        assert!(e.steps > e.budget);
        assert!(e.to_string().contains("step budget"));
        // The infallible shape cannot tell the two apart; it reports no match.
        assert_eq!(re.find_at(hay, 0), None);
    }

    #[test]
    fn a_larger_budget_finishes_the_same_pattern() {
        let pat = "(a+)+$";
        // The trailing `b` makes every split fail, which is the catastrophic path.
        let hay = "aaaaaaaaaab";
        let small = Pcre::with_budget(pat, 50).unwrap();
        assert!(small.try_find(hay.as_bytes()).is_err());
        let big = Pcre::with_budget(pat, 5_000_000).unwrap();
        assert_eq!(big.try_find(hay.as_bytes()), Ok(None));
        // The same pattern on a matching haystack is cheap at any budget above its cost.
        assert_eq!(
            big.try_find(b"aaaaaaaaaa"),
            Ok(Some(Match { start: 0, end: 10 }))
        );
    }

    #[test]
    fn budget_exhaustion_inside_a_lookaround_is_reported() {
        let re = Pcre::with_budget("(?=(a+)+b)a", 5_000).unwrap();
        let hay = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(re.try_find_at(hay, 0).is_err());
    }

    #[test]
    fn step_budget_is_readable() {
        let re = Pcre::with_budget("x", 12345).unwrap();
        assert_eq!(re.step_budget(), 12345);
        assert_eq!(Pcre::new("x").unwrap().step_budget(), DEFAULT_STEP_BUDGET);
    }

    #[test]
    fn compile_error_offsets_are_reported() {
        assert!(err("[a-").contains("offset"));
        let e = err(r"a\p{L}");
        assert!(e.contains("Unicode property"), "{e}");
        assert!(e.contains("offset 1"), "{e}");
        assert!(err("(a").contains("unterminated group"));
        assert!(err("a)").contains("unexpected character"));
        assert!(err("*a").contains("nothing to repeat"));
        assert!(err(r"(?=a)*").contains("nothing to repeat"));
        assert!(err("[z-a]").contains("backwards"));
        assert!(err("a{3,2}").contains("empty"));
        assert!(err("a{1001}").contains("above 1000"));
    }

    #[test]
    fn unsupported_constructs_are_rejected_with_a_reason() {
        assert!(err("(?|a|b)").contains("branch reset"));
        assert!(err(r"\Z").contains("use \\z"));
        assert!(err(r"\G").contains("not supported"));
        assert!(err(r"(?R)").contains("subroutine"));
        assert!(err(r"\1").contains("not defined"));
        assert!(err(r"(?<a>x)\k<a").contains("terminated"));
        assert!(err(r"\k<nope>").contains("undefined group"));
        assert!(err(r"(?<a>x)(?<a>y)").contains("duplicate group name"));
        assert!(err(r"\10").contains("only \\1-\\9"));
        assert!(err("(?bogus)").contains("unknown (? group"));
    }

    #[test]
    fn lookbehind_limits_are_enforced() {
        let e = err("(?<=a*)b");
        assert!(e.contains("no bounded length"), "{e}");
        assert!(e.contains("offset 0"), "{e}");
        let e = err(r"(?<=(\w+)\1)a");
        assert!(e.contains("no bounded length"), "{e}");
        let e = err("(?<=(?:ab){600})b");
        assert!(e.contains("longer than"), "{e}");
    }

    #[test]
    fn capture_groups_reset_across_iterations() {
        // Each iteration overwrites the capture, so \1 is the last one matched.
        assert_eq!(m(r"(a|b)+\1", "abb"), Some((0, 3)));
        assert_eq!(m(r"(ab)+c\1", "ababcab"), Some((0, 7)));
        none(r"(ab)+c\1", "ababcx");
    }

    #[test]
    fn multiline_with_lookaround() {
        assert_eq!(m("(?m)^x(?=y)", "ax\nxy"), Some((3, 4)));
        assert_eq!(m("(?m)(?<=x)y$", "xy\nab"), Some((1, 2)));
    }

    #[test]
    fn backreference_inside_lookahead() {
        assert_eq!(m(r"(a)(?=\1)b", "aab"), None);
        assert_eq!(m(r"(a)(?=\1)a", "aa"), Some((0, 2)));
    }

    #[test]
    fn lookbehind_variable_with_capture_reuse() {
        // The lookbehind matches at offset 1 already, so leftmost-first stops there.
        assert_eq!(m(r"(?<=(x|xy))y", "xyy"), Some((1, 2)));
    }

    #[test]
    fn class_with_dash_endpoints() {
        assert_eq!(m("[a-]+", "-a-"), Some((0, 3)));
        assert!(err("[a-\\d]").contains("range endpoint"));
        assert!(err("[é]").contains("non-ASCII class members"));
    }

    #[test]
    fn noncapturing_groups_do_not_shift_numbers() {
        assert_eq!(m(r"(?:x)(a)\1", "xaa"), Some((0, 3)));
        assert_eq!(m(r"(?<n>a)(?:y)\1", "aya"), Some((0, 3)));
    }

    #[test]
    fn empty_alternation_branch() {
        assert_eq!(m("a|", "zz"), Some((0, 0)));
        assert_eq!(m("(?:)|x", "x"), Some((0, 0)));
    }

    #[test]
    fn lookahead_only_pattern_is_zero_width() {
        assert_eq!(m("(?=needle)", "a needle"), Some((2, 2)));
        // At offset 1 the negative lookahead holds, so the empty match is there.
        assert_eq!(m("(?!needle)", "needle"), Some((1, 1)));
    }

    #[test]
    fn byte_oriented_dot_on_utf8() {
        // `.` consumes one byte, matching the documented byte semantics.
        assert_eq!(m("^caf.$", "caf\u{e9}"), None);
        assert_eq!(m("^caf..$", "caf\u{e9}"), Some((0, 5)));
    }

    #[test]
    fn repeat_bounds_are_copied_correctly() {
        assert_eq!(m("(ab){2,3}", "ababab"), Some((0, 6)));
        assert_eq!(m("(ab){2,3}?", "ababab"), Some((0, 4)));
        none("(ab){4}", "ababab");
    }

    #[test]
    fn lookbehind_inside_lookahead_and_vice_versa() {
        assert_eq!(m("(?=(?<=a)b)", "ab"), Some((1, 1)));
        none("(?=(?<!a)b)", "ab");
        none("(?<=(?=b))a", "ab");
        assert_eq!(m("(?<=(?=a))a", "a"), Some((0, 1)));
    }

    #[test]
    fn quantified_group_with_backref() {
        // Greedy takes all three, then backtracks until \1 can repeat the last byte.
        assert_eq!(m(r"(\w)*\1", "abb"), Some((0, 3)));
        assert_eq!(m(r"(ss)+", "ssss"), Some((0, 4)));
    }

    #[test]
    fn hash_and_brace_literals() {
        assert_eq!(m("a{b", "a{b"), Some((0, 3)));
        assert_eq!(m("#x", "y#x"), Some((1, 3)));
    }

    #[test]
    fn newline_escapes() {
        assert_eq!(m(r"a\r\nb", "a\r\nb"), Some((0, 4)));
        assert_eq!(m(r"a\x41", "aA"), Some((0, 2)));
        assert_eq!(m("\u{7}", "\u{7}"), Some((0, 1)));
    }

    #[test]
    fn max_depth_is_enforced() {
        let deep = format!("{}a{}", "(".repeat(40), ")".repeat(40));
        assert!(err(&deep).contains("nested more than"));
        let ok = format!("{}a{}", "(".repeat(30), ")".repeat(30));
        assert_eq!(m(&ok, "a"), Some((0, 1)));
    }

    #[test]
    fn matches_regex_engine_on_shared_syntax() {
        // The two engines must agree where their grammars overlap.
        for (pat, hay) in [
            ("needle", "a needle here"),
            (r"[0-9]{2,3}", "x 1234"),
            (r"\bcat\b", "the cat sat"),
            ("(?:ab)+", "xabab"),
            ("(?i)NEEDLE", "needle"),
            ("a|ab", "ab"),
            (r"\d+", "ab 123"),
            ("(?m)^b", "a\nb"),
        ] {
            let want = crate::regex::Regex::new(pat)
                .unwrap()
                .find(hay.as_bytes())
                .map(|m| (m.start, m.end));
            assert_eq!(m(pat, hay), want, "{pat:?} on {hay:?}");
        }
    }

    #[test]
    fn malformed_patterns_report_errors_instead_of_panicking() {
        let odd = [
            "",
            "(",
            ")",
            "[",
            "[]",
            "[^",
            "\\",
            "a{",
            "a{2",
            "a{}",
            "{2}",
            "?",
            "*",
            "+",
            "(?",
            "(?<",
            "(?<=",
            "(?<!",
            "(?<x",
            "\\k",
            "\\k<",
            "\\1",
            "(?i",
            "(?i-",
            "a|*",
            "[[:",
            "[[:alpha",
            "((((((",
            "))))",
            "(?s)a{1000}{1000}",
        ];
        for pat in odd {
            if let Ok(re) = Pcre::new(pat) {
                let _ = re.try_find(b"abc 123");
                let _ = re.find_at(b"\x00\xff", 1);
            }
        }
    }

    #[test]
    fn prefilter_never_hides_a_match() {
        // Every start byte the pattern can match must survive the first-byte filter, so
        // sweeping a haystack must find the same match as a full scan.
        let re = Pcre::new(r"(?i)\bneedle\b").unwrap();
        let hay = b"hay needle NEEDLE";
        assert_eq!(re.find_at(hay, 0), Some(Match { start: 4, end: 10 }));
        assert_eq!(re.find_at(hay, 5), Some(Match { start: 11, end: 17 }));
    }
}

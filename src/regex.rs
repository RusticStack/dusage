//! A small, linear-time regular-expression engine (std only).
//!
//! Enough of the syntax to power a grep/rg replacement, compiled to a Thompson NFA and
//! executed by a Pike VM: no backtracking, so `(a+)+$` cannot blow up.
//!
//! # Syntax
//!
//! ```text
//! literal bytes        any UTF-8 text; a character is matched as its UTF-8 bytes
//! .                    any byte except \n      (?s) makes it any byte
//! *  +  ?  {n}  {n,}  {n,m}                     greedy, or lazy with a trailing ?
//! |                    alternation, leftmost-first (Perl) preference
//! ( ... )  (?: ... )   capturing and non-capturing groups (captures are not reported)
//! [abc]  [^abc]  [a-z]  [\d_]  [\]]            byte classes
//! ^  $  \A  \z  \b  \B  anchors and word boundaries
//! \d \D \w \W \s \S    ASCII digit / word / space classes and their negations
//! \n \r \t \f \v \0 \xHH   escapes; \. matches a literal dot
//! (?i) (?s) (?m) (?-i) (?ism)   inline flags, at the pattern start or a group start
//! ```
//!
//! # Semantics
//!
//! Matching is byte-oriented: the haystack is `&[u8]` and every atom consumes bytes, so
//! `.` matches one byte of a multi-byte character. `^` and `$` anchor at the ends of the
//! haystack, or at line boundaries with `(?m)`/`multi_line`. `$` matches only at the end of
//! the haystack (or before a `\n` when multi-line), never before a trailing newline.
//! Case-insensitive mode folds ASCII letters only. Alternation and repetition are
//! leftmost-first, not POSIX longest. Lookaround, backreferences and Unicode classes
//! (`\p{..}`) are rejected with an error naming the offset.

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

#[derive(Debug)]
enum Inst {
    /// Consume one byte from a set.
    Set(ByteSet),
    /// Continue at one of two program counters, preferring the first.
    Split(usize, usize),
    Jmp(usize),
    Assert(Assert),
    Match,
}

/// AST node produced by the parser.
enum Node {
    Empty,
    Byte(u8),
    Set(ByteSet),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>, bool),
    Assert(Assert),
}

/// A compiled pattern.
#[derive(Debug)]
pub struct Regex {
    prog: Vec<Inst>,
    flags: Flags,
    /// The whole pattern, when it is a case-sensitive literal.
    lit: Option<Vec<u8>>,
    /// A byte every match must contain, for prefiltering.
    required: Option<u8>,
}

impl Regex {
    pub fn new(pattern: &str) -> Result<Regex, String> {
        Regex::new_with(pattern, Flags::default())
    }

    pub fn new_with(pattern: &str, flags: Flags) -> Result<Regex, String> {
        let mut p = Parser {
            src: pattern.as_bytes(),
            at: 0,
            flags,
            depth: 0,
            folded: flags.case_insensitive,
        };
        let node = p.parse_alternation()?;
        if p.at < p.src.len() {
            return Err(p.err_at("unexpected character", p.at));
        }
        let mut prog = Vec::new();
        let compiled = compile_node(&node, &mut prog);
        prog.push(Inst::Match);
        // An inline `(?i)` anywhere in the pattern rules out the literal fast path.
        let lit = literal_of(&node, p.folded);
        let required = required_byte(&node);
        // A literal's rarest byte is the best prefilter the parser can give us cheaply;
        // for everything else use the single mandatory byte of the leftmost atom.
        let required = match &lit {
            Some(bytes) => rarest_byte(bytes).or(required),
            None => required,
        };
        let _ = compiled;
        Ok(Regex {
            prog,
            flags,
            lit,
            required,
        })
    }

    pub fn flags(&self) -> Flags {
        self.flags
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
        if let Some(lit) = &self.lit {
            let i = crate::bytes::memmem_from(hay, lit, start)?;
            return Some(Match {
                start: i,
                end: i + lit.len(),
            });
        }
        self.run(hay, start, true)
    }

    /// True when the pattern matches anywhere in `hay`.
    pub fn is_match(&self, hay: &[u8]) -> bool {
        if let Some(lit) = &self.lit {
            return crate::bytes::memmem(hay, lit).is_some();
        }
        self.run(hay, 0, false).is_some()
    }

    /// Pike VM: leftmost-first semantics, linear time in the haystack length.
    /// `capture` false stops at the first match (an existence test).
    fn run(&self, hay: &[u8], start: usize, capture: bool) -> Option<Match> {
        let _ = capture;
        let n = self.prog.len();
        let mut clist: Vec<Thread> = Vec::with_capacity(n);
        let mut nlist: Vec<Thread> = Vec::with_capacity(n);
        let mut cl_marks = vec![false; n];
        let mut nl_marks = vec![false; n];
        let mut st = Pending::default();
        let mut sp = start;
        loop {
            // A fresh attempt starts at every position until something has matched; it is
            // lower priority than the threads carried over from earlier positions.
            st.cut = false;
            if st.best.is_none() {
                self.add_thread(&mut clist, &mut cl_marks, 0, sp, hay, sp, &mut st);
            }
            let Some(byte) = hay.get(sp).copied() else {
                // Past the last byte: the closures above have recorded any match that
                // ends here, and no thread can advance further.
                break;
            };
            for t in clist.drain(..) {
                if let Inst::Set(set) = &self.prog[t.pc] {
                    if set.contains(byte) {
                        self.add_thread(
                            &mut nlist,
                            &mut nl_marks,
                            t.pc + 1,
                            t.start,
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
            sp += 1;
            if st.best.is_some() && clist.is_empty() {
                break;
            }
        }
        st.best
    }

    /// Epsilon closure: follow Split/Jmp/Assert/Save from `pc`, recording matches.
    #[allow(clippy::too_many_arguments)]
    fn add_thread(
        &self,
        list: &mut Vec<Thread>,
        visited: &mut [bool],
        mut pc: usize,
        start: usize,
        hay: &[u8],
        sp: usize,
        st: &mut Pending,
    ) {
        if st.cut || visited[pc] {
            return;
        }
        visited[pc] = true;
        loop {
            if st.cut {
                return;
            }
            match &self.prog[pc] {
                Inst::Jmp(to) => {
                    if visited[*to] {
                        return;
                    }
                    visited[*to] = true;
                    pc = *to;
                }
                Inst::Split(a, b) => {
                    self.add_thread(list, visited, *a, start, hay, sp, st);
                    if visited[*b] {
                        return;
                    }
                    visited[*b] = true;
                    pc = *b;
                }
                Inst::Assert(a) => {
                    if !assert_holds(*a, hay, sp) {
                        return;
                    }
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
    /// A match was recorded in this step: lower-priority threads are dropped.
    cut: bool,
}

fn assert_holds(a: Assert, hay: &[u8], sp: usize) -> bool {
    let is_word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    let before = if sp == 0 { None } else { hay.get(sp - 1) };
    let after = hay.get(sp);
    match a {
        Assert::StartText => sp == 0,
        Assert::EndText => sp == hay.len(),
        Assert::StartLine => sp == 0 || hay.get(sp - 1) == Some(&b'\n'),
        Assert::EndLine => sp == hay.len() || hay.get(sp) == Some(&b'\n'),
        Assert::WordBoundary => is_word(before) != is_word(after),
        Assert::NotWordBoundary => is_word(before) == is_word(after),
    }
}

fn compile_node(n: &Node, prog: &mut Vec<Inst>) -> usize {
    match n {
        Node::Empty => {}
        Node::Byte(b) => {
            let mut set = ByteSet::empty();
            set.add(*b);
            prog.push(Inst::Set(set));
        }
        Node::Set(s) => prog.push(Inst::Set(*s)),
        Node::Concat(parts) => {
            for p in parts {
                compile_node(p, prog);
            }
        }
        Node::Alt(branches) => {
            // split a | split b | ... | last
            let mut jumps = Vec::new();
            for (i, b) in branches.iter().enumerate() {
                if i + 1 == branches.len() {
                    compile_node(b, prog);
                } else {
                    let split = prog.len();
                    prog.push(Inst::Split(0, 0));
                    compile_node(b, prog);
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
                compile_node(inner, prog);
            }
            match max {
                None => {
                    // (inner)* : L: split(body, out); body; jmp L
                    let split = prog.len();
                    prog.push(Inst::Split(0, 0));
                    let body = prog.len();
                    compile_node(inner, prog);
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
                        compile_node(inner, prog);
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
    }
    prog.len()
}

/// The whole pattern as bytes when it is a case-sensitive literal.
fn literal_of(n: &Node, ci: bool) -> Option<Vec<u8>> {
    if ci {
        return None;
    }
    match n {
        Node::Empty => Some(Vec::new()),
        Node::Byte(b) => Some(vec![*b]),
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

/// The single byte every match must contain, when the parser can prove one cheaply.
fn required_byte(n: &Node) -> Option<u8> {
    match n {
        Node::Byte(b) => Some(*b),
        Node::Set(s) => s.sole(),
        Node::Concat(parts) => parts.first().and_then(required_byte),
        Node::Alt(branches) => {
            let mut it = branches.iter().filter_map(required_byte);
            let first = it.next()?;
            it.all(|b| b == first).then_some(first)
        }
        Node::Repeat(inner, _, _, _) => required_byte(inner),
        Node::Assert(_) | Node::Empty => None,
    }
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
// Parser

struct Parser<'a> {
    src: &'a [u8],
    at: usize,
    flags: Flags,
    depth: usize,
    /// Case-insensitive matching was enabled somewhere, inline flags included.
    folded: bool,
}

impl Parser<'_> {
    fn err_at(&self, msg: &str, at: usize) -> String {
        format!("{msg} at offset {at} in pattern {:?}", String::from_utf8_lossy(self.src))
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
            return Err(self.err_at("nothing to repeat: an assertion cannot be quantified", self.at));
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
        if self.peek() == Some(b',') {
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
                max = Some(digits.parse().map_err(|_| self.err_at("bad repetition bound", save))?);
            }
        }
        if self.peek() != Some(b'}') {
            self.at = save;
            return Ok(None);
        }
        self.at += 1;
        let min: u32 = min.parse().map_err(|_| self.err_at("bad repetition bound", save))?;
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
        let c = self.peek().ok_or_else(|| self.err_at("unexpected end", self.at))?;
        match c {
            b'(' => self.parse_group(),
            b'[' => self.parse_class(),
            b'.' => {
                self.at += 1;
                let set = if self.flags.dot_matches_newline {
                    ByteSet::full()
                } else {
                    ByteSet(full_without_newline())
                };
                Ok(Node::Set(set))
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
                // One UTF-8 character becomes the sequence of its bytes.
                let s = std::str::from_utf8(&self.src[self.at..])
                    .map_err(|_| self.err_at("invalid UTF-8 in pattern", self.at))?;
                let ch = s.chars().next().ok_or_else(|| self.err_at("unexpected end", self.at))?;
                let mut buf = [0u8; 4];
                let bytes = ch.encode_utf8(&mut buf).as_bytes().to_vec();
                self.at += bytes.len();
                if bytes.len() == 1 {
                    Ok(self.literal_node(bytes[0]))
                } else {
                    Ok(Node::Concat(
                        bytes.iter().map(|b| self.literal_node(*b)).collect(),
                    ))
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
        let _ = capturing;
        let body = self.parse_alternation()?;
        if self.peek() != Some(b')') {
            return Err(self.err_at("unterminated group", open));
        }
        self.at += 1;
        self.flags = saved;
        self.depth -= 1;
        Ok(body)
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
        let c = self.peek().ok_or_else(|| self.err_at("trailing backslash", start))?;
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
                let h = self.src.get(self.at..self.at + 2).ok_or_else(|| {
                    self.err_at("\\x needs exactly two hex digits", start)
                })?;
                let hex = std::str::from_utf8(h).map_err(|_| self.err_at("bad \\x", start))?;
                let b = u8::from_str_radix(hex, 16)
                    .map_err(|_| self.err_at("\\x needs exactly two hex digits", start))?;
                self.at += 2;
                Ok(single(b))
            }
            b'd' | b'D' | b'w' | b'W' | b's' | b'S' => {
                self.at += 1;
                let mut s = ByteSet::empty();
                match c {
                    b'd' | b'D' => s.add_range(b'0', b'9'),
                    b'w' | b'W' => {
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
                Ok(Node::Set(s))
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
            b'p' | b'P' => Err(self.err_at(
                "Unicode property classes (\\p{..}) are not supported",
                start,
            )),
            b'1'..=b'9' => Err(self.err_at("backreferences are not supported", start)),
            c if c.is_ascii_alphanumeric() => Err(self.err_at("unknown escape", start)),
            c => {
                self.at += 1;
                Ok(single(c))
            }
        }
    }

    fn parse_class(&mut self) -> Result<Node, String> {
        let open = self.at;
        self.at += 1;
        let mut negated = false;
        if matches!(self.peek(), Some(b'^') | Some(b'!')) {
            negated = true;
            self.at += 1;
        }
        let mut set = ByteSet::empty();
        // Multi-byte class members become alternatives: `[é]` is one character, not one
        // of its two bytes.
        let mut seqs: Vec<Vec<u8>> = Vec::new();
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
            if c == b'\\' {
                let (item, end) = self.class_escape()?;
                self.at = end;
                match item {
                    ClassItem::Byte(b) => set.add(b),
                    ClassItem::Set(s) => or_set(&mut set, &s),
                }
                continue;
            }
            let ch = self.next_char()?;
            if ch.is_ascii() {
                let lo = ch as u8;
                // A range needs an ASCII end; non-ASCII ranges are rejected rather than
                // silently matched byte-wise.
                if self.peek() == Some(b'-') && self.src.get(self.at + 1) != Some(&b']') {
                    self.at += 1;
                    let hi = match self.peek() {
                        Some(b'\\') => {
                            let (item, end) = self.class_escape()?;
                            self.at = end;
                            match item {
                                ClassItem::Byte(b) => b,
                                ClassItem::Set(s) => {
                                    set.add(lo);
                                    or_set(&mut set, &s);
                                    continue;
                                }
                            }
                        }
                        Some(_) => {
                            let h = self.next_char()?;
                            if !h.is_ascii() {
                                return Err(self.err_at(
                                    "non-ASCII ranges in a class are not supported",
                                    self.at,
                                ));
                            }
                            h as u8
                        }
                        None => return Err(self.err_at("unterminated character class", open)),
                    };
                    if hi < lo {
                        return Err(self.err_at("character class range is backwards", self.at));
                    }
                    set.add_range(lo, hi);
                } else {
                    set.add(lo);
                }
            } else {
                if self.peek() == Some(b'-') && self.src.get(self.at + 1) != Some(&b']') {
                    return Err(self.err_at(
                        "non-ASCII ranges in a class are not supported",
                        self.at,
                    ));
                }
                let mut buf = [0u8; 4];
                seqs.push(ch.encode_utf8(&mut buf).as_bytes().to_vec());
            }
        }
        if !seqs.is_empty() {
            if negated {
                return Err(self.err_at(
                    "a negated class with a non-ASCII character is not supported",
                    open,
                ));
            }
            let mut alts = Vec::new();
            if !set.is_empty() {
                let mut s = set;
                if self.flags.case_insensitive {
                    s.fold_ascii();
                }
                alts.push(Node::Set(s));
            }
            for seq in seqs {
                alts.push(Node::Concat(seq.into_iter().map(Node::Byte).collect()));
            }
            return Ok(if alts.len() == 1 {
                alts.pop().unwrap()
            } else {
                Node::Alt(alts)
            });
        }
        if self.flags.case_insensitive {
            set.fold_ascii();
        }
        if negated {
            set.negate();
        }
        if set.is_empty() {
            return Err(self.err_at("empty character class", open));
        }
        Ok(Node::Set(set))
    }

    /// Decode and consume one character, or fail with an offset.
    fn next_char(&mut self) -> Result<char, String> {
        let s = std::str::from_utf8(&self.src[self.at..])
            .map_err(|_| self.err_at("invalid UTF-8 in pattern", self.at))?;
        let ch = s.chars().next().ok_or_else(|| self.err_at("unexpected end", self.at))?;
        self.at += ch.len_utf8();
        Ok(ch)
    }

    fn class_escape(&mut self) -> Result<(ClassItem, usize), String> {
        let start = self.at;
        self.at += 1;
        let c = self.peek().ok_or_else(|| self.err_at("trailing backslash", start))?;
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
            b'd' | b'w' | b's' => Ok((ClassItem::Set(perl_class(c, false)), start + 2)),
            b'D' | b'W' | b'S' => Ok((
                ClassItem::Set(perl_class(c.to_ascii_lowercase(), true)),
                start + 2,
            )),
            c if c.is_ascii_alphanumeric() => Err(self.err_at("unknown escape in class", start)),
            c => Ok(single(c)),
        }
    }
}

enum ClassItem {
    Byte(u8),
    Set(ByteSet),
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
            Regex::new_with("a.c", Flags { dot_matches_newline: true, ..Flags::default() })
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
        assert!(Regex::new("a{1001}").is_err(), "bound over 1000 is rejected");
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
        assert!(Regex::new(r"\p{L}").is_err());
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
        let re = Regex::new_with("^b", Flags { multi_line: true, ..Flags::default() }).unwrap();
        assert_eq!(re.find(b"a\nb\nc").map(|m| m.start), Some(2));
        let re = Regex::new_with("^b", Flags::default()).unwrap();
        assert!(re.find(b"a\nb\nc").is_none());
    }

    #[test]
    fn case_insensitive() {
        let re = Regex::new_with("needle", Flags { case_insensitive: true, ..Flags::default() })
            .unwrap();
        assert!(re.is_match(b"a NEEDLE b"));
        assert!(re.is_match(b"Needle"));
        assert!(re.literal().is_none(), "folding disables the literal fast path");
        let re = Regex::new_with("[a-c]+", Flags { case_insensitive: true, ..Flags::default() })
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
        for bad in ["*a", "+a", "?a", "(|", "a)", "a**", "^*", "$+", "{2}", "a{2,1}"] {
            assert!(Regex::new(bad).is_err(), "{bad:?} should not compile");
        }
        // Braces that are not a repetition are literal text, and `|` with empty sides
        // compiles to the empty pattern, as in rg.
        assert!(Regex::new("a{").unwrap().is_match(b"a{"));
        assert!(Regex::new("|").unwrap().is_match(b""));
        assert!(Regex::new("a{,3}").unwrap().is_match(b"a{,3}"));
    }

    #[test]
    fn required_byte_and_literal_hints() {
        assert_eq!(Regex::new("needle").unwrap().literal(), Some(&b"needle"[..]));
        assert_eq!(Regex::new("nee+dle").unwrap().literal(), None);
        assert_eq!(Regex::new("abc|abd").unwrap().required_byte(), Some(b'a'));
        assert_eq!(Regex::new(r"\d+").unwrap().required_byte(), None);
        assert_eq!(Regex::new("z.*").unwrap().required_byte(), Some(b'z'));
        assert_eq!(Regex::new("x").unwrap().required_byte(), Some(b'x'));
    }

    #[test]
    fn utf8_characters_match_as_bytes() {
        assert_eq!(m("é", "café"), Some((3, 5)));
        assert!(matched("日本", "a日本語b"));
        assert_eq!(m("[é]", "aé"), Some((1, 3)));
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

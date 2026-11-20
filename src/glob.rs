//! Glob matching for `--glob`, `--iglob` and ignore files.
//!
//! One [`Glob`] compiles one gitignore-style pattern and matches it against a path
//! relative to the walk root. `?` and a character class consume exactly one Unicode scalar
//! value, so `?` matches `é`. A pattern that holds no `?`, no negated class and nothing
//! outside ASCII reads its subject one byte per character instead, which cannot differ:
//! nothing in such a pattern can consume part of a multi-byte character. Only the rest is
//! matched character by character.
//!
//! Supported syntax:
//!
//! - `*` matches any run of characters except `/`; `?` matches one such character.
//! - `**` matches any run of characters including `/`, wherever it appears in a path
//!   pattern (`a/**b` matches `a/x/b`). `**/` additionally matches nothing at all, so
//!   `a/**/b` matches `a/b` and `**/x` matches `x` at any depth. A trailing `**` does not
//!   reach the directory it hangs off: `foo/**` is `foo`, `/`, then `**`, so it matches what
//!   is below `foo` and never `foo` itself.
//! - `[...]` matches one character from the class; `[!...]` and `[^...]` negate it.
//!   Members are single characters or `a-z` code point ranges, `\]` `\-` `\\` escape a
//!   member, and a `]` immediately after `[` or `[!` is a literal member. Classes never
//!   match `/`, and a reversed range (`[z-a]`) is an error. POSIX bracket names
//!   (`[[:alpha:]]`) are not implemented: a `[` inside a class is a literal member.
//! - `{a,b}` expands at compile time into alternatives, nested groups included, so
//!   `*.{rs,{toml,yaml}}` becomes three patterns. An empty alternative (`{a,}`) is
//!   allowed. `{a}` has no comma: ripgrep rejects it, here it is a group of one
//!   alternative, i.e. the same as `a` without the braces. An unclosed `{` or an
//!   unopened `}` is an error naming the character offset. `\{` is a literal brace and
//!   braces inside a class are literal. Expansion is capped at 4096 alternatives.
//! - A trailing `/` restricts the pattern to directories. A leading `!` marks negation:
//!   it only sets [`Glob::negated`], matching stays un-inverted for the caller to apply,
//!   and `\!` is a literal `!`.
//! - A leading `/` anchors the pattern at the walk root.
//! - A pattern with no `/` matches the file name of any entry (gitignore semantics); a
//!   pattern containing `/` matches the whole relative path, anchored at the root.
//! - `\` escapes the next character (`\*`, `\?`, `\[`, `\\`); a trailing lone backslash
//!   is an error. `/` is the only separator a pattern can contain, so a Windows-style
//!   path in a pattern never matches — while the *path* may use either separator and is
//!   read with both.
//! - [`Glob::new_with`] folds ASCII letters in both the pattern and the path
//!   (`--iglob`); [`Glob::new`] is case sensitive.
//!
//! Matching is a suffix dynamic program over the token list, so no input can recurse,
//! panic, or backtrack exponentially: cost is O(tokens * subject length) per alternative
//! with O(subject) memory. An alternative that is literals, or literals around one `*`, is
//! answered by a direct comparison instead, and one that does reach the DP is first tested
//! against the literal runs every path it matches must hold.

use std::fmt;

/// Cap on brace alternatives so `{a,b}{c,d}…` cannot exhaust memory.
const MAX_ALTS: usize = 4096;

#[derive(Debug, Clone)]
enum Item {
    Ch(char),
    Range(char, char),
}

#[derive(Debug, Clone)]
enum Tok {
    /// Literal character.
    Lit(char),
    /// `?`: one character that is not `/`.
    Any,
    /// `*`: any run of characters that does not cross `/`.
    Star,
    /// `**`: any run including `/`.
    DoubleStar,
    /// `**/`: nothing, or any run ending in `/`.
    StarSlash,
    /// `[...]`: one character from the class, never `/`.
    Class { neg: bool, items: Vec<Item> },
}

/// The shape one alternative has, when its tokens allow a direct test instead of the
/// suffix DP. Decided once at compile time so the per-entry path never inspects tokens to
/// find out what it could have done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// No wildcard at all: the subject must equal the text.
    Literal,
    /// A lone `*`: anything, empty included.
    Any,
    /// `*` then literals: a tail, with no `/` in what precedes it.
    Suffix,
    /// Literals then `*`: a head, with no `/` after it.
    Prefix,
    /// Literals, `*`, literals: a head and a tail with no `/` between them.
    Between,
    /// Anything else, including every `**` and every class: the DP decides.
    Full,
}

/// Necessary conditions on a path, read off one alternative's tokens: its leading literals,
/// its trailing literals, and its longest literal run. Every one of them is a substring of
/// any path the alternative matches, so a miss settles the alternative without the DP.
#[derive(Debug, Clone, Default)]
struct Pre {
    /// The alternative starts with these bytes, so a match starts there too.
    head: Vec<u8>,
    /// The alternative ends with these bytes, so a match ends with them too.
    tail: Vec<u8>,
    /// The longest run of literals anywhere in the alternative.
    mid: Vec<u8>,
}

/// The longest run of these kept per anchor. A shorter run is still a necessary condition,
/// and these are the bytes that reject an entry, so a cap only costs a comparison.
const PRE_CAP: usize = 16;

impl Pre {
    /// True when `path` can still be a match: cheap, and never in the caller's favour.
    fn may_match(&self, path: &[u8]) -> bool {
        path.starts_with(&self.head)
            && path.ends_with(&self.tail)
            && crate::bytes::memmem(path, &self.mid).is_some()
    }
}

/// One compiled glob pattern.
#[derive(Debug, Clone)]
pub struct Glob {
    raw: String,
    alts: Vec<Vec<Tok>>,
    /// The shape of each alternative, in the same order as [`Glob::alts`].
    shapes: Vec<Shape>,
    /// For an alternative the DP has to run: what every path it matches must hold. A pattern
    /// that has one rejects almost every entry before the DP, which is most of the cost of
    /// a `**`-style pattern.
    pres: Vec<Option<Pre>>,
    /// Pattern began with `!`; matching is not inverted, the caller decides.
    pub negated: bool,
    /// Pattern ended with `/`; it only matches directories.
    pub dir_only: bool,
    anchored: bool,
    has_slash: bool,
    ignore_case: bool,
    /// Every alternative is ASCII-only and holds no `?` and no negated class, so a byte
    /// subject answers exactly as a character subject would and the subject is never
    /// scanned to find out whether it needs the character DP.
    byte_safe: bool,
    /// Every alternative holds a literal `/`, so a subject without a separator cannot match.
    /// A `**/` prefix does not count: it is allowed to match nothing at all.
    needs_separator: bool,
}

impl Glob {
    /// Compile `pattern` with case-sensitive matching.
    pub fn new(pattern: &str) -> Result<Glob, String> {
        Self::new_with(pattern, false)
    }

    /// Compile `pattern`, folding ASCII letters when `ignore_case` (`--iglob`).
    pub fn new_with(pattern: &str, ignore_case: bool) -> Result<Glob, String> {
        let mut body = pattern;
        let negated = body.starts_with('!');
        if negated {
            body = &body[1..];
        }
        let mut dir_only = false;
        while let Some(rest) = body.strip_suffix('/') {
            dir_only = true;
            body = rest;
        }
        let mut anchored = false;
        while let Some(rest) = body.strip_prefix('/') {
            anchored = true;
            body = rest;
        }
        if body.is_empty() {
            return Err(format!("empty glob pattern {pattern:?}"));
        }
        let mut alts = Vec::new();
        for alt in expand_braces(body)? {
            alts.push(compile(&alt, ignore_case)?);
        }
        let shapes: Vec<Shape> = alts.iter().map(|toks| shape_of(toks)).collect();
        let byte_safe = alts.iter().all(|toks| toks.iter().all(byte_token));
        let needs_separator = alts
            .iter()
            .all(|toks| toks.iter().any(|tok| matches!(tok, Tok::Lit('/'))));
        let pres = alts
            .iter()
            .zip(&shapes)
            .map(|(toks, shape)| match shape {
                Shape::Full => Some(prefilter(toks)),
                _ => None,
            })
            .collect();
        Ok(Glob {
            raw: pattern.to_string(),
            alts,
            shapes,
            pres,
            negated,
            dir_only,
            anchored,
            has_slash: body.contains('/'),
            ignore_case,
            byte_safe,
            needs_separator,
        })
    }

    /// The pattern exactly as it was passed to [`Glob::new`]/[`Glob::new_with`].
    pub fn pattern(&self) -> &str {
        &self.raw
    }

    /// True when the pattern constrains the whole relative path rather than just the
    /// file name of an entry (it contains a `/`, or was anchored with a leading `/`).
    pub fn is_path_pattern(&self) -> bool {
        self.anchored || self.has_slash
    }

    /// True when every alternative must consume a separator, so no subject that holds none
    /// can match and a walk need not test this pattern against a bare name.
    pub fn needs_separator(&self) -> bool {
        self.needs_separator
    }

    /// Match an entry whose directory nature is unknown; a `dir/`-style pattern never
    /// matches. Matching ignores [`Glob::negated`].
    pub fn is_match(&self, rel: &str) -> bool {
        self.matches(rel, false)
    }

    /// Match `rel` (relative to the walk root) as a directory or a file. This splits `rel`
    /// for one pattern; a stack of patterns testing the same entry shares one [`PathView`]
    /// through [`Glob::matches_view`] instead.
    pub fn matches(&self, rel: &str, is_dir: bool) -> bool {
        self.matches_view(&PathView::new(rel), is_dir)
    }

    /// Match one entry's [`PathView`], whose strings are already split as [`Glob::matches`]
    /// splits them, so the shape and anchor dispatch below is that method's exactly.
    pub fn matches_view(&self, view: &PathView<'_>, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if view.blank() {
            return false;
        }
        let subject = if self.is_path_pattern() {
            view.path()
        } else {
            view.name()
        };
        // A path pattern's subject may spell its separators `\`; a name's cannot, because
        // it starts after the last one. Normalising here is what makes `a\main.rs` match
        // `a/*.rs`, and it is skipped for every subject that holds no backslash at all.
        let owned;
        let subject = if self.is_path_pattern() && subject.contains('\\') {
            owned = subject.replace('\\', "/");
            owned.as_str()
        } else {
            subject
        };
        // A byte subject is matched byte for byte: `?` and a class consume one byte, which is
        // the same thing they consume from a one-byte character, and a pattern character
        // outside ASCII can never equal one of those bytes. A pattern that [`byte_token`]
        // accepts holds no `?` and nothing that a non-ASCII byte could satisfy, so it reads
        // every subject as one byte per character and the subject is not scanned at all.
        // Anything else keeps the character DP, where `?` and a class consume one scalar
        // value.
        if self.byte_safe || subject.is_ascii() {
            let path = subject.as_bytes();
            // One zip, and the anchor read only where a `**` or a class needs it: the three
            // vectors are parallel, and walking them as one pair keeps the iterator state
            // out of the per-entry cost.
            for (at, (toks, shape)) in self.alts.iter().zip(&self.shapes).enumerate() {
                let hit = match shape {
                    Shape::Literal => lit_eq(toks, path, self.ignore_case),
                    Shape::Any => true,
                    Shape::Suffix => lit_suffix(toks, path, self.ignore_case),
                    Shape::Prefix => lit_prefix(toks, path, self.ignore_case),
                    Shape::Between => lit_between(toks, path, self.ignore_case),
                    Shape::Full => {
                        let found = match &self.pres[at] {
                            // Folding compares literals case-insensitively one at a time,
                            // so a byte prefilter would answer for the unfolded path only.
                            Some(pre) if !self.ignore_case => pre.may_match(path),
                            _ => true,
                        };
                        found && matches_bytes(toks, path, self.ignore_case)
                    }
                };
                if hit {
                    return true;
                }
            }
            return false;
        }
        let mut chars: Vec<char> = subject.chars().collect();
        if self.ignore_case {
            for c in &mut chars {
                c.make_ascii_lowercase();
            }
        }
        self.alts.iter().any(|toks| matches_tokens(toks, &chars))
    }
}

/// One relative path split into the two subjects a glob tests: the whole path, and its last
/// component (what a gitignore pattern without a `/` is matched against).
///
/// Splitting is the cost, and a walk tests one entry against a stack of patterns — the `-g`
/// filter, every ignore layer, every rule in it — so the split is made once, by
/// [`PathView::new`], and the same view is matched by all of them. Building one allocates
/// nothing: only a path that spells its separators `\` is normalised, once per pattern that
/// reads the whole path, and a name always starts after the last separator.
#[derive(Debug, Clone)]
pub struct PathView<'a> {
    /// The whole path, trailing `/` removed.
    path: &'a str,
    /// Byte offset within `path` where the last component starts.
    name_at: usize,
    /// The caller's string was empty. No pattern matches that, while a path of a lone `/`
    /// trims away to nothing and is matched as the empty string it becomes.
    blank: bool,
}

impl<'a> PathView<'a> {
    pub fn new(rel: &'a str) -> PathView<'a> {
        let path = rel.trim_end_matches('/');
        // The last separator of either spelling, searched backwards: the distance to it is
        // one component, not the whole path. A `\` counts as a separator because it is read
        // as one, and what follows it is a name that then holds no separator to normalise.
        let name_at = path
            .as_bytes()
            .iter()
            .rposition(|&b| b == b'/' || b == b'\\')
            .map_or(0, |at| at + 1);
        PathView {
            path,
            name_at,
            blank: rel.is_empty(),
        }
    }

    /// The whole path, as a pattern containing a `/` sees it.
    pub fn path(&self) -> &str {
        self.path
    }

    /// The last component, as a pattern without a `/` sees it.
    pub fn name(&self) -> &str {
        &self.path[self.name_at..]
    }

    /// True when the path is one component, so no separator is available to a pattern.
    pub fn is_name(&self) -> bool {
        self.name_at == 0
    }

    /// Everything above the last component, without the separator: the directory an entry
    /// sits in, which every sibling of the directory repeats.
    pub fn parent(&self) -> &str {
        match self.name_at {
            0 => "",
            at => &self.path[..at - 1],
        }
    }

    /// The same entry's path as it reads below the directory `base`, or `None` when `base`
    /// is not a whole-component prefix of it. The cut lands on a component boundary, so the
    /// result names the same last component.
    pub fn below(&self, base: &str) -> Option<PathView<'_>> {
        let rest = match base.is_empty() {
            true => self.path,
            false => self.path.strip_prefix(base)?.strip_prefix('/')?,
        };
        Some(PathView {
            path: rest,
            name_at: self.name_at - (self.path.len() - rest.len()),
            blank: false,
        })
    }

    fn blank(&self) -> bool {
        self.blank
    }
}

impl fmt::Display for Glob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Expand brace groups left to right into one pattern per alternative.
fn expand_braces(pat: &str) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    let mut work = vec![pat.to_string()];
    while let Some(cur) = work.pop() {
        let chars: Vec<char> = cur.chars().collect();
        let (open, close, commas) = match find_brace(&chars, &cur)? {
            Some(hit) => hit,
            None => {
                if done.len() > MAX_ALTS || done.len() + work.len() > MAX_ALTS * 4 {
                    return Err(format!(
                        "too many brace alternatives (limit {MAX_ALTS}) in {pat:?}"
                    ));
                }
                done.push(cur);
                continue;
            }
        };
        let prefix: String = chars[..open].iter().collect();
        let suffix: String = chars[close + 1..].iter().collect();
        let mut bounds = commas;
        bounds.push(close);
        let mut start = open + 1;
        let mut pieces = Vec::with_capacity(bounds.len());
        for end in bounds {
            let inner: String = chars[start..end].iter().collect();
            pieces.push(format!("{prefix}{inner}{suffix}"));
            start = end + 1;
        }
        work.extend(pieces.into_iter().rev());
    }
    Ok(done)
}

/// Find the first top-level brace group: `(open, close, commas between them)`.
fn find_brace(chars: &[char], pat: &str) -> Result<Option<(usize, usize, Vec<usize>)>, String> {
    let mut depth = 0usize;
    let mut open = 0usize;
    let mut commas = Vec::new();
    let mut escape = false;
    let mut in_class = false;
    for (i, &c) in chars.iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        match c {
            '\\' => escape = true,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '{' if !in_class => {
                if depth == 0 {
                    open = i;
                    commas.clear();
                }
                depth += 1;
            }
            '}' if !in_class => {
                if depth == 0 {
                    return Err(at(pat, i, "unopened '}'"));
                }
                depth -= 1;
                if depth == 0 {
                    return Ok(Some((open, i, commas)));
                }
            }
            ',' if !in_class && depth == 1 => commas.push(i),
            _ => {}
        }
    }
    if depth > 0 {
        return Err(at(pat, open, "unclosed '{'"));
    }
    Ok(None)
}

fn at(pat: &str, i: usize, msg: &str) -> String {
    format!("{msg} at character offset {i} in pattern {pat:?}")
}

/// Tokenise one brace-free alternative.
fn compile(pat: &str, ignore_case: bool) -> Result<Vec<Tok>, String> {
    let fold = |c: char| {
        if ignore_case {
            c.to_ascii_lowercase()
        } else {
            c
        }
    };
    let chars: Vec<char> = pat.chars().collect();
    let mut toks = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                let mut n = 1;
                while i + n < chars.len() && chars[i + n] == '*' {
                    n += 1;
                }
                if n == 1 {
                    toks.push(Tok::Star);
                    i += 1;
                } else if i + n < chars.len() && chars[i + n] == '/' {
                    toks.push(Tok::StarSlash);
                    i += n + 1;
                } else {
                    toks.push(Tok::DoubleStar);
                    i += n;
                }
            }
            '?' => {
                toks.push(Tok::Any);
                i += 1;
            }
            '[' => {
                let (tok, next) = parse_class(&chars, i, pat, &fold)?;
                toks.push(tok);
                i = next;
            }
            '\\' => {
                if i + 1 >= chars.len() {
                    return Err(at(pat, i, "trailing backslash"));
                }
                toks.push(Tok::Lit(fold(chars[i + 1])));
                i += 2;
            }
            c => {
                toks.push(Tok::Lit(fold(c)));
                i += 1;
            }
        }
    }
    Ok(toks)
}

fn parse_class(
    chars: &[char],
    start: usize,
    pat: &str,
    fold: &dyn Fn(char) -> char,
) -> Result<(Tok, usize), String> {
    let mut i = start + 1;
    let mut neg = false;
    if i < chars.len() && (chars[i] == '!' || chars[i] == '^') {
        neg = true;
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    loop {
        if i >= chars.len() {
            return Err(at(pat, start, "unclosed '['"));
        }
        if chars[i] == ']' && !first {
            i += 1;
            break;
        }
        first = false;
        let (lo, next) = class_member(chars, i, pat)?;
        i = next;
        if i + 1 < chars.len() && chars[i] == '-' && chars[i + 1] != ']' {
            let (hi, next) = class_member(chars, i + 1, pat)?;
            if hi < lo {
                return Err(at(pat, i, "reversed character range"));
            }
            items.push(Item::Range(fold(lo), fold(hi)));
            i = next;
        } else {
            items.push(Item::Ch(fold(lo)));
        }
    }
    Ok((Tok::Class { neg, items }, i))
}

fn class_member(chars: &[char], i: usize, pat: &str) -> Result<(char, usize), String> {
    if chars[i] == '\\' {
        if i + 1 >= chars.len() {
            return Err(at(pat, i, "trailing backslash"));
        }
        Ok((chars[i + 1], i + 2))
    } else {
        Ok((chars[i], i + 1))
    }
}

fn class_hit(neg: bool, items: &[Item], c: char) -> bool {
    let hit = items.iter().any(|it| match it {
        Item::Ch(x) => *x == c,
        Item::Range(lo, hi) => *lo <= c && c <= *hi,
    });
    hit != neg
}

/// Whether one token answers the same on bytes as it does on characters: an ASCII literal
/// or an ASCII class member can never equal a byte of a multi-byte character, so a byte
/// subject fails those tokens exactly where a character subject would.
fn byte_token(tok: &Tok) -> bool {
    match tok {
        Tok::Lit(c) => c.is_ascii(),
        Tok::Any => false,
        Tok::Star | Tok::DoubleStar | Tok::StarSlash => true,
        // A negated class accepts every non-ASCII byte, and would then match one byte of a
        // multi-byte character where the character DP rejects it.
        Tok::Class { neg, items } => {
            !*neg
                && items.iter().all(|item| match item {
                    Item::Ch(c) => c.is_ascii(),
                    Item::Range(lo, hi) => lo.is_ascii() && hi.is_ascii(),
                })
        }
    }
}

/// Which of the direct tests an alternative's tokens allow, if any.
fn shape_of(toks: &[Tok]) -> Shape {
    let mut wild = toks
        .iter()
        .enumerate()
        .filter(|(_, t)| !matches!(t, Tok::Lit(c) if c.is_ascii()));
    let Some((i, t)) = wild.next() else {
        return Shape::Literal;
    };
    if wild.next().is_some() || !matches!(t, Tok::Star) {
        return Shape::Full;
    }
    match i {
        0 if toks.len() == 1 => Shape::Any,
        0 => Shape::Suffix,
        i if i == toks.len() - 1 => Shape::Prefix,
        _ => Shape::Between,
    }
}

/// Read the necessary conditions of one alternative. A leading or trailing run of ASCII
/// literals pins the start or the end of every path it matches; the longest run anywhere is
/// a substring of it. Two bytes are worth a test at either end, three in the middle.
fn prefilter(toks: &[Tok]) -> Pre {
    let is_lit = |t: &Tok| matches!(t, Tok::Lit(c) if c.is_ascii());
    let as_bytes = |run: &[Tok]| -> Vec<u8> {
        run.iter()
            .filter_map(|t| match t {
                Tok::Lit(c) => Some(*c as u8),
                _ => None,
            })
            .collect()
    };
    let mut pre = Pre::default();
    let head = toks.iter().take_while(|t| is_lit(t)).count();
    if head >= 2 {
        pre.head = as_bytes(&toks[..head.min(PRE_CAP)]);
    }
    let tail = toks.iter().rev().take_while(|t| is_lit(t)).count();
    if tail >= 2 {
        pre.tail = as_bytes(&toks[toks.len() - tail.min(PRE_CAP)..]);
    }
    let mut best: &[Tok] = &[];
    let mut start = 0;
    while start < toks.len() {
        let mut end = start;
        while end < toks.len() && is_lit(&toks[end]) {
            end += 1;
        }
        if end - start > best.len() {
            best = &toks[start..end];
        }
        start = end + 1;
    }
    if best.len() >= 3 {
        pre.mid = as_bytes(&best[..best.len().min(PRE_CAP)]);
    }
    pre
}

/// Whether two path bytes are the same character under the pattern's case rule.
#[inline]
fn byte_eq(a: u8, b: u8, fold: bool) -> bool {
    a == b || (fold && a.eq_ignore_ascii_case(&b))
}

/// One literal token against one path byte. A wildcard here means the caller asked for a
/// shape the tokens do not have, which no match satisfies.
#[inline]
fn lit_hit(t: &Tok, b: u8, fold: bool) -> bool {
    match t {
        Tok::Lit(c) if c.is_ascii() => byte_eq(*c as u8, b, fold),
        _ => false,
    }
}

fn lit_eq(toks: &[Tok], path: &[u8], fold: bool) -> bool {
    path.len() == toks.len() && toks.iter().zip(path).all(|(t, b)| lit_hit(t, *b, fold))
}

fn lit_suffix(toks: &[Tok], path: &[u8], fold: bool) -> bool {
    let tail = &toks[1..];
    let Some(head) = path.len().checked_sub(tail.len()) else {
        return false;
    };
    // What `*` covers may not hold a separator.
    !path[..head].contains(&b'/')
        && tail
            .iter()
            .zip(&path[head..])
            .all(|(t, b)| lit_hit(t, *b, fold))
}

fn lit_prefix(toks: &[Tok], path: &[u8], fold: bool) -> bool {
    let head = &toks[..toks.len() - 1];
    head.len() <= path.len()
        && head.iter().zip(path).all(|(t, b)| lit_hit(t, *b, fold))
        && !path[head.len()..].contains(&b'/')
}

/// Literals, one `*`, literals: a head and a tail, with no separator in between.
fn lit_between(toks: &[Tok], path: &[u8], fold: bool) -> bool {
    let Some(star) = toks.iter().position(|t| matches!(t, Tok::Star)) else {
        return false;
    };
    let (head, tail) = (&toks[..star], &toks[star + 1..]);
    let Some(rest) = path.len().checked_sub(tail.len()) else {
        return false;
    };
    head.len() <= rest
        && head.iter().zip(path).all(|(t, b)| lit_hit(t, *b, fold))
        && tail
            .iter()
            .zip(&path[rest..])
            .all(|(t, b)| lit_hit(t, *b, fold))
        && !path[head.len()..rest].contains(&b'/')
}

/// Suffix DP: `row[j]` says whether the token suffix at this row matches `path[j..]`.
/// Rows run from the end of the token list back to its start, so nothing recurses.
///
/// Kept out of line: inlined, its row buffers size the frame of every caller, and most
/// entries are answered by a direct shape or a prefilter without ever reaching it.
#[inline(never)]
fn matches_tokens(toks: &[Tok], path: &[char]) -> bool {
    let m = path.len();
    let mut next: Vec<bool> = (0..=m).map(|j| j == m).collect();
    let mut cur = vec![false; m + 1];
    let mut mid = vec![false; m + 1];
    let mut i = toks.len();
    while i > 0 {
        i -= 1;
        match &toks[i] {
            Tok::Lit(c) => {
                let mut j = 0;
                while j <= m {
                    cur[j] = j < m && path[j] == *c && next[j + 1];
                    j += 1;
                }
            }
            Tok::Any => {
                let mut j = 0;
                while j <= m {
                    cur[j] = j < m && path[j] != '/' && next[j + 1];
                    j += 1;
                }
            }
            Tok::Class { neg, items } => {
                let mut j = 0;
                while j <= m {
                    cur[j] =
                        j < m && path[j] != '/' && class_hit(*neg, items, path[j]) && next[j + 1];
                    j += 1;
                }
            }
            Tok::Star => {
                let mut j = m + 1;
                while j > 0 {
                    j -= 1;
                    let step = j < m && path[j] != '/' && cur[j + 1];
                    cur[j] = next[j] || step;
                }
            }
            Tok::DoubleStar => {
                let mut j = m + 1;
                while j > 0 {
                    j -= 1;
                    let step = j < m && cur[j + 1];
                    cur[j] = next[j] || step;
                }
            }
            Tok::StarSlash => {
                mid[m] = false;
                let mut j = m;
                while j > 0 {
                    j -= 1;
                    mid[j] = if path[j] == '/' {
                        next[j + 1] || mid[j + 1]
                    } else {
                        mid[j + 1]
                    };
                }
                let mut j = 0;
                while j <= m {
                    cur[j] = next[j] || mid[j];
                    j += 1;
                }
            }
        }
        std::mem::swap(&mut cur, &mut next);
    }
    next[0]
}

/// Rows up to this many bytes use a small stack buffer, so the buffer a call zeroes is the
/// size of a path rather than the size of the largest path allowed: every entry of a walk
/// pays this, and nearly every one is short.
const DP_SMALL: usize = 64;

/// Rows above this many bytes spill to the heap instead of the stack. A path that long is
/// rare enough that the allocation does not matter.
const DP_STACK: usize = 512;

/// The suffix DP over bytes, for an ASCII subject: one byte is one character there, so the
/// same transitions run on `u8` with no `Vec<char>` and no heap row buffers.
#[inline(never)]
fn matches_bytes(toks: &[Tok], path: &[u8], fold: bool) -> bool {
    let m = path.len();
    let mut small = [0u8; 3 * (DP_SMALL + 1)];
    let mut stack = [0u8; 3 * (DP_STACK + 1)];
    let mut spill: Vec<u8> = Vec::new();
    let buf: &mut [u8] = if m <= DP_SMALL {
        &mut small[..3 * (m + 1)]
    } else if m <= DP_STACK {
        &mut stack[..3 * (m + 1)]
    } else {
        spill.resize(3 * (m + 1), 0);
        &mut spill
    };
    let (mut cur, rest) = buf.split_at_mut(m + 1);
    let (mut next, mid) = rest.split_at_mut(m + 1);
    next[m] = 1;
    let mut i = toks.len();
    while i > 0 {
        i -= 1;
        match &toks[i] {
            Tok::Lit(c) => {
                for j in 0..m {
                    cur[j] = (lit_hit(&Tok::Lit(*c), path[j], fold) && next[j + 1] == 1) as u8;
                }
                cur[m] = 0;
            }
            Tok::Any => {
                for j in 0..m {
                    cur[j] = (path[j] != b'/' && next[j + 1] == 1) as u8;
                }
                cur[m] = 0;
            }
            Tok::Class { neg, items } => {
                for j in 0..m {
                    cur[j] = (path[j] != b'/'
                        && class_hit_byte(*neg, items, path[j], fold)
                        && next[j + 1] == 1) as u8;
                }
                cur[m] = 0;
            }
            Tok::Star => {
                cur[m] = next[m];
                for j in (0..m).rev() {
                    let step = path[j] != b'/' && cur[j + 1] == 1;
                    cur[j] = (next[j] == 1 || step) as u8;
                }
            }
            Tok::DoubleStar => {
                cur[m] = next[m];
                for j in (0..m).rev() {
                    let step = cur[j + 1] == 1;
                    cur[j] = (next[j] == 1 || step) as u8;
                }
            }
            Tok::StarSlash => {
                mid[m] = 0;
                for j in (0..m).rev() {
                    mid[j] = if path[j] == b'/' {
                        (next[j + 1] == 1 || mid[j + 1] == 1) as u8
                    } else {
                        mid[j + 1]
                    };
                }
                for j in 0..=m {
                    cur[j] = (next[j] == 1 || mid[j] == 1) as u8;
                }
            }
        }
        std::mem::swap(&mut cur, &mut next);
    }
    next[0] == 1
}

/// A class against an ASCII path byte, which is one character there, folded like the
/// character DP folds the subject.
fn class_hit_byte(neg: bool, items: &[Item], b: u8, fold: bool) -> bool {
    let c = if fold {
        (b as char).to_ascii_lowercase()
    } else {
        b as char
    };
    class_hit(neg, items, c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn g(p: &str) -> Glob {
        Glob::new(p).expect("pattern should compile")
    }

    fn gi(p: &str) -> Glob {
        Glob::new_with(p, true).expect("pattern should compile")
    }

    fn m(p: &str, rel: &str) -> bool {
        g(p).is_match(rel)
    }

    fn md(p: &str, rel: &str) -> bool {
        g(p).matches(rel, true)
    }

    /// What `matches` answered before the shape shortcuts: the character DP over the whole
    /// alternative list, with the same subject rule.
    fn reference(glob: &Glob, rel: &str) -> bool {
        if glob.dir_only || rel.is_empty() {
            return false;
        }
        let rel = rel.trim_end_matches('/');
        let subject = if glob.is_path_pattern() {
            rel
        } else {
            rel.rsplit('/').next().unwrap_or(rel)
        };
        let mut chars: Vec<char> = subject.chars().collect();
        if glob.ignore_case {
            for c in &mut chars {
                c.make_ascii_lowercase();
            }
        }
        glob.alts.iter().any(|toks| matches_tokens(toks, &chars))
    }

    /// Every shape `matches` shortcuts has to answer exactly what the DP answers, or the
    /// listing loses entries. This is the test that keeps the two in step.
    #[test]
    fn shapes_agree_with_the_dp() {
        let patterns = [
            "*.go",
            "*",
            "a*b",
            "a",
            "*x",
            "x*",
            "a*/b",
            "**/x",
            "a/**b",
            "x?.go",
            "a[b-d]*",
            "*.[ch]",
            "src/*.go",
            "**/*.min.js",
            "a/*/b",
            "*/*",
            "x*y*z",
            "*/x",
            "**/src/strings/*.go",
            "*.GP",
            "{a,b}*.txt",
            "{x,}y",
            "a*b*",
            "a*b/c",
            "/x*",
            "x?",
        ];
        let subjects = [
            "",
            "a",
            "b",
            "ab",
            "a.x",
            "x",
            "xa",
            "ax",
            "a/b",
            "x/b",
            "a/x/b",
            "src/a.go",
            "a/x.go",
            "pkg/src/strings/x.go",
            "x/y/z",
            "a.min.js",
            "x.a.min.js",
            "a/xyz",
            "zzz",
            "a.c",
            "b.h",
            "a-b.txt",
            "A.GO",
            "Ab",
            "a/b/c",
            "x??",
            "y",
        ];
        for p in patterns {
            for fold in [false, true] {
                let glob = Glob::new_with(p, fold).expect("patterns in the corpus compile");
                for s in subjects {
                    assert_eq!(
                        glob.matches(s, false),
                        reference(&glob, s),
                        "pattern {p:?} subject {s:?} fold {fold}"
                    );
                    assert_eq!(
                        glob.matches(s, true),
                        reference(&glob, s),
                        "pattern {p:?} subject {s:?} as a directory, fold {fold}"
                    );
                }
            }
        }
    }

    /// A pattern [`byte_token`] accepts reads its subject as one byte per character. That is
    /// only sound if it answers the byte-free DP on every subject, multi-byte characters
    /// included, so this compares the two on text that is not ASCII.
    #[test]
    fn a_byte_safe_pattern_answers_like_the_character_dp() {
        let patterns = [
            "*.go",
            "*.log",
            "*",
            "a*b",
            "build/",
            "pkg/gen/",
            "vendor/**/*.min.js",
            "**/x",
            "a/**",
            "**/*.日本",
            "café/*.txt",
            "[a-z]*",
            "x*é",
            "**/日本/**",
        ];
        let subjects = [
            "",
            "é.go",
            "café/x.go",
            "café/x.txt",
            "aé/b.txt",
            "日本.go",
            "a/日本/b.go",
            "vendor/lib/v1.min.js",
            "vendor/é/v1.min.js",
            "pkg/gen/é.go",
            "build",
            "a/b/c",
            "é/é/é",
            "xé",
            "x/é",
        ];
        // `reference` answers an entry whose directory nature is unknown and refuses every
        // `dir/` pattern, so it is read here with that guard moved out in front of it.
        let dp = |glob: &Glob, s: &str| {
            let rel = s.trim_end_matches('/');
            let subject = if glob.is_path_pattern() {
                rel
            } else {
                rel.rsplit('/').next().unwrap_or(rel)
            };
            if subject.is_empty() {
                return false;
            }
            let mut chars: Vec<char> = subject.chars().collect();
            if glob.ignore_case {
                for c in &mut chars {
                    c.make_ascii_lowercase();
                }
            }
            glob.alts.iter().any(|toks| matches_tokens(toks, &chars))
        };
        let expect =
            |glob: &Glob, s: &str, is_dir: bool| !(glob.dir_only && !is_dir) && dp(glob, s);
        for p in patterns {
            for fold in [false, true] {
                let glob = Glob::new_with(p, fold).expect("corpus compiles");
                for s in subjects {
                    assert_eq!(
                        glob.matches(s, false),
                        expect(&glob, s, false),
                        "pattern {p:?} subject {s:?} fold {fold}"
                    );
                    assert_eq!(
                        glob.matches(s, true),
                        expect(&glob, s, true),
                        "pattern {p:?} subject {s:?} as a directory, fold {fold}"
                    );
                }
            }
        }
    }

    /// The prefilter is a set of necessary conditions, so it can only turn a path away that
    /// the DP would have rejected too. Checked against the DP itself, including for the
    /// alternatives the prefilter belongs to.
    #[test]
    fn the_prefilter_only_rejects_what_the_dp_rejects() {
        let patterns = [
            "vendor/**/*.min.js",
            "**/src/strings/*.go",
            "a/**/f",
            "/x*/**/y?.rs",
            "**{a,b}/*.go",
            "*/**/*.min.js",
        ];
        let subjects = [
            "src/main.go",
            "pkg/src/strings/x.go",
            "vendor/lib/v1.min.js",
            "vendor/lib/v1.js",
            "a/b/f",
            "a/f",
            "x/y2.rs",
            "zz/a.go",
            "b/ab.go",
            "deep/a/b/c.min.js",
        ];
        for p in patterns {
            let glob = g(p);
            for s in subjects {
                let view = PathView::new(s);
                let subject = if glob.is_path_pattern() {
                    view.path()
                } else {
                    view.name()
                };
                let path = subject.as_bytes();
                let dp = glob
                    .alts
                    .iter()
                    .any(|toks| matches_bytes(toks, path, false));
                for (at, (toks, shape)) in glob.alts.iter().zip(&glob.shapes).enumerate() {
                    if *shape != Shape::Full {
                        continue;
                    }
                    if let Some(pre) = &glob.pres[at] {
                        assert!(
                            !matches_bytes(toks, path, false) || pre.may_match(path),
                            "{p:?} prefilter rejects {s:?}, which the DP accepts"
                        );
                    }
                    let _ = dp;
                }
                assert_eq!(
                    glob.matches_view(&view, false),
                    reference(&glob, s),
                    "{p:?} {s:?}"
                );
            }
        }
    }

    /// A trailing `**` matches everything below a directory and not the directory: the
    /// tokens are the literals, the separator, and then `**`, so the separator is part of
    /// the pattern.
    #[test]
    fn a_trailing_double_star_needs_the_separator() {
        assert!(
            !g("foo/**").is_match("foo"),
            "the separator is not optional"
        );
        assert!(g("foo/**").is_match("foo/x"));
        assert!(g("foo/**").is_match("foo/x/y"));
        assert!(!g("foo/**").matches("foo", true));
        assert!(m("foo/**", "foo/x"), "the directory's own entries do match");
    }

    /// Whether an alternative needs a separator at all: `**/` may match nothing, so a
    /// pattern that only reaches a separator through it still matches a bare name.
    #[test]
    fn needs_separator_reads_the_alternatives() {
        assert!(!g("*.go").needs_separator());
        assert!(!g("**").needs_separator());
        assert!(!g("**/x").needs_separator(), "`**/` matches nothing");
        assert!(!g("a**b").needs_separator());
        assert!(g("a/b").needs_separator());
        assert!(
            !g("/x").needs_separator(),
            "an anchoring `/` is not a separator"
        );
        assert!(g("a/**").needs_separator());
        assert!(g("*/x").needs_separator());
        assert!(g("a\\/b").needs_separator(), "an escaped separator is one");
        assert!(!g("{a/b,c}").needs_separator(), "one alternative has none");
        assert!(g("{a/b,c/d}").needs_separator());
    }

    #[test]
    fn star_matches_basename() {
        assert!(m("*.log", "a.log"));
        assert!(m("*.log", "deep/dir/b.log"));
        assert!(!m("*.log", "a.log.txt"));
    }

    #[test]
    fn star_does_not_cross_slash() {
        assert!(!m("a*b", "a/b"));
        assert!(m("a*b", "axxb"));
    }

    #[test]
    fn star_matches_empty() {
        assert!(m("*.rs", ".rs"));
        assert!(m("a*b", "ab"));
        assert!(m("a*b*c", "abc"));
        assert!(!m("a*b*c", "ac"));
    }

    #[test]
    fn question_mark_one_char() {
        assert!(m("?.rs", "a.rs"));
        assert!(!m("?.rs", "ab.rs"));
        assert!(m("a?c", "abc"));
    }

    #[test]
    fn question_mark_not_slash() {
        assert!(!m("a?/b", "a//b"));
        assert!(m("a?/b", "ax/b"));
    }

    #[test]
    fn multiple_stars_collapse() {
        assert!(m("a***b", "axxb"));
        assert!(m("a/***b", "a/x/b"));
        assert!(m("a/****b", "a/x/y/b"));
    }

    #[test]
    fn double_star_crosses_slash() {
        assert!(m("a/**b", "a/x/b"));
        assert!(m("a/**b", "a/b"));
        assert!(m("a**/b", "a/x/b"));
        assert!(!m("a/**c", "a/x/b"));
    }

    #[test]
    fn double_star_prefix_any_depth() {
        assert!(m("**/x", "x"));
        assert!(m("**/x", "a/x"));
        assert!(m("**/x", "a/b/x"));
        assert!(!m("**/x", "a/xy"));
    }

    #[test]
    fn double_star_middle_matches_zero_dirs() {
        assert!(m("a/**/b", "a/b"));
        assert!(m("a/**/b", "a/x/b"));
        assert!(m("a/**/b", "a/x/y/b"));
        assert!(!m("a/**/b", "a/xb"));
        assert!(!m("a/**/b", "ab"));
    }

    #[test]
    fn trailing_double_star_matches_descendants() {
        assert!(m("foo/**", "foo/a"));
        assert!(m("foo/**", "foo/a/b"));
    }

    #[test]
    fn no_slash_matches_deep_basename() {
        assert!(m("build", "build"));
        assert!(m("build", "a/build"));
        assert!(m("build", "a/b/build"));
        assert!(!m("build", "build/x"));
    }

    #[test]
    fn slash_patterns_are_anchored() {
        assert!(m("sub/local.txt", "sub/local.txt"));
        assert!(!m("sub/local.txt", "x/sub/local.txt"));
        assert!(!m("sub/local.txt", "local.txt"));
    }

    #[test]
    fn anchored_leading_slash() {
        assert!(m("/build", "build"));
        assert!(!m("/build", "a/build"));
        assert!(m("/src/*.rs", "src/main.rs"));
        assert!(!m("/src/*.rs", "a/src/main.rs"));
    }

    #[test]
    fn dir_only_pattern_requires_dir() {
        let glob = g("build/");
        assert!(glob.dir_only);
        assert!(glob.matches("build", true));
        assert!(!glob.matches("build", false));
        assert!(!glob.is_match("build"));
    }

    #[test]
    fn dir_only_matches_nested_dir() {
        assert!(md("build/", "a/build"));
        assert!(md("build/", "x/y/build"));
        assert!(!md("build/", "build.rs"));
    }

    #[test]
    fn negation_is_a_field_only() {
        let glob = g("!keep.log");
        assert!(glob.negated);
        assert!(glob.is_match("keep.log"));
        assert!(!g("keep.log").negated);
    }

    #[test]
    fn escaped_bang_is_literal() {
        let glob = g("\\!foo");
        assert!(!glob.negated);
        assert!(glob.is_match("!foo"));
        assert!(!glob.is_match("foo"));
    }

    #[test]
    fn class_basic() {
        assert!(m("[abc].rs", "a.rs"));
        assert!(!m("[abc].rs", "d.rs"));
        assert!(m("[abc]x", "cx"));
    }

    #[test]
    fn class_negated_bang() {
        assert!(m("[!abc].rs", "d.rs"));
        assert!(!m("[!abc].rs", "a.rs"));
    }

    #[test]
    fn class_negated_caret() {
        assert!(m("[^abc].rs", "d.rs"));
        assert!(!m("[^abc].rs", "b.rs"));
    }

    #[test]
    fn class_range() {
        assert!(m("[a-z].rs", "q.rs"));
        assert!(!m("[a-z].rs", "Q.rs"));
        assert!(m("file[0-9][0-9].txt", "file42.txt"));
        assert!(!m("file[0-9][0-9].txt", "file4a.txt"));
    }

    #[test]
    fn class_literal_close_bracket_first() {
        assert!(m("[]]x", "]x"));
        assert!(m("[!]]x", "ax"));
        assert!(!m("[!]]x", "]x"));
    }

    #[test]
    fn class_escaped_members() {
        assert!(m("[\\]]x", "]x"));
        assert!(m("[a\\-z]x", "-x"));
        assert!(m("[a\\-z]x", "zx"));
        assert!(!m("[a\\-z]x", "mx"));
    }

    #[test]
    fn posix_class_names_are_not_supported() {
        // `[[` is a literal member and the first `]` closes the class, so the trailing
        // `]` is a literal character; documented in the module docs.
        let glob = g("[[:alpha:]]");
        assert!(!glob.is_match("a"));
        assert!(glob.is_match("a]"));
    }

    #[test]
    fn class_never_matches_slash() {
        assert!(!m("a/[/]b", "a//b"));
        assert!(!m("a/[!x]/b", "a//b"));
        assert!(m("a/[!x]/b", "a/y/b"));
    }

    #[test]
    fn class_dash_at_edges_is_literal() {
        assert!(m("[-a]x", "-x"));
        assert!(m("[-a]x", "ax"));
        assert!(m("[a-]x", "-x"));
    }

    #[test]
    fn class_unclosed_is_error() {
        assert!(Glob::new("[abc").is_err());
        assert!(Glob::new("[]").is_err());
        assert!(Glob::new("a[b").unwrap_err().contains("unclosed '['"));
    }

    #[test]
    fn class_reversed_range_is_error() {
        let err = Glob::new("[z-a]").unwrap_err();
        assert!(err.contains("reversed character range"), "{err}");
    }

    #[test]
    fn brace_two_alternatives() {
        assert!(m("*.{rs,toml}", "main.rs"));
        assert!(m("*.{rs,toml}", "Cargo.toml"));
        assert!(!m("*.{rs,toml}", "notes.md"));
    }

    #[test]
    fn brace_nested() {
        let glob = g("*.{rs,{toml,yaml}}");
        assert_eq!(glob.alts.len(), 3);
        assert!(glob.is_match("a.yaml"));
        assert!(glob.is_match("a.rs"));
        assert!(!glob.is_match("a.yml"));
    }

    #[test]
    fn brace_single_alternative_drops_braces() {
        let glob = g("{a}bc");
        assert_eq!(glob.alts.len(), 1);
        assert!(glob.is_match("abc"));
        assert!(!glob.is_match("{a}bc"));
    }

    #[test]
    fn brace_empty_alternative() {
        assert!(m("a{,b}c", "ac"));
        assert!(m("a{,b}c", "abc"));
    }

    #[test]
    fn brace_unclosed_is_error() {
        let err = Glob::new("a{b,c").unwrap_err();
        assert!(err.contains("unclosed '{'"), "{err}");
        assert!(err.contains("offset 1"), "{err}");
    }

    #[test]
    fn brace_unopened_is_error() {
        let err = Glob::new("ab}c").unwrap_err();
        assert!(err.contains("unopened '}'"), "{err}");
        assert!(err.contains("offset 2"), "{err}");
    }

    #[test]
    fn brace_escaped_is_literal() {
        assert!(m("a\\{b,c\\}", "a{b,c}"));
        assert!(!m("a\\{b,c\\}", "ab"));
    }

    #[test]
    fn brace_inside_class_is_literal() {
        assert!(m("[{a}]x", "{x"));
        assert!(m("[{a}]x", "ax"));
    }

    #[test]
    fn brace_with_slash_anchors_path() {
        assert!(m("{src,tests}/main.rs", "tests/main.rs"));
        assert!(!m("{src,tests}/main.rs", "lib/main.rs"));
    }

    #[test]
    fn brace_too_many_alternatives_is_error() {
        let err = Glob::new(&"{a,b}".repeat(13)).unwrap_err();
        assert!(err.contains("too many brace alternatives"), "{err}");
        assert!(Glob::new(&"{a,b}".repeat(10)).is_ok());
    }

    #[test]
    fn escape_star_is_literal() {
        assert!(m("a\\*b", "a*b"));
        assert!(!m("a\\*b", "axb"));
    }

    #[test]
    fn escape_question_is_literal() {
        assert!(m("a\\?c", "a?c"));
        assert!(!m("a\\?c", "abc"));
    }

    #[test]
    fn escape_bracket_is_literal() {
        assert!(m("\\[abc\\]", "[abc]"));
        assert!(!m("\\[abc\\]", "a"));
    }

    #[test]
    fn escape_backslash_is_literal() {
        let glob = g("a\\\\b");
        assert_eq!(glob.alts[0].len(), 3);
        assert!(!glob.is_match("a/b"));
        assert!(!glob.is_match("axb"));
    }

    #[test]
    fn escape_trailing_backslash_is_error() {
        let err = Glob::new("abc\\").unwrap_err();
        assert!(err.contains("trailing backslash"), "{err}");
        assert!(Glob::new("[a\\").is_err());
    }

    #[test]
    fn escape_comma_inside_brace() {
        let glob = g("{a\\,b,c}");
        assert_eq!(glob.alts.len(), 2);
        assert!(glob.is_match("a,b"));
        assert!(glob.is_match("c"));
    }

    #[test]
    fn ignore_star_log() {
        let glob = g("*.log");
        assert!(glob.is_match("app.log"));
        assert!(glob.is_match("logs/app.log"));
        assert!(!glob.is_match("app.txt"));
    }

    #[test]
    fn ignore_build_dir_only() {
        let glob = g("build/");
        assert!(glob.dir_only);
        assert!(glob.matches("build", true));
        assert!(glob.matches("crate/build", true));
        assert!(!glob.matches("build", false));
        assert!(!glob.matches("build/log", false));
    }

    #[test]
    fn ignore_negated_keep_log() {
        let glob = g("!keep.log");
        assert!(glob.negated);
        assert!(glob.is_match("keep.log"));
        assert!(!glob.is_match("keep.log.bak"));
    }

    #[test]
    fn ignore_sub_local_txt() {
        let glob = g("sub/local.txt");
        assert!(glob.is_path_pattern());
        assert!(glob.is_match("sub/local.txt"));
        assert!(!glob.is_match("other/sub/local.txt"));
    }

    #[test]
    fn ignore_walk_simulation() {
        let globs = [g("*.log"), g("build/"), g("!keep.log"), g("sub/local.txt")];
        let ignored = |path: &str, dir: bool| {
            let mut hit = false;
            for glob in &globs {
                if glob.matches(path, dir) {
                    hit = !glob.negated;
                }
            }
            hit
        };
        assert!(ignored("a.log", false));
        assert!(!ignored("keep.log", false));
        assert!(ignored("build", true));
        assert!(ignored("deep/build", true));
        assert!(ignored("sub/local.txt", false));
        assert!(!ignored("sub/other.txt", false));
        assert!(!ignored("build", false));
    }

    #[test]
    fn empty_pattern_is_error() {
        assert!(Glob::new("").is_err());
        assert!(Glob::new("!").is_err());
        assert!(Glob::new("/").is_err());
    }

    #[test]
    fn is_path_pattern_flags() {
        assert!(!g("*.log").is_path_pattern());
        assert!(!g("build/").is_path_pattern());
        assert!(g("sub/x").is_path_pattern());
        assert!(g("/build").is_path_pattern());
        assert!(g("**/x").is_path_pattern());
    }

    #[test]
    fn pattern_and_display_keep_raw_text() {
        let glob = g("!build/**");
        assert_eq!(glob.pattern(), "!build/**");
        assert_eq!(format!("{glob}"), "!build/**");
        assert_eq!(g("build/").to_string(), "build/");
    }

    #[test]
    fn new_equals_new_with_false() {
        let pattern = "src/**/{a,b}?.rs";
        let plain = Glob::new(pattern).unwrap();
        let folded = Glob::new_with(pattern, false).unwrap();
        for path in ["src/a.rs", "src/x/y/b1.rs", "src/A.RS"] {
            assert_eq!(plain.is_match(path), folded.is_match(path));
        }
        assert!(plain.is_match("src/x/y/b1.rs"));
        assert!(!plain.is_match("src/A.RS"));
    }

    #[test]
    fn unicode_question_mark_is_one_char() {
        assert!(g("?").is_match("é"));
        assert!(g("??").is_match("éé"));
        assert!(!g("??").is_match("é"));
        assert!(g("a?c").is_match("a€c"));
    }

    #[test]
    fn unicode_literal_match() {
        assert!(g("café/*.txt").is_match("café/x.txt"));
        assert!(!g("cafe/*.txt").is_match("café/x.txt"));
        assert!(g("*.日本").is_match("a.日本"));
    }

    #[test]
    fn case_insensitive_basic() {
        assert!(gi("*.LOG").is_match("a.log"));
        assert!(gi("*.log").is_match("A.LOG"));
        assert!(!g("*.LOG").is_match("a.log"));
    }

    #[test]
    fn case_insensitive_class_and_path() {
        assert!(gi("[A-Z].txt").is_match("q.txt"));
        assert!(gi("Src/*.RS").is_match("src/main.rs"));
        assert!(gi("[!A]x").is_match("bx"));
        assert!(!gi("[!A]x").is_match("ax"));
    }

    #[test]
    fn matches_on_empty_rel() {
        assert!(!g("*").is_match(""));
        assert!(!g("**").is_match(""));
    }

    /// `matches` on a path split once, which is how one entry is tested against a stack of
    /// patterns. Every test below goes through it, so the two spellings stay in step.
    fn mv(p: &str, rel: &str, is_dir: bool) -> bool {
        let glob = g(p);
        let split = glob.matches_view(&PathView::new(rel), is_dir);
        assert_eq!(
            split,
            glob.matches(rel, is_dir),
            "{p:?} on {rel:?}, is_dir {is_dir}"
        );
        split
    }

    #[test]
    fn double_star_answers_at_one_component_and_at_six() {
        assert!(mv("**/x", "x", false), "**/ matches nothing at all");
        assert!(mv("**/x", "a/b/c/d/e/x", false));
        assert!(!mv("**/x", "a/b/c/d/e/f", false));
        assert!(mv("**", "x", false), "a lone ** is anything");
        assert!(mv("**", "a/b/c/d/e/f", false));
        assert!(mv("a/**/f", "a/f", false));
        assert!(mv("a/**/f", "a/b/c/d/e/f", false));
        assert!(md("a/**", "a/b/c/d/e/f"));
        assert!(mv("a/**", "a/b", true));
        assert!(
            !mv("a/**", "a", true),
            "a/** is a/ then **, so it needs the slash"
        );
    }

    #[test]
    fn brace_alternatives_each_carry_their_star() {
        assert!(mv("*.{rs,toml}", "main.rs", false));
        assert!(mv("*.{rs,toml}", "Cargo.toml", false));
        assert!(!mv("*.{rs,toml}", "main.md", false));
        assert!(mv("{a*,b*}.txt", "abc.txt", false));
        assert!(mv("{a*,b*}.txt", "bc.txt", false));
        assert!(!mv("{a*,b*}.txt", "c.txt", false));
        assert!(mv("{*,}/*.md", "notes/x.md", false), "a */ prefix");
        assert!(mv("{*,}/*.md", "/x.md", false), "the empty alternative");
        assert!(mv("**/{src,tests}/*.rs", "a/b/src/main.rs", false));
        assert!(!mv("**/{src,tests}/*.rs", "a/b/lib/main.rs", false));
    }

    #[test]
    fn a_trailing_slash_on_the_subject_is_the_path_without_it() {
        assert!(mv("*.go", "a.go/", false), "the trailing slash is dropped");
        assert!(mv("build/", "x/build/", true));
        assert!(
            !mv("build/", "x/build/", false),
            "dir_only still needs a dir"
        );
        assert!(
            !mv("build/*", "build/", true),
            "the path is left with no child"
        );
        assert!(mv("**/x", "a/b/x/", false));
        assert!(mv("a/**", "a/b/", true));
    }

    #[test]
    fn an_entry_named_exactly_like_a_pattern() {
        assert!(
            mv("*.log", "*.log", false),
            "the wildcard matches a literal *"
        );
        assert!(mv("*", "*", false));
        assert!(
            mv("a\\*b", "a*b", false),
            "an escaped star is that character"
        );
        assert!(!mv("a\\*b", "axb", false));
        assert!(mv("**/*", "**/*", false));
        assert!(
            !mv("[a-z].txt", "[a-z].txt", false),
            "a class is one character"
        );
    }

    #[test]
    fn backslash_paths_are_normalised() {
        assert!(g("a/*.rs").is_match("a\\main.rs"));
        assert!(md("build/", "x\\build"));
    }

    #[test]
    fn wildcard_pathological_pattern_is_fast() {
        let path = "ab".repeat(1000);
        let start = Instant::now();
        let hit = m("*a*a*a*a*a*a*a*b", &path);
        let elapsed = start.elapsed();
        assert!(hit);
        assert!(elapsed.as_millis() < 100, "took {elapsed:?}");
        let start = Instant::now();
        let miss = m("*a*a*a*a*a*a*a*c", &path);
        assert!(!miss);
        assert!(start.elapsed().as_millis() < 100);
    }

    #[test]
    fn fuzz_never_panics() {
        let atoms = [
            "a", "b", "/", "*", "**", "?", "[", "]", "!", "-", "{", "}", ",", "\\", ".", "é",
            "**/", "[a-z]", "{a,b}", "0",
        ];
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };
        for _ in 0..2000 {
            let mut pat = String::new();
            let len = 1 + next(8);
            for _ in 0..len {
                pat.push_str(atoms[next(atoms.len())]);
            }
            let mut path = String::new();
            for _ in 0..len {
                path.push_str(["a", "b", "/", ".", "é", "A", "0"][next(7)]);
            }
            let ci = next(2) == 1;
            match Glob::new_with(&pat, ci) {
                Ok(glob) => {
                    let _ = glob.matches(&path, next(2) == 1);
                    let _ = glob.is_match(&path);
                    let _ = glob.is_path_pattern();
                    assert_eq!(glob.pattern(), pat);
                }
                Err(msg) => assert!(!msg.is_empty(), "error for {pat:?} must explain itself"),
            }
        }
    }
}

//! Glob matching for `--glob`, `--iglob` and ignore files.
//!
//! One [`Glob`] compiles one gitignore-style pattern and matches it against a path
//! relative to the walk root. Matching is character based, not byte based: `?` and a
//! character class consume exactly one Unicode scalar value, so `?` matches `é`.
//!
//! Supported syntax:
//!
//! - `*` matches any run of characters except `/`; `?` matches one such character.
//! - `**` matches any run of characters including `/`, wherever it appears in a path
//!   pattern (`a/**b` matches `a/x/b`). `**/` additionally matches nothing at all, so
//!   `a/**/b` matches `a/b` and `**/x` matches `x` at any depth. Consequence: a trailing `**` also
//!   matches the directory itself — `foo/**` matches `foo`, where git does not.
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
//! panic, or backtrack exponentially: cost is O(tokens * path characters) per
//! alternative with O(path) memory.

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

/// One compiled glob pattern.
#[derive(Debug, Clone)]
pub struct Glob {
    raw: String,
    alts: Vec<Vec<Tok>>,
    /// Pattern began with `!`; matching is not inverted, the caller decides.
    pub negated: bool,
    /// Pattern ended with `/`; it only matches directories.
    pub dir_only: bool,
    anchored: bool,
    has_slash: bool,
    ignore_case: bool,
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
        Ok(Glob {
            raw: pattern.to_string(),
            alts,
            negated,
            dir_only,
            anchored,
            has_slash: body.contains('/'),
            ignore_case,
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

    /// Match an entry whose directory nature is unknown; a `dir/`-style pattern never
    /// matches. Matching ignores [`Glob::negated`].
    pub fn is_match(&self, rel: &str) -> bool {
        self.matches(rel, false)
    }

    /// Match `rel` (relative to the walk root) as a directory or a file.
    pub fn matches(&self, rel: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if rel.is_empty() {
            return false;
        }
        let rel = rel.trim_end_matches('/');
        let owned;
        let rel = if rel.contains('\\') {
            owned = rel.replace('\\', "/");
            owned.as_str()
        } else {
            rel
        };
        let subject = if self.is_path_pattern() {
            rel
        } else {
            rel.rsplit('/').next().unwrap_or(rel)
        };
        let mut chars: Vec<char> = subject.chars().collect();
        if self.ignore_case {
            for c in &mut chars {
                c.make_ascii_lowercase();
            }
        }
        self.alts.iter().any(|toks| matches_tokens(toks, &chars))
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

/// Suffix DP: `row[j]` says whether the token suffix at this row matches `path[j..]`.
/// Rows run from the end of the token list back to its start, so nothing recurses.
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

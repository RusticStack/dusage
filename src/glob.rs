//! Shell-style glob matching for `--glob`, `--type`, path search and ignore files.
//!
//! Supported syntax: `*` (any run of characters except `/`), `?` (one character except
//! `/`), `**` (any run including `/`, and `**/` may match nothing at all), `[...]` and
//! `[!...]` character classes with `a-z` ranges, `\` escapes, a trailing `/` that means
//! "directories only", and a leading `!` that marks the pattern as a negation (the caller
//! decides what to do with it). A pattern without a `/` matches the file name of any
//! entry, the way `.gitignore` treats it; a pattern with a `/` is anchored at the root.

use std::fmt;

/// One parsed glob.
#[derive(Debug, Clone)]
pub struct Glob {
    src: String,
    /// Characters of the body, without the `!` prefix.
    pat: Vec<char>,
    /// The pattern was written with a `!` prefix.
    pub negated: bool,
    /// The pattern ends with `/`.
    pub dir_only: bool,
    /// A `/` appears in the body, so it matches paths rather than names.
    anchored: bool,
}

impl Glob {
    pub fn new(pattern: &str) -> Result<Glob, String> {
        let mut p = pattern.trim();
        if p.is_empty() {
            return Err("empty glob".to_string());
        }
        let mut negated = false;
        if let Some(rest) = p.strip_prefix('!') {
            if rest.is_empty() {
                return Err(format!("invalid glob {pattern:?}: bare '!'"));
            }
            negated = true;
            p = rest;
        }
        let dir_only = p.ends_with('/');
        if dir_only {
            p = p.trim_end_matches('/');
        }
        let body = p.trim_start_matches('/');
        if body.is_empty() {
            return Err(format!("invalid glob {pattern:?}: no pattern"));
        }
        validate(body, pattern)?;
        Ok(Glob {
            src: pattern.to_string(),
            pat: body.chars().collect(),
            negated,
            dir_only,
            anchored: body.contains('/'),
        })
    }

    pub fn pattern(&self) -> &str {
        &self.src
    }

    /// True when this glob has to be matched against a relative path, not a file name.
    pub fn is_path_pattern(&self) -> bool {
        self.anchored
    }

    /// Match a relative path (`a/b/c.txt`) or, for a name pattern, the entry's name.
    pub fn is_match(&self, rel: &str) -> bool {
        self.matches(rel, true)
    }

    /// Like [`Glob::is_match`] but honours a trailing `/` on the pattern.
    pub fn matches(&self, rel: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let rel = rel.trim_start_matches('/');
        if self.anchored {
            if wildcard(&self.pat, &chars(rel)) {
                return true;
            }
            // `**/x` and `a/**` also match deeper spellings; catch `dir/**` style tails.
            return false;
        }
        // Unanchored: match the whole relative path or just its last component, so
        // `*.rs` and `node_modules` behave like their gitignore counterparts.
        if wildcard(&self.pat, &chars(rel)) {
            return true;
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        wildcard(&self.pat, &chars(name))
    }
}

impl fmt::Display for Glob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.src)
    }
}

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn validate(body: &str, orig: &str) -> Result<(), String> {
    let mut it = body.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                if it.next().is_none() {
                    return Err(format!("invalid glob {orig:?}: trailing backslash"));
                }
            }
            '[' => {
                let mut closed = false;
                let mut first = true;
                while let Some(d) = it.next() {
                    if d == ']' && !first {
                        closed = true;
                        break;
                    }
                    first = false;
                }
                if !closed {
                    return Err(format!("invalid glob {orig:?}: unterminated class"));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Iterative wildcard match with backtracking for `*` and `**`.
fn wildcard(pat: &[char], text: &[char]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    // Backtracking state: (pattern index after the star, text index at the star).
    let mut star: Option<(usize, usize)> = None;
    while ti < text.len() {
        if pi < pat.len() {
            match pat[pi] {
                '*' => {
                    let doublestar = pi + 1 < pat.len() && pat[pi + 1] == '*';
                    let mut next = pi + 1;
                    while next < pat.len() && pat[next] == '*' {
                        next += 1;
                    }
                    // `**/` may match nothing, so skip the following slash too.
                    if doublestar && next < pat.len() && pat[next] == '/' {
                        if wildcard(&pat[next + 1..], &text[ti..]) {
                            return true;
                        }
                    }
                    star = Some((next, ti));
                    pi = next;
                    continue;
                }
                '?' => {
                    if text[ti] == '/' {
                        // Fall through to the star backtrack, if any.
                    } else {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
                '[' => {
                    if let Some((ok, end)) = class(pat, pi, text[ti]) {
                        if ok {
                            pi = end;
                            ti += 1;
                            continue;
                        }
                    }
                }
                '\\' if pi + 1 < pat.len() => {
                    if pat[pi + 1] == text[ti] {
                        pi += 2;
                        ti += 1;
                        continue;
                    }
                }
                c => {
                    if c == text[ti] {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
            }
        }
        // Mismatch: retry inside the last star, growing it by one character.
        if let Some((sp, st)) = star {
            if st < text.len() {
                star = Some((sp, st + 1));
                pi = sp;
                ti = st + 1;
                continue;
            }
        }
        return false;
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Match one character class starting at `pat[pi] == '['`.
/// Returns (matched, index just past the class), or None when the class is malformed.
fn class(pat: &[char], pi: usize, c: char) -> Option<(bool, usize)> {
    let mut i = pi + 1;
    let negated = matches!(pat.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    while i < pat.len() {
        let a = pat[i];
        if a == ']' && !first {
            return Some((hit != negated, i + 1));
        }
        first = false;
        let a = if a == '\\' && i + 1 < pat.len() {
            i += 1;
            pat[i]
        } else {
            a
        };
        if i + 2 < pat.len() && pat[i + 1] == '-' && pat[i + 2] != ']' {
            let b = pat[i + 2];
            if a <= c && c <= b {
                hit = true;
            }
            i += 3;
        } else {
            if a == c {
                hit = true;
            }
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(p: &str) -> Glob {
        Glob::new(p).unwrap()
    }

    fn m(p: &str, rel: &str) -> bool {
        g(p).matches(rel, false)
    }

    #[test]
    fn stars_and_questions() {
        assert!(m("*.rs", "src/main.rs"));
        assert!(m("*.rs", "main.rs"));
        assert!(!m("*.rs", "main.rss"));
        assert!(m("ma?n.rs", "main.rs"));
        assert!(!m("ma?n.rs", "ma/n.rs"), "? must not cross a slash");
        assert!(m("src/*.rs", "src/main.rs"));
        assert!(!m("src/*.rs", "src/deep/main.rs"));
        assert!(m("src/**/*.rs", "src/deep/main.rs"));
        assert!(m("src/**/main.rs", "src/main.rs"), "**/ may match nothing");
        assert!(m("**/*.rs", "a/b/c.rs"));
        assert!(m("*.rs", ".config/rustfmt.rs"));
        assert!(!m("a*c", "ab/dc"), "* must not cross a slash");
    }

    #[test]
    fn classes_escapes_and_ranges() {
        assert!(m("file[0-9].txt", "file7.txt"));
        assert!(!m("file[0-9].txt", "filex.txt"));
        assert!(m("file[!0-9].txt", "filex.txt"));
        assert!(m("[a-c]at", "bat"));
        assert!(m("\\*.rs", "*.rs"));
        assert!(!m("\\*.rs", "x.rs"));
        assert!(m("a[]]b", "a]b"), "leading ] is literal");
        assert_eq!(Glob::new("[abc"), Err("invalid glob \"[abc\": unterminated class".into()));
        assert!(Glob::new("a\\").is_err());
    }

    #[test]
    fn negation_and_directory_only() {
        let gt = g("!target/");
        assert!(gt.negated);
        assert!(gt.dir_only);
        assert!(gt.matches("target", true));
        assert!(!gt.matches("target", false));
        assert!(g("node_modules").matches("node_modules", true));
        assert!(g("node_modules").matches("a/node_modules", true));
    }

    #[test]
    fn display_and_errors() {
        assert_eq!(g("*.rs").to_string(), "*.rs");
        assert!(Glob::new("").is_err());
        assert!(Glob::new("!").is_err());
        assert!(Glob::new("/").is_err());
    }

    #[test]
    fn deep_paths_do_not_explode() {
        // A pathological pattern and text stay fast (no exponential backtracking).
        let mut text = String::new();
        for _ in 0..500 {
            text.push_str("ab");
        }
        let pat = "*a*a*a*a*a*a*a*a*a*a*b";
        assert!(!g(pat).is_match(&text));
        let text: String = std::iter::repeat_n('a', 4000).collect();
        assert!(g("*a*b*").is_match(&format!("{text}b")));
    }
}

//! `dusage search` (grep/rg replacement) and `dusage find` (file name and path search).

use super::Fail;
use crate::cli::{self, parse_age, parse_size, Args};
use crate::encodings::Encoding;
use crate::glob::Glob;
use crate::pattern::Pattern;
use crate::report;
use crate::search::{search, Scope, SearchOptions, Segment, SortKey};
use std::io::Read;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::SystemTime;

const SEARCH_HELP: &str = "dusage search - search file contents (grep/rg replacement)

USAGE:
    dusage search [OPTIONS] <pattern> [paths...]
    dusage grep  [OPTIONS] <pattern> [paths...]      (alias)

PATTERN SYNTAX (regular expressions):
    literals, . * + ? {n,m} [...] [^...] ( ) (?: ) | ^ $ \\A \\z \\b \\B \\d \\w \\s
    and their negations; inline flags (?i) (?s) (?m). No lookaround or backreferences.
    A pattern that is a plain literal is matched with a much faster byte scan.

OPTIONS:
    -F, --fixed-strings    treat the pattern as literal text
    -E, --extended-regexp  the default dialect, accepted for grep compatibility
    -G, --basic-regexp     read the pattern as a POSIX basic regular expression
    -P, --pcre2            force the backtracking engine (lookaround, backreferences)
    -i, --ignore-case      case-insensitive (ASCII folding)
    -s, --case-sensitive   case-sensitive, overriding an earlier -i
    -S, --smart-case       ignore case unless the pattern holds an uppercase letter
    -w, --word-regexp      match whole words only
    -v, --invert-match     select lines that do not match
    -e, --regexp PATTERN   pattern to search for; repeatable
    -f, --file PATH        read patterns from PATH, one per line; repeatable
    -n, --line-number      print line numbers
        --column           print the column of the first match on each line
        --byte-offset      print the byte offset of each line within its file
        --heading          print the file name on its own line above its matches
        --line-regexp      match only whole lines
    -H, --with-filename    always print the file name
    -h, --no-filename      never print the file name
    -l, --files-with-matches
                           print only the names of matching files
    -L, --files-without-match
                           print only the names of files with no match
    -c, --count            print the number of matching lines per file
        --include-zero     with -c, also print files that matched nothing
        --count-matches    print the number of individual matches per file
    -o, --only-matching    print only the matching part of each line
    -z, --search-zip       expand gzip files and search their contents
    -U, --multiline        let a match span lines: the file is searched whole
        --multiline-dotall with -U, also let '.' match a newline
    -r, --replace TEXT     print each line with its matches replaced by TEXT;
                           $0 is the whole match, $1..$9 its groups, $$ a literal $
    -q, --quiet            print nothing; exit 0 on the first match
        --passthru         print every line, matching or not (matched lines stay marked)
    -m, --max-count N      stop after N matching lines per file
    -A, --after-context N  print N lines after each match
    -B, --before-context N print N lines before each match
    -C, --context N        print N lines of context around each match (same as -A N -B N)
    -a, --text             search binary files instead of reporting them
    -j, --threads N        worker threads (default: one per core, 4 to 16)
    -g, --glob PATTERN     include files matching a glob; prefix with ! to exclude
        --include PATTERN  grep's spelling of a narrowing glob; repeatable
        --exclude PATTERN  grep's spelling of an exclusion glob; repeatable
        --exclude-dir PATTERN  exclude directories by this glob; repeatable
        --binary-files T    binary, text or without-match: what a NUL-carrying file is
    -I                     the same as --binary-files without-match
    -d, --directories A    recurse (default) or skip: whether to descend into directories
        --iglob PATTERN    the same, matched without case
    -t, --type NAME        include files of a type: rust, python, js, ts, c, cpp, go, java,
                           ruby, markdown, json, toml, yaml, sh, html, css, xml, txt, log, lock
    -T, --type-not NAME    exclude a type; repeatable
        --type-list        print the type names and their extensions
    -u, --unrestricted     repeatable: -u drops ignore rules, -uu also looks at hidden paths,
                           -uuu also searches binary files
    --max-depth N          do not descend more than N levels below each root
    --max-filesize SIZE    skip files larger than SIZE (e.g. 10M)
    --max-columns N        truncate printed lines at N bytes
        --max-columns-preview  say so when a line was cut
    --encoding NAME        auto (default), utf-8, utf-16le, utf-16be, latin1
    --files                with no pattern, list the files a search would read
    -Z, --null             NUL instead of ':' after each file name (for xargs -0)

A path of `-` is standard input, and with no paths a piped stdin is searched instead of `.`.
    --sort KEY             result order: path (default), none, modified, accessed, created
                           (each oldest first)
    --sortr KEY            the same, reversed
    --hidden               also search hidden files and directories
    --no-ignore            do not skip .gitignore/.ignore rules or cache directories
    --no-ignore-vcs        read neither .gitignore nor .git/info/exclude
    --no-ignore-dot        do not read .ignore files
    --no-ignore-parent     do not read the ignore files above a root
    --no-ignore-files      read no ignore file at all (the three switches above)
    --no-ignore-global     accepted for compatibility; this never reads a global ignore file
    --follow               follow symbolic links, walking each real directory once
                           (ripgrep spells this -L; here -L means --files-without-match)
    -x, --one-filesystem   stay on the starting filesystem
    --json                 machine-readable output
    --color[=WHEN]         WHEN is always, never or auto (default auto)
    --stats                print a summary line on stderr
        --no-messages      do not report paths that could not be read
    -h, --help             this text (note: -h alone means --no-filename)
    -V, --version          print the version

Exit status: 0 when something matched, 1 when nothing did, 2 on an error.";

const FIND_HELP: &str = "dusage find - find files and directories by name or path

USAGE:
    dusage find [OPTIONS] <glob> [paths...]
    dusage ls   [OPTIONS] <glob> [paths...]      (alias)

The pattern is a glob matched against the path relative to each root, or against the file
name when it has no '/': * ? **, [...] and [!...] classes, and a trailing / for directories.
Prefix with ! to exclude instead. Use --fixed for a plain substring match on the path.

OPTIONS:
    -F, --fixed            match the pattern as a substring of the path
    -i, --ignore-case      match the glob without case
    --name                 match the glob against the file name only
    --full-path            match the glob against the whole relative path
    --mindepth N           skip anything fewer than N levels below a root
    -t, --type NAME        restrict to a type (see `dusage search --help` for the list)
    --ext rs,md            restrict to these extensions
    --size +10M            at least 10M; -10M at most; 10M within 5%; repeatable
    --older 7d             last modified more than 7d ago
    --newer 7d             last modified less than 7d ago
    --dirs                 directories only
    --files                files only
    --long                 print size, modification time and path
    --max-depth N          do not descend more than N levels below each root
    -g, --glob PATTERN     include files matching a glob; prefix with ! to exclude
        --include PATTERN  grep's spelling of a narrowing glob; repeatable
        --exclude PATTERN  grep's spelling of an exclusion glob; repeatable
        --exclude-dir PATTERN  exclude directories by this glob; repeatable
        --binary-files T    binary, text or without-match: what a NUL-carrying file is
    -I                     the same as --binary-files without-match
    -d, --directories A    recurse (default) or skip: whether to descend into directories
        --iglob PATTERN    the same, matched without case
    --sort KEY             result order: path (default), none, modified, accessed, created
                           (each oldest first)
    --sortr KEY            the same, reversed
    --hidden               also look at hidden files and directories
    --no-ignore            do not skip .gitignore/.ignore rules or cache directories
    --no-ignore-vcs        read neither .gitignore nor .git/info/exclude
    --no-ignore-dot        do not read .ignore files
    --no-ignore-parent     do not read the ignore files above a root
    --no-ignore-files      read no ignore file at all (the three switches above)
    --no-ignore-global     accepted for compatibility; this never reads a global ignore file
    --follow               follow symbolic links, walking each real directory once
                           (ripgrep spells this -L; here -L means --files-without-match)
    -x, --one-filesystem   stay on the starting filesystem
    --json                 machine-readable output
    --color[=WHEN]         WHEN is always, never or auto (default auto)
    -h, --help             this text
    -V, --version          print the version

Exit status: 0 when something matched, 1 when nothing did, 2 on an error.";

/// Write a whole cold-path output (`--help`, `--version`, `--type-list`) and treat a reader
/// that went away as success, since `println!` would panic on it.
fn write_out(text: &str) -> Result<(), Fail> {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(Fail::output)
}

/// `--color=WHEN`, which the shared parser cannot express as a value flag.
fn take_color_flag(raw: &mut Vec<String>) -> Option<String> {
    let mut mode = None;
    let mut i = 0;
    while i < raw.len() {
        if let Some(v) = raw[i].strip_prefix("--color=") {
            mode = Some(v.to_string());
            raw.remove(i);
            continue;
        }
        i += 1;
    }
    mode
}

/// Colour only for an explicit yes, or for `auto` (and no flag) on a terminal.
fn color_enabled(mode: Option<&str>) -> bool {
    match mode {
        Some("always" | "yes" | "true") => true,
        Some("never" | "no" | "false") => false,
        _ => std::io::stdout().is_terminal(),
    }
}

/// One pattern that matches any of `patterns`, for a repeated `-e`. Literal patterns are
/// escaped individually; the rest are wrapped so a top-level `|` in one cannot leak.
fn alternation(patterns: &[String], fixed: bool) -> String {
    let escape = |p: &str| -> String {
        let mut out = String::with_capacity(p.len() + 4);
        for c in p.chars() {
            if matches!(
                c,
                '\\' | '^' | '$' | '.' | '[' | ']' | '{' | '}' | '(' | ')' | '|' | '*' | '+' | '?'
            ) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    if patterns.len() == 1 {
        return if fixed {
            escape(&patterns[0])
        } else {
            patterns[0].clone()
        };
    }
    patterns
        .iter()
        .map(|p| {
            let body = if fixed { escape(p) } else { p.clone() };
            format!("(?:{body})")
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// The patterns in one `-f` file: one per line, blank lines skipped, CRLF accepted.
fn read_pattern_file(path: &str) -> Result<Vec<String>, Fail> {
    let data = match path {
        // A pattern file of `-` is the standard input, as in grep, but only when the haystack
        // is not itself the standard input: two readers of one stream cannot both have it.
        "-" => {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| Fail::from(format!("-f -: {e}")))?;
            buf
        }
        _ => std::fs::read(path).map_err(|e| Fail::from(format!("-f {path}: {e}")))?,
    };
    let text = String::from_utf8_lossy(&data);
    Ok(text
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Every type `-t` accepts, with the extensions it stands for, in the order `--type-list`
/// prints them.
const TYPE_TABLE: &[(&str, &[&str])] = &[
    ("rust", &["rs"]),
    ("python", &["py", "pyi"]),
    ("js", &["js", "jsx", "mjs", "cjs"]),
    ("ts", &["ts", "tsx"]),
    ("c", &["c", "h"]),
    ("cpp", &["cpp", "cc", "cxx", "hpp", "hh", "hxx"]),
    ("go", &["go"]),
    ("java", &["java"]),
    ("ruby", &["rb"]),
    ("markdown", &["md", "markdown"]),
    ("json", &["json"]),
    ("toml", &["toml"]),
    ("yaml", &["yaml", "yml"]),
    ("sh", &["sh", "bash", "zsh"]),
    ("html", &["html", "htm"]),
    ("css", &["css"]),
    ("xml", &["xml"]),
    ("txt", &["txt"]),
    ("log", &["log"]),
    ("lock", &["lock"]),
];

/// The type names the flags accept: the built-in table, then whatever `--type-add` taught
/// it, minus whatever `--type-clear` removed. A name stands for the globs its extensions
/// spell, so `-t rust` is `-g '*.rs'` without the side effect of an explicit glob.
struct Types {
    table: Vec<(String, Vec<String>)>,
}

impl Types {
    fn new() -> Types {
        Types {
            table: TYPE_TABLE
                .iter()
                .map(|(name, exts)| {
                    let globs = exts.iter().map(|e| format!("*.{e}")).collect();
                    (name.to_string(), globs)
                })
                .collect(),
        }
    }

    /// `--type-add NAME:GLOB[,GLOB]`, or `NAME:+GLOB` to extend a name that exists.
    fn add(&mut self, spec: &str) -> Result<(), Fail> {
        let (name, rest) = spec
            .split_once(':')
            .ok_or_else(|| Fail(2, format!("--type-add wants NAME:GLOB, not {spec:?}")))?;
        let globs = rest
            .trim_start_matches('+')
            .split(',')
            .filter(|g| !g.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if name.is_empty() || globs.is_empty() {
            return Err(Fail(2, format!("--type-add wants NAME:GLOB, not {spec:?}")));
        }
        // Naming a type that already exists adds the globs to it, with or without the `+`,
        // which is what ripgrep does; `--type-clear` first is how a type gets redefined.
        match self.table.iter_mut().find(|(n, _)| n == name) {
            Some((_, have)) => have.extend(globs),
            None => self.table.push((name.to_string(), globs)),
        }
        Ok(())
    }

    /// `--type-clear NAME`: drop a name, so it can be redefined without `+`. A name that is
    /// not there is not an error, which is how ripgrep treats it.
    fn clear(&mut self, name: &str) {
        self.table.retain(|(n, _)| n != name);
    }

    fn globs(&self, name: &str) -> Result<Vec<String>, String> {
        match self.table.iter().find(|(n, _)| n == name) {
            Some((_, globs)) => Ok(globs.clone()),
            None => Err(format!(
                "unknown type {name:?} (see 'dusage search --help' for the list)"
            )),
        }
    }
}

const SEARCH_VALUE_FLAGS: &[&str] = &[
    "-e",
    "--regexp",
    "-f",
    "--file",
    "-r",
    "--replace",
    "-T",
    "--type-not",
    "--type-add",
    "--type-clear",
    "--iglob",
    "-A",
    "--after-context",
    "-B",
    "--before-context",
    "-C",
    "--context",
    "-m",
    "--max-count",
    "-g",
    "--glob",
    "-t",
    "--type",
    "-j",
    "--threads",
    "--max-depth",
    "--max-filesize",
    "--max-columns",
    "--encoding",
    "--include",
    "--exclude",
    "--exclude-dir",
    "--binary-files",
    "-d",
    "--directories",
    "--sort",
    "--sortr",
];

const SEARCH_BOOL_FLAGS: &[&str] = &[
    "-F",
    "--fixed-strings",
    "-P",
    "--pcre2",
    "-i",
    "--ignore-case",
    "-s",
    "--case-sensitive",
    "-S",
    "--smart-case",
    "-w",
    "--word-regexp",
    "-v",
    "--invert-match",
    "-n",
    "--line-number",
    "-H",
    "--with-filename",
    "-h",
    "--no-filename",
    "-l",
    "--files-with-matches",
    "-L",
    "--files-without-match",
    "-c",
    "--count",
    "--count-matches",
    "-o",
    "--only-matching",
    "-q",
    "--quiet",
    "-a",
    "--text",
    "--hidden",
    "--no-ignore",
    "--no-ignore-vcs",
    "--no-ignore-dot",
    "--no-ignore-parent",
    "--no-ignore-global",
    "--no-ignore-files",
    "--follow",
    "-x",
    "--one-filesystem",
    "--one-file-system",
    "--column",
    "--byte-offset",
    "--heading",
    "--no-messages",
    "--include-zero",
    "--line-regexp",
    "-E",
    "--extended-regexp",
    "-G",
    "--basic-regexp",
    "-I",
    "--passthru",
    "--sort-files",
    "-U",
    "--multiline",
    "--multiline-dotall",
    "-z",
    "--search-zip",
    "--type-list",
    "--json",
    "--color",
    "-Z",
    "--null",
    "--stats",
    "--max-columns-preview",
    "--no-heading",
    "--crlf",
    "--binary",
    "--line-buffered",
    "--block-buffered",
    "--help",
    "-V",
    "--version",
];

/// Count and remove `-u`/`--unrestricted`, which repeat: one widen, two widen more, three
/// search binaries too, the way ripgrep's do.
fn take_unrestricted(raw: &mut Vec<String>) -> u8 {
    let mut n = 0u8;
    let mut i = 0;
    while i < raw.len() {
        let tok = raw[i].as_str();
        if tok == "--unrestricted" {
            n = n.saturating_add(1);
            raw.remove(i);
            continue;
        }
        // `-uu` and `-uuu` are one token; `-u` alone is one.
        if tok.starts_with('-') && !tok.starts_with("--") && tok.len() > 1 {
            let body = &tok[1..];
            if !body.is_empty() && body.bytes().all(|b| b == b'u') {
                n = n.saturating_add(body.len() as u8);
                raw.remove(i);
                continue;
            }
        }
        i += 1;
    }
    n
}

/// Rewrite a POSIX basic regular expression into the extended one this engine reads.
/// In BRE the operators are the backslashed spellings — `\(`, `\{`, `\+`, `\?`, `\|` —
/// and the bare `( ) { } + ? |` are literal, so the two dialects are the same grammar with
/// the punctuation swapped. A character class is copied out untouched, and an escape of
/// anything else means the same thing in both.
fn bre_to_ere(pattern: &str) -> String {
    let bytes = pattern.as_bytes();
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut i = 0;
    let mut in_class = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_class {
            out.push(c as char);
            if c == b'\\' {
                if let Some(next) = bytes.get(i + 1) {
                    out.push(*next as char);
                    i += 1;
                }
            } else if c == b']' {
                in_class = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'[' => {
                in_class = true;
                out.push('[');
                // A leading `]` or `^]` is a member, not the end.
                if bytes.get(i + 1) == Some(&b'^') {
                    out.push('^');
                    i += 1;
                }
                if bytes.get(i + 1) == Some(&b']') {
                    out.push(']');
                    i += 1;
                }
                i += 1;
            }
            b'\\' => match bytes.get(i + 1) {
                Some(&n) if matches!(n, b'(' | b')' | b'{' | b'}' | b'+' | b'?' | b'|') => {
                    out.push(n as char);
                    i += 2;
                }
                Some(&n) => {
                    out.push('\\');
                    out.push(n as char);
                    i += 2;
                }
                None => {
                    out.push('\\');
                    i += 1;
                }
            },
            b'(' | b')' | b'{' | b'}' | b'+' | b'?' | b'|' => {
                out.push('\\');
                out.push(c as char);
                i += 1;
            }
            _ => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

/// True when a pattern asks for a newline byte: a real one, or an unescaped `\n` escape.
fn holds_newline(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => return true,
            b'\\' => {
                if bytes.get(i + 1) == Some(&b'n') {
                    return true;
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    false
}

/// Split a `-r` template into literals and group references. `$0`, `$1`..`$99` and `${1}`
/// name a group, `$$` is a dollar sign, and `\1` is accepted for grep's spelling of the same
/// thing. A reference the pattern cannot satisfy is refused here rather than printed as
/// nothing.
fn parse_replacement(text: &str, groups: usize, pcre: bool) -> Result<Vec<Segment>, Fail> {
    let mut out: Vec<Segment> = Vec::new();
    let mut lit: Vec<u8> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    let check_group = |n: usize| -> Result<(), Fail> {
        if pcre {
            return Err(Fail(
                2,
                "-r cannot use capture groups with -P/--pcre2: the backtracking engine does \
                 not record them"
                    .to_string(),
            ));
        }
        if n > groups {
            return Err(Fail(
                2,
                format!("-r refers to group ${n} but the pattern has {groups}"),
            ));
        }
        Ok(())
    };
    while i < bytes.len() {
        match bytes[i] {
            b'$' => match bytes.get(i + 1) {
                Some(b'$') => {
                    lit.push(b'$');
                    i += 2;
                }
                Some(b'{') => {
                    let end = bytes[i + 2..]
                        .iter()
                        .position(|b| *b == b'}')
                        .map(|p| i + 2 + p)
                        .ok_or_else(|| Fail(2, "-r: unclosed ${".to_string()))?;
                    let digits = std::str::from_utf8(&bytes[i + 2..end]).unwrap_or("");
                    let n: usize = digits
                        .parse()
                        .map_err(|_| Fail(2, format!("-r: bad group ${{{digits}}}")))?;
                    check_group(n)?;
                    if !lit.is_empty() {
                        out.push(Segment::Literal(std::mem::take(&mut lit)));
                    }
                    out.push(Segment::Group(n));
                    i = end + 1;
                }
                Some(c) if c.is_ascii_digit() => {
                    // The name runs to the end of the alphanumeric run, as ripgrep reads it:
                    // `$0B` names a group called "0B", so it is refused here rather than
                    // quietly becoming group 0 followed by a letter.
                    let mut n = usize::from(c - b'0');
                    i += 2;
                    while let Some(d) = bytes.get(i).filter(|d| d.is_ascii_digit()) {
                        n = n * 10 + usize::from(d - b'0');
                        i += 1;
                    }
                    if bytes
                        .get(i)
                        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
                    {
                        let start = i - 1;
                        while bytes
                            .get(i)
                            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
                        {
                            i += 1;
                        }
                        let name = String::from_utf8_lossy(&bytes[start..i]).into_owned();
                        return Err(Fail(
                            2,
                            format!("-r: ${{{name}}} is not a group of this pattern (named and mixed group references are unsupported; use ${{{n}}})"),
                        ));
                    }
                    check_group(n)?;
                    if !lit.is_empty() {
                        out.push(Segment::Literal(std::mem::take(&mut lit)));
                    }
                    out.push(Segment::Group(n));
                }
                _ => {
                    lit.push(b'$');
                    i += 1;
                }
            },
            b'\\' => match bytes.get(i + 1) {
                Some(b'\\') => {
                    lit.push(b'\\');
                    i += 2;
                }
                Some(d) if (b'1'..=b'9').contains(d) => {
                    let n = usize::from(d - b'0');
                    check_group(n)?;
                    if !lit.is_empty() {
                        out.push(Segment::Literal(std::mem::take(&mut lit)));
                    }
                    out.push(Segment::Group(n));
                    i += 2;
                }
                _ => {
                    lit.push(b'\\');
                    i += 1;
                }
            },
            b => {
                lit.push(b);
                i += 1;
            }
        }
    }
    if !lit.is_empty() {
        out.push(Segment::Literal(lit));
    }
    Ok(out)
}

/// `--sort KEY` / `--sortr KEY`: the order results print in.
fn parse_sort(key: &str) -> Result<SortKey, Fail> {
    match key {
        "path" => Ok(SortKey::Path),
        "none" => Ok(SortKey::None),
        "modified" => Ok(SortKey::Modified),
        "accessed" => Ok(SortKey::Accessed),
        "created" => Ok(SortKey::Created),
        other => Err(Fail(
            2,
            format!("unsupported --sort key {other:?} (path, none, modified, accessed, created)"),
        )),
    }
}

fn run_query(mut raw: Vec<String>, scope: Scope) -> Result<(), Fail> {
    let color_mode = take_color_flag(&mut raw);
    let unrestricted = take_unrestricted(&mut raw);
    let (value_flags, bool_flags): (&[&str], &[&str]) = if scope == Scope::Content {
        (SEARCH_VALUE_FLAGS, SEARCH_BOOL_FLAGS)
    } else {
        (
            &[
                "-g",
                "--glob",
                "-t",
                "--type",
                "--ext",
                "--size",
                "--older",
                "--newer",
                "--max-depth",
                "--mindepth",
                "--sort",
                "--sortr",
            ],
            &[
                "-F",
                "--fixed",
                "-i",
                "--ignore-case",
                "--name",
                "--full-path",
                "--dirs",
                "--files",
                "--long",
                "--hidden",
                "--no-ignore",
                "--no-ignore-vcs",
                "--no-ignore-dot",
                "--no-ignore-parent",
                "--no-ignore-global",
                "--no-ignore-files",
                "--follow",
                "-x",
                "--one-filesystem",
                "--one-file-system",
                "--no-messages",
                "--type-list",
                "--json",
                "--color",
                "-Z",
                "--null",
                "--help",
                "-V",
                "--version",
            ],
        )
    };
    let a = Args::parse(raw, value_flags, bool_flags)?;
    let mut types = Types::new();
    for name in a.values("--type-clear").iter() {
        types.clear(name);
    }
    for spec in a.values("--type-add").iter() {
        types.add(spec)?;
    }
    if a.flag("--type-list") {
        let mut text = String::new();
        for (name, globs) in &types.table {
            text.push_str(name);
            text.push_str(": ");
            text.push_str(&globs.join(", "));
            text.push('\n');
        }
        return write_out(&text);
    }
    if a.flag("--help") {
        let text = if scope == Scope::Content {
            SEARCH_HELP
        } else {
            FIND_HELP
        };
        return write_out(&format!("{text}\n"));
    }
    if a.flag("-V") || a.flag("--version") {
        return write_out(&format!("dusage {}\n", env!("CARGO_PKG_VERSION")));
    }
    let basic = a.flag("-G") || a.flag("--basic-regexp");
    let mut multi_pattern = false;
    let mut given: Vec<String> = a.values("-e").to_vec();
    given.extend(a.values("--regexp").iter().cloned());
    let mut from_flags = !given.is_empty();
    let mut patterns_stdin = false;
    for path in a.values("-f").iter().chain(a.values("--file").iter()) {
        from_flags = true;
        patterns_stdin |= path == "-";
        given.extend(read_pattern_file(path)?);
    }
    let (pattern_text, paths) = if !from_flags {
        match a.positionals().split_first() {
            Some((p, rest)) => {
                let text = if basic { bre_to_ere(p) } else { p.clone() };
                (text, rest.to_vec())
            }
            None => return Err("missing <pattern>".to_string().into()),
        }
    } else {
        if given.is_empty() {
            return Err("no patterns in the pattern file".to_string().into());
        }
        // `-e` and `-f` may repeat: every pattern is an alternative, as in grep and rg. A
        // basic-regexp pattern is translated before they are joined, so the `|` that joins
        // them stays an operator.
        if basic {
            for p in given.iter_mut() {
                *p = bre_to_ere(p);
            }
        }
        let fixed = a.flag("-F") || a.flag("--fixed-strings");
        // Several literals become one escaped regex, so the fixed-string path is off.
        multi_pattern = given.len() > 1;
        (alternation(&given, fixed), a.positionals().to_vec())
    };
    if pattern_text.is_empty() {
        return Err("empty pattern".to_string().into());
    }
    // A pattern that spells a newline cannot match through a line-at-a-time scanner, so it
    // is refused the way ripgrep refuses it rather than quietly finding nothing.
    if !(a.flag("-U") || a.flag("--multiline")) && holds_newline(&pattern_text) {
        return Err(Fail(
            2,
            "the pattern contains a newline; pass -U/--multiline to search across lines"
                .to_string(),
        ));
    }
    // `--line-regexp` asks for the whole line, which is an anchor pair around whatever the
    // pattern is: an alternation has to be grouped so the anchors cover all of it.
    let mut pattern_text = if a.flag("--line-regexp") {
        format!("^(?:{pattern_text})$")
    } else {
        pattern_text
    };
    // `--multiline-dotall` widens `.` to a newline, which is the `(?s)` inline flag.
    if a.flag("--multiline-dotall") {
        pattern_text = format!("(?s){pattern_text}");
    }
    // `-` is standard input, as in grep and rg.
    let stdin_wanted = paths.iter().any(|p| p == "-");
    let piped = scope == Scope::Content && !std::io::stdin().is_terminal();
    let roots: Vec<PathBuf> = if paths.is_empty() {
        // A piped stdin with no paths searches the pipe, not the directory.
        if piped {
            Vec::new()
        } else {
            vec![PathBuf::from(".")]
        }
    } else {
        paths
            .iter()
            .filter(|p| p.as_str() != "-")
            .map(PathBuf::from)
            .collect()
    };
    // An explicit path always wins: stdin is only read when `-` names it or nothing else does.
    let read_stdin = scope == Scope::Content && (stdin_wanted || (paths.is_empty() && piped));
    if patterns_stdin && read_stdin {
        return Err(
            "stdin cannot be both the pattern file (-f -) and the text to search"
                .to_string()
                .into(),
        );
    }
    for r in &roots {
        if !r.exists() {
            return Err(format!("{}: no such file or directory", r.display()).into());
        }
    }

    // `-g` is an override filter (ripgrep semantics: it also switches ignore rules off);
    // `-t` and `--ext` only narrow the results.
    let mut globs: Vec<String> = Vec::new();
    for g in ["-g", "--glob", "--include"] {
        globs.extend(a.values(g).iter().cloned());
    }
    // grep's exclusions are the negated form of the same glob, and `--exclude-dir` adds the
    // trailing slash that makes a pattern only match a directory.
    for g in a.values("--exclude").iter() {
        globs.push(format!("!{g}"));
    }
    for g in a.values("--exclude-dir").iter() {
        globs.push(format!("!{g}/"));
    }
    let mut type_globs: Vec<String> = Vec::new();
    for t in a.values("-t").iter().chain(a.values("--type").iter()) {
        type_globs.extend(types.globs(t)?);
    }
    // `-T` excludes a type: every glob of it becomes an exclusion glob.
    for t in a.values("-T").iter().chain(a.values("--type-not").iter()) {
        for glob in types.globs(t)? {
            type_globs.push(format!("!{glob}"));
        }
    }
    // `--iglob` is `-g` without case: it is an override like `-g`, so it switches the ignore
    // files off the same way.
    let iglobs: Vec<String> = a.values("--iglob").to_vec();
    for exts in a.values("--ext") {
        for e in exts.split(',').filter(|e| !e.is_empty()) {
            type_globs.push(format!("*.{e}"));
        }
    }

    let unsigned = |name: &str| -> Result<Option<usize>, Fail> {
        match a.value(name) {
            None => Ok(None),
            Some(v) => v
                .trim()
                .parse::<usize>()
                .map(Some)
                .map_err(|_| Fail(2, format!("invalid {name} {v:?}"))),
        }
    };
    // Path mode never compiles the pattern as a regex: it is a glob (or a substring
    // with `--fixed`), so a literal placeholder is all the engine needs.
    let fixed = (a.flag("-F") || a.flag("--fixed-strings")) && !multi_pattern;
    // The last case mode wins, as in ripgrep; smart case decides from the pattern itself.
    let mut ignore_case = a.flag("-i") || a.flag("--ignore-case");
    if a.flag("-s") || a.flag("--case-sensitive") {
        ignore_case = false;
    }
    if a.flag("-S") || a.flag("--smart-case") {
        ignore_case = !pattern_text.chars().any(|c| c.is_uppercase());
    }
    let mut o = SearchOptions::new(
        Pattern::with_engine(
            &pattern_text,
            scope == Scope::Paths || fixed,
            ignore_case,
            a.flag("-w") || a.flag("--word-regexp"),
            a.flag("-P") || a.flag("--pcre2"),
        )?,
        roots,
    );
    o.scope = scope;
    o.globs = globs.clone();
    o.iglobs = iglobs.clone();
    o.type_globs = type_globs.clone();
    // `-u` widens the walk: one drops the ignore files, two also takes hidden paths, three
    // searches binary files as text.
    o.hidden = a.flag("--hidden") || unrestricted >= 2;
    o.no_ignore = a.flag("--no-ignore") || unrestricted >= 1;
    o.text = a.flag("-a") || a.flag("--text") || unrestricted >= 3;
    o.one_filesystem = a.flag("-x") || a.flag("--one-filesystem") || a.flag("--one-file-system");
    o.null = a.flag("-Z") || a.flag("--null");
    o.heading = a.flag("--heading") && !a.flag("--no-heading");
    o.max_columns_preview = a.flag("--max-columns-preview");
    o.column = a.flag("--column");
    o.byte_offset = a.flag("--byte-offset");
    o.include_zero = a.flag("--include-zero");
    o.quiet_errors = a.flag("--no-messages");
    o.passthru = a.flag("--passthru");
    if let Some(template) = a.value("-r").or(a.value("--replace")) {
        let groups = o.pattern.group_count();
        o.replace = Some(parse_replacement(template, groups, o.pattern.is_pcre())?);
    }
    // `--sort-files` is ripgrep's older spelling of `--sort path`.
    let (sort, sort_reverse) = match (a.value("--sort"), a.value("--sortr")) {
        (_, Some(v)) => (parse_sort(v)?, true),
        (Some(v), None) => (parse_sort(v)?, false),
        (None, None) => (SortKey::Path, false),
    };
    o.sort = sort;
    o.sort_reverse = sort_reverse;
    o.details |= sort != SortKey::Path && sort != SortKey::None;
    o.no_ignore_vcs = a.flag("--no-ignore-vcs") || a.flag("--no-ignore-files");
    o.no_ignore_dot = a.flag("--no-ignore-dot") || a.flag("--no-ignore-files");
    o.no_ignore_parent = a.flag("--no-ignore-parent") || a.flag("--no-ignore-files");
    o.follow = a.flag("--follow");
    let binary_files = a.value("--binary-files").map(|v| v.to_ascii_lowercase());
    match binary_files.as_deref() {
        Some("text") => o.text = true,
        Some("without-match") => o.skip_binary = true,
        Some("binary") | None => {}
        Some(other) => {
            return Err(Fail(
                2,
                format!("unknown --binary-files {other:?} (binary, text, without-match)"),
            ))
        }
    }
    if a.flag("-I") {
        o.skip_binary = true;
    }
    o.multiline = a.flag("-U") || a.flag("--multiline");
    o.search_zip = a.flag("-z") || a.flag("--search-zip");
    let json = a.flag("--json");
    let color = color_enabled(color_mode.as_deref());
    // Colour, the JSON report and `--column` all need each match located inside its line.
    o.paint = color || json || o.column;
    if let Some(v) = a.value("-j").or(a.value("--threads")) {
        o.threads = cli::parse_threads(v)?;
    }
    o.max_depth = unsigned("--max-depth")?;
    match a.value("-d").or(a.value("--directories")) {
        None | Some("recurse") => {}
        // grep's `-d skip` does not descend: the files directly inside a root still count.
        Some("skip") => o.max_depth = Some(o.max_depth.map_or(1, |d| d.min(1))),
        Some("read") => {
            return Err(Fail(
                2,
                "-d read is not supported: dusage reads files, never directories".to_string(),
            ))
        }
        Some(other) => return Err(Fail(2, format!("unknown -d {other:?} (recurse, skip)"))),
    }
    let report = match scope {
        Scope::Content => {
            o.invert = a.flag("-v") || a.flag("--invert-match");
            o.line_numbers = a.flag("-n") || a.flag("--line-number");
            o.with_filename = if a.flag("-H") || a.flag("--with-filename") {
                Some(true)
            } else if a.flag("-h") || a.flag("--no-filename") {
                Some(false)
            } else {
                None
            };
            o.only_matching = a.flag("-o") || a.flag("--only-matching");
            o.files_with_matches = a.flag("-l") || a.flag("--files-with-matches");
            o.files_without_matches = a.flag("-L") || a.flag("--files-without-match");
            o.count = a.flag("-c") || a.flag("--count");
            o.count_matches = a.flag("--count-matches");
            o.quiet = a.flag("-q") || a.flag("--quiet");
            o.text = a.flag("-a") || a.flag("--text");
            o.max_columns = unsigned("--max-columns")?.unwrap_or(0);
            if let Some(name) = a.value("--encoding") {
                if !name.eq_ignore_ascii_case("auto") {
                    let e = Encoding::from_name(name).ok_or_else(|| {
                        Fail(
                            2,
                            format!(
                                "unknown encoding {name:?} (auto, utf-8, utf-16le, utf-16be, latin1)"
                            ),
                        )
                    })?;
                    o.encoding = Some(e);
                }
            }
            let ctx = unsigned("-C")?.or(unsigned("--context")?);
            o.before = unsigned("-B")?
                .or(unsigned("--before-context")?)
                .or(ctx)
                .unwrap_or(0);
            o.after = unsigned("-A")?
                .or(unsigned("--after-context")?)
                .or(ctx)
                .unwrap_or(0);
            o.max_count = match a.value("-m").or(a.value("--max-count")) {
                None => None,
                Some(v) => Some(
                    v.trim()
                        .parse::<u64>()
                        .map_err(|_| Fail(2, format!("invalid --max-count {v:?}")))?,
                ),
            };
            if let Some(v) = a.value("--max-filesize") {
                o.max_file_size = Some(parse_size(v)?);
            }
            // Multiline search answers questions the line pipeline cannot: an inverted match
            // or a context window around a match that is itself several lines long.
            if o.multiline {
                if o.invert {
                    return Err("-U cannot be combined with -v/--invert-match"
                        .to_string()
                        .into());
                }
                if o.before > 0 || o.after > 0 {
                    return Err("-U cannot be combined with -A/-B/-C".to_string().into());
                }
            }
            if read_stdin {
                let mut data = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut data)
                    .map_err(|e| Fail(2, e.to_string()))?;
                if o.with_filename.is_none() {
                    o.with_filename = Some(false);
                }
                crate::search::search_buffer(&o, "(standard input)", &data)
                    .map_err(|e| Fail(1, e.to_string()))?
            } else {
                search(&o).map_err(|e| Fail(1, e.to_string()))?
            }
        }
        Scope::Paths => {
            // The walk takes every path the filters allow; the positional glob decides
            // which of them are results (`--fixed` matches a substring instead).
            let fixed = a.flag("-F") || a.flag("--fixed");
            let fold = a.flag("-i") || a.flag("--ignore-case");
            let glob = if fixed {
                None
            } else {
                Some(Glob::new_with(&pattern_text, fold)?)
            };
            let name_only = a.flag("--name");
            let full_path = a.flag("--full-path");
            let mindepth = unsigned("--mindepth")?.unwrap_or(0);
            o.pattern = Pattern::new(&pattern_text, true, false, false)?;
            o.details = json
                || a.flag("--long")
                || a.value("--size").is_some()
                || a.value("--older").is_some()
                || a.value("--newer").is_some();
            let mut r = search(&o).map_err(|e| Fail(1, e.to_string()))?;
            let size_filter = match a.value("--size") {
                None => None,
                Some(v) => Some(SizeFilter::parse(v)?),
            };
            let older = a.value("--older").map(parse_age).transpose()?.flatten();
            let newer = a.value("--newer").map(parse_age).transpose()?.flatten();
            let now = SystemTime::now();
            let dirs_only = a.flag("--dirs");
            // A type filter asks for files: `find -t js` never lists directories.
            let files_only = a.flag("--files") || (!dirs_only && !type_globs.is_empty());
            r.paths.retain(|c| {
                // `--mindepth` counts levels below the root, so a file in the root is 1.
                if mindepth > 0 && c.rel.split('/').count() < mindepth {
                    return false;
                }
                let subject = match (name_only, full_path) {
                    (true, _) => c.rel.rsplit('/').next().unwrap_or(&c.rel),
                    (false, true) => c.rel.as_str(),
                    // Without a hint a pattern with no `/` is a name and one with a `/` is a
                    // path, which is the rule `Glob` already applies.
                    (false, false) => c.rel.as_str(),
                };
                if let Some(g) = &glob {
                    if !g.matches(subject, c.is_dir) {
                        return false;
                    }
                }
                if fixed {
                    let hit = if fold {
                        subject
                            .to_lowercase()
                            .contains(&pattern_text.to_lowercase())
                    } else {
                        subject.contains(&pattern_text)
                    };
                    if !hit {
                        return false;
                    }
                }
                if dirs_only && !c.is_dir || files_only && c.is_dir {
                    return false;
                }
                if let Some(f) = &size_filter {
                    if !f.matches(c.size) {
                        return false;
                    }
                }
                if let Some(d) = older {
                    if !c
                        .modified
                        .is_some_and(|m| now.duration_since(m).unwrap_or_default() > d)
                    {
                        return false;
                    }
                }
                if let Some(d) = newer {
                    if !c
                        .modified
                        .is_some_and(|m| now.duration_since(m).unwrap_or_default() < d)
                    {
                        return false;
                    }
                }
                true
            });
            let long = a.flag("--long");
            let mut out = std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout().lock());
            let res = if json {
                report::render_paths_json(&r, &mut out)
            } else {
                report::render_paths(&r, long, color, a.flag("-Z") || a.flag("--null"), &mut out)
            };
            res.and_then(|()| out.flush()).map_err(Fail::output)?;
            let errs = r.stats.errors.total();
            if errs > 0 && !a.flag("--no-messages") {
                eprintln!("dusage: {errs} path(s) could not be read");
            }
            // An error is exit 2 even when something was found, which is grep's contract.
            return if errs > 0 {
                Err(Fail(2, String::new()))
            } else if r.paths.is_empty() {
                Err(Fail(1, String::new()))
            } else {
                Ok(())
            };
        }
    };

    // grep prints the file name whenever more than one file is in play, which a directory or
    // a second root guarantees; `-h` and `-H` have already had their say above.
    if o.with_filename.is_none() && (o.roots.iter().any(|p| p.is_dir()) || o.roots.len() > 1) {
        o.with_filename = Some(true);
    }
    let errs = report.stats.errors.total();
    if errs > 0 && !a.flag("-q") && !a.flag("--quiet") && !a.flag("--no-messages") {
        eprintln!("dusage: {errs} path(s) could not be read");
    }
    let stats = a.flag("--stats");
    {
        let mut out = std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout().lock());
        let res = if json {
            report::render_search_json(&report, &o, &mut out)
        } else {
            report::render_search(&report, &o, color, &mut out)
        };
        res.and_then(|()| out.flush()).map_err(Fail::output)?;
    }
    if stats {
        eprintln!("dusage: {}", report::stats_line(&report));
    }
    // A path that could not be read makes the whole run an error, whatever else matched:
    // exit 2 is grep's answer for "something went wrong", and sets it apart from "nothing
    // matched", which is 1.
    if errs > 0 {
        Err(Fail(2, String::new()))
    } else if report.any_match {
        Ok(())
    } else {
        Err(Fail(1, String::new()))
    }
}

/// `--size +10M`, `-10M` or `10M` (within 5%).
struct SizeFilter {
    min: u64,
    max: u64,
}

impl SizeFilter {
    fn parse(v: &str) -> Result<SizeFilter, Fail> {
        let (min, max) = match v.trim().strip_prefix('+') {
            Some(rest) => (parse_size(rest)?, u64::MAX),
            None => match v.trim().strip_prefix('-') {
                Some(rest) => (0, parse_size(rest)?),
                None => {
                    let exact = parse_size(v)?;
                    (exact.saturating_sub(exact / 20), exact + exact / 20)
                }
            },
        };
        Ok(SizeFilter { min, max })
    }

    fn matches(&self, size: u64) -> bool {
        size >= self.min && size <= self.max
    }
}

/// `dusage search <pattern> [paths...]`, and `dusage search --files` to list the files a
/// search would read (the same walk `find '*' --files` does).
pub fn run_search(raw: Vec<String>) -> Result<(), Fail> {
    if raw.iter().any(|t| t == "--files") && !raw.iter().any(|t| t == "-e" || t == "--regexp") {
        let mut args = vec!["*".to_string(), "--files".to_string()];
        args.extend(raw.iter().filter(|t| *t != "--files").cloned());
        return run_query(args, Scope::Paths);
    }
    run_query(raw, Scope::Content)
}

/// `dusage find <glob> [paths...]`
pub fn run_find(raw: Vec<String>) -> Result<(), Fail> {
    run_query(raw, Scope::Paths)
}

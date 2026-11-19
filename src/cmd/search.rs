//! `dusage search` (grep/rg replacement) and `dusage find` (file name and path search).

use super::Fail;
use crate::cli::{self, parse_age, parse_size, Args};
use crate::encodings::Encoding;
use crate::glob::Glob;
use crate::pattern::Pattern;
use crate::report;
use crate::search::{search, Scope, SearchOptions};
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
    -q, --quiet            print nothing; exit 0 on the first match
    -m, --max-count N      stop after N matching lines per file
    -A, --after-context N  print N lines after each match
    -B, --before-context N print N lines before each match
    -C, --context N        print N lines of context around each match (same as -A N -B N)
    -a, --text             search binary files instead of reporting them
    -j, --threads N        worker threads (default: one per core, 4 to 16)
    -g, --glob PATTERN     include files matching a glob; prefix with ! to exclude
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
    --encoding NAME        auto (default), utf-8, utf-16le, utf-16be, latin1
    --files                with no pattern, list the files a search would read
    -Z, --null             NUL instead of ':' after each file name (for xargs -0)

A path of `-` is standard input, and with no paths a piped stdin is searched instead of `.`.
    --hidden               also search hidden files and directories
    --no-ignore            do not skip .gitignore/.ignore rules or cache directories
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
        --iglob PATTERN    the same, matched without case
    --hidden               also look at hidden files and directories
    --no-ignore            do not skip .gitignore/.ignore rules or cache directories
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

fn ext_globs(t: &str) -> Result<Vec<String>, String> {
    let exts: &[&str] = match t {
        "rust" | "rs" => &["rs"],
        "python" | "py" => &["py", "pyi"],
        "js" | "javascript" => &["js", "jsx", "mjs", "cjs"],
        "ts" | "typescript" => &["ts", "tsx"],
        "c" => &["c", "h"],
        "cpp" | "cxx" | "c++" => &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
        "go" => &["go"],
        "java" => &["java"],
        "rb" | "ruby" => &["rb"],
        "md" | "markdown" => &["md", "markdown"],
        "json" => &["json"],
        "toml" => &["toml"],
        "yaml" | "yml" => &["yaml", "yml"],
        "sh" | "shell" | "bash" => &["sh", "bash", "zsh"],
        "html" => &["html", "htm"],
        "css" => &["css"],
        "xml" => &["xml"],
        "txt" | "text" => &["txt"],
        "log" => &["log"],
        "lock" => &["lock"],
        other => {
            return Err(format!(
                "unknown type {other:?} (see 'dusage search --help' for the list)"
            ))
        }
    };
    Ok(exts.iter().map(|e| format!("*.{e}")).collect())
}

const SEARCH_VALUE_FLAGS: &[&str] = &[
    "-e",
    "--regexp",
    "-f",
    "--file",
    "-T",
    "--type-not",
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
    "-x",
    "--one-filesystem",
    "--one-file-system",
    "--column",
    "--byte-offset",
    "--heading",
    "--no-messages",
    "--include-zero",
    "--line-regexp",
    "--type-list",
    "--json",
    "--color",
    "-Z",
    "--null",
    "--stats",
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
            ],
            &[
                "-F",
                "--fixed",
                "--dirs",
                "--files",
                "--long",
                "--hidden",
                "--no-ignore",
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
    if a.flag("--type-list") {
        let mut text = String::new();
        for (name, exts) in TYPE_TABLE {
            text.push_str(name);
            text.push_str(": ");
            text.push_str(&exts.join(", "));
            text.push('\n');
        }
        return write_out(&text);
    }
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
            Some((p, rest)) => (p.clone(), rest.to_vec()),
            None => return Err("missing <pattern>".to_string().into()),
        }
    } else {
        if given.is_empty() {
            return Err("no patterns in the pattern file".to_string().into());
        }
        // `-e` and `-f` may repeat: every pattern is an alternative, as in grep and rg.
        let fixed = a.flag("-F") || a.flag("--fixed-strings");
        // Several literals become one escaped regex, so the fixed-string path is off.
        multi_pattern = given.len() > 1;
        (alternation(&given, fixed), a.positionals().to_vec())
    };
    if pattern_text.is_empty() {
        return Err("empty pattern".to_string().into());
    }
    // `--line-regexp` asks for the whole line, which is an anchor pair around whatever the
    // pattern is: an alternation has to be grouped so the anchors cover all of it.
    let pattern_text = if a.flag("--line-regexp") {
        format!("^(?:{pattern_text})$")
    } else {
        pattern_text
    };
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
    for g in ["-g", "--glob"] {
        globs.extend(a.values(g).iter().cloned());
    }
    let mut type_globs: Vec<String> = Vec::new();
    for t in a.values("-t").iter().chain(a.values("--type").iter()) {
        type_globs.extend(ext_globs(t)?);
    }
    // `-T` excludes a type: every extension of it becomes an exclusion glob.
    for t in a.values("-T").iter().chain(a.values("--type-not").iter()) {
        for glob in ext_globs(t)? {
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
    o.heading = a.flag("--heading");
    o.column = a.flag("--column");
    o.byte_offset = a.flag("--byte-offset");
    o.include_zero = a.flag("--include-zero");
    o.quiet_errors = a.flag("--no-messages");
    let json = a.flag("--json");
    let color = color_enabled(color_mode.as_deref());
    // Colour, the JSON report and `--column` all need each match located inside its line.
    o.paint = color || json || o.column;
    if let Some(v) = a.value("-j").or(a.value("--threads")) {
        o.threads = cli::parse_threads(v)?;
    }
    o.max_depth = unsigned("--max-depth")?;
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
            let glob = if fixed {
                None
            } else {
                Some(Glob::new(&pattern_text)?)
            };
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
                if let Some(g) = &glob {
                    if !g.matches(&c.rel, c.is_dir) {
                        return false;
                    }
                }
                if fixed && !c.rel.contains(&pattern_text) {
                    return false;
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
            return if r.paths.is_empty() {
                Err(Fail(if errs > 0 { 2 } else { 1 }, String::new()))
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
    if report.any_match {
        Ok(())
    } else {
        Err(Fail(if errs > 0 { 2 } else { 1 }, String::new()))
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

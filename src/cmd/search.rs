//! `dusage search` (grep/rg replacement) and `dusage find` (file name and path search).

use super::Fail;
use crate::cli::{self, parse_age, parse_size, Args};
use crate::encodings::Encoding;
use crate::glob::Glob;
use crate::pattern::Pattern;
use crate::report;
use crate::search::{search, Scope, SearchOptions};
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
    -i, --ignore-case      case-insensitive (ASCII folding)
    -w, --word-regexp      match whole words only
    -v, --invert-match     select lines that do not match
    -e, --regexp PATTERN   pattern to search for; repeatable-free, one pattern
    -n, --line-number      print line numbers
    -H, --with-filename    always print the file name
    -h, --no-filename      never print the file name
    -l, --files-with-matches
                           print only the names of matching files
    -L, --files-without-match
                           print only the names of files with no match
    -c, --count            print the number of matching lines per file
    -o, --only-matching    print only the matching part of each line
    -q, --quiet            print nothing; exit 0 on the first match
    -m, --max-count N      stop after N matching lines per file
    -A, --after-context N  print N lines after each match
    -B, --before-context N print N lines before each match
    -C, --context N        print N lines of context around each match (same as -A N -B N)
    -a, --text             search binary files instead of reporting them
    -j, --threads N        worker threads (default: one per core, 4 to 16)
    -g, --glob PATTERN     include files matching a glob; prefix with ! to exclude
    -t, --type NAME        include files of a type: rust, python, js, ts, c, cpp, go, java,
                           ruby, md, json, toml, yaml, sh, html, css, xml, txt, log, lock
    --max-depth N          do not descend more than N levels below each root
    --max-filesize SIZE    skip files larger than SIZE (e.g. 10M)
    --max-columns N        truncate printed lines at N bytes
    --encoding NAME        auto (default), utf-8, utf-16le, utf-16be, latin1
    -Z, --null             NUL instead of ':' after each file name (for xargs -0)
    --hidden               also search hidden files and directories
    --no-ignore            do not skip .gitignore/.ignore rules or cache directories
    -x, --one-filesystem   stay on the starting filesystem
    --json                 machine-readable output
    --color[=WHEN]         WHEN is always, never or auto (default auto)
    --stats                print a summary line on stderr
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
    --hidden               also look at hidden files and directories
    --no-ignore            do not skip .gitignore/.ignore rules or cache directories
    -x, --one-filesystem   stay on the starting filesystem
    --json                 machine-readable output
    --color[=WHEN]         WHEN is always, never or auto (default auto)
    -h, --help             this text
    -V, --version          print the version

Exit status: 0 when something matched, 1 when nothing did, 2 on an error.";

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
    "-i",
    "--ignore-case",
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
    "--json",
    "--color",
    "-Z",
    "--null",
    "--stats",
    "--help",
    "-V",
    "--version",
];

fn run_query(mut raw: Vec<String>, scope: Scope) -> Result<(), Fail> {
    let color_mode = take_color_flag(&mut raw);
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
        println!(
            "{}",
            if scope == Scope::Content {
                SEARCH_HELP
            } else {
                FIND_HELP
            }
        );
        return Ok(());
    }
    if a.flag("-V") || a.flag("--version") {
        println!("dusage {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let (pattern_text, paths) = match a.value("-e").or(a.value("--regexp")) {
        Some(p) => (p.to_string(), a.positionals().to_vec()),
        None => match a.positionals().split_first() {
            Some((p, rest)) => (p.clone(), rest.to_vec()),
            None => return Err("missing <pattern>".to_string().into()),
        },
    };
    if pattern_text.is_empty() {
        return Err("empty pattern".to_string().into());
    }
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.into_iter().map(PathBuf::from).collect()
    };
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
    let fixed = a.flag("-F") || a.flag("--fixed-strings");
    let mut o = SearchOptions::new(
        Pattern::new(
            &pattern_text,
            scope == Scope::Paths || fixed,
            a.flag("-i") || a.flag("--ignore-case"),
            a.flag("-w") || a.flag("--word-regexp"),
        )?,
        roots,
    );
    o.scope = scope;
    o.globs = globs.clone();
    o.type_globs = type_globs.clone();
    o.hidden = a.flag("--hidden");
    o.no_ignore = a.flag("--no-ignore");
    o.one_filesystem = a.flag("-x") || a.flag("--one-filesystem");
    o.null = a.flag("-Z") || a.flag("--null");
    let json = a.flag("--json");
    let color = color_enabled(color_mode.as_deref());
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
            search(&o).map_err(|e| Fail(1, e.to_string()))?
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
            let mut out = std::io::stdout().lock();
            let res = if json {
                report::render_paths_json(&r, &mut out)
            } else {
                report::render_paths(&r, long, color, a.flag("-Z") || a.flag("--null"), &mut out)
            };
            res.and_then(|()| out.flush())
                .map_err(|e| Fail(1, e.to_string()))?;
            let errs = r.stats.errors.total();
            if errs > 0 {
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
    if errs > 0 && !a.flag("-q") && !a.flag("--quiet") {
        eprintln!("dusage: {errs} path(s) could not be read");
    }
    let stats = a.flag("--stats");
    {
        let mut out = std::io::stdout().lock();
        let res = if json {
            report::render_search_json(&report, &o, &mut out)
        } else {
            report::render_search(&report, &o, color, &mut out)
        };
        res.and_then(|()| out.flush())
            .map_err(|e| Fail(1, e.to_string()))?;
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

/// `dusage search <pattern> [paths...]`
pub fn run_search(raw: Vec<String>) -> Result<(), Fail> {
    run_query(raw, Scope::Content)
}

/// `dusage find <glob> [paths...]`
pub fn run_find(raw: Vec<String>) -> Result<(), Fail> {
    run_query(raw, Scope::Paths)
}

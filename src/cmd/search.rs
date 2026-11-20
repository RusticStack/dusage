//! `dusage search` (grep/rg replacement) and `dusage find` (file name and path search).

use super::Fail;
use crate::cli::{self, parse_age, parse_size, Args};
use crate::encodings::Encoding;
use crate::findexpr::{self, Ctx, ExecRequest, Ids, Sink};
use crate::glob::Glob;
use crate::pattern::Pattern;
use crate::report;
use crate::search::{
    search, walk_expr, walk_paths, Candidate, AbsPath, Scope, SearchOptions, Segment, SortKey,
};
use std::io::Read;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
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
        --engine ENGINE    auto (default), default (no lookaround) or pcre2
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
        --vimgrep          print every match on its own path:line:column:text row
        --heading          print the file name on its own line above its matches
        --no-heading       the opposite of --heading, and the last one given wins
        --trim             drop leading whitespace from printed lines
        --path-separator SEP   print SEP where a path has a separator (one byte)
        --field-match-separator SEP
                           between the fields of a matching line (default ':')
        --field-context-separator SEP
                           between the fields of a context line (default '-')
        --context-separator SEP
                           between context groups (default '--'); --group-separator is
                           grep's spelling of it, --no-context-separator prints none
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
    -z, --search-zip       expand compressed files and search their contents: gzip,
                           bzip2, lz4, xz, zstd by their magic, brotli by its .br name
    -U, --multiline        let a match span lines: the file is searched whole
        --multiline-dotall with -U, also let '.' match a newline
    -r, --replace TEXT     print each line with its matches replaced by TEXT;
                           $0 is the whole match, $1..$9 its groups, $$ a literal $
    -q, --quiet            print nothing; exit 0 on the first match
        --passthru         print every line, matching or not (matched lines stay marked)
    -m, --max-count N      stop after N matching lines per file
    -A, --after-context N  print N lines after each match
        --stop-on-nonmatch stop reading a file at the first line after a match that does
                           not match, which bounds the -A window
    -B, --before-context N print N lines before each match
    -C, --context N        print N lines of context around each match (same as -A N -B N)
    -a, --text             search binary files instead of reporting them
    -j, --threads N        worker threads (default: one per core, 4 to 16)
    -g, --glob PATTERN     include files matching a glob; prefix with ! to exclude
        --include PATTERN  grep's spelling of a narrowing glob; repeatable
        --exclude PATTERN  grep's spelling of an exclusion glob; repeatable
        --exclude-dir PATTERN  exclude directories by this glob; repeatable
        --pre COMMAND          run COMMAND PATH and search its output
        --pre-glob PATTERN     only run --pre on files matching this glob; repeatable
        --binary-files T    binary, text or without-match: what a NUL-carrying file is
    -I                     the same as --binary-files without-match
    -d, --directories A    recurse (default) or skip: whether to descend into directories
        --iglob PATTERN    the same, matched without case
    -t, --type NAME        include files of a type: rust, python, js, ts, c, cpp, go, java,
                           ruby, markdown, json, toml, yaml, sh, html, css, xml, txt, log, lock
    -T, --type-not NAME    exclude a type; repeatable
        --type-list        print the type names and their extensions
    --type-add NAME:GLOB[,GLOB]
                           define a type, or add globs to one that exists; repeatable
    --type-clear NAME      drop a type, so it can be defined again
    -u, --unrestricted     repeatable: -u drops ignore rules, -uu also looks at hidden paths,
                           -uuu also searches binary files
    --max-depth N          do not descend more than N levels below each root
    --max-filesize SIZE    skip files larger than SIZE (e.g. 10M)
    -M, --max-columns N    truncate printed lines at N bytes
        --max-columns-preview  say so when a line was cut
    --encoding NAME        auto (default), utf-8, utf-16le, utf-16be, latin1
    --crlf                 read \r\n as the line terminator: '$' matches before the \r
                           and the \r is not printed (already the behaviour here)
    --files                with no pattern, list the files a search would read
    -Z, --null, -0         NUL instead of ':' after each file name (for xargs -0)
        --null-data        records end in NUL instead of a newline, and so does the output

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
    --no-ignore-global     do not read git's global excludes file (core.excludesFile)
        --ignore-file PATH     read ignore patterns from PATH as well; repeatable
        --ignore-file-case-insensitive
                           match the --ignore-file patterns without case
    --mmap, --no-mmap      accepted and ignored: this reads in blocks, and mapping the file
                           was measured slower (docs/BENCHMARKS.md)
    --binary, --line-buffered, --block-buffered
                           accepted and ignored, so a ripgrep command line runs unchanged: a
                           binary file is reported by name, and the output buffer is the
                           caller's
    --sort-files           accepted and ignored: it asks for the default --sort path order
    --follow               follow symbolic links, walking each real directory once
                           (ripgrep spells this -L; here -L means --files-without-match)
    -x, --one-filesystem, --one-file-system
                           stay on the starting filesystem
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
    dusage find [OPTIONS] [paths...] <expression...>
    dusage ls   ...                                    (alias)

The first form takes a glob matched against the path relative to each root, or against the
file name when it has no '/': * ? **, [...] and [!...] classes, and a trailing / for
directories. Prefix with ! to exclude instead. Use --fixed for a plain substring match.

The second form runs a find(1) expression: the first argument that is not one of the options
below and starts with '-' (or is '!', '(' or ')') begins it, the arguments before it are the
starting points, and everything after it belongs to the expression. So 'dusage find . -name
\"*.rs\" -o -type d' is find's own command line. `-name`/`-iname`, `-path`/`-ipath`, `-regex`,
`-type`, `-size`, `-empty`, `-perm`, `-user`/`-group`/`-uid`/`-gid`, `-links`, `-inum`,
`-newer*`, `-mtime`/`-atime`/`-ctime` and their -mmin forms, `-fstype`, `-true`, `-false`,
`-a`/`-and`, `-o`/`-or`, `!`/`-not` and '(' ')' are tests; `-print`, `-print0`, `-printf`,
`-ls`, `-exec`, `-execdir` are actions, and an expression with no action prints each match,
so 'dusage find . -name \"*.rs\"' lists the same paths the glob form does.

The expression also carries find's global options, and those win over the option of the same
name below because they are applied after it: -maxdepth N, -mindepth N, -depth (report a
directory after its contents), -xdev (stay on one filesystem) and -follow. -prune stops the
walk descending into the directory it matched (a no-op under -depth, as in find), and -quit
ends the run. Exit status is 0 when something was printed or the expression was true for some
entry, 1 when nothing was, and 2 on an error.

-exec and -execdir run commands, which is the one way this read-only tool writes anything, so
they are refused with exit 2 unless --allow-exec is given. The command is run directly, with
no shell: every '{}' in every argument is replaced by the entry's path, and a trailing
'{} +' collects the paths into one invocation run when the walk ends. The command's exit
status is the predicate's value. -delete is always refused, with or without --allow-exec.
-execdir runs the command in the entry's directory with its path as ./name, and its '+'
form runs once per directory. An expression holding both -exec and -execdir runs both the
-exec way: the parsed request carries the command and the terminator, not which predicate
asked for it.

The options that describe the walk carry over: --hidden, --no-ignore and friends, -g/--glob
and grep's --include/--exclude/--exclude-dir/--iglob, -t/--type, --ext, --max-depth,
--mindepth, -x, --follow and --allow-exec. --files, --dirs and
--empty become the matching test. The options that describe the glob itself are refused with
a message naming the expression spelling (-F, -i, --name, --full-path, --size, --older,
--newer, --newermt, --newerat, --newerct, -Z, --long, --sort, --sortr, --json), because
silently ignoring them would print what they did not ask for.

find matches paths, not contents, so the flags that say how bytes are read (--pre,
--pre-glob, --binary-files, -I) and how a directory tree is descended (-d/--directories) are
unknown options here, not silently ignored; `dusage search` is the front end that takes them.

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
    --newermt DATE         modified after DATE (YYYY-MM-DD[ HH:MM[:SS]], or @SECONDS, UTC)
    --newerat DATE         accessed after DATE
    --newerct DATE         changed after DATE
    --dirs                 directories only
    --files                files only
    --empty                only empty files and directories (find -empty)
    --long                 print size, modification time and path
    --max-depth N          do not descend more than N levels below each root
    -g, --glob PATTERN     include files matching a glob; prefix with ! to exclude
        --include PATTERN  grep's spelling of a narrowing glob; repeatable
        --exclude PATTERN  grep's spelling of an exclusion glob; repeatable
        --exclude-dir PATTERN  exclude directories by this glob; repeatable
        --iglob PATTERN    the same as --include, matched without case; repeatable
    --sort KEY             result order: path (default), none, modified, accessed, created
                           (each oldest first); ignored by an expression walk, which prints
                           in find's order
    --sortr KEY            the same, reversed
    --hidden               also look at hidden files and directories
    --no-ignore            do not skip .gitignore/.ignore rules or cache directories
    --no-ignore-vcs        read neither .gitignore nor .git/info/exclude
    --no-ignore-dot        do not read .ignore files
    --no-ignore-parent     do not read the ignore files above a root
    --no-ignore-files      read no ignore file at all (the three switches above)
    --no-ignore-global     do not read git's global excludes file (core.excludesFile)
        --ignore-file PATH     read ignore patterns from PATH as well; repeatable
        --ignore-file-case-insensitive
                           match the --ignore-file patterns without case
    --follow               follow symbolic links, walking each real directory once
    -x, --one-filesystem, --one-file-system
                           stay on the starting filesystem
    -u, --unrestricted     repeatable: -u drops ignore rules, -uu also looks at hidden paths
    -Z, --null, -0         end each printed path with NUL instead of a newline (xargs -0)
        --no-messages      do not report paths that could not be read
        --type-list        print the type names and their extensions
    --allow-exec           let an expression's -exec and -execdir run commands
    --json                 machine-readable output (the glob form; not with an expression)
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

/// `--heading` and `--no-heading` are one switch and the last spelling wins, which the
/// shared parser cannot say: it keeps a set. Both are taken out here and read in order.
fn take_heading(raw: &mut Vec<String>) -> Option<bool> {
    let mut mode = None;
    raw.retain(|t| match t.as_str() {
        "--heading" => {
            mode = Some(true);
            false
        }
        "--no-heading" => {
            mode = Some(false);
            false
        }
        _ => true,
    });
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

/// One pattern that matches any of `patterns`, for a repeated `-e`. The rest are wrapped so a
/// top-level `|` in one cannot leak. A single pattern is handed over as its own text, so a
/// lone `-F` literal reaches the literal matcher unescaped; only the joined form, which the
/// engine reads as a regex, escapes a literal.
fn alternation(patterns: &[String], fixed: bool) -> String {
    match patterns {
        [one] => one.clone(),
        _ => patterns
            .iter()
            .map(|p| {
                let body = if fixed { escape_literal(p) } else { p.clone() };
                format!("(?:{body})")
            })
            .collect::<Vec<_>>()
            .join("|"),
    }
}

/// `text` with every regex metacharacter escaped, so a regex engine reads it as itself.
fn escape_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 4);
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '^' | '$' | '.' | '[' | ']' | '{' | '}' | '(' | ')' | '|' | '*' | '+' | '?'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
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

/// The flags `search` takes, for [`super::accepted_flags`].
pub(super) const SEARCH_VALUE_FLAGS: &[&str] = &[
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
    "-M",
    "--max-columns",
    "--encoding",
    "--path-separator",
    "--field-match-separator",
    "--field-context-separator",
    "--context-separator",
    "--group-separator",
    "--engine",
    "--include",
    "--exclude",
    "--exclude-dir",
    "--pre",
    "--pre-glob",
    "--ignore-file",
    "--binary-files",
    "-d",
    "--directories",
    "--sort",
    "--sortr",
];

pub(super) const SEARCH_BOOL_FLAGS: &[&str] = &[
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
    "--ignore-file-case-insensitive",
    "--no-ignore-files",
    "--follow",
    "-x",
    "--one-filesystem",
    "--one-file-system",
    "--column",
    "--byte-offset",
    "--vimgrep",
    "--heading",
    "--trim",
    "--stop-on-nonmatch",
    "--no-context-separator",
    "--mmap",
    "--no-mmap",
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
    "-0",
    "--null-data",
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

/// Remove `-0` from `raw`. The shared parser reads a leading `-` followed by digits as a
/// negative number, which is the right reading for an age or a size and the wrong one for
/// grep's NUL-after-name flag, so it is taken out before parsing.
fn take_dash_zero(raw: &mut Vec<String>) -> bool {
    let mut seen = false;
    raw.retain(|t| {
        if t == "-0" {
            seen = true;
            false
        } else {
            true
        }
    });
    seen
}

/// The walk's glob filters. `-g` is an override filter (ripgrep semantics: it also switches
/// the ignore rules off), and grep's spellings are the same filter seen through grep:
/// `--include` narrows, `--exclude` is its negation, `--exclude-dir` adds the trailing slash
/// that makes a pattern match a directory only, and `--iglob` is `--include` without case, an
/// override like `-g`. Every front end reads them here, so the two `find` forms and `search`
/// agree about which paths exist for them.
fn walk_globs(a: &Args) -> (Vec<String>, Vec<String>) {
    let mut globs: Vec<String> = Vec::new();
    for g in ["-g", "--glob", "--include"] {
        globs.extend(a.values(g).iter().cloned());
    }
    for g in a.values("--exclude") {
        globs.push(format!("!{g}"));
    }
    for g in a.values("--exclude-dir") {
        globs.push(format!("!{g}/"));
    }
    (globs, a.values("--iglob").to_vec())
}

/// The flags `dusage find` takes before its pattern or expression, and the ones that carry
/// a value. The expression splitter reads the same tables, so a flag's value is never
/// mistaken for the start of an expression.
/// The flags `find` takes, for [`super::accepted_flags`].
pub(super) const FIND_VALUE_FLAGS: &[&str] = &[
    "-g",
    "--glob",
    "--include",
    "--exclude",
    "--exclude-dir",
    "--iglob",
    "-t",
    "--type",
    "--ext",
    "--size",
    "--older",
    "--newer",
    "--newermt",
    "--newerat",
    "--newerct",
    "--max-depth",
    "--mindepth",
    "--sort",
    "--sortr",
    "--ignore-file",
];

pub(super) const FIND_BOOL_FLAGS: &[&str] = &[
    "-F",
    "--fixed",
    "-i",
    "--ignore-case",
    "--name",
    "--full-path",
    "--empty",
    "--dirs",
    "--files",
    "--long",
    "--hidden",
    "--no-ignore",
    "--no-ignore-vcs",
    "--no-ignore-dot",
    "--no-ignore-parent",
    "--no-ignore-global",
    "--ignore-file-case-insensitive",
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
    "--allow-exec",
    "-h",
    "--help",
    "-V",
    "--version",
];

fn run_query(mut raw: Vec<String>, scope: Scope) -> Result<(), Fail> {
    let color_mode = take_color_flag(&mut raw);
    let heading = take_heading(&mut raw);
    let dash_zero = take_dash_zero(&mut raw);
    let unrestricted = take_unrestricted(&mut raw);
    let found = if scope == Scope::Paths {
        split_find_args(raw)
    } else {
        FindArgs {
            args: raw,
            roots: Vec::new(),
            expr: Vec::new(),
        }
    };
    let (value_flags, bool_flags): (&[&str], &[&str]) = if scope == Scope::Content {
        (SEARCH_VALUE_FLAGS, SEARCH_BOOL_FLAGS)
    } else {
        (FIND_VALUE_FLAGS, FIND_BOOL_FLAGS)
    };
    let a = Args::parse(found.args, value_flags, bool_flags)?;
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
    // `-h` is help in `find`, which has no file-name suppression to spend it on; in
    // `search` it stays `--no-filename`, which is what grep means by it.
    if a.flag("--help") || (scope == Scope::Paths && a.flag("-h")) {
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
    // `find` accepts a whole expression in place of the positional glob; the splitter
    // above has already taken its tokens out of the argument list.
    if !found.expr.is_empty() {
        return run_find_expression(&a, &found.expr, &found.roots, &types, color_mode.as_deref());
    }
    let basic = a.flag("-G") || a.flag("--basic-regexp");
    let fixed_strings = a.flag("-F") || a.flag("--fixed-strings");
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
                let text = if basic && !fixed_strings {
                    bre_to_ere(p)
                } else {
                    p.clone()
                };
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
        // them stays an operator; `-F` leaves every pattern exactly as it was given, and the
        // escaping of the joined form happens once, in `alternation`.
        if basic && !fixed_strings {
            for p in given.iter_mut() {
                *p = bre_to_ere(p);
            }
        }
        // Several literals become one escaped regex, so the fixed-string path is off.
        multi_pattern = given.len() > 1;
        (alternation(&given, fixed_strings), a.positionals().to_vec())
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
    let mut fixed = fixed_strings && !multi_pattern;
    // The two rewrites below build a regex around the pattern text, which `-F` must not be
    // read as: it is escaped once and takes the regex path instead.
    let mut pattern_text = if a.flag("--line-regexp") || a.flag("--multiline-dotall") {
        if fixed {
            fixed = false;
            escape_literal(&pattern_text)
        } else {
            pattern_text
        }
    } else {
        pattern_text
    };
    // `--line-regexp` asks for the whole line, which is an anchor pair around whatever the
    // pattern is: an alternation has to be grouped so the anchors cover all of it.
    if a.flag("--line-regexp") {
        pattern_text = format!("^(?:{pattern_text})$");
    }
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
    // `-q` asks one question and a found answer outranks a later error, so a bad starting
    // point is not refused up front in that mode: the search records it, and the exit code
    // follows whether anything matched. Every other mode refuses it before reading anything.
    let quiet_mode = a.flag("-q") || a.flag("--quiet");
    for r in &roots {
        if !r.exists() && !quiet_mode {
            return Err(format!("{}: no such file or directory", r.display()).into());
        }
    }

    // `-t` and `--ext` only narrow the results; the glob filters come from `walk_globs`.
    let (globs, iglobs) = walk_globs(&a);
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
    // The last case mode wins, as in ripgrep; smart case decides from the pattern itself.
    let mut ignore_case = a.flag("-i") || a.flag("--ignore-case");
    if a.flag("-s") || a.flag("--case-sensitive") {
        ignore_case = false;
    }
    if a.flag("-S") || a.flag("--smart-case") {
        ignore_case = !pattern_text.chars().any(|c| c.is_uppercase());
    }
    // `--engine` picks the matcher: `auto` keeps the Pike VM and falls back to the
    // backtracking one when the pattern needs it, `default` refuses that fallback, and
    // `pcre2` is `-P`.
    let engine = a.value("--engine").unwrap_or("auto");
    let pcre = match engine {
        "auto" => a.flag("-P") || a.flag("--pcre2"),
        "default" => false,
        "pcre2" => true,
        other => {
            return Err(Fail(
                2,
                format!("unrecognized regex engine {other:?} (auto, default, pcre2)"),
            ))
        }
    };
    let mut o = SearchOptions::new(
        Pattern::with_engine(
            &pattern_text,
            scope == Scope::Paths || fixed,
            ignore_case,
            a.flag("-w") || a.flag("--word-regexp"),
            pcre,
        )?,
        roots,
    );
    if engine == "default" && scope == Scope::Content && o.pattern.is_pcre() {
        return Err(Fail(
            2,
            format!(
                "{}: lookaround and backreferences need the backtracking engine; pass \
                 -P/--pcre2 or --engine pcre2",
                o.pattern.source()
            ),
        ));
    }
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
    o.null = dash_zero || a.flag("-Z") || a.flag("--null");
    o.null_data = a.flag("--null-data");
    if o.null_data {
        o.term = 0;
    }
    o.heading = heading.unwrap_or(false);
    o.max_columns_preview = a.flag("--max-columns-preview");
    o.column = a.flag("--column");
    o.byte_offset = a.flag("--byte-offset");
    o.vimgrep = a.flag("--vimgrep");
    o.trim = a.flag("--trim");
    o.stop_on_nonmatch = a.flag("--stop-on-nonmatch");
    if let Some(sep) = a.value("--path-separator") {
        let bytes = sep.as_bytes();
        if bytes.len() != 1 {
            return Err(Fail(
                2,
                format!("--path-separator wants exactly one byte, not {sep:?}"),
            ));
        }
        o.path_separator = Some(bytes[0]);
    }
    if let Some(sep) = a.value("--field-match-separator") {
        o.field_match_sep = sep.as_bytes().to_vec();
    }
    if let Some(sep) = a.value("--field-context-separator") {
        o.field_context_sep = sep.as_bytes().to_vec();
    }
    if a.flag("--no-context-separator") {
        o.context_sep = None;
    } else if let Some(sep) = a
        .value("--context-separator")
        .or(a.value("--group-separator"))
    {
        o.context_sep = Some(sep.as_bytes().to_vec());
    }
    o.include_zero = a.flag("--include-zero");
    o.quiet_errors = a.flag("--no-messages");
    o.passthru = a.flag("--passthru");
    if let Some(template) = a.value("-r").or(a.value("--replace")) {
        let groups = o.pattern.group_count();
        o.replace = Some(parse_replacement(template, groups, o.pattern.is_pcre())?);
    }
    // `--sort-files` is ripgrep's older spelling of `--sort path`, which is the fallthrough.
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
    o.no_ignore_global = a.flag("--no-ignore-global") || a.flag("--no-ignore-files");
    o.ignore_file_icase = a.flag("--ignore-file-case-insensitive");
    o.ignore_files = a.values("--ignore-file").to_vec();
    // An explicit ignore file that cannot be read is a usage error, not a silent empty
    // stack: the rules the caller asked for would be missing from every result.
    for path in &o.ignore_files {
        if std::fs::read_to_string(path).is_err() {
            return Err(Fail::from(format!("{path}: cannot read ignore file")));
        }
    }
    o.follow = a.flag("--follow");
    o.pre = a.value("--pre").map(str::to_string);
    o.pre_globs = a.values("--pre-glob").to_vec();
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
    o.paint = color || json || o.column || o.vimgrep;
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
            // `--vimgrep` is `-n --column` with a row per match, so `-n` comes with it.
            o.line_numbers = a.flag("-n") || a.flag("--line-number") || o.vimgrep;
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
            o.max_columns = unsigned("-M")?.or(unsigned("--max-columns")?).unwrap_or(0);
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
            // grep prints the file name whenever more than one file is in play, which a
            // directory or a second root guarantees, and `--vimgrep` has a field for the path
            // even on one file; `-h` and `-H` have already had their say above.
            if o.with_filename.is_none()
                && (o.vimgrep || o.roots.iter().any(|p| p.is_dir()) || o.roots.len() > 1)
            {
                o.with_filename = Some(true);
            }
            // Every mode that prints the result of a file as it is found, in the order the
            // whole answer would have printed them. A reversed or time-keyed sort has to see
            // every result before the first one, `--json` closes with a count, and `-q` prints
            // nothing at all, so those keep the collected report.
            let stream =
                !json && o.sort == SortKey::Path && !o.sort_reverse && !o.quiet && !read_stdin;
            let report = if read_stdin {
                let mut data = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut data)
                    .map_err(|e| Fail(2, e.to_string()))?;
                if o.with_filename.is_none() {
                    o.with_filename = Some(false);
                }
                crate::search::search_buffer(&o, "(standard input)", &data)
                    .map_err(|e| Fail(1, e.to_string()))?
            } else if stream {
                let mut out =
                    std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout().lock());
                let mut printer =
                    report::SearchStream::new(&o, color, o.with_filename.unwrap_or(false));
                crate::search::search_stream(&o, &mut |f| printer.file(&mut out, f))
                    .and_then(|r| out.flush().map(|()| r))
                    .map_err(Fail::output)?
            } else {
                search(&o).map_err(|e| Fail(1, e.to_string()))?
            };
            // A backtracking match that ran out of steps is an unknown answer, not "no
            // match": report it and exit 2 rather than printing a plan that exits 1.
            if let Some(e) = o.pattern.budget_overrun() {
                return Err(Fail(2, e.to_string()));
            }
            report
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
            let stamp = |flag: &str| -> Result<Option<(SystemTime, u8)>, Fail> {
                match a.value(flag) {
                    None => Ok(None),
                    Some(v) => {
                        let t = cli::parse_date(v).map_err(Fail::from)?;
                        // 0 = modified, 1 = accessed, 2 = changed, read in the retain below.
                        let which = match flag {
                            "--newerat" => 1,
                            "--newerct" => 2,
                            _ => 0,
                        };
                        Ok(Some((t, which)))
                    }
                }
            };
            let since = stamp("--newermt")?.or(stamp("--newerat")?.or(stamp("--newerct")?));
            let empty_only = a.flag("--empty");
            o.details = json
                || empty_only
                || a.flag("--long")
                || since.is_some()
                || a.value("--size").is_some()
                || a.value("--older").is_some()
                || a.value("--newer").is_some();
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
            let keep = |c: &Candidate| -> bool {
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
                // `find -empty`: a file of no bytes, or a directory with no entries.
                if empty_only {
                    let empty = if c.is_dir {
                        std::fs::read_dir(&c.path)
                            .map(|mut d| d.next().is_none())
                            .unwrap_or(false)
                    } else {
                        c.size == 0
                    };
                    if !empty {
                        return false;
                    }
                }
                if let Some(f) = &size_filter {
                    if !f.matches(c.size) {
                        return false;
                    }
                }
                if let Some((t, which)) = since {
                    let got = match which {
                        1 => std::fs::metadata(&c.path).and_then(|m| m.accessed()),
                        2 => std::fs::metadata(&c.path).and_then(|m| m.created()),
                        _ => c.modified.ok_or_else(|| std::io::Error::other("no mtime")),
                    };
                    if !got.is_ok_and(|g| g > t) {
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
            };
            let long = a.flag("--long");
            let null = a.flag("-Z") || a.flag("--null");
            let mut out = std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout().lock());
            let mut kept = 0u64;
            // A stream cannot be reversed and a time key has to be read for every result
            // before the first one prints; `--json` closes with a count. Those keep the
            // listing in memory. Everything else, `--sort path` included, prints as the walk
            // finds each result.
            let stats = if json || o.sort != SortKey::Path || o.sort_reverse {
                let mut r = search(&o).map_err(|e| Fail(1, e.to_string()))?;
                r.paths.retain(|c| keep(c));
                kept = r.paths.len() as u64;
                // `--json` counts what it prints, which the filters have already decided.
                let res = if json {
                    report::render_paths_json(&r, &mut out)
                } else {
                    report::render_paths(&r, long, color, null, &mut out)
                };
                res.and_then(|()| out.flush()).map_err(Fail::output)?;
                r.stats
            } else {
                let mut write_err: Option<std::io::Error> = None;
                // A listing prints `rel`; only the filters that stat a path read the other
                // one, so the walk skips it unless one of them is on.
                let paths = if since.is_some_and(|(_, which)| which != 0) {
                    AbsPath::Fill
                } else {
                    AbsPath::Skip
                };
                let stats = walk_paths(&o, paths, &mut |c: &Candidate| {
                    if !keep(c) {
                        return true;
                    }
                    kept += 1;
                    match report::render_path(c, long, color, null, &mut out) {
                        Ok(()) => true,
                        Err(e) => {
                            write_err = Some(e);
                            false
                        }
                    }
                });
                if let Some(e) = write_err {
                    return Err(Fail::output(e));
                }
                out.flush().map_err(Fail::output)?;
                stats
            };
            let errs = stats.errors.total();
            if errs > 0 && !a.flag("--no-messages") {
                eprintln!("dusage: {errs} path(s) could not be read");
            }
            // An error is exit 2 even when something was found, which is grep's contract.
            return if errs > 0 {
                Err(Fail(2, String::new()))
            } else if kept == 0 {
                Err(Fail(1, String::new()))
            } else {
                Ok(())
            };
        }
    };

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
    // matched", which is 1. `-q` is the exception, and grep's own: the question was whether
    // a line is selected, and a "yes" outranks an error, so an unreadable second path does
    // not turn a found answer into a failure.
    if errs > 0 && !(o.quiet && report.any_match) {
        Err(Fail(2, String::new()))
    } else if report.any_match {
        Ok(())
    } else {
        Err(Fail(1, String::new()))
    }
}

/// Where `dusage find`'s arguments divide into flags, starting points and a find expression.
struct FindArgs {
    /// Everything the shared parser sees: our own flags and their values.
    args: Vec<String>,
    /// The starting points written before the expression.
    roots: Vec<String>,
    /// The expression's tokens, empty for the `<glob> [paths...]` form.
    expr: Vec<String>,
}

/// Split the arguments at the first token that begins a find expression.
///
/// `dusage find` keeps its positional glob, so a run is an expression only when one of its
/// arguments is expression syntax: a token that starts with `-` and is not one of our own
/// flags, or a bare `!`, `(` or `)`. Everything before that token is a starting point (the
/// glob, for the form without an expression) and everything from it on is the expression,
/// which is find's own order: `find [paths...] [expression]`.
///
/// Our own flags are recognised wherever they appear — find's expression has no `--`-spelled
/// predicate, and `--allow-exec` would otherwise have to sit before the action that needs it
/// — except inside a `-exec` command, where every token belongs to the command.
fn split_find_args(raw: Vec<String>) -> FindArgs {
    let mut args = Vec::with_capacity(raw.len());
    let mut roots = Vec::new();
    let mut expr = Vec::new();
    let mut in_expr = false;
    let mut in_exec = false;
    let mut i = 0;
    while i < raw.len() {
        let tok = raw[i].clone();
        i += 1;
        let ours =
            !in_exec && (FIND_BOOL_FLAGS.contains(&tok.as_str()) || tok.starts_with("--color="));
        if ours {
            args.push(tok);
            continue;
        }
        if !in_exec && FIND_VALUE_FLAGS.contains(&tok.as_str()) {
            args.push(tok);
            if let Some(v) = raw.get(i) {
                args.push(v.clone());
                i += 1;
            }
            continue;
        }
        if tok == "--" && !in_expr {
            // After the terminator every token is positional, and no expression starts.
            args.push(tok);
            args.extend(raw[i..].iter().cloned());
            break;
        }
        if !in_expr
            && ((tok.len() > 1 && tok.starts_with('-')) || tok == "!" || tok == "(" || tok == ")")
        {
            in_expr = true;
        }
        if !in_expr {
            roots.push(tok.clone());
            args.push(tok);
            continue;
        }
        if tok == "-exec" || tok == "-execdir" {
            in_exec = true;
        } else if in_exec && (tok == ";" || tok == "+") {
            in_exec = false;
        }
        expr.push(tok);
    }
    FindArgs { args, roots, expr }
}

/// How many `-exec` and `-execdir` predicates an expression holds, skipping the tokens of
/// their commands: the pair `(execdir, exec)`.
fn exec_predicates(tokens: &[String]) -> (usize, usize) {
    let (mut dir, mut plain) = (0, 0);
    let mut in_exec = false;
    for tok in tokens {
        if in_exec {
            in_exec = tok != ";" && tok != "+";
            continue;
        }
        match tok.as_str() {
            "-exec" => plain += 1,
            "-execdir" => dir += 1,
            _ => {}
        }
        in_exec = tok == "-exec" || tok == "-execdir";
    }
    (dir, plain)
}

/// `dusage find <paths...> <expression...>`: find's own expression language, evaluated by
/// the walk in `search::walk_expr`, with the actions in [`StdoutSink`].
fn run_find_expression(
    a: &Args,
    tokens: &[String],
    root_args: &[String],
    types: &Types,
    color_mode: Option<&str>,
) -> Result<(), Fail> {
    // Flags that describe the glob, or a filter with find's own spelling, do not carry into
    // an expression; saying so beats printing what the flags did not ask for.
    for (flag, instead) in [
        ("-F", "-name and -path"),
        ("--fixed", "-name and -path"),
        ("-i", "-iname and -ipath"),
        ("--ignore-case", "-iname and -ipath"),
        ("--name", "-name"),
        ("--full-path", "-path"),
        ("--size", "-size"),
        ("--older", "-mtime"),
        ("--newer", "-mtime"),
        ("--newermt", "-newermt"),
        ("--newerat", "-newerat"),
        ("--newerct", "-newerct"),
        ("-Z", "-print0"),
        ("--null", "-print0"),
        ("--long", "-ls and -printf"),
        (
            "--sort",
            "nothing: an expression walk prints in find's order",
        ),
        (
            "--sortr",
            "nothing: an expression walk prints in find's order",
        ),
        ("--json", "nothing: the actions decide what is printed"),
    ] {
        if a.flag(flag) || a.value(flag).is_some() {
            return Err(Fail(
                2,
                format!("{flag} does not combine with a find expression; use {instead}"),
            ));
        }
    }
    // Our own flags become the expression's leading terms, so an option written inside the
    // expression comes later and wins.
    let mut all: Vec<String> = Vec::new();
    for (flag, option) in [("--max-depth", "-maxdepth"), ("--mindepth", "-mindepth")] {
        if let Some(v) = a.value(flag) {
            all.push(option.to_string());
            all.push(v.to_string());
        }
    }
    if a.flag("-x") || a.flag("--one-filesystem") || a.flag("--one-file-system") {
        all.push("-xdev".to_string());
    }
    if a.flag("--follow") {
        all.push("-follow".to_string());
    }
    if a.flag("--files") {
        all.extend(["-type".to_string(), "f".to_string()]);
    }
    if a.flag("--dirs") {
        all.extend(["-type".to_string(), "d".to_string()]);
    }
    if a.flag("--empty") {
        all.push("-empty".to_string());
    }
    all.extend(tokens.iter().cloned());
    let expr = findexpr::Expr::parse(&all).map_err(Fail::from)?;
    for warning in expr.warnings() {
        eprintln!("dusage: {warning}");
    }
    // A run that would execute something is refused as a whole, before any output, rather
    // than failing at the first entry that reaches the action.
    let allow_exec = a.flag("--allow-exec");
    if !allow_exec && !expr.exec_requests().is_empty() {
        return Err(Fail(
            2,
            "-exec and -execdir run commands; pass --allow-exec to allow them".to_string(),
        ));
    }
    let roots: Vec<PathBuf> = if root_args.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        root_args.iter().map(PathBuf::from).collect()
    };
    for r in &roots {
        if !r.exists() {
            return Err(format!("{}: no such file or directory", r.display()).into());
        }
    }
    // The walk never reads the pattern in expression mode, but `SearchOptions` carries the
    // walk's configuration, so it gets a pattern that cannot matter.
    let mut o = SearchOptions::new(
        Pattern::new("*", true, false, false).map_err(Fail::from)?,
        roots,
    );
    o.scope = Scope::Paths;
    let (globs, iglobs) = walk_globs(a);
    let mut type_globs: Vec<String> = Vec::new();
    for t in a.values("-t").iter().chain(a.values("--type").iter()) {
        type_globs.extend(types.globs(t)?);
    }
    for exts in a.values("--ext") {
        for e in exts.split(',').filter(|e| !e.is_empty()) {
            type_globs.push(format!("*.{e}"));
        }
    }
    o.globs = globs;
    o.iglobs = iglobs;
    o.type_globs = type_globs;
    o.hidden = a.flag("--hidden");
    o.no_ignore = a.flag("--no-ignore");
    o.no_ignore_vcs = a.flag("--no-ignore-vcs") || a.flag("--no-ignore-files");
    o.no_ignore_dot = a.flag("--no-ignore-dot") || a.flag("--no-ignore-files");
    o.no_ignore_parent = a.flag("--no-ignore-parent") || a.flag("--no-ignore-files");
    o.no_ignore_global = a.flag("--no-ignore-global") || a.flag("--no-ignore-files");
    o.ignore_file_icase = a.flag("--ignore-file-case-insensitive");
    o.ignore_files = a.values("--ignore-file").to_vec();
    for path in &o.ignore_files {
        if std::fs::read_to_string(path).is_err() {
            return Err(Fail::from(format!("{path}: cannot read ignore file")));
        }
    }
    // `findexpr::ExecRequest` carries the command, not which predicate asked for it, so a run
    // whose only exec predicate is -execdir is told to run them find's -execdir way, and a
    // run holding both keeps -exec's. The count skips tokens inside an exec command.
    let (execdir, plain) = exec_predicates(tokens);
    let ids = run_ids();
    let ctx = Ctx {
        now: SystemTime::now(),
        ids: &ids,
    };
    // `-empty` costs one `read_dir` per directory, so the walk only answers it when the
    // expression asks.
    let want_empty = all.iter().any(|t| t == "-empty");
    let mut sink = StdoutSink::new(
        &ids,
        allow_exec,
        color_enabled(color_mode),
        execdir > 0 && plain == 0,
    );
    let outcome = match walk_expr(&o, &expr, &ctx, want_empty, &mut sink) {
        Ok(out) => out,
        Err(msg) => return Err(sink.failure().unwrap_or(Fail(2, msg))),
    };
    sink.finish()?;
    let errs = outcome.stats.errors.total();
    if errs > 0 && !a.flag("--no-messages") {
        eprintln!("dusage: {errs} path(s) could not be read");
    }
    if errs > 0 {
        Err(Fail(2, String::new()))
    } else if outcome.matched == 0 && sink.printed == 0 {
        Err(Fail(1, String::new()))
    } else {
        Ok(())
    }
}

/// Where a find expression's actions go: stdout, and the processes `-exec` names.
struct StdoutSink<'a> {
    out: std::io::BufWriter<std::io::Stdout>,
    ids: &'a dyn Ids,
    allow_exec: bool,
    /// Run each command in the entry's own directory with its path as `./name`, which is
    /// what `-execdir` does.
    execdir: bool,
    color: bool,
    /// How many actions wrote something, for the exit status.
    printed: u64,
    /// `-exec ... {} +` collects its paths and runs once when the walk ends, one batch per
    /// directory under `-execdir` and one for the whole walk otherwise.
    batches: Vec<(Vec<String>, Option<PathBuf>, Vec<String>)>,
    write_err: Option<std::io::Error>,
}

impl<'a> StdoutSink<'a> {
    fn new(ids: &'a dyn Ids, allow_exec: bool, color: bool, execdir: bool) -> StdoutSink<'a> {
        StdoutSink {
            out: std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout()),
            ids,
            allow_exec,
            execdir,
            color,
            printed: 0,
            batches: Vec::new(),
            write_err: None,
        }
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.printed += 1;
        if self.write_err.is_some() {
            return Err(String::new());
        }
        self.out.write_all(bytes).map_err(|e| {
            let kind = e.kind();
            self.write_err = Some(e);
            // A reader that went away is not an error, but it does end the output.
            if kind == std::io::ErrorKind::BrokenPipe {
                String::new()
            } else {
                "write failed".to_string()
            }
        })
    }

    /// The stream's own failure, when it was the reason the walk stopped.
    fn failure(&mut self) -> Option<Fail> {
        self.write_err.take().map(Fail::output)
    }

    /// Flush what was printed, then run the batched `-exec ... {} +` commands.
    fn finish(&mut self) -> Result<(), Fail> {
        if let Err(e) = self.out.flush() {
            return Err(Fail::output(e));
        }
        for (command, dir, paths) in std::mem::take(&mut self.batches) {
            match exec_batch(&command, &paths, dir.as_deref()) {
                Ok(true) => {}
                Ok(false) => return Err(Fail(2, format!("-exec {}: command failed", command[0]))),
                Err(e) => return Err(Fail(2, format!("-exec {}: {e}", command[0]))),
            }
        }
        Ok(())
    }
}

impl Sink for StdoutSink<'_> {
    fn print(&mut self, path: &str, terminator: u8) -> Result<(), String> {
        let mut bytes = Vec::with_capacity(path.len() + 1);
        if self.color {
            bytes.extend_from_slice(report::color::PATH.as_bytes());
            bytes.extend_from_slice(path.as_bytes());
            bytes.extend_from_slice(report::color::RESET.as_bytes());
        } else {
            bytes.extend_from_slice(path.as_bytes());
        }
        bytes.push(terminator);
        self.emit(&bytes)
    }

    fn printf(&mut self, text: &str) -> Result<(), String> {
        self.emit(text.as_bytes())
    }

    fn ls(&mut self, entry: &findexpr::Entry) -> Result<(), String> {
        let line = ls_line(entry, self.ids);
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        self.emit(&bytes)
    }

    fn exec(&mut self, request: &ExecRequest, entry: &findexpr::Entry) -> Result<bool, String> {
        if !self.allow_exec {
            return Err(
                "-exec: dusage does not run commands from a find expression (pass --allow-exec)"
                    .to_string(),
            );
        }
        // `-execdir` runs in the entry's directory and names it `./name`; `-exec` names it
        // as the walk printed it.
        let (dir, named) = match self.execdir {
            true => match Path::new(entry.path).parent() {
                Some(parent) => (
                    Some(parent.to_path_buf()),
                    format!(".{}{}", std::path::MAIN_SEPARATOR, entry.name),
                ),
                None => (None, entry.path.to_string()),
            },
            false => (None, entry.path.to_string()),
        };
        if request.batch {
            let slot = match self
                .batches
                .iter_mut()
                .find(|(command, at, _)| *command == request.command && *at == dir)
            {
                Some(slot) => slot,
                None => {
                    self.batches
                        .push((request.command.clone(), dir.clone(), Vec::new()));
                    self.batches.last_mut().expect("just pushed")
                }
            };
            slot.2.push(named);
            return Ok(true);
        }
        exec_one(&request.command, &named, dir.as_deref())
            .map_err(|e| format!("-exec {}: {e}", request.command[0]))
    }
}

/// One `-ls` row in find's column layout: inode, 1K blocks, mode, links, user, group, size,
/// time and path.
fn ls_line(entry: &findexpr::Entry, ids: &dyn Ids) -> String {
    let user = ids
        .user_of(entry.uid)
        .unwrap_or_else(|| entry.uid.to_string());
    let group = ids
        .group_of(entry.gid)
        .unwrap_or_else(|| entry.gid.to_string());
    // A symlink prints what it points at, the way find and ls do, so the line says where a
    // walk stopped rather than only that it stopped.
    let mut line = format!(
        "{:>9} {:>6} {} {:>3} {:<8} {:<8} {:>8} {} {}",
        entry.inode,
        entry.blocks.div_ceil(2),
        ls_mode(entry.mode, entry.kind),
        entry.links,
        user,
        group,
        entry.size,
        ls_time(entry.mtime),
        entry.path
    );
    if let Some(target) = &entry.target {
        line.push_str(" -> ");
        line.push_str(target);
    }
    line
}

/// `Nov 19 21:41`, or `Nov 19  2025` for a file more than six months from now, which is the
/// shape `ls -l` and `find -ls` print. Times are UTC here, as everywhere else in this tool.
fn ls_time(t: SystemTime) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(secs);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = crate::report::civil_from_days_for_ls(days);
    let month = MONTHS[(m as usize).saturating_sub(1).min(11)];
    // Six months in seconds is the ls convention, with a day of slack for a future time.
    let recent = (now - secs).abs() < 15_552_000;
    if recent {
        format!(
            "{} {:>2} {:02}:{:02}",
            month,
            d,
            rem / 3600,
            (rem % 3600) / 60
        )
    } else {
        format!("{} {:>2}  {:04}", month, d, y)
    }
}

/// `drwxr-xr-x` and friends, the way `find -ls` prints a mode. `%M` in `-printf` is rendered
/// by the expression module itself; this is the same text for the `-ls` column.
fn ls_mode(mode: u32, kind: findexpr::Kind) -> String {
    let mut s = String::with_capacity(10);
    s.push(if kind == findexpr::Kind::File {
        '-'
    } else {
        kind.letter()
    });
    let bit = |shift: u32| mode & (1 << shift) != 0;
    for (r, w, x, special, letter) in [
        (8u32, 7u32, 6u32, 11u32, 's'),
        (5, 4, 3, 10, 's'),
        (2, 1, 0, 9, 't'),
    ] {
        s.push(if bit(r) { 'r' } else { '-' });
        s.push(if bit(w) { 'w' } else { '-' });
        s.push(match (bit(x), bit(special)) {
            (true, true) => letter,
            (true, false) => 'x',
            (false, true) => letter.to_ascii_uppercase(),
            (false, false) => '-',
        });
    }
    s
}

/// Run one `-exec ... ;`, replacing every `{}` in every argument with the entry's path,
/// from `dir` when the predicate was `-execdir`.
fn exec_one(command: &[String], path: &str, dir: Option<&Path>) -> std::io::Result<bool> {
    let mut argv: Vec<String> = command.iter().map(|a| a.replace("{}", path)).collect();
    let program = argv.remove(0);
    let mut cmd = std::process::Command::new(program);
    cmd.args(&argv);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    Ok(cmd.status()?.success())
}

/// Run one `-exec ... {} +` over every path it collected: a trailing `{}` becomes all of
/// them as separate arguments, as find does.
fn exec_batch(command: &[String], paths: &[String], dir: Option<&Path>) -> std::io::Result<bool> {
    let mut argv: Vec<String> = command[1..].to_vec();
    if argv.last().is_some_and(|a| a == "{}") {
        argv.pop();
    }
    argv.extend(paths.iter().cloned());
    let mut cmd = std::process::Command::new(&command[0]);
    cmd.args(&argv);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    Ok(cmd.status()?.success())
}

/// User and group names for `-user`, `-group`, `-nouser`, `-nogroup`, `%u`, `%g` and the
/// `-ls` columns.
#[cfg(unix)]
struct PasswdIds {
    users: std::collections::HashMap<String, u32>,
    uids: std::collections::HashMap<u32, String>,
    groups: std::collections::HashMap<String, u32>,
    gids: std::collections::HashMap<u32, String>,
}

#[cfg(unix)]
impl PasswdIds {
    /// Read `/etc/passwd` and `/etc/group` once; a missing file leaves that half numeric.
    fn load() -> PasswdIds {
        let mut ids = PasswdIds {
            users: std::collections::HashMap::new(),
            uids: std::collections::HashMap::new(),
            groups: std::collections::HashMap::new(),
            gids: std::collections::HashMap::new(),
        };
        for (path, names, numbers) in [
            ("/etc/passwd", &mut ids.users, &mut ids.uids),
            ("/etc/group", &mut ids.groups, &mut ids.gids),
        ] {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            for line in text.lines() {
                let mut fields = line.split(':');
                let (Some(name), Some(_), Some(id)) = (fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                let Ok(id) = id.parse::<u32>() else { continue };
                names.insert(name.to_string(), id);
                numbers.entry(id).or_insert_with(|| name.to_string());
            }
        }
        ids
    }
}

#[cfg(unix)]
impl Ids for PasswdIds {
    fn uid_of(&self, name: &str) -> Option<u32> {
        name.parse().ok().or_else(|| self.users.get(name).copied())
    }

    fn gid_of(&self, name: &str) -> Option<u32> {
        name.parse().ok().or_else(|| self.groups.get(name).copied())
    }

    fn user_of(&self, uid: u32) -> Option<String> {
        self.uids.get(&uid).cloned()
    }

    fn group_of(&self, gid: u32) -> Option<String> {
        self.gids.get(&gid).cloned()
    }
}

/// Windows has no passwd table to read, so names stay numeric and `-nouser`/`-nogroup` are
/// true for every entry, which [`findexpr::NumericIds`] documents.
#[cfg(unix)]
fn run_ids() -> PasswdIds {
    PasswdIds::load()
}

#[cfg(not(unix))]
fn run_ids() -> findexpr::NumericIds {
    findexpr::NumericIds
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

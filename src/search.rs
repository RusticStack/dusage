//! Content and path search behind `dusage search` (a grep/rg replacement) and
//! `dusage find` (a name/path walker).
//!
//! The walk is single-threaded and deterministic — it lists candidates in one pass,
//! applying hidden, ignore-file and `--glob` rules — and the expensive part, reading and
//! matching file contents, runs on all worker threads through [`par_for_each`].

use crate::bytes;
use crate::encodings::{self, Encoding};
use crate::ignore::IgnoreRules;
use crate::model::ErrorCounts;
use crate::native::enumerate;
use crate::par_for_each;
use crate::pattern::{Match, Pattern};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Read size for content scanning. Large enough to matter, small enough to stay in cache.
const BLOCK: usize = 64 * 1024;
/// Bytes inspected for a NUL when deciding whether a file is binary.
const BINARY_SCAN: usize = 8 * 1024;
/// Upper bound on how many matches `-o` will report on one line, so a pathological
/// pattern cannot blow up memory.
const MAX_PER_LINE: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Match file contents (`search`).
    Content,
    /// Match paths (`find`).
    Paths,
}

/// Everything the searcher needs to know.
pub struct SearchOptions {
    pub roots: Vec<PathBuf>,
    pub scope: Scope,
    pub pattern: Pattern,
    /// Invert the match (`-v`), content scope only.
    pub invert: bool,
    pub line_numbers: bool,
    /// `None` decides from the number of roots: on for many or a directory walk.
    pub with_filename: Option<bool>,
    pub before: usize,
    pub after: usize,
    pub max_count: Option<u64>,
    pub only_matching: bool,
    pub files_with_matches: bool,
    pub files_without_matches: bool,
    pub count: bool,
    pub quiet: bool,
    pub hidden: bool,
    pub no_ignore: bool,
    pub globs: Vec<String>,
    /// `-t`/`--ext` patterns: they filter results but leave the ignore rules in force.
    pub type_globs: Vec<String>,
    pub max_depth: Option<usize>,
    pub max_file_size: Option<u64>,
    pub threads: usize,
    pub one_filesystem: bool,
    /// Search binary files (`-a`) instead of reporting them as binary.
    pub text: bool,
    /// Print the largest-match number of bytes per line: `--max-columns`, 0 = unlimited.
    pub max_columns: usize,
    /// Force a text encoding, or `None` to detect one per file.
    pub encoding: Option<Encoding>,
    /// Separate the file name from the rest with a NUL byte (`-Z`/`--null`).
    pub null: bool,
}

impl SearchOptions {
    pub fn new(pattern: Pattern, roots: Vec<PathBuf>) -> Self {
        SearchOptions {
            roots,
            scope: Scope::Content,
            pattern,
            invert: false,
            line_numbers: false,
            with_filename: None,
            before: 0,
            after: 0,
            max_count: None,
            only_matching: false,
            files_with_matches: false,
            files_without_matches: false,
            count: false,
            quiet: false,
            hidden: false,
            no_ignore: false,
            globs: Vec::new(),
            type_globs: Vec::new(),
            max_depth: None,
            max_file_size: None,
            threads: crate::cli::default_threads(),
            one_filesystem: false,
            text: false,
            max_columns: 0,
            encoding: None,
            null: false,
        }
    }
}

/// One emitted line: a match, or a line of context around one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHit {
    pub number: u64,
    /// Line bytes without the newline (a trailing `\r` is removed for `\r\n` files).
    pub text: Vec<u8>,
    /// False for context lines.
    pub matched: bool,
    /// Byte spans inside `text` for `-o` and colouring; empty on context lines.
    pub spans: Vec<(usize, usize)>,
}

/// Everything found in one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileHits {
    pub path: String,
    pub hits: Vec<LineHit>,
    /// Matching lines (not context lines).
    pub matches: u64,
    /// The file looks binary (a NUL in its first 8 KiB) and `-a` was not given.
    pub binary: bool,
}

/// Counters for `--stats`.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub files_scanned: u64,
    pub files_matched: u64,
    pub files_binary: u64,
    pub dirs_walked: u64,
    pub entries_ignored: u64,
    pub bytes_scanned: u64,
    pub matches: u64,
    pub errors: ErrorCounts,
    pub elapsed: Duration,
}

#[derive(Debug, Default)]
pub struct SearchReport {
    pub files: Vec<FileHits>,
    /// Every candidate path, for path scope (a flat listing).
    pub paths: Vec<Candidate>,
    pub stats: Stats,
    /// True when at least one file matched (or, for `-L`, at least one path printed).
    pub any_match: bool,
}

impl SearchReport {
    /// Record one matched file, honouring the modes that do not keep line text.
    fn files_matched_added(&mut self, got: &FileHits, o: &SearchOptions) {
        self.stats.files_matched += 1;
        self.any_match = true;
        if o.count || o.files_with_matches {
            self.files.push(FileHits {
                hits: Vec::new(),
                ..got.clone()
            });
        } else {
            self.files.push(got.clone());
        }
    }
}

/// One file or directory the walker produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub rel: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub is_dir: bool,
    pub is_link: bool,
}

struct Walker<'a> {
    o: &'a SearchOptions,
    rules: IgnoreRules,
    root_dev: u64,
    out: Vec<Candidate>,
    stats: Stats,
    /// Emit directories as candidates too (path scope wants them).
    want_dirs: bool,
}

impl Walker<'_> {
    fn dir(&mut self, dir: &Path, rel: &str, depth: usize) {
        let rel = rel.trim_end_matches('/');
        let mark = if self.o.no_ignore {
            self.rules.mark()
        } else {
            self.rules.push_dir(dir, rel)
        };
        let mut ents = Vec::new();
        match enumerate::DirReader::new().read(dir, &mut ents, &mut self.stats.errors) {
            Ok(()) => {}
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    self.stats.errors.record(&e);
                }
                self.rules.pop_to(mark);
                return;
            }
        }
        self.stats.dirs_walked += 1;
        for e in &ents {
            let name = e.name.to_string_lossy();
            let child_rel = join_rel(rel, &name);
            let is_dir = e.is_dir() && !e.is_reparse();
            // A walk never follows a link: a reparse point is listed as a link or not at all.
            if e.is_reparse() {
                self.stats.entries_ignored += 1;
                continue;
            }
            // `--max-depth N` counts the path levels below the root that may be reported.
            if self.o.max_depth.is_some_and(|m| depth + 1 > m) {
                self.stats.entries_ignored += 1;
                continue;
            }
            if !self.o.hidden && name.starts_with('.') {
                // A positive glob re-includes a dotfile it names, as rg does; directories
                // still need `--hidden` so the walk never wanders into `.git`.
                let named = !is_dir && self.rules.glob_named(&child_rel);
                if !named {
                    self.stats.entries_ignored += 1;
                    continue;
                }
            }
            // A positive glob switches the ignore files off (`-g '*.log'` finds a file a
            // `.gitignore` hides), exactly as ripgrep does; exclusions always apply.
            let globs_win = self.rules.whitelisted() || self.o.no_ignore;
            if !globs_win && self.rules.is_ignored(&child_rel, is_dir) {
                self.stats.entries_ignored += 1;
                continue;
            }
            if !self.rules.glob_allows(&child_rel, is_dir) {
                self.stats.entries_ignored += 1;
                continue;
            }
            if is_dir {
                if self.o.one_filesystem {
                    let child = dir.join(&e.name);
                    match enumerate::dev_of(&child) {
                        Ok(dev) if dev != self.root_dev => continue,
                        _ => {}
                    }
                }
                if self.want_dirs {
                    self.out
                        .push(self.candidate(dir, &e.name, &child_rel, true, e.size, None));
                }
                if self.o.max_depth.is_none_or(|m| depth + 1 < m) {
                    self.dir(&dir.join(&e.name), &child_rel, depth + 1);
                }
            } else {
                if let Some(limit) = self.o.max_file_size {
                    if e.size > limit {
                        continue;
                    }
                }
                let modified = Some(e.modified());
                self.out
                    .push(self.candidate(dir, &e.name, &child_rel, false, e.size, modified));
            }
        }
        self.rules.pop_to(mark);
    }

    fn candidate(
        &self,
        dir: &Path,
        name: &std::ffi::OsStr,
        rel: &str,
        is_dir: bool,
        size: u64,
        modified: Option<SystemTime>,
    ) -> Candidate {
        Candidate {
            path: dir.join(name),
            rel: rel.to_string(),
            size,
            modified,
            is_dir,
            is_link: false,
        }
    }
}

fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

fn build_walker(o: &SearchOptions) -> io::Result<Walker<'_>> {
    let mut rules = IgnoreRules::new(!o.no_ignore);
    rules
        .add_globs(&o.globs)
        .and_then(|()| rules.add_filters(&o.type_globs))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    Ok(Walker {
        o,
        rules,
        root_dev: 0,
        out: Vec::new(),
        stats: Stats::default(),
        want_dirs: o.scope == Scope::Paths,
    })
}

/// List every candidate under the roots, in a deterministic order.
fn walk(o: &SearchOptions, root_dev: u64) -> (Vec<Candidate>, Stats) {
    let mut all: Vec<Candidate> = Vec::new();
    let mut stats = Stats::default();
    for root in &o.roots {
        let mut w = build_walker(o).expect("glob dot-rule validation");
        w.root_dev = root_dev;
        // Explicit files on the command line are taken as given, filters aside.
        match std::fs::metadata(root) {
            Ok(md) if md.is_file() => {
                let rel = root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                w.out.push(Candidate {
                    path: root.clone(),
                    rel,
                    size: md.len(),
                    modified: md.modified().ok(),
                    is_dir: false,
                    is_link: false,
                });
            }
            Ok(_) => {
                let mark = if o.no_ignore {
                    w.rules.mark()
                } else {
                    w.rules.push_ancestors(root)
                };
                w.dir(root, "", 0);
                w.rules.pop_to(mark);
            }
            Err(e) => {
                w.stats.errors.record(&e);
            }
        }
        all.append(&mut w.out);
        stats.dirs_walked += w.stats.dirs_walked;
        stats.entries_ignored += w.stats.entries_ignored;
        stats.errors.add(&w.stats.errors);
    }
    if o.scope == Scope::Content {
        all.retain(|c| !c.is_dir);
    }
    all.sort_by(|a, b| a.rel.cmp(&b.rel).then_with(|| a.path.cmp(&b.path)));
    (all, stats)
}

/// Search `o.roots`. Files that match are returned with their lines; errors on individual
/// files are counted, never returned.
/// Search one in-memory buffer, for `-` on the command line (stdin, a pipe). The buffer is
/// treated as a single file named `name`: no walk, no ignore rules, no encoding detection.
pub fn search_buffer(o: &SearchOptions, name: &str, data: &[u8]) -> io::Result<SearchReport> {
    let t0 = Instant::now();
    let mut stats = Stats {
        files_scanned: 1,
        bytes_scanned: data.len() as u64,
        ..Default::default()
    };
    if bytes::is_binary(data, BINARY_SCAN.min(data.len())) {
        stats.files_binary += 1;
    }
    let mut sc = Scanner::new(o);
    sc.feed(data);
    let hit = finished(Path::new(name), o, &mut stats, sc, false)?;
    let mut report = SearchReport {
        files: Vec::new(),
        paths: Vec::new(),
        stats,
        any_match: false,
    };
    if let Some(f) = hit {
        report.files_matched_added(&f, o);
    }
    if o.files_without_matches {
        report.any_match = !report.files.is_empty();
    }
    report.stats.elapsed = t0.elapsed();
    Ok(report)
}

pub fn search(o: &SearchOptions) -> io::Result<SearchReport> {
    let t0 = Instant::now();
    let root_dev = if o.one_filesystem {
        o.roots
            .first()
            .and_then(|r| enumerate::dev_of(r).ok())
            .unwrap_or(0)
    } else {
        0
    };
    let (cands, mut stats) = walk(o, root_dev);
    let mut report = SearchReport {
        paths: Vec::new(),
        files: Vec::new(),
        stats: Stats::default(),
        any_match: false,
    };
    if o.scope == Scope::Paths {
        report.paths = cands;
        report.stats = stats;
        report.any_match = !report.paths.is_empty();
        report.stats.elapsed = t0.elapsed();
        return Ok(report);
    }
    if o.quiet {
        for c in &cands {
            match scan_file(&c.path, o, &mut stats) {
                Ok(Some(f)) if f.matches > 0 => {
                    report.any_match = true;
                    report.stats = stats;
                    report.stats.elapsed = t0.elapsed();
                    return Ok(report);
                }
                Ok(_) => {}
                Err(e) => {
                    if e.kind() != io::ErrorKind::NotFound {
                        stats.errors.record(&e);
                    }
                }
            }
        }
        report.stats = stats;
        report.stats.elapsed = t0.elapsed();
        return Ok(report);
    }
    let slots: Mutex<(Vec<Option<FileHits>>, Stats)> =
        Mutex::new((vec![None; cands.len()], Stats::default()));
    let items: Vec<(usize, PathBuf)> = cands
        .iter()
        .enumerate()
        .map(|(i, c)| (i, c.path.clone()))
        .collect();
    par_for_each(items, o.threads, |(i, path), _push| {
        let mut local = Stats::default();
        let got = match scan_file(path, o, &mut local) {
            Ok(got) => got,
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    local.errors.record(&e);
                }
                None
            }
        };
        let mut guard = slots.lock().unwrap_or_else(|e| e.into_inner());
        guard.0[*i] = got;
        let agg = &mut guard.1;
        agg.files_scanned += local.files_scanned;
        agg.files_binary += local.files_binary;
        agg.bytes_scanned += local.bytes_scanned;
        agg.matches += local.matches;
        agg.errors.add(&local.errors);
    });
    let (found, agg) = slots.into_inner().unwrap_or_else(|e| e.into_inner());
    stats.files_scanned += agg.files_scanned;
    stats.files_binary += agg.files_binary;
    stats.bytes_scanned += agg.bytes_scanned;
    stats.matches += agg.matches;
    stats.errors.add(&agg.errors);
    for got in found.into_iter().flatten() {
        if o.files_without_matches {
            if got.matches == 0 {
                report.files.push(got);
            }
            continue;
        }
        if got.matches == 0 {
            continue;
        }
        stats.files_matched += 1;
        report.any_match = true;
        // `-c` and `-l` print a number or a path, so the lines are not kept.
        if o.count || o.files_with_matches {
            report.files.push(FileHits {
                hits: Vec::new(),
                ..got
            });
        } else {
            report.files.push(got);
        }
    }
    if o.files_without_matches {
        report.any_match = !report.files.is_empty();
    }
    report.stats = stats;
    report.stats.elapsed = t0.elapsed();
    Ok(report)
}

/// Scan one file. `None` means "nothing to report" (no match, or filtered out).
fn scan_file(path: &Path, o: &SearchOptions, stats: &mut Stats) -> io::Result<Option<FileHits>> {
    // `-m 0` asks for no match at all: never open the file.
    if o.max_count == Some(0) {
        return Ok(None);
    }
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; BLOCK];
    stats.files_scanned += 1;
    let n = f.read(&mut buf)?;
    if n == 0 {
        return finished(path, o, stats, Scanner::new(o), false);
    }
    // A file that needs transcoding is read whole: UTF-16 code units straddle the block
    // boundary, so there is nothing to stream. Everything else keeps the streaming path.
    let binary = bytes::is_binary(&buf[..n], BINARY_SCAN.min(n));
    // Only a NUL-heavy or BOM-carrying file can be UTF-16, and the binary check has already
    // looked for NULs, so plain text skips the sniff entirely.
    let encoding = o.encoding.unwrap_or_else(|| {
        if binary || buf[..n].starts_with(&[0xEF, 0xBB, 0xBF]) {
            encodings::detect(&buf[..n.min(BINARY_SCAN)])
        } else {
            Encoding::Utf8
        }
    });
    if encodings::needs_transcode(encoding) {
        let mut all = buf[..n].to_vec();
        f.read_to_end(&mut all)?;
        stats.bytes_scanned += all.len() as u64;
        let text = encodings::to_utf8(&all, encoding);
        let mut sc = Scanner::new(o);
        sc.feed(&text);
        return finished(path, o, stats, sc, false);
    }
    let mut sc = Scanner::new(o);
    if binary {
        stats.files_binary += 1;
    }
    stats.bytes_scanned += n as u64;
    sc.feed(&buf[..n]);
    while !sc.done() {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        stats.bytes_scanned += n as u64;
        sc.feed(&buf[..n]);
    }
    finished(path, o, stats, sc, binary)
}

/// The per-file line state: carries the partial last line across blocks and accumulates hits.
struct Scanner<'a> {
    o: &'a SearchOptions,
    carry: Vec<u8>,
    history: VecDeque<(u64, Vec<u8>)>,
    hits: Vec<LineHit>,
    line_no: u64,
    matches: u64,
    remaining_after: usize,
    last_emitted: u64,
    want_lines: bool,
    limit: u64,
    stopped: bool,
    /// One reusable buffer per file: the current line, so no line costs an allocation.
    line: Vec<u8>,
}

impl<'a> Scanner<'a> {
    fn new(o: &'a SearchOptions) -> Self {
        Scanner {
            o,
            carry: Vec::new(),
            history: VecDeque::new(),
            hits: Vec::new(),
            line_no: 0,
            matches: 0,
            remaining_after: 0,
            last_emitted: 0,
            want_lines: !o.count && !o.files_with_matches && !o.files_without_matches && !o.quiet,
            limit: o.max_count.unwrap_or(u64::MAX),
            stopped: false,
            line: Vec::new(),
        }
    }

    /// True once `--max-count` is reached: the file needs no more reading.
    fn done(&self) -> bool {
        self.stopped || self.matches >= self.limit
    }

    fn feed(&mut self, chunk: &[u8]) {
        if self.stopped || self.matches >= self.limit {
            return;
        }
        let mut start = 0;
        while let Some(pos) = bytes::find_byte(chunk, b'\n', start) {
            self.line.clear();
            if !self.carry.is_empty() {
                self.line.extend_from_slice(&self.carry);
                self.carry.clear();
            }
            self.line.extend_from_slice(&chunk[start..pos]);
            start = pos + 1;
            self.line_no += 1;
            if self.emit() {
                self.stopped = true;
                return;
            }
            if self.matches >= self.limit {
                return;
            }
        }
        if start < chunk.len() {
            self.carry.extend_from_slice(&chunk[start..]);
        }
    }

    /// Handle the trailing line of a file that does not end in a newline.
    fn flush(&mut self) {
        if self.done() || self.carry.is_empty() {
            return;
        }
        self.line.clear();
        std::mem::swap(&mut self.line, &mut self.carry);
        self.carry.clear();
        self.line_no += 1;
        self.emit();
    }

    /// Run the matcher over the buffered line; true means the file is finished (`-m`).
    fn emit(&mut self) -> bool {
        handle_line(
            &self.line,
            self.line_no,
            self.o,
            &mut self.history,
            &mut self.hits,
            &mut self.matches,
            &mut self.remaining_after,
            &mut self.last_emitted,
            self.want_lines,
            self.limit,
        )
    }
}

/// Turn a finished scan into the report for one file.
fn finished(
    path: &Path,
    o: &SearchOptions,
    stats: &mut Stats,
    mut sc: Scanner<'_>,
    binary: bool,
) -> io::Result<Option<FileHits>> {
    sc.flush();
    if sc.matches == 0 {
        return Ok(if o.files_without_matches {
            Some(FileHits {
                path: path.to_string_lossy().into_owned(),
                hits: Vec::new(),
                matches: 0,
                binary,
            })
        } else {
            None
        });
    }
    stats.matches += sc.matches;
    for h in &mut sc.hits {
        if o.max_columns > 0 && h.text.len() > o.max_columns {
            h.text.truncate(o.max_columns);
            h.spans.retain(|(s, _)| *s < o.max_columns);
            for (_, e) in h.spans.iter_mut() {
                if *e > o.max_columns {
                    *e = o.max_columns;
                }
            }
        }
    }
    Ok(Some(FileHits {
        path: path.to_string_lossy().into_owned(),
        hits: sc.hits,
        matches: sc.matches,
        binary,
    }))
}

/// Handle one line. Returns true when the file should stop being read (`-m` reached).
#[allow(clippy::too_many_arguments)]
fn handle_line(
    line: &[u8],
    no: u64,
    o: &SearchOptions,
    history: &mut VecDeque<(u64, Vec<u8>)>,
    hits: &mut Vec<LineHit>,
    matches: &mut u64,
    remaining_after: &mut usize,
    last_emitted: &mut u64,
    want_lines: bool,
    limit: u64,
) -> bool {
    let text = strip_cr(line);
    let found = o.pattern.find_at(text, 0);
    if found.is_some() == o.invert {
        // A context line after a match, while the window lasts.
        if want_lines && *remaining_after > 0 {
            hits.push(LineHit {
                number: no,
                text: text.to_vec(),
                matched: false,
                spans: Vec::new(),
            });
            *last_emitted = no;
            *remaining_after -= 1;
        } else if want_lines && o.before > 0 {
            history.push_back((no, text.to_vec()));
            while history.len() > o.before {
                history.pop_front();
            }
        }
        return false;
    }
    *matches += 1;
    if !want_lines {
        return *matches >= limit;
    }
    // Background: emit the before-context lines that were not emitted yet.
    if o.before > 0 {
        let keep: Vec<(u64, Vec<u8>)> = history
            .iter()
            .filter(|(n, _)| *n > *last_emitted)
            .cloned()
            .collect();
        for (n, t) in keep {
            hits.push(LineHit {
                number: n,
                text: t,
                matched: false,
                spans: Vec::new(),
            });
            *last_emitted = n;
        }
        history.clear();
    }
    let spans: Vec<(usize, usize)> = if o.invert {
        Vec::new()
    } else {
        o.pattern
            .find_all(text, MAX_PER_LINE)
            .into_iter()
            .map(|m: Match| (m.start, m.end))
            .collect()
    };
    if o.only_matching && !spans.is_empty() {
        for (s, e) in spans {
            hits.push(LineHit {
                number: no,
                text: text[s..e].to_vec(),
                matched: true,
                spans: vec![(0, e - s)],
            });
            *last_emitted = no;
        }
    } else {
        hits.push(LineHit {
            number: no,
            text: text.to_vec(),
            matched: true,
            spans,
        });
        *last_emitted = no;
    }
    *remaining_after = o.after;
    *matches >= limit
}

fn strip_cr(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

/// True when a report has output worth printing.
pub fn has_output(r: &SearchReport, o: &SearchOptions) -> bool {
    if o.scope == Scope::Paths {
        return !r.paths.is_empty();
    }
    if o.count || o.files_with_matches || o.files_without_matches {
        return !r.files.is_empty();
    }
    r.files.iter().any(|f| !f.hits.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering as O};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "dusage_search_{tag}_{}_{}",
                std::process::id(),
                N.fetch_add(1, O::Relaxed)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Fixture(p)
        }
        fn file(&self, rel: &str, text: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, text).unwrap();
            p
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn opts(f: &Fixture, pat: &str) -> SearchOptions {
        SearchOptions {
            threads: 4,
            ..SearchOptions::new(
                Pattern::new(pat, false, false, false).unwrap(),
                vec![f.0.clone()],
            )
        }
    }

    fn lines(r: &SearchReport) -> Vec<(String, u64, String, bool)> {
        r.files
            .iter()
            .flat_map(|f| {
                f.hits.iter().map(move |h| {
                    (
                        f.path.clone(),
                        h.number,
                        String::from_utf8_lossy(&h.text).into_owned(),
                        h.matched,
                    )
                })
            })
            .collect()
    }

    #[test]
    fn finds_lines_across_files() {
        let f = Fixture::new("basic");
        f.file("a.txt", "alpha\nneedle here\nomega\n");
        f.file("b/c.txt", "no match\n\nanother needle and needle\n");
        let r = search(&opts(&f, "needle")).unwrap();
        let got = lines(&r);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].1, 2);
        assert_eq!(got[1].1, 3);
        assert_eq!(r.stats.files_matched, 2);
        assert_eq!(r.stats.matches, 2, "matching lines, not occurrences");
        assert_eq!(
            r.files[1].hits[0].spans.len(),
            2,
            "two occurrences on one line"
        );
        assert!(r.any_match);
    }

    #[test]
    fn invert_count_and_max_count() {
        let f = Fixture::new("modes");
        f.file("a.txt", "one\ntwo\nthree\nfour\n");
        let mut o = opts(&f, "o");
        o.invert = true;
        let r = search(&o).unwrap();
        assert_eq!(r.stats.matches, 1, "only 'three' has no 'o'");
        let mut o = opts(&f, "o");
        o.count = true;
        let r = search(&o).unwrap();
        assert_eq!(r.stats.matches, 3, "one, two and four contain an 'o'");
        assert!(r.files[0].hits.is_empty(), "count mode prints numbers only");
        let mut o = opts(&f, "e");
        o.max_count = Some(1);
        let r = search(&o).unwrap();
        assert_eq!(r.stats.matches, 1);
    }

    #[test]
    fn context_windows() {
        let f = Fixture::new("ctx");
        f.file("a.txt", "1\n2\n3\nHIT\n5\n6\n7\n8\n");
        let mut o = opts(&f, "HIT");
        o.before = 1;
        o.after = 2;
        let r = search(&o).unwrap();
        let got: Vec<(u64, bool)> = r.files[0]
            .hits
            .iter()
            .map(|h| (h.number, h.matched))
            .collect();
        assert_eq!(got, [(3, false), (4, true), (5, false), (6, false)]);
    }

    #[test]
    fn only_matching_and_spans() {
        let f = Fixture::new("only");
        f.file("a.txt", "x foo y foo z\n");
        let mut o = opts(&f, "foo");
        o.only_matching = true;
        let r = search(&o).unwrap();
        let got = lines(&r);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].2, "foo");
        let mut o = opts(&f, "foo");
        o.only_matching = false;
        let r = search(&o).unwrap();
        assert_eq!(r.files[0].hits[0].spans, vec![(2, 5), (8, 11)]);
    }

    #[test]
    fn ignore_and_hidden_rules() {
        let f = Fixture::new("ignore");
        f.file("keep.txt", "needle\n");
        f.file("node_modules/x.js", "needle\n");
        f.file(".hidden.txt", "needle\n");
        f.file("sub/.gitignore", "*.log\n");
        f.file("sub/skip.log", "needle\n");
        f.file("sub/keep.log", "needle\n");
        let r = search(&opts(&f, "needle")).unwrap();
        let paths: Vec<String> = r.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(paths.len(), 1, "only keep.txt survives: {paths:?}");
        assert!(paths[0].ends_with("keep.txt"));
        let mut o = opts(&f, "needle");
        o.hidden = true;
        o.no_ignore = true;
        let r = search(&o).unwrap();
        assert_eq!(r.files.len(), 5, "everything comes back with filters off");
    }

    #[test]
    fn binary_files_are_flagged() {
        let f = Fixture::new("bin");
        let p = f.0.join("blob.bin");
        fs::write(&p, b"head\0needle\ntail\n").unwrap();
        let r = search(&opts(&f, "needle")).unwrap();
        assert!(r.files[0].binary);
        assert_eq!(r.stats.files_binary, 1);
        let mut o = opts(&f, "needle");
        o.text = true;
        let r = search(&o).unwrap();
        assert!(r.files[0].binary, "still flagged, but searched");
    }

    #[test]
    fn glob_filters_and_depth() {
        let f = Fixture::new("glob");
        f.file("a.rs", "needle\n");
        f.file("b.py", "needle\n");
        f.file("d/e/f.rs", "needle\n");
        let mut o = opts(&f, "needle");
        o.globs = vec!["*.rs".into()];
        let r = search(&o).unwrap();
        assert_eq!(r.files.len(), 2);
        let mut o = opts(&f, "needle");
        o.max_depth = Some(1);
        let r = search(&o).unwrap();
        assert_eq!(r.files.len(), 2, "a.rs and b.py, not d/e/f.rs");
    }

    #[test]
    fn quiet_stops_at_the_first_match() {
        let f = Fixture::new("quiet");
        f.file("a.txt", "needle\n");
        f.file("b.txt", "needle\n");
        let mut o = opts(&f, "needle");
        o.quiet = true;
        let r = search(&o).unwrap();
        assert!(r.any_match);
        assert!(r.files.is_empty());
        assert_eq!(r.stats.files_scanned, 1);
    }

    #[test]
    fn files_without_matches_lists_the_rest() {
        let f = Fixture::new("without");
        f.file("yes.txt", "needle\n");
        f.file("no.txt", "other\n");
        let mut o = opts(&f, "needle");
        o.files_without_matches = true;
        let r = search(&o).unwrap();
        let paths: Vec<String> = r.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("no.txt"));
    }

    #[test]
    fn crlf_lines_drop_the_carriage_return() {
        let f = Fixture::new("crlf");
        f.file("a.txt", "one\r\nneedle\r\nthree\r\n");
        let r = search(&opts(&f, "^needle$")).unwrap();
        assert_eq!(r.stats.matches, 1);
        assert_eq!(r.files[0].hits[0].text, b"needle");
    }

    #[test]
    fn path_scope_collects_files_and_dirs() {
        let f = Fixture::new("paths");
        f.file("a.rs", "x");
        f.file("sub/b.rs", "x");
        f.file("sub/c.txt", "x");
        let mut o = SearchOptions::new(
            Pattern::new("*.rs", true, false, false).unwrap(),
            vec![f.0.clone()],
        );
        o.scope = Scope::Paths;
        let r = search(&o).unwrap();
        let rels: Vec<&str> = r.paths.iter().map(|c| c.rel.as_str()).collect();
        assert!(
            rels.contains(&"a.rs") && rels.contains(&"sub/b.rs"),
            "{rels:?}"
        );
        assert!(
            rels.contains(&"sub"),
            "directories are candidates too: {rels:?}"
        );
        assert!(!r.paths.is_empty());
    }

    #[test]
    fn long_line_is_split_correctly_across_reads() {
        let f = Fixture::new("long");
        let mut text = String::new();
        for i in 0..40_000 {
            text.push_str(&format!("line {i} "));
            if i % 1000 == 0 {
                text.push_str("needle ");
            }
        }
        text.push('\n');
        f.file("big.txt", &text);
        let r = search(&opts(&f, "needle")).unwrap();
        assert_eq!(r.stats.matches, 1);
        assert_eq!(r.files[0].hits[0].number, 1);
    }
}

//! Content and path search behind `dusage search` (a grep/rg replacement) and
//! `dusage find` (a name/path walker).
//!
//! The walk is single-threaded and deterministic — it lists candidates in one pass,
//! applying hidden, ignore-file and `--glob` rules — and the expensive part, reading and
//! matching file contents, runs on all worker threads through [`par_for_each`].

use crate::bytes;
use crate::encodings::{self, Encoding};
use crate::findexpr::{self, Ctx, Expr, Kind, Needs, Sink};
use crate::ignore::IgnoreRules;
use crate::model::{ErrorCounts, Stat};
use crate::native::enumerate;
use crate::par_for_each;
use crate::pattern::{Match, Pattern};
use std::borrow::Cow;
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Read size for content scanning. Large enough to matter, small enough to stay in cache.
const BLOCK: usize = 64 * 1024;
/// The read size a file switches to once one [`BLOCK`] has not finished it.
const BIG_BLOCK: usize = 1024 * 1024;
/// The longest line this scanner will hold before deciding the data is not really made of
/// lines: a multi-megabyte binary with no newline in it otherwise accumulates the whole file in
/// the carry buffer, which is how a search over `/usr` reached 300 MB resident. Beyond this a
/// line is matched in pieces, so a `-w` boundary at such a split is not a real boundary.
const CARRY_MAX: usize = 8 * 1024 * 1024;
/// Bytes inspected for a NUL when deciding whether a file is binary.
const BINARY_SCAN: usize = 8 * 1024;
/// Upper bound on how many matches `-o` will report on one line, so a pathological
/// pattern cannot blow up memory.
const MAX_PER_LINE: usize = 1024;
/// How much match text one file pools before the rest of it is stored per record instead: a
/// match's place in the pool is a `u32` pair.
const POOL_MAX: usize = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Match file contents (`search`).
    Content,
    /// Match paths (`find`).
    Paths,
}

/// Whether a walk fills each candidate's absolute path. A listing prints `rel` and takes the
/// size and time from the directory record, so the path is dead weight per entry unless a
/// filter that stats by path (`--newerat`, `--newerct`) is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AbsPath {
    #[default]
    Skip,
    Fill,
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
    /// Print the total number of matches per file instead of matching lines (`--count-matches`).
    pub count_matches: bool,
    pub quiet: bool,
    pub hidden: bool,
    pub no_ignore: bool,
    pub globs: Vec<String>,
    /// `--iglob` patterns: like `globs`, matched without case.
    pub iglobs: Vec<String>,
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
    /// Mark a line that `--max-columns` cut (`--max-columns-preview`).
    pub max_columns_preview: bool,
    /// Force a text encoding, or `None` to detect one per file.
    pub encoding: Option<Encoding>,
    /// Separate the file name from the rest with a NUL byte (`-Z`/`--null`).
    pub null: bool,
    /// Ask the walk for each entry's size and mtime: only the modes that print or filter on
    /// them need it, and every other walk saves an lstat per file.
    pub details: bool,
    /// Colour will be used, so match spans have to be located per line.
    pub paint: bool,
    /// Print the file name on its own line above its matches (`--heading`).
    pub heading: bool,
    /// Print the column of the first match on each line (`--column`).
    pub column: bool,
    /// Print the byte offset of each line within its file (`--byte-offset`).
    pub byte_offset: bool,
    /// With `-c`, also print the files that matched nothing (`--include-zero`).
    pub include_zero: bool,
    /// Do not report paths that could not be read (`--no-messages`).
    pub quiet_errors: bool,
    /// Print every line, matching or not (`--passthru`).
    pub passthru: bool,
    /// Replace each match with this template before printing (`-r`), already parsed into
    /// literal text and group references.
    pub replace: Option<Vec<Segment>>,
    /// The order results print in, and whether it is reversed (`--sort`, `--sortr`).
    pub sort: SortKey,
    pub sort_reverse: bool,
    /// Leave the VCS rule files (`.gitignore`, `.git/info/exclude`) unread.
    pub no_ignore_vcs: bool,
    /// Leave `.ignore` unread.
    pub no_ignore_dot: bool,
    /// Do not stack the rule files of a root's parent directories.
    pub no_ignore_parent: bool,
    /// Leave git's global excludes file unread (`--no-ignore-global`).
    pub no_ignore_global: bool,
    /// Explicit ignore files from `--ignore-file`, in the order given.
    pub ignore_files: Vec<String>,
    /// Read the `--ignore-file` patterns without case (`--ignore-file-case-insensitive`).
    pub ignore_file_icase: bool,
    /// Follow symbolic links, refusing to walk a directory twice (`--follow`).
    pub follow: bool,
    /// Let a match span lines (`-U`): the file is searched whole rather than line by line.
    pub multiline: bool,
    /// Expand gzip files before searching them (`-z`).
    pub search_zip: bool,
    /// A binary file is not a match at all (`-I`, `--binary-files=without-match`).
    pub skip_binary: bool,
    /// A command to run on each file first, whose output is what gets searched (`--pre`).
    pub pre: Option<String>,
    /// `--pre-glob` patterns: the files `pre` runs on, or every file when empty.
    pub pre_globs: Vec<String>,
    /// Records are separated by NUL instead of newline (`--null-data`), and printed the same
    /// way: the terminator is `self.term`.
    pub null_data: bool,
    /// The byte that ends a record and an output line: `\n`, or NUL under `--null-data`.
    pub term: u8,
    /// Print every match on its own `path:line:column:text` row (`--vimgrep`).
    pub vimgrep: bool,
    /// Drop the leading whitespace of each printed line (`--trim`), and measure
    /// `--max-columns` against what is left.
    pub trim: bool,
    /// Stop reading a file at the first line that does not match once one has
    /// (`--stop-on-nonmatch`).
    pub stop_on_nonmatch: bool,
    /// The byte printed wherever a path holds a separator (`--path-separator`).
    pub path_separator: Option<u8>,
    /// Between the fields of a matching line (`--field-match-separator`), `:` by default.
    pub field_match_sep: Vec<u8>,
    /// Between the fields of a context line (`--field-context-separator`), `-` by default.
    pub field_context_sep: Vec<u8>,
    /// Between context groups (`--context-separator`), `--` by default; `None` prints
    /// nothing at all (`--no-context-separator`).
    pub context_sep: Option<Vec<u8>>,
}

/// One piece of a `-r` replacement: text to print, or a group of the match it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(Vec<u8>),
    /// A capture group, 0 being the whole match.
    Group(usize),
}

/// How a report orders its results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// Lexicographic by path relative to the root: the default, and what makes two runs of
    /// the same search print the same thing.
    #[default]
    Path,
    /// Leave the order the walk and the workers produced (`--sort none`).
    None,
    /// Oldest modification time first (`--sort modified`).
    Modified,
    /// Oldest access time first (`--sort accessed`).
    Accessed,
    /// Oldest creation time first (`--sort created`), which on Linux is the inode's change
    /// time where no birth time is recorded.
    Created,
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
            count_matches: false,
            quiet: false,
            hidden: false,
            no_ignore: false,
            globs: Vec::new(),
            iglobs: Vec::new(),
            type_globs: Vec::new(),
            max_depth: None,
            max_file_size: None,
            threads: crate::cli::default_threads(),
            one_filesystem: false,
            text: false,
            max_columns: 0,
            max_columns_preview: false,
            encoding: None,
            null: false,
            details: false,
            paint: false,
            heading: false,
            column: false,
            byte_offset: false,
            include_zero: false,
            quiet_errors: false,
            passthru: false,
            replace: None,
            sort: SortKey::Path,
            sort_reverse: false,
            no_ignore_vcs: false,
            no_ignore_dot: false,
            no_ignore_parent: false,
            no_ignore_global: false,
            ignore_files: Vec::new(),
            ignore_file_icase: false,
            follow: false,
            multiline: false,
            search_zip: false,
            skip_binary: false,
            pre: None,
            pre_globs: Vec::new(),
            null_data: false,
            term: b'\n',
            vimgrep: false,
            trim: false,
            stop_on_nonmatch: false,
            path_separator: None,
            field_match_sep: b":".to_vec(),
            field_context_sep: b"-".to_vec(),
            context_sep: Some(b"--".to_vec()),
        }
    }
}

/// One emitted line: a match, or a line of context around one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHit {
    /// Byte offset of this line's first byte within the file.
    pub offset: u64,
    pub number: u64,
    /// Line bytes without the newline (a trailing `\r` is removed for `\r\n` files), or
    /// empty when they live in the file's pool and `pooled` says where.
    pub text: Vec<u8>,
    /// Where this record's text is in its file's pool: what `-o` shares, so a match costs no
    /// allocation of its own. `(0, 0)` means the text is in `text`.
    pub pooled: (u32, u32),
    /// False for context lines.
    pub matched: bool,
    /// Byte spans inside `text` for colouring; empty on context lines and on `-o` records,
    /// which are nothing but the match and so need no span of their own.
    pub spans: Vec<(usize, usize)>,
    /// Where `text` starts within its line: 0 for a whole line, the match's offset for `-o`.
    /// `--column` and `--vimgrep` count a column from here.
    pub text_start: usize,
}

impl LineHit {
    /// The bytes this record prints, read out of the file's pool when it shares one.
    pub fn text<'a>(&'a self, pool: &'a [u8]) -> &'a [u8] {
        match self.pooled {
            (0, 0) => &self.text,
            (s, e) => &pool[s as usize..e as usize],
        }
    }
}

/// Everything found in one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileHits {
    pub path: String,
    pub hits: Vec<LineHit>,
    /// Matching lines (not context lines).
    pub matches: u64,
    /// Individual matches in the file, for `--count-matches`.
    pub total: u64,
    /// The file looks binary (a NUL in its first 8 KiB) and `-a` was not given.
    pub binary: bool,
    /// The text of the records that share one buffer rather than owning theirs (`-o`), so a
    /// file's matches cost it one allocation instead of one each.
    pub pool: Vec<u8>,
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
        if o.count || o.count_matches || o.files_with_matches {
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
    /// The entry's real path. [`search`] and the collecting walk always fill it; a streaming
    /// `--sort path` listing fills it only when asked ([`AbsPath::Fill`]), because printing a
    /// listing reads `rel` and a path costs an allocation and a copy per entry.
    pub path: PathBuf,
    pub rel: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub is_dir: bool,
    pub is_link: bool,
}

/// A walk entry that survived the filters, with the type its directory record gave it.
///
/// The type is free — it came with the `getdents` record — and it is what lets a scan stop
/// asking the kernel what it already knows: a `Kind::File` here means the `fstat` after the
/// open could only repeat it.
struct Found {
    cand: Candidate,
    /// A followed link is reported as what it resolved to, which the walk had to read anyway.
    kind: Kind,
}

fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        return name.to_string();
    }
    // One allocation of the exact size, rather than `format!`'s formatter and its growth: a
    // walk builds one of these per entry.
    let mut out = String::with_capacity(rel.len() + 1 + name.len());
    out.push_str(rel);
    out.push('/');
    out.push_str(name);
    out
}

/// One child's real path. `Path::join` copies the parent into a fresh buffer and then grows
/// it, which is two allocations and two copies of the parent's bytes for every entry a walk
/// lists; this asks for the whole length up front.
fn child_path(dir: &Path, name: &OsStr) -> PathBuf {
    let mut out = PathBuf::with_capacity(dir.as_os_str().len() + name.len() + 2);
    out.push(dir);
    out.push(name);
    out
}

/// One directory to read. The rules are this directory's ancestors; the directory's own
/// ignore files are pushed when the job runs, so a snapshot can be shared across threads.
#[derive(Clone)]
struct WalkJob {
    /// This job's identity in the read-ahead's maps. A directory is asked for by a value
    /// that is never hashed as a path.
    id: u64,
    dir: PathBuf,
    rel: String,
    depth: usize,
    rules: Arc<IgnoreRules>,
    dev: u64,
    /// The real paths of the directories from the root down to this one. Only kept when
    /// `--follow` can make a walk revisit one, which is the only way a cycle forms.
    chain: Arc<Vec<std::path::PathBuf>>,
}

/// What one job produced: the candidates it listed, its share of the statistics, and the
/// subdirectories another thread should read.
struct JobOut {
    cands: Vec<Found>,
    stats: Stats,
    children: Vec<WalkJob>,
}

fn walk_rules(o: &SearchOptions) -> Arc<IgnoreRules> {
    let mut rules = IgnoreRules::new(!o.no_ignore);
    rules.set_sources(!o.no_ignore_vcs, !o.no_ignore_dot, !o.no_ignore_parent);
    rules.set_global(!o.no_ignore_global);
    // The global file and the explicit ones are one layer stack shared by every directory,
    // so they are read once here rather than per walk step.
    rules.push_global();
    for path in &o.ignore_files {
        rules
            .push_file(std::path::Path::new(path), o.ignore_file_icase)
            .expect("--ignore-file was validated at parse time");
    }
    rules
        .add_globs_with_case(&o.globs, &o.iglobs)
        .and_then(|()| rules.add_filters(&o.type_globs))
        .expect("glob dot-rule validation");
    Arc::new(rules)
}

/// The rule stack one root starts from: the shared layers, plus the ignore files of its
/// parent directories unless those are switched off.
fn root_rules(o: &SearchOptions, root: &Path) -> Arc<IgnoreRules> {
    if o.no_ignore || o.no_ignore_parent {
        return walk_rules(o);
    }
    let mut rules = (*walk_rules(o)).clone();
    rules.push_ancestors(root);
    Arc::new(rules)
}

/// Whether a walk may report one entry: the hidden and ignore rules, then the `-g` globs.
/// Both walks answer it here, so `find <glob>` and `find <path> <expression>` agree about
/// which paths exist for them.
fn allow(rules: &IgnoreRules, o: &SearchOptions, rel: &str, name: &str, is_dir: bool) -> bool {
    if !o.hidden && name.starts_with('.') {
        // A positive glob re-includes a dotfile it names, as rg does; directories still
        // need `--hidden` so the walk never wanders into `.git`.
        let named = !is_dir && rules.glob_named(rel);
        if !named {
            return false;
        }
    }
    // A positive glob switches the ignore files off (`-g '*.log'` finds a file a
    // `.gitignore` hides), exactly as ripgrep does; exclusions always apply.
    let globs_win = rules.whitelisted() || o.no_ignore;
    if !globs_win && rules.is_ignored(rel, is_dir) {
        return false;
    }
    rules.glob_allows(rel, is_dir)
}

/// Read one directory: apply the hidden, ignore, glob, depth and filesystem filters, and
/// return the entries that survive together with the subdirectories to visit next.
fn read_job(
    job: &WalkJob,
    o: &SearchOptions,
    want_dirs: bool,
    paths: AbsPath,
    ids: &Ids,
) -> JobOut {
    let mut out = JobOut {
        cands: Vec::new(),
        stats: Stats::default(),
        children: Vec::new(),
    };
    let mut rules = (*job.rules).clone();
    if !o.no_ignore {
        rules.push_dir(&job.dir, &job.rel);
    }
    // One shared copy of this directory's rules, handed to every child as a pointer: cloning
    // the rule stack once per child instead costs a copy of every glob and ignore pattern.
    let rules = Arc::new(rules);
    let mut ents = Vec::new();
    if let Err(e) = enumerate::list(&job.dir, &mut ents, &mut out.stats.errors) {
        if e.kind() != io::ErrorKind::NotFound {
            out.stats.errors.record(&e);
        }
        return out;
    }
    out.stats.dirs_walked += 1;
    let rel = job.rel.as_str();
    for e in &ents {
        // A name is usually valid UTF-8, and `to_str` says so with one validating scan;
        // `to_string_lossy` would build its lossy iterator over every name either way.
        let name = match e.name.to_str() {
            Some(s) => Cow::Borrowed(s),
            None => e.name.to_string_lossy(),
        };
        let child_rel = join_rel(rel, &name);
        let is_dir = e.is_dir() && !e.is_reparse();
        // A reparse point or symlink is followed only when asked: `--follow` reads the
        // target and walks it if it is a directory, otherwise the entry is left alone.
        let mut target: Option<std::fs::Metadata> = None;
        let mut is_dir = is_dir;
        if e.is_reparse() {
            if !o.follow {
                out.stats.entries_ignored += 1;
                continue;
            }
            match std::fs::metadata(job.dir.join(&e.name)) {
                Ok(md) => {
                    is_dir = md.is_dir();
                    target = Some(md);
                }
                Err(err) => {
                    // A link to nothing is reported the way an unreadable path is.
                    out.stats.errors.record(&err);
                    continue;
                }
            }
        }
        // `--max-depth N` counts the path levels below the root that may be reported.
        if o.max_depth.is_some_and(|m| job.depth + 1 > m) {
            out.stats.entries_ignored += 1;
            continue;
        }
        if !allow(&rules, o, &child_rel, &name, is_dir) {
            out.stats.entries_ignored += 1;
            continue;
        }
        if is_dir {
            let descend = o.max_depth.is_none_or(|m| job.depth + 1 < m);
            let mut child =
                (want_dirs || descend || o.one_filesystem).then(|| child_path(&job.dir, &e.name));
            if o.one_filesystem
                && child
                    .as_deref()
                    .is_some_and(|c| enumerate::dev_of(c).is_ok_and(|dev| dev != job.dev))
            {
                continue;
            }
            if want_dirs {
                // A directory that is also descended into needs two owners; one that is only
                // listed takes the single copy the walk already has.
                let shown = if descend { child.clone() } else { child.take() }
                    .unwrap_or_else(|| child_path(&job.dir, &e.name));
                out.cands.push(Found {
                    cand: Candidate {
                        path: shown,
                        rel: child_rel.clone(),
                        size: 0,
                        modified: None,
                        is_dir: true,
                        is_link: false,
                    },
                    kind: Kind::Dir,
                });
            }
            if descend {
                let child = child.unwrap_or_else(|| child_path(&job.dir, &e.name));
                let mut chain = job.chain.clone();
                if o.follow {
                    // A link can point back up the tree: walk each real directory once.
                    let Some(real) = std::fs::canonicalize(&child).ok() else {
                        out.stats.entries_ignored += 1;
                        continue;
                    };
                    if chain.contains(&real) {
                        out.stats.entries_ignored += 1;
                        continue;
                    }
                    Arc::make_mut(&mut chain).push(real);
                }
                out.children.push(WalkJob {
                    id: ids.next(),
                    dir: child,
                    rel: child_rel,
                    depth: job.depth + 1,
                    rules: rules.clone(),
                    dev: job.dev,
                    chain,
                });
            }
        } else {
            // The type the entry had before the open: the record's own, or the target's when
            // `--follow` already resolved the link.
            let kind = match &target {
                Some(md) if md.is_file() => Kind::File,
                Some(_) => Kind::Other,
                None => e.kind(),
            };
            // The inode is read only when a mode prints or filters on it, or when `--follow`
            // already had to read the target to know what it is.
            let (mut size, mut modified) = (0u64, None);
            if let Some(md) = &target {
                if o.max_file_size.is_some_and(|limit| md.len() > limit) {
                    continue;
                }
                size = md.len();
                modified = md.modified().ok();
            } else if o.details || o.max_file_size.is_some() {
                match e.stat() {
                    Ok(st) => {
                        if o.max_file_size.is_some_and(|limit| st.size > limit) {
                            continue;
                        }
                        size = st.size;
                        modified = Some(st.modified());
                    }
                    Err(err) => {
                        out.stats.errors.record(&err);
                        continue;
                    }
                }
            }
            out.cands.push(Found {
                cand: Candidate {
                    path: match paths {
                        AbsPath::Fill => child_path(&job.dir, &e.name),
                        AbsPath::Skip => PathBuf::new(),
                    },
                    rel: child_rel,
                    size,
                    modified,
                    is_dir: false,
                    is_link: false,
                },
                kind,
            });
        }
    }
    out
}

/// The device every root is held to under `--one-filesystem`, 0 when it is off.
fn root_device(o: &SearchOptions) -> u64 {
    if o.one_filesystem {
        o.roots
            .first()
            .and_then(|r| enumerate::dev_of(r).ok())
            .unwrap_or(0)
    } else {
        0
    }
}

/// The jobs a walk starts from: the roots that are directories, plus the files named directly
/// on the command line, which are taken as given.
fn root_jobs(o: &SearchOptions, root_dev: u64, ids: &Ids) -> (Vec<Found>, Vec<WalkJob>, Stats) {
    let mut all: Vec<Found> = Vec::new();
    let mut stats = Stats::default();
    let mut jobs: Vec<WalkJob> = Vec::new();
    for root in &o.roots {
        // Explicit files on the command line are taken as given, filters aside.
        match std::fs::metadata(root) {
            Ok(md) if md.is_file() => {
                let rel = root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                all.push(Found {
                    cand: Candidate {
                        path: root.clone(),
                        rel,
                        size: md.len(),
                        modified: md.modified().ok(),
                        is_dir: false,
                        is_link: false,
                    },
                    kind: Kind::File,
                });
                continue;
            }
            Ok(_) => {
                let ancestors = root_rules(o, root);
                let dev = if o.one_filesystem {
                    enumerate::dev_of(root).unwrap_or(root_dev)
                } else {
                    root_dev
                };
                let chain = if o.follow {
                    Arc::new(vec![
                        std::fs::canonicalize(root).unwrap_or_else(|_| root.clone())
                    ])
                } else {
                    Arc::new(Vec::new())
                };
                jobs.push(WalkJob {
                    id: ids.next(),
                    dir: root.clone(),
                    rel: String::new(),
                    depth: 0,
                    rules: ancestors,
                    dev,
                    chain,
                });
            }
            Err(e) => {
                stats.errors.record(&e);
            }
        }
    }
    (all, jobs, stats)
}

/// List every candidate under the roots, in a deterministic order. Directories are read by
/// every worker thread at once, so the walk costs a fraction of its sequential time on a
/// tree with many directories.
fn walk(o: &SearchOptions, root_dev: u64) -> (Vec<Candidate>, Stats) {
    // A listing prints paths, not contents, so the type the walk recorded is dropped here.
    let ids = Ids::default();
    let (all, jobs, mut stats) = root_jobs(o, root_dev, &ids);
    let mut all: Vec<Candidate> = all.into_iter().map(|f| f.cand).collect();
    if !jobs.is_empty() {
        let want_dirs = o.scope == Scope::Paths;
        let threads = o.threads.max(1);
        let shared = Mutex::new((Vec::<Candidate>::new(), Stats::default()));
        par_for_each(jobs, threads, |job, spawn| {
            let out = read_job(job, o, want_dirs, AbsPath::Fill, &ids);
            for child in out.children {
                spawn(child);
            }
            let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
            guard.0.extend(out.cands.into_iter().map(|f| f.cand));
            guard.1.dirs_walked += out.stats.dirs_walked;
            guard.1.entries_ignored += out.stats.entries_ignored;
            guard.1.errors.add(&out.stats.errors);
        });
        let (cands, walked) = shared.into_inner().unwrap_or_else(|e| e.into_inner());
        all.extend(cands);
        stats.dirs_walked += walked.dirs_walked;
        stats.entries_ignored += walked.entries_ignored;
        stats.errors.add(&walked.errors);
    }
    if o.scope == Scope::Content {
        all.retain(|c| !c.is_dir);
    }
    sort_paths(&mut all, o);
    (all, stats)
}

/// The order `--sort path` puts two candidates in, and the one the streaming walk emits in,
/// so one definition decides both.
fn cmp_paths(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    a.rel.cmp(&b.rel).then_with(|| a.path.cmp(&b.path))
}

/// Order the walk's results, and reverse them when `--sortr` asked.
fn sort_paths(v: &mut Vec<Candidate>, o: &SearchOptions) {
    match o.sort {
        SortKey::None => {}
        SortKey::Path => v.sort_by(cmp_paths),
        SortKey::Modified | SortKey::Accessed | SortKey::Created => {
            // The walk only stats entries when it must, so an explicit time sort reads the
            // time of each result once instead of once per comparison.
            let key = o.sort;
            let mut keyed: Vec<(SystemTime, Candidate)> = std::mem::take(v)
                .into_iter()
                .map(|c| {
                    let t = match key {
                        SortKey::Modified => c.modified.unwrap_or_else(|| time_of(&c.path, key)),
                        _ => time_of(&c.path, key),
                    };
                    (t, c)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.rel.cmp(&b.1.rel)));
            v.extend(keyed.into_iter().map(|(_, c)| c));
        }
    }
    if o.sort_reverse {
        v.reverse();
    }
}

/// How many directories the streaming walk may have queued or read ahead of the merge.
const AHEAD: usize = 64;

/// How many results the read-ahead may be holding unread. A directory listing is what the
/// walk buffers, and one directory can be huge, so the bound on memory has to count results
/// rather than directories.
const AHEAD_RESULTS: usize = 16 * 1024;

/// Identities for the walk's jobs. Every directory a read-ahead holds a listing for is asked
/// for by one of these, so the maps that track it hash a counter rather than a path.
#[derive(Default)]
struct Ids(std::sync::atomic::AtomicU64);

impl Ids {
    fn next(&self) -> u64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

/// The read-ahead's map keyed by [`Ids`]: one multiply per lookup, where hashing the path
/// itself means splitting it into components and reading every byte of each.
#[derive(Default)]
struct IdHash(u64);

impl std::hash::Hasher for IdHash {
    fn write(&mut self, bytes: &[u8]) {
        let mut n = [0u8; 8];
        let len = bytes.len().min(8);
        n[..len].copy_from_slice(&bytes[..len]);
        self.0 = (self.0 ^ u64::from_le_bytes(n)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 ^= self.0 >> 31;
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0 ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 ^= self.0 >> 31;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type IdMap<V> = HashMap<u64, V, std::hash::BuildHasherDefault<IdHash>>;

/// The read-ahead half of the streaming walk: workers read directories the merge has not
/// reached yet, and the merge takes their listings by directory.
///
/// The walking itself stays sequential — the heap decides which entry prints next — so the
/// reading is what goes parallel. A directory is asked for once (`queued` holds it until it
/// lands), and the merge waits on the condition variable only for a listing it needs now.
struct Ahead {
    state: Mutex<AheadState>,
    cv: std::sync::Condvar,
    /// Job identities, handed out to every directory a read produces.
    ids: Ids,
    /// Whether the listings it reads fill each candidate's absolute path.
    paths: AbsPath,
}

struct AheadState {
    queue: VecDeque<WalkJob>,
    /// Requested and not yet delivered, so a second request never reads a directory twice.
    queued: IdMap<()>,
    done: IdMap<JobOut>,
    /// Results read and not yet taken, the read-ahead's actual cost in memory.
    buffered: usize,
    closed: bool,
    /// Workers asleep on [`Ahead::cv`]. A wake is a futex syscall, so a push notifies only
    /// when somebody can be waiting.
    parked: std::sync::atomic::AtomicUsize,
}

impl Ahead {
    fn new(paths: AbsPath) -> Ahead {
        Ahead {
            paths,
            state: Mutex::new(AheadState {
                queue: VecDeque::new(),
                queued: IdMap::default(),
                done: IdMap::default(),
                buffered: 0,
                closed: false,
                parked: std::sync::atomic::AtomicUsize::new(0),
            }),
            cv: std::sync::Condvar::new(),
            ids: Ids::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AheadState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read these directories when a worker is free, without ever waiting for one. Past the
    /// read-ahead bound the rest are simply left to be asked for when the merge reaches them.
    fn want<'a>(&self, jobs: impl Iterator<Item = &'a WalkJob>) {
        let mut g = self.lock();
        let mut pushed = 0usize;
        for job in jobs {
            // `queued` holds everything asked for and not yet delivered, the queue included,
            // so it is the whole read-ahead's size.
            if g.queued.len() >= AHEAD || g.buffered >= AHEAD_RESULTS {
                break;
            }
            // A job already buffered or in flight is not asked for a second time. An
            // accepted job is the only one copied into the queue.
            if g.done.contains_key(&job.id) || g.queued.insert(job.id, ()).is_some() {
                continue;
            }
            g.queue.push_back(job.clone());
            pushed += 1;
        }
        let parked = g.parked.load(std::sync::atomic::Ordering::Relaxed);
        drop(g);
        for _ in 0..pushed.min(parked) {
            self.cv.notify_one();
        }
    }

    /// One directory's listing: the read-ahead's when it has it or is reading it, and this
    /// thread's own otherwise. Reading it here rather than asking and waiting keeps the
    /// order the merge consumes in from paying a wakeup per directory.
    fn take(&self, job: WalkJob, o: &SearchOptions) -> JobOut {
        let mut g = self.lock();
        if let Some(out) = g.done.remove(&job.id) {
            g.buffered -= out.cands.len();
            return out;
        }
        if !g.queued.contains_key(&job.id) {
            drop(g);
            return read_job(&job, o, true, self.paths, &self.ids);
        }
        loop {
            if let Some(out) = g.done.remove(&job.id) {
                g.buffered -= out.cands.len();
                return out;
            }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn close(&self) {
        let mut g = self.lock();
        g.closed = true;
        drop(g);
        self.cv.notify_all();
    }
}

/// One worker of the read-ahead: read a queued directory, hand its listing back.
fn ahead_worker(ahead: &Ahead, o: &SearchOptions) {
    loop {
        let job = {
            let mut g = ahead.lock();
            loop {
                if g.closed {
                    return;
                }
                if let Some(job) = g.queue.pop_front() {
                    break job;
                }
                g.parked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                g = ahead.cv.wait(g).unwrap_or_else(|e| e.into_inner());
                g.parked.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        };
        let out = read_job(&job, o, true, ahead.paths, &ahead.ids);
        // The subdirectories of a directory nobody has reached yet are the read-ahead's
        // next work: that is what keeps the pool fed in the order the merge will want. They
        // are queued before the listing is handed over, so the merge's own copy stays put.
        ahead.want(out.children.iter());
        {
            let mut g = ahead.lock();
            g.queued.remove(&job.id);
            g.buffered += out.cands.len();
            g.done.insert(job.id, out);
        }
        ahead.cv.notify_all();
    }
}

/// One directory the streaming walk has open: its allowed entries in print order, and the
/// subdirectories to open when their own entry is reached.
struct Cursor {
    items: VecDeque<Found>,
    kids: VecDeque<WalkJob>,
    /// Breaks a tie between two cursors whose heads are identical, which only two roots
    /// holding the same relative path can produce.
    seq: u64,
}

impl Cursor {
    fn head(&self) -> Option<&Found> {
        self.items.front()
    }

    /// Whether this cursor's head leads everything still in the heap. The heap orders cursors
    /// by head, so a cursor that leads cannot be overtaken while it prints: the walk takes the
    /// run its head starts without a pop and a push per entry. The order is the heap's own,
    /// which breaks a tie between two identical heads by sequence.
    fn leads(&self, heap: &BinaryHeap<Reverse<Cursor>>) -> bool {
        match heap.peek() {
            None => true,
            Some(Reverse(other)) => match (self.head(), other.head()) {
                (Some(a), Some(b)) => {
                    cmp_paths(&a.cand, &b.cand).then_with(|| self.seq.cmp(&other.seq))
                        == Ordering::Less
                }
                (Some(_), None) => true,
                _ => false,
            },
        }
    }

    /// The job for a directory entry about to be visited, when the walk may descend into it.
    /// Every job here has an entry in `items`, and both are in the same order, so the front
    /// job is this entry's or a later one's.
    fn take_kid(&mut self, rel: &str) -> Option<WalkJob> {
        match self.kids.front() {
            Some(front) if front.rel == rel => self.kids.pop_front(),
            _ => None,
        }
    }
}

impl Ord for Cursor {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.head(), other.head()) {
            (Some(a), Some(b)) => {
                cmp_paths(&a.cand, &b.cand).then_with(|| self.seq.cmp(&other.seq))
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => self.seq.cmp(&other.seq),
        }
    }
}

impl PartialOrd for Cursor {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Cursor {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Cursor {}

/// List the paths a walk finds, in the order `--sort path` prints them, without holding the
/// listing: one directory is open per level, and a heap orders the open directories' next
/// entries among each other, so memory follows the frontier rather than the result count.
///
/// `visit` sees each candidate once and returns whether the walk should go on. This is the
/// order a stream can reach — the caller uses it only for `--sort path` without `--sortr`,
/// since a reversed or time-keyed listing needs the whole set.
pub fn walk_paths<F>(o: &SearchOptions, paths: AbsPath, visit: &mut F) -> Stats
where
    F: FnMut(&Candidate) -> bool,
{
    let ahead = Ahead::new(paths);
    let (direct, jobs, mut stats) = root_jobs(o, root_device(o), &ahead.ids);
    let mut heap: BinaryHeap<Reverse<Cursor>> = BinaryHeap::new();
    let mut seq = 0u64;
    for cand in direct {
        let mut items = VecDeque::with_capacity(1);
        items.push_back(cand);
        heap.push(Reverse(Cursor {
            items,
            kids: VecDeque::new(),
            seq,
        }));
        seq += 1;
    }
    std::thread::scope(|scope| {
        for _ in 0..o.threads.max(1) {
            let (ahead, o) = (&ahead, &*o);
            scope.spawn(move || ahead_worker(ahead, o));
        }
        for job in jobs {
            let cursor = open_cursor(ahead.take(job, o), &mut stats, seq);
            seq += 1;
            ahead.want(cursor.kids.iter());
            if cursor.head().is_some() {
                heap.push(Reverse(cursor));
            }
        }
        'walk: while let Some(Reverse(mut cur)) = heap.pop() {
            while cur.leads(&heap) {
                let Some(found) = cur.items.pop_front() else {
                    break;
                };
                let kid = if found.cand.is_dir {
                    cur.take_kid(&found.cand.rel)
                } else {
                    None
                };
                if !visit(&found.cand) {
                    break 'walk;
                }
                if let Some(job) = kid {
                    let child = open_cursor(ahead.take(job, o), &mut stats, seq);
                    seq += 1;
                    ahead.want(child.kids.iter());
                    if child.head().is_some() {
                        heap.push(Reverse(child));
                    }
                    // A subdirectory holds entries that sort inside its parent's, so the
                    // parent goes back in the heap for the merge to order.
                    break;
                }
            }
            if cur.head().is_some() {
                heap.push(Reverse(cur));
            }
        }
        ahead.close();
    });
    stats
}

/// Order one directory's listing into a cursor, adding its share of the statistics to
/// `stats`.
fn open_cursor(out: JobOut, stats: &mut Stats, seq: u64) -> Cursor {
    stats.dirs_walked += out.stats.dirs_walked;
    stats.entries_ignored += out.stats.entries_ignored;
    stats.errors.add(&out.stats.errors);
    let mut items: VecDeque<Found> = out.cands.into();
    items
        .make_contiguous()
        .sort_by(|a, b| cmp_paths(&a.cand, &b.cand));
    let mut kids: VecDeque<WalkJob> = out.children.into();
    kids.make_contiguous().sort_by(|a, b| a.rel.cmp(&b.rel));
    Cursor { items, kids, seq }
}

/// What an expression walk produced, for the front end's exit status.
#[derive(Debug, Default)]
pub struct ExprOutcome {
    /// Entries whose expression evaluated true.
    pub matched: u64,
    pub stats: Stats,
}

/// One entry's facts, in the shape [`findexpr::Entry`] wants them: the platform's [`Stat`]
/// (one `statx`/directory record, whichever the platform has) plus the path pieces the
/// expression and the sink print.
struct Facts<'a> {
    path: &'a Path,
    st: Stat,
    /// A symlink's target, and what the target is.
    target: Option<String>,
    target_kind: Option<Kind>,
}

impl<'a> Facts<'a> {
    /// The facts of a path on its own: the walk's starting point, read with one `lstat`.
    fn of(path: &'a Path) -> io::Result<Facts<'a>> {
        Ok(Facts {
            path,
            st: enumerate::lstat(path)?,
            target: None,
            target_kind: None,
        }
        .link(false))
    }

    /// The facts of one entry of a directory being read, whose `stat` resolves the name
    /// against the open directory instead of the full path. The `stat` runs only when the
    /// expression reads a field the directory record does not carry; the kind is always
    /// exact, because a `DT_UNKNOWN` record was already stat-ed when it was listed.
    fn of_entry(path: &'a Path, e: &enumerate::Lite, needs: Needs) -> io::Result<Facts<'a>> {
        let st = if needs.stat {
            e.stat()?
        } else {
            Stat {
                kind: e.kind(),
                ..Stat::default()
            }
        };
        Ok(Facts {
            path,
            st,
            target: None,
            target_kind: None,
        }
        .link(needs.link))
    }

    /// A symlink's target text (`%l`) and target kind (`-xtype`), which only a link has and
    /// only an expression that asks for one pays for: two syscalls per link otherwise.
    fn link(mut self, want: bool) -> Facts<'a> {
        if want && self.st.kind == Kind::Symlink {
            self.target = std::fs::read_link(self.path)
                .ok()
                .map(|t| t.to_string_lossy().into_owned());
            self.target_kind = enumerate::stat(self.path).ok().map(|s| s.kind);
        }
        self
    }

    /// The entry described by its target instead of the link, which is what `-follow` turns
    /// a symlink into; the link's own name and target stay for `%l` and `-xtype`.
    fn followed(mut self, target: &Path) -> Facts<'a> {
        let Ok(st) = enumerate::stat(target) else {
            return self;
        };
        let kind = match st.kind {
            Kind::Other => self.st.kind,
            k => k,
        };
        self.target_kind = self.target_kind.or(Some(kind));
        self.st = Stat { kind, ..st };
        self
    }
}

/// A walk driven by a parsed `find` expression.
///
/// Unlike [`walk`] this one is sequential by nature: `-prune` decides the next step from
/// what the expression said about the entry just seen, and `-depth` reverses the order the
/// entries come out in, so there is no list to sort and nothing to hand to the workers.
struct ExprWalk<'a> {
    o: &'a SearchOptions,
    expr: &'a Expr,
    ctx: &'a Ctx<'a>,
    sink: &'a mut dyn Sink,
    /// Compute `-empty` per entry: one `read_dir` per directory, so only when asked.
    want_empty: bool,
    /// What the expression reads, so an entry it only names and types is never stat-ed.
    needs: Needs,
    out: ExprOutcome,
    /// Filesystem type names by device, for `-fstype` (`ext4`, `ntfs`, ...).
    fs_names: HashMap<u64, String>,
    stop: bool,
}

/// Walk every root under a parsed expression, evaluating it entry by entry as find does:
/// pre-order, or after the contents under `-depth`, and never below a `-prune`.
///
/// An expression that cannot be evaluated here is the only thing that comes back as `Err`;
/// per-entry I/O failures go to [`ExprOutcome::stats`] and the walk continues.
pub fn walk_expr(
    o: &SearchOptions,
    expr: &Expr,
    ctx: &Ctx,
    want_empty: bool,
    sink: &mut dyn Sink,
) -> Result<ExprOutcome, String> {
    let needs = expr.needs();
    let mut w = ExprWalk {
        o,
        expr,
        ctx,
        sink,
        want_empty,
        needs,
        out: ExprOutcome::default(),
        fs_names: if needs.fs {
            enumerate::filesystem_types()
        } else {
            HashMap::new()
        },
        stop: false,
    };
    for root in &o.roots {
        let rules = root_rules(o, root);
        w.root(root, &rules)?;
        if w.stop {
            break;
        }
    }
    Ok(w.out)
}

impl ExprWalk<'_> {
    fn follow(&self) -> bool {
        self.o.follow || self.expr.settings().follow
    }

    fn one_filesystem(&self) -> bool {
        self.o.one_filesystem || self.expr.settings().one_filesystem
    }

    /// The starting point itself: find evaluates it too, at depth 0.
    fn root(&mut self, root: &Path, rules: &Arc<IgnoreRules>) -> Result<(), String> {
        let mut facts = match Facts::of(root) {
            Ok(f) => f,
            Err(e) => {
                self.out.stats.errors.record(&e);
                return Ok(());
            }
        };
        if self.follow() && facts.st.kind == Kind::Symlink {
            facts = facts.followed(root);
        }
        let settings = self.expr.settings();
        let mut descend = facts.st.kind == Kind::Dir;
        // `-depth` reports a directory after its contents, the starting point included.
        let visit_now = !settings.depth_first;
        if visit_now && settings.mindepth.is_none_or(|m| m == 0) {
            descend &= self.visit(&facts, "", 0)?;
        }
        if descend && settings.maxdepth != Some(0) {
            let mut chain = vec![real_path(root)];
            self.dir(root, "", 0, rules, facts.st.dev, &mut chain)?;
        }
        if !visit_now && settings.mindepth.is_none_or(|m| m == 0) {
            self.visit(&facts, "", 0)?;
        }
        Ok(())
    }

    /// Read one directory and evaluate what it holds, `depth` being this directory's own.
    fn dir(
        &mut self,
        dir: &Path,
        rel: &str,
        depth: usize,
        rules: &Arc<IgnoreRules>,
        root_dev: u64,
        chain: &mut Vec<PathBuf>,
    ) -> Result<(), String> {
        if self.stop {
            return Ok(());
        }
        let mut own = (**rules).clone();
        if !self.o.no_ignore {
            own.push_dir(dir, rel);
        }
        let rules = Arc::new(own);
        let mut ents = Vec::new();
        if let Err(e) = enumerate::list(dir, &mut ents, &mut self.out.stats.errors) {
            if e.kind() != io::ErrorKind::NotFound {
                self.out.stats.errors.record(&e);
            }
            return Ok(());
        }
        self.out.stats.dirs_walked += 1;
        // find prints a directory in the order the kernel returns it; sorting keeps two runs
        // of the same walk comparable, which is what the rest of this tool promises.
        ents.sort_by(|a, b| a.name.cmp(&b.name));
        let depth = depth + 1;
        let settings = self.expr.settings();
        let mindepth = settings.mindepth.unwrap_or(0) as usize;
        let depth_first = settings.depth_first;
        for e in &ents {
            if self.stop {
                break;
            }
            if settings.maxdepth.is_some_and(|m| depth as u64 > m) {
                continue;
            }
            let name = e.name.to_string_lossy().into_owned();
            let child_rel = join_rel(rel, &name);
            let child = dir.join(&e.name);
            // A symlink is an entry of its own, as it is for find; it is only ever walked
            // with `-follow`, and then as the directory it points at.
            let mut is_dir = e.is_dir() && !e.is_reparse();
            if e.is_reparse() && self.follow() {
                match enumerate::stat(&child) {
                    Ok(st) => is_dir = st.kind == Kind::Dir,
                    Err(err) => {
                        self.out.stats.errors.record(&err);
                        continue;
                    }
                }
            }
            if !allow(&rules, self.o, &child_rel, &name, is_dir) {
                self.out.stats.entries_ignored += 1;
                continue;
            }
            let mut facts = match Facts::of_entry(&child, e, self.needs) {
                Ok(f) => f,
                Err(err) => {
                    self.out.stats.errors.record(&err);
                    continue;
                }
            };
            if self.follow() && facts.st.kind == Kind::Symlink {
                facts = facts.followed(&child);
            }
            // `-maxdepth` bounds what is walked as well as what is reported: the entry at
            // the limit is still evaluated, but its directory is never read.
            let mut descend =
                facts.st.kind == Kind::Dir && settings.maxdepth.is_none_or(|m| (depth as u64) < m);
            if descend && self.one_filesystem() && facts.st.dev != root_dev {
                self.out.stats.entries_ignored += 1;
                descend = false;
            }
            // `-depth` reports a directory after its contents, so the descent comes first.
            if descend && depth_first {
                self.descend(&child, &child_rel, depth, &rules, root_dev, chain)?;
            }
            if depth >= mindepth {
                descend &= self.visit(&facts, &child_rel, depth)?;
            }
            if descend && !depth_first {
                self.descend(&child, &child_rel, depth, &rules, root_dev, chain)?;
            }
        }
        Ok(())
    }

    fn descend(
        &mut self,
        child: &Path,
        child_rel: &str,
        depth: usize,
        rules: &Arc<IgnoreRules>,
        root_dev: u64,
        chain: &mut Vec<PathBuf>,
    ) -> Result<(), String> {
        match self.enter(child, chain) {
            Some(real) => {
                chain.push(real);
                self.dir(child, child_rel, depth, rules, root_dev, chain)?;
                chain.pop();
            }
            None => self.out.stats.entries_ignored += 1,
        }
        Ok(())
    }

    /// The canonical path to push while descending, or `None` when `--follow` has already
    /// walked it: a link pointing back up the tree must not be entered twice.
    fn enter(&self, dir: &Path, chain: &[PathBuf]) -> Option<PathBuf> {
        if !self.follow() {
            return Some(PathBuf::new());
        }
        let real = real_path(dir);
        (!chain.contains(&real)).then_some(real)
    }

    /// Evaluate one entry and hand its actions to the sink. `Ok(false)` means `-prune` fired
    /// on a directory, which the caller turns into "do not descend".
    fn visit(&mut self, facts: &Facts, sub: &str, depth: usize) -> Result<bool, String> {
        let path = facts.path.to_string_lossy().into_owned();
        // Sliced out of the text the walk already has rather than asked of the `Path` again:
        // `file_name` walks the components and re-converts them, which measured 3.9% of an
        // expression walk, and its answer is the tail of this same string.
        let name = last_component(&path).unwrap_or(&path).to_string();
        let below = sub_path(sub);
        let st = &facts.st;
        let empty = self.want_empty
            && match st.kind {
                Kind::File => st.size == 0,
                Kind::Dir => std::fs::read_dir(facts.path).is_ok_and(|mut d| d.next().is_none()),
                _ => false,
            };
        let entry = findexpr::Entry {
            size: st.size,
            blocks: st.blocks,
            mtime: st.mtime,
            atime: st.atime,
            ctime: st.ctime,
            links: st.links,
            inode: st.inode,
            dev: st.dev,
            mode: st.mode,
            uid: st.uid,
            gid: st.gid,
            depth,
            empty,
            fs: self.fs_names.get(&st.dev).map(String::as_str),
            target: facts.target.as_deref(),
            target_kind: facts.target_kind,
            ..findexpr::Entry::new(&path, &name, &below, st.kind)
        };
        let verdict = self.expr.eval(&entry, self.ctx, &mut *self.sink)?;
        if verdict.selected {
            self.out.matched += 1;
        }
        if verdict.quit {
            self.stop = true;
        }
        Ok(!verdict.prune)
    }
}

/// The path below the starting point with the platform's separators, which is what `%P`
/// prints: the walk keeps `/` internally because the ignore rules are matched that way.
/// The last component of a path's text, answering what `Path::file_name` answers: `None` for
/// a path that ends in `..`, in `.`, or is the root, the final component otherwise. Sliced out
/// of text the caller already owns, so it costs no component walk and no second conversion.
fn last_component(path: &str) -> Option<&str> {
    let mut text = path.strip_suffix('/').unwrap_or(path);
    loop {
        let name = text.rsplit('/').next().unwrap_or(text);
        if name == "." {
            text = match text.strip_suffix('.') {
                Some(rest) => rest.strip_suffix('/').unwrap_or(rest),
                None => text,
            };
            continue;
        }
        return match name {
            ".." | "" => None,
            _ => Some(name),
        };
    }
}

fn sub_path(rel: &str) -> String {
    if std::path::MAIN_SEPARATOR == '/' {
        rel.to_string()
    } else {
        rel.replace('/', std::path::MAIN_SEPARATOR_STR)
    }
}

/// The real path of a directory, for the `--follow` cycle check: the path itself when it
/// cannot be resolved, so the check fails closed rather than skipping the entry.
fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Order the matched files, and reverse them when `--sortr` asked. `--sort modified` pays one
/// stat per matched file, which is why it is not the default.
fn sort_hits(v: &mut Vec<FileHits>, o: &SearchOptions) {
    match o.sort {
        SortKey::None => {}
        SortKey::Path => v.sort_by(|a, b| a.path.cmp(&b.path)),
        SortKey::Modified | SortKey::Accessed | SortKey::Created => {
            let key = o.sort;
            let mut keyed: Vec<(SystemTime, FileHits)> = std::mem::take(v)
                .into_iter()
                .map(|f| {
                    let t = time_of(Path::new(&f.path), key);
                    (t, f)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
            v.extend(keyed.into_iter().map(|(_, f)| f));
        }
    }
    if o.sort_reverse {
        v.reverse();
    }
}

/// One of the three times a result can be sorted by, `UNIX_EPOCH` when the filesystem does
/// not keep it (an access time is often disabled, and Windows may have no birth time).
fn time_of(path: &Path, key: SortKey) -> SystemTime {
    let Ok(md) = std::fs::metadata(path) else {
        return std::time::UNIX_EPOCH;
    };
    let t = match key {
        SortKey::Accessed => md.accessed(),
        SortKey::Created => md.created(),
        _ => md.modified(),
    };
    t.unwrap_or(std::time::UNIX_EPOCH)
}

/// True for the modes whose answer for a binary file is just "it matches" or "it does not".
fn binary_by_name_only(o: &SearchOptions) -> bool {
    !o.text
        && !o.invert
        && !o.count
        && !o.count_matches
        && !o.only_matching
        && o.before == 0
        && o.after == 0
        && !o.pattern.is_word()
}

/// The report for a binary file scanned block by block.
fn binary_hit(path: &Path, o: &SearchOptions, stats: &mut Stats, found: bool) -> Option<FileHits> {
    if found {
        stats.matches += 1;
    } else if !(o.files_without_matches || (o.include_zero && o.count)) {
        return None;
    }
    Some(FileHits {
        path: path.to_string_lossy().into_owned(),
        hits: Vec::new(),
        matches: u64::from(found),
        total: u64::from(found),
        binary: true,
        pool: Vec::new(),
    })
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
    let root_dev = root_device(o);
    let mut report = SearchReport {
        paths: Vec::new(),
        files: Vec::new(),
        stats: Stats::default(),
        any_match: false,
    };
    if o.scope == Scope::Paths {
        let (cands, stats) = walk(o, root_dev);
        report.paths = cands;
        report.stats = stats;
        report.any_match = !report.paths.is_empty();
        report.stats.elapsed = t0.elapsed();
        return Ok(report);
    }
    // Content scope: one pass. A worker that lists a directory scans the files it found before
    // it takes the next directory, so the tree is never held in memory: the queue carries
    // directories, and a directory's files live only while it is being read.
    let ids = Ids::default();
    let (direct, jobs, mut stats) = root_jobs(o, root_dev, &ids);
    let shared: Mutex<(Vec<FileHits>, Stats)> = Mutex::new((Vec::new(), Stats::default()));
    // `-q` answers the first match it sees and asks the rest of the walk to stop early.
    let found_one = std::sync::atomic::AtomicBool::new(false);
    let scan_one =
        |path: &Path, kind: Kind, shared: &Mutex<(Vec<FileHits>, Stats)>, early: bool| {
            let mut local = Stats::default();
            let got = match scan_file(path, kind, o, &mut local) {
                Ok(got) => got,
                Err(e) => {
                    if e.kind() != io::ErrorKind::NotFound {
                        local.errors.record(&e);
                    }
                    None
                }
            };
            let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(hit) = got {
                if early && hit.matches > 0 {
                    found_one.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                guard.0.push(hit);
            }
            merge_stats(&mut guard.1, &local);
        };
    for f in &direct {
        scan_one(&f.cand.path, f.kind, &shared, o.quiet);
    }
    if !jobs.is_empty() {
        let early = o.quiet;
        par_for_each(jobs, o.threads, |job, spawn| {
            if found_one.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let out = read_job(job, o, false, AbsPath::Fill, &ids);
            for child in out.children {
                spawn(child);
            }
            let mut local = out.stats;
            for f in &out.cands {
                if found_one.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let mut one = Stats::default();
                let got = match scan_file(&f.cand.path, f.kind, o, &mut one) {
                    Ok(got) => got,
                    Err(e) => {
                        if e.kind() != io::ErrorKind::NotFound {
                            one.errors.record(&e);
                        }
                        None
                    }
                };
                merge_stats(&mut local, &one);
                let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(hit) = got {
                    if early && hit.matches > 0 {
                        found_one.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    guard.0.push(hit);
                }
                merge_stats(&mut guard.1, &local);
                local = Stats::default();
            }
            let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
            merge_stats(&mut guard.1, &local);
        });
    }
    let (mut found, agg) = shared.into_inner().unwrap_or_else(|e| e.into_inner());
    // Results come back in whatever order the workers finished; the report is sorted so two
    // runs of the same search print the same thing.
    sort_hits(&mut found, o);
    merge_stats(&mut stats, &agg);
    for got in found {
        if o.files_without_matches {
            if got.matches == 0 {
                report.files.push(got);
            }
            continue;
        }
        if got.matches == 0 {
            // `--passthru` keeps a file the walk found nothing in but printed anyway, and
            // `-c --include-zero` prints the zero; nothing else does.
            if (o.passthru && !got.hits.is_empty()) || (o.include_zero && o.count) {
                report.files.push(got);
            }
            continue;
        }
        stats.files_matched += 1;
        report.any_match = true;
        // `-c` and `-l` print a number or a path, so the lines are not kept.
        if o.count || o.count_matches || o.files_with_matches {
            report.files.push(FileHits {
                hits: Vec::new(),
                ..got
            });
        } else {
            report.files.push(got);
        }
    }
    if o.quiet && found_one.load(std::sync::atomic::Ordering::Relaxed) {
        report.any_match = true;
        // `-q` prints nothing: the matches it found are not part of the answer.
        report.files.clear();
    }
    if o.files_without_matches {
        report.any_match = !report.files.is_empty();
    }
    report.stats = stats;
    report.stats.elapsed = t0.elapsed();
    Ok(report)
}

/// How many files are handed to a worker at once. The batch is the unit the queue, the printer
/// and the walk's own turn at the scan all work in, so what it costs per file is a fraction of
/// what the same step costs one file at a time — which on a tree of fifteen thousand files is
/// the difference between the scan overlapping the walk and the two taking turns.
const BATCH: usize = 128;

/// How many times a waiter looks again before it parks. A batch is scanned in tens of
/// microseconds, so a wait this short usually ends without a context switch.
const SPIN: usize = 256;

/// The most result bytes a stream will hold between the scan and the printer. This is the bound
/// that matters: a search of a large tree keeps a buffer rather than the whole answer.
const BUFFER_MAX: usize = 4 * 1024 * 1024;

/// How many files may be waiting for a worker at once: the walk runs a little ahead of the
/// scan, and a deep queue is what keeps the workers fed — a queue that empties wakes a worker
/// for every batch it hands over, and on a machine with no idle core a wake costs more than the
/// batch.
fn scan_window(o: &SearchOptions) -> usize {
    (o.threads.max(1) * 1024).max(2048)
}

/// One open directory in a streamed content search, ordered by the whole path of its next
/// entry. A result list is one flat order over the whole paths, unlike a path listing's tree
/// order: a directory takes its place in that order by its own path, and everything under it
/// sorts where that path does.
///
/// The node carries the key of the entry it offers, built once when the entry is reached: the
/// merge runs a comparison per level of the heap for every result, and reading a path back to
/// text inside one costs several times what the comparison itself does.
struct Flat {
    cursor: Cursor,
    /// The path of the cursor's next entry, as it prints.
    key: String,
}

impl Flat {
    fn new(cursor: Cursor) -> Flat {
        let key = cursor
            .head()
            .map(|f| f.cand.path.to_string_lossy().into_owned())
            .unwrap_or_default();
        Flat { cursor, key }
    }

    /// The entry this cursor offers next, leaving it on the key of the one after.
    fn next(&mut self) -> Option<Found> {
        let found = self.cursor.items.pop_front();
        if let Some(head) = self.cursor.head() {
            self.key.clear();
            self.key.push_str(&head.cand.path.to_string_lossy());
        }
        found
    }
}

impl Ord for Flat {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| self.cursor.seq.cmp(&other.cursor.seq))
    }
}

impl PartialOrd for Flat {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Flat {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Flat {}

/// The scan half of a streamed search: the walk hands files to the workers in print order and
/// the printer takes their results back in the same order, so a file that is slow to scan holds
/// the results behind it rather than the whole answer.
struct Scans {
    state: Mutex<ScanState>,
    /// Workers asleep waiting for work.
    jobs_cv: std::sync::Condvar,
    /// The printer asleep waiting for the batch it prints next.
    done_cv: std::sync::Condvar,
    /// The walk asleep waiting for the printer to make room.
    room_cv: std::sync::Condvar,
}

/// Why a batch was not accepted by the queue.
enum Push {
    Full(Vec<Found>),
    Stopped,
}

/// What one worker found in the files of one batch, in the order the writer handed them over.
type Scanned = Vec<(Option<FileHits>, Stats)>;

struct ScanState {
    jobs: VecDeque<(u64, Vec<Found>)>,
    /// Results the printer has not taken, by the batch number the walk gave them.
    done: std::collections::BTreeMap<u64, Scanned>,
    /// Batches handed out, and the next one the printer will take.
    handed: u64,
    next: u64,
    /// Files waiting for a worker, so the walk cannot run away with the tree.
    queued: usize,
    /// Result bytes the printer has not taken yet: the stream's actual cost in memory.
    buffered: usize,
    closed: bool,
    /// The printer gave up: a write failed, so nobody should scan or walk any further.
    aborted: bool,
    job_waiters: usize,
    done_waiters: usize,
    room_waiters: usize,
}

impl Scans {
    fn new() -> Scans {
        Scans {
            state: Mutex::new(ScanState {
                jobs: VecDeque::new(),
                done: std::collections::BTreeMap::new(),
                handed: 0,
                next: 0,
                queued: 0,
                buffered: 0,
                closed: false,
                aborted: false,
                job_waiters: 0,
                done_waiters: 0,
                room_waiters: 0,
            }),
            jobs_cv: std::sync::Condvar::new(),
            done_cv: std::sync::Condvar::new(),
            room_cv: std::sync::Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ScanState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hand one batch over, in print order, or give it back when the queue already holds
    /// `window` files: how far the walk may run ahead of the scan is what bounds the paths in
    /// memory, and the walk has better things to do with the wait than sleep (see [`claim`]).
    ///
    /// `Stopped` means the print failed — a broken pipe — so the walk should stop too.
    ///
    /// [`claim`]: Scans::claim
    fn push(&self, files: Vec<Found>, window: usize) -> Result<(), Push> {
        let mut g = self.lock();
        if g.aborted || g.closed {
            return Err(Push::Stopped);
        }
        if g.queued >= window {
            return Err(Push::Full(files));
        }
        let seq = g.handed;
        g.handed += 1;
        g.queued += files.len();
        g.jobs.push_back((seq, files));
        let wake = g.job_waiters;
        drop(g);
        if wake > 0 {
            self.jobs_cv.notify_one();
        }
        Ok(())
    }

    /// Sleep until a worker takes a batch, which is what makes room again.
    fn wait_for_room(&self) {
        let mut g = self.lock();
        if g.aborted || g.closed || g.queued == 0 {
            return;
        }
        g.room_waiters += 1;
        let mut g = self.room_cv.wait(g).unwrap_or_else(|e| e.into_inner());
        g.room_waiters -= 1;
    }

    /// Take a batch over when one is waiting: the walk thread spends the wait for room on the
    /// work it is waiting for, so the walk costs the scan no parallelism on a machine with no
    /// core to spare.
    fn claim(&self) -> Option<(u64, Vec<Found>)> {
        let mut g = self.lock();
        if g.buffered >= BUFFER_MAX {
            return None;
        }
        let (seq, files) = g.jobs.pop_front()?;
        g.queued -= files.len();
        Some((seq, files))
    }

    /// The next batch to scan, or `None` when the search is over. A worker waits here both for
    /// work and for the printer to make room: the results waiting to be printed are what bounds
    /// the stream's memory, so a worker stops taking batches while they hold the buffer.
    fn next_job(&self) -> Option<(u64, Vec<Found>)> {
        let mut g = self.lock();
        let mut spins = 0;
        loop {
            if g.aborted {
                return None;
            }
            // Past the end of the walk the queue is drained whatever the buffer holds: a batch
            // left unscanned is one the printer would wait for forever.
            if g.jobs.front().is_some() && (g.buffered < BUFFER_MAX || g.closed) {
                let (seq, files) = g.jobs.pop_front().expect("checked above");
                g.queued -= files.len();
                let wake = g.room_waiters;
                drop(g);
                if wake > 0 {
                    self.room_cv.notify_one();
                }
                return Some((seq, files));
            }
            if g.closed {
                return None;
            }
            if spins < SPIN {
                spins += 1;
                drop(g);
                std::hint::spin_loop();
                g = self.lock();
                continue;
            }
            g.job_waiters += 1;
            g = self.jobs_cv.wait(g).unwrap_or_else(|e| e.into_inner());
            g.job_waiters -= 1;
        }
    }

    /// The next batch of results, waiting for the worker that has it. `None` means the walk is
    /// finished and everything it handed over has been printed.
    fn take(&self) -> Option<Scanned> {
        let mut g = self.lock();
        let mut spins = 0;
        loop {
            let next = g.next;
            if let Some(out) = g.done.remove(&next) {
                g.next += 1;
                g.buffered -= batch_bytes(&out);
                let wake = g.job_waiters;
                drop(g);
                if wake > 0 {
                    self.jobs_cv.notify_all();
                }
                return Some(out);
            }
            if g.closed && next >= g.handed {
                return None;
            }
            if g.aborted {
                return None;
            }
            if spins < SPIN {
                spins += 1;
                drop(g);
                std::hint::spin_loop();
                g = self.lock();
                continue;
            }
            g.done_waiters += 1;
            g = self.done_cv.wait(g).unwrap_or_else(|e| e.into_inner());
            g.done_waiters -= 1;
        }
    }

    /// Hand a scanned batch back under its number.
    fn finish(&self, seq: u64, done: Scanned) {
        let wake = {
            let mut g = self.lock();
            g.buffered += batch_bytes(&done);
            g.done.insert(seq, done);
            g.done_waiters
        };
        if wake > 0 {
            self.done_cv.notify_all();
        }
    }

    /// No more batches are coming: the workers drain what is queued, and the printer stops once
    /// it has printed everything handed over.
    fn close(&self) {
        let mut g = self.lock();
        g.closed = true;
        drop(g);
        self.jobs_cv.notify_all();
        self.done_cv.notify_all();
    }

    /// Stop everything, results outstanding or not: the printer failed, so no batch it hands
    /// over will ever be read.
    fn abort(&self) {
        let mut g = self.lock();
        g.closed = true;
        g.aborted = true;
        drop(g);
        self.jobs_cv.notify_all();
        self.done_cv.notify_all();
        self.room_cv.notify_all();
    }
}

/// What one batch of results costs in memory while it waits for the printer.
fn batch_bytes(batch: &Scanned) -> usize {
    batch
        .iter()
        .map(|(got, _)| match got {
            Some(f) => f.hits.len() * std::mem::size_of::<LineHit>() + f.pool.len() + f.path.len(),
            None => 0,
        })
        .sum()
}

/// One scan worker: take the next batch in print order, scan its files in order, hand the
/// results back together.
fn scan_worker(scans: &Scans, o: &SearchOptions) {
    loop {
        let Some((seq, files)) = scans.next_job() else {
            return;
        };
        let done = scan_batch_of(&files, o);
        scans.finish(seq, done);
    }
}

/// Scan the files of one batch, in the order the walk handed them over.
fn scan_batch_of(files: &[Found], o: &SearchOptions) -> Scanned {
    let mut done: Scanned = Vec::with_capacity(files.len());
    for found in files {
        let mut stats = Stats::default();
        let got = match scan_file(&found.cand.path, found.kind, o, &mut stats) {
            Ok(got) => got,
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    stats.errors.record(&e);
                }
                None
            }
        };
        done.push((got, stats));
    }
    done
}

/// Search `o.roots`, printing each file's result as the walk reaches its turn instead of
/// holding the whole answer.
///
/// Three stages, one thread each for the two that are not the scan: the walk lists the tree in
/// result order and hands files over a batch at a time, the scan workers search them, and this
/// thread prints them in that same order. Only one of them may be busy at a time on a small
/// machine — a printer that stalls the walk, or a walk that stalls the scan, costs more than the
/// batching saves — so the walk is not the printer's thread.
///
/// Only `--sort path` without `--sortr` has an order a stream can reach, so the caller keeps
/// [`search`] for a reversed or time-keyed sort.
pub fn search_stream<F>(o: &SearchOptions, sink: &mut F) -> io::Result<SearchReport>
where
    F: FnMut(&FileHits) -> io::Result<()>,
{
    let t0 = Instant::now();
    let mut report = SearchReport {
        paths: Vec::new(),
        files: Vec::new(),
        stats: Stats::default(),
        any_match: false,
    };
    let scans = Scans::new();
    let window = scan_window(o);
    let mut walk_stats = Stats::default();
    let mut write_err: Option<io::Error> = None;
    std::thread::scope(|scope| {
        // One worker fewer than the machine has threads: the walk is the other one, and it
        // scans whatever the queue holds while it waits for room.
        for _ in 0..o.threads.saturating_sub(1).max(1) {
            let (scans, o) = (&scans, &*o);
            scope.spawn(move || scan_worker(scans, o));
        }
        let walker = {
            let (scans, o) = (&scans, &*o);
            scope.spawn(move || walk_stream(scans, o, window))
        };
        while let Some(batch) = scans.take() {
            if let Err(e) = print_batch(batch, o, &mut report, sink) {
                write_err = Some(e);
                break;
            }
        }
        // A broken pipe stops the walk where it is; nothing it hands over can be printed.
        scans.abort();
        walk_stats = walker.join().unwrap_or_default();
    });
    merge_stats(&mut report.stats, &walk_stats);
    report.stats.elapsed = t0.elapsed();
    match write_err {
        Some(e) => Err(e),
        None => Ok(report),
    }
}

/// The walk half of a streamed search: list the tree in the order a result list prints, and
/// hand its files to the scan in batches, waiting whenever the printer has fallen a window
/// behind.
fn walk_stream(scans: &Scans, o: &SearchOptions, window: usize) -> Stats {
    let root_dev = root_device(o);
    let ids = Ids::default();
    let (direct, jobs, mut stats) = root_jobs(o, root_dev, &ids);
    let mut heap: BinaryHeap<Reverse<Flat>> = BinaryHeap::new();
    let mut pending: Vec<Found> = Vec::with_capacity(BATCH);
    let mut seq = 0u64;
    // A file named on the command line is a result of its own, as in a path listing.
    for cand in direct {
        let mut items = VecDeque::with_capacity(1);
        items.push_back(cand);
        heap.push(Reverse(Flat::new(Cursor {
            items,
            kids: VecDeque::new(),
            seq,
        })));
        seq += 1;
    }
    // Directories are read here, in the order their entries print: a content search is bound
    // by the files, so reading listings ahead of the scan only bought wakeups.
    for job in jobs {
        let cursor = open_cursor(
            read_job(&job, o, true, AbsPath::Fill, &ids),
            &mut stats,
            seq,
        );
        seq += 1;
        if cursor.head().is_some() {
            heap.push(Reverse(Flat::new(cursor)));
        }
    }
    while let Some(Reverse(mut top)) = heap.pop() {
        let Some(found) = top.next() else {
            continue;
        };
        let kid = if found.cand.is_dir {
            top.cursor.take_kid(&found.cand.rel)
        } else {
            None
        };
        if top.cursor.head().is_some() {
            heap.push(Reverse(top));
        }
        if found.cand.is_dir {
            // Nothing prints for a directory: opening it is what puts the entries below it in
            // their place in the order.
            let Some(job) = kid else { continue };
            let child = open_cursor(
                read_job(&job, o, true, AbsPath::Fill, &ids),
                &mut stats,
                seq,
            );
            seq += 1;
            if child.head().is_some() {
                heap.push(Reverse(Flat::new(child)));
            }
            continue;
        }
        pending.push(found);
        if pending.len() >= BATCH {
            let mut files = std::mem::replace(&mut pending, Vec::with_capacity(BATCH));
            loop {
                match scans.push(files, window) {
                    Ok(()) => break,
                    Err(Push::Stopped) => return stats,
                    // The scan has more queued than it needs: take one over rather than sleep,
                    // and sleep only when there is nothing queued to take.
                    Err(Push::Full(back)) => {
                        files = back;
                        match scans.claim() {
                            Some((seq, ready)) => scans.finish(seq, scan_batch_of(&ready, o)),
                            None => scans.wait_for_room(),
                        }
                    }
                }
            }
        }
    }
    if !pending.is_empty() {
        while let Err(Push::Full(back)) = scans.push(pending, window) {
            pending = back;
            match scans.claim() {
                Some((seq, files)) => scans.finish(seq, scan_batch_of(&files, o)),
                None => scans.wait_for_room(),
            }
        }
    }
    scans.close();
    stats
}

/// Print one scanned batch in the order the walk handed its files over.
fn print_batch<F>(
    got: Scanned,
    o: &SearchOptions,
    report: &mut SearchReport,
    sink: &mut F,
) -> io::Result<()>
where
    F: FnMut(&FileHits) -> io::Result<()>,
{
    for (hit, stats) in got {
        merge_stats(&mut report.stats, &stats);
        emit_file(hit, o, report, sink)?;
    }
    Ok(())
}

/// Print one finished file, under the rules [`search`] applies to its collected results:
/// `-L` prints the files that did not match, `--passthru` and `-c --include-zero` keep a file
/// the walk found nothing in, and the modes that print no lines drop them first.
fn emit_file<F>(
    got: Option<FileHits>,
    o: &SearchOptions,
    report: &mut SearchReport,
    sink: &mut F,
) -> io::Result<()>
where
    F: FnMut(&FileHits) -> io::Result<()>,
{
    let Some(got) = got else { return Ok(()) };
    if o.files_without_matches {
        if got.matches == 0 {
            sink(&got)?;
            report.any_match = true;
        }
        return Ok(());
    }
    if got.matches == 0 {
        if (o.passthru && !got.hits.is_empty()) || (o.include_zero && o.count) {
            sink(&got)?;
        }
        return Ok(());
    }
    report.stats.files_matched += 1;
    report.any_match = true;
    if o.count || o.count_matches || o.files_with_matches {
        sink(&FileHits {
            hits: Vec::new(),
            ..got
        })
    } else {
        sink(&got)
    }
}

thread_local! {
    /// The scan buffer, allocated once per worker thread and handed back after every file.
    static SCAN_BUF: std::cell::RefCell<Vec<u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// The line-level buffers a scan of one file needs. Allocating them per file is what made a
    /// 162k-file search sit at 250 MB resident: not the live data, but the freed blocks each
    /// arena keeps, which a trim threshold of 128 KiB brings back to 170 MB.
    static SCAN_LINES: std::cell::RefCell<Lines> =
        std::cell::RefCell::new(Lines::new());
}

/// The reusable half of a [`Scanner`]: everything that is only scratch space per file.
struct Lines {
    carry: Vec<u8>,
    line: Vec<u8>,
    history: VecDeque<(u64, Vec<u8>)>,
    hits: Vec<LineHit>,
    pool: Vec<u8>,
}

impl Lines {
    fn new() -> Lines {
        Lines {
            carry: Vec::new(),
            line: Vec::new(),
            history: VecDeque::new(),
            hits: Vec::new(),
            pool: Vec::new(),
        }
    }

    fn take() -> Lines {
        SCAN_LINES.with(|l| std::mem::replace(&mut *l.borrow_mut(), Lines::new()))
    }

    fn put_back(&mut self) {
        self.carry.clear();
        self.line.clear();
        self.history.clear();
        self.hits.clear();
        self.pool.clear();
    }
}

fn scan_buffer() -> Vec<u8> {
    SCAN_BUF.with(|b| {
        let mut slot = b.borrow_mut();
        if slot.len() < BIG_BLOCK {
            slot.resize(BIG_BLOCK, 0);
        }
        std::mem::take(&mut *slot)
    })
}

/// Read the next chunk, allowing the big block once the file has proved bigger than one.
///
/// A small first read keeps the cost of the 162k files that are one block long down to one
/// 64 KiB read; a file that keeps going is then read a megabyte at a time, which is what the
/// few hundred large files on a system want and what a 64 KiB loop pays a syscall per block for.
fn read_chunk(f: &mut File, buf: &mut [u8], big: bool) -> io::Result<usize> {
    let want = if big { buf.len() } else { BLOCK.min(buf.len()) };
    f.read(&mut buf[..want])
}

fn recycle_buffer(buf: Vec<u8>) {
    SCAN_BUF.with(|b| *b.borrow_mut() = buf);
}

/// Scan one file. `None` means "nothing to report" (no match, or filtered out).
fn scan_file(
    path: &Path,
    kind: Kind,
    o: &SearchOptions,
    stats: &mut Stats,
) -> io::Result<Option<FileHits>> {
    // One scan buffer per thread: a fresh `vec![0u8; BLOCK]` per file zeroes 64 KiB that the
    // read is about to overwrite, which on a 162k-file tree is ten gigabytes of memset.
    let mut buf = scan_buffer();
    let res = scan_file_with(path, kind, o, stats, &mut buf);
    recycle_buffer(buf);
    res
}

/// Open a file for the scan, or skip it when it is not a regular file.
///
/// A FIFO or a device node blocks `open` until a writer appears, which would hang the whole
/// search on a tree that happens to hold one; ripgrep skips such entries inside a walk, so
/// this does too. `O_NONBLOCK` is what makes the open return, so the entry is recognised and
/// passed over instead of waited on. `find` never reads a file's contents and `du` answers
/// from the directory record, so their handling of special files is unaffected.
///
/// `kind` is what the walk's directory record said about the entry. When it already says
/// regular file the `fstat` below could only repeat it, and it is a syscall per file: the
/// walk types by `getdents`, so the only entries that reach it unknown are those a
/// filesystem declined to type, which is exactly when asking again is worth the call.
fn open_scan(path: &Path, kind: Kind) -> io::Result<Option<File>> {
    let mut opts = File::options();
    opts.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(crate::unix::sys::O_NONBLOCK);
    }
    let f = opts.open(path)?;
    if kind == Kind::File {
        return Ok(Some(f));
    }
    // The open did not block, so this answers about the entry itself.
    if f.metadata()?.is_file() {
        Ok(Some(f))
    } else {
        Ok(None)
    }
}

fn scan_file_with(
    path: &Path,
    kind: Kind,
    o: &SearchOptions,
    stats: &mut Stats,
    buf: &mut Vec<u8>,
) -> io::Result<Option<FileHits>> {
    // `-m 0` asks for no match at all: never open the file.
    if o.max_count == Some(0) {
        return Ok(None);
    }
    if o.multiline {
        return scan_multiline(path, kind, o, stats);
    }
    if o.null_data {
        return scan_records(path, kind, o, stats);
    }
    // `--pre` hands the file to a command and searches what it prints, which is how a PDF or
    // an office document becomes text without this tool knowing anything about the format.
    if let Some(cmd) = &o.pre {
        if o.pre_globs.is_empty() || pre_matches(path, &o.pre_globs) {
            return scan_prepared(path, cmd, o, stats);
        }
    }
    let Some(mut f) = open_scan(path, kind)? else {
        return Ok(None);
    };
    stats.files_scanned += 1;
    let n = f.read(&mut *buf)?;
    if n == 0 {
        return finished(path, o, stats, Scanner::new(o), false);
    }
    // A compressed file has no lines to stream: it is expanded whole first, and what is
    // searched is the expansion. The first block is already read, so the magic costs a
    // comparison against bytes a scan would have read anyway.
    if o.search_zip && may_be_compressed(path, &buf[..n]) {
        let mut packed = buf[..n].to_vec();
        f.read_to_end(&mut packed)?;
        stats.bytes_scanned += packed.len() as u64;
        if let Some(data) = expand_stream(path, &packed)? {
            return scan_expanded(path, &data, o, stats);
        }
    }
    // A file that needs transcoding is read whole: UTF-16 code units straddle the block
    // boundary, so there is nothing to stream. Everything else keeps the streaming path.
    let binary = bytes::is_binary(&buf[..n], BINARY_SCAN.min(n));
    // `-I`/`--binary-files=without-match`: a NUL-carrying file is not even a match.
    if binary && !o.text && o.skip_binary {
        stats.files_binary += 1;
        return Ok(None);
    }
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
    // A binary file is reported by name, never by line, so there is no line structure to keep:
    // scanning its blocks for one match avoids the carry buffer a 550 MB executable otherwise
    // grows to 90 MB of resident memory. The modes that print counts or invert the match keep
    // the line path, where those answers come from.
    if binary && binary_by_name_only(o) {
        stats.files_binary += 1;
        let mut found = o.pattern.find_at(&buf[..n], 0).is_some();
        stats.bytes_scanned += n as u64;
        let mut more = n == buf.len();
        while !found && more {
            let n = read_chunk(&mut f, buf, true)?;
            stats.bytes_scanned += n as u64;
            found = o.pattern.find_at(&buf[..n.min(BIG_BLOCK)], 0).is_some();
            more = n == buf.len();
        }
        return Ok(binary_hit(path, o, stats, found));
    }
    let mut sc = Scanner::pooled(o);
    if binary {
        stats.files_binary += 1;
    }
    stats.bytes_scanned += n as u64;
    sc.feed(&buf[..n]);
    // A block that came back short is the end of a regular file: the next read would only
    // learn the same thing from a syscall that returns zero, so the one that returns zero is
    // not made. `open_scan` has established the file is regular, which is what lets a short
    // read mean "no more" rather than "not yet".
    let mut more = n == buf.len();
    while more && !sc.done() {
        let n = read_chunk(&mut f, buf, true)?;
        stats.bytes_scanned += n as u64;
        sc.feed(&buf[..n]);
        more = n == buf.len();
    }
    finished(path, o, stats, sc, binary)
}

/// Search bytes that did not come from the file as they lie: a gzip file's expansion.
fn scan_expanded(
    path: &Path,
    data: &[u8],
    o: &SearchOptions,
    stats: &mut Stats,
) -> io::Result<Option<FileHits>> {
    stats.bytes_scanned += data.len() as u64;
    let binary = bytes::is_binary(data, BINARY_SCAN.min(data.len()));
    if binary && !o.text && o.skip_binary {
        stats.files_binary += 1;
        return Ok(None);
    }
    if binary && binary_by_name_only(o) {
        stats.files_binary += 1;
        let found = o.pattern.find_at(data, 0).is_some();
        return Ok(binary_hit(path, o, stats, found));
    }
    let mut sc = Scanner::pooled(o);
    if binary {
        stats.files_binary += 1;
    }
    sc.feed(data);
    finished(path, o, stats, sc, binary)
}

/// True when the first block could be a compressed stream this tool expands. Brotli carries
/// no signature, so it is recognised by the `.br` name and everything else by its magic.
fn may_be_compressed(path: &Path, head: &[u8]) -> bool {
    crate::archive::is_gzip(head)
        || crate::archive_bz2::is_bzip2(head)
        || crate::archive_lz4::is_lz4(head)
        || crate::archive_xz::is_xz(head)
        || crate::archive_zstd::is_zstd(head)
        || has_br_extension(path)
}

fn has_br_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("br"))
}

/// The expansion of a compressed file, or `None` when these bytes are not one of the
/// formats `-z` understands. A stream that starts right but is corrupt is an error that
/// names the path, never a silently short answer.
fn expand_stream(path: &Path, data: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let out = if crate::archive::is_gzip(data) {
        crate::archive::gunzip(data)
    } else if crate::archive_bz2::is_bzip2(data) {
        crate::archive_bz2::decompress(data)
    } else if crate::archive_lz4::is_lz4(data) {
        crate::archive_lz4::decompress(data)
    } else if crate::archive_xz::is_xz(data) {
        crate::archive_xz::decompress(data)
    } else if crate::archive_zstd::is_zstd(data) {
        crate::archive_zstd::decompress(data)
    } else if has_br_extension(path) {
        crate::archive_brotli::decompress(data)
    } else {
        return Ok(None);
    };
    out.map(Some).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {e}", path.display()),
        )
    })
}

/// Search a whole file at once, so a match may span lines (`-U`). The bytes have to be in
/// memory for that, which is the price of the mode; every other mode streams.
fn scan_multiline(
    path: &Path,
    kind: Kind,
    o: &SearchOptions,
    stats: &mut Stats,
) -> io::Result<Option<FileHits>> {
    let Some(mut f) = open_scan(path, kind)? else {
        return Ok(None);
    };
    stats.files_scanned += 1;
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;
    stats.bytes_scanned += data.len() as u64;
    if o.search_zip {
        if let Some(expanded) = expand_stream(path, &data)? {
            data = expanded;
            stats.bytes_scanned += data.len() as u64;
        }
    }
    // The whole file at once: `find_all_in` decides once, rather than once per match,
    // whether a Unicode pattern has to read these bytes as characters.
    let found = o.pattern.find_all_in(&data, usize::MAX);
    let name = || path.to_string_lossy().into_owned();
    if found.is_empty() {
        let wants = o.files_without_matches || (o.include_zero && o.count) || o.passthru;
        return Ok(wants.then(|| FileHits {
            path: name(),
            hits: if o.passthru {
                plain_lines(&data)
            } else {
                Vec::new()
            },
            matches: 0,
            total: 0,
            binary: false,
            pool: Vec::new(),
        }));
    }
    stats.matches += found.len() as u64;
    let mut hits = if o.passthru {
        every_line(&data, &found)
    } else if o.only_matching {
        // Each record carries the line its match starts on, counted forward from the previous
        // record's line rather than from the start of the file.
        let mut number = 1u64;
        let mut pos = 0usize;
        let mut out = Vec::with_capacity(found.len());
        for m in &found {
            let ls = line_start(&data, m.start);
            if ls >= pos {
                number += bytes::count_byte(&data[pos..ls], b'\n') as u64;
                pos = ls;
            }
            out.push(LineHit {
                number,
                offset: m.start as u64,
                text: data[m.start..m.end].to_vec(),
                pooled: (0, 0),
                matched: true,
                spans: vec![(0, m.end - m.start)],
                text_start: m.start - ls,
            });
        }
        out
    } else if o.count || o.files_with_matches || o.files_without_matches || o.quiet {
        Vec::new()
    } else {
        matched_lines(&data, &found)
    };
    // `-c` counts the lines the matches cover, which is what a per-line mode would count.
    let lines = if hits.is_empty() {
        matched_line_count(&data, &found)
    } else {
        hits.iter().filter(|h| h.matched).count() as u64
    };
    present(&mut hits, o, &[]);
    let out = FileHits {
        path: name(),
        hits,
        matches: lines,
        total: found.len() as u64,
        binary: false,
        pool: Vec::new(),
    };
    Ok(Some(out))
}

/// True when `--pre-glob` names this file. The walk already narrowed the set; this is only
/// for the mode that runs a command per path.
fn pre_matches(path: &Path, globs: &[String]) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    globs.iter().any(|g| {
        crate::glob::Glob::new(g)
            .map(|g| name.as_deref().is_some_and(|n| g.matches(n, false)))
            .unwrap_or(false)
    })
}

/// Run `--pre` on `path` and search its output. The command gets the path as its only
/// argument, exactly as ripgrep runs one, so a pipeline is the caller's business.
fn scan_prepared(
    path: &Path,
    cmd: &str,
    o: &SearchOptions,
    stats: &mut Stats,
) -> io::Result<Option<FileHits>> {
    let out = std::process::Command::new(cmd)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()?;
    // A preprocessor that fails is reported the way an unreadable path is, unless it still
    // printed something to search.
    if !out.status.success() && out.stdout.is_empty() {
        return Err(io::Error::other(format!(
            "{cmd} {} exited with {}",
            path.display(),
            out.status
        )));
    }
    stats.files_scanned += 1;
    scan_expanded(path, &out.stdout, o, stats)
}

/// Search a file whose records end in NUL instead of a newline (`--null-data`). Records are
/// cut in memory rather than streamed: a file of records is small, and the line scanner is
/// built around a newline.
fn scan_records(
    path: &Path,
    kind: Kind,
    o: &SearchOptions,
    stats: &mut Stats,
) -> io::Result<Option<FileHits>> {
    let Some(mut f) = open_scan(path, kind)? else {
        return Ok(None);
    };
    stats.files_scanned += 1;
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;
    stats.bytes_scanned += data.len() as u64;
    let mut hits: Vec<LineHit> = Vec::new();
    let mut matches = 0u64;
    let mut total = 0u64;
    let mut offset = 0u64;
    for (i, record) in data.split(|b| *b == b'\0').enumerate() {
        let found = o.pattern.find_at(record, 0);
        let number = (i + 1) as u64;
        if found.is_some() != o.invert {
            matches += 1;
            total += o.pattern.find_all(record, MAX_PER_LINE).len() as u64;
            let spans: Vec<(usize, usize)> = if o.invert {
                Vec::new()
            } else {
                o.pattern
                    .find_all(record, MAX_PER_LINE)
                    .into_iter()
                    .map(|m| (m.start, m.end))
                    .collect()
            };
            if o.only_matching && !spans.is_empty() {
                for (s, e) in spans {
                    hits.push(LineHit {
                        number,
                        offset: offset + s as u64,
                        text: record[s..e].to_vec(),
                        pooled: (0, 0),
                        matched: true,
                        spans: vec![(0, e - s)],
                        text_start: s,
                    });
                }
            } else {
                hits.push(LineHit {
                    number,
                    offset,
                    text: record.to_vec(),
                    pooled: (0, 0),
                    matched: true,
                    spans,
                    text_start: 0,
                });
            }
        }
        offset += record.len() as u64 + 1;
    }
    if matches == 0 {
        let wants = o.files_without_matches || (o.include_zero && o.count);
        return Ok(wants.then(|| FileHits {
            path: path.to_string_lossy().into_owned(),
            hits: Vec::new(),
            matches: 0,
            total: 0,
            binary: false,
            pool: Vec::new(),
        }));
    }
    stats.matches += matches;
    let keep =
        o.count || o.count_matches || o.files_with_matches || o.files_without_matches || o.quiet;
    if !keep {
        present(&mut hits, o, &[]);
    }
    Ok(Some(FileHits {
        path: path.to_string_lossy().into_owned(),
        hits: if keep { Vec::new() } else { hits },
        matches,
        total,
        binary: false,
        pool: Vec::new(),
    }))
}

/// Every line of `data`, with the byte ranges each starts and ends at.
fn line_spans(data: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    // A file that ends with a newline has no line after it, unlike the split a reader would
    // see: the empty tail is not a line and must not print.
    while start < data.len() {
        let end = match data[start..].iter().position(|b| *b == b'\n') {
            Some(p) => start + p,
            None => data.len(),
        };
        out.push((start, end));
        start = end + 1;
    }
    out
}

/// The offset at which the line holding `at` starts.
fn line_start(data: &[u8], at: usize) -> usize {
    match data[..at.min(data.len())].iter().rposition(|b| *b == b'\n') {
        Some(p) => p + 1,
        None => 0,
    }
}

/// The offset of the newline that ends the line holding `at`, or the end of the data.
fn line_end(data: &[u8], at: usize) -> usize {
    bytes::find_byte(data, b'\n', at).unwrap_or(data.len())
}

/// The last byte a match covers, as a position within the data: a match ending past the final
/// newline covers the line that newline ends, not the empty tail after it, which is not a line.
fn last_byte(data: &[u8], m: &Match) -> usize {
    m.end
        .saturating_sub(1)
        .max(m.start)
        .min(data.len().saturating_sub(1))
}

/// The lines the matches cover, once each, with each line's own share of every match on it.
///
/// One forward walk: the lines a match covers are the lines it starts and ends on, the line
/// numbers are the newlines counted between one reported line and the next, and the matches
/// that can overlap a line are the run of them from the first that has not ended before it.
fn matched_lines(data: &[u8], found: &[Match]) -> Vec<LineHit> {
    let mut out: Vec<LineHit> = Vec::new();
    // An empty file has no lines, so no match in it can be reported on one.
    if data.is_empty() {
        return out;
    }
    // The first byte of the first line no match has reported, and its number.
    let mut pos = 0usize;
    let mut number = 1u64;
    // The first match that has not ended before the line being reported.
    let mut first = 0usize;
    for (i, m) in found.iter().enumerate() {
        let last = line_end(data, last_byte(data, m));
        if last < pos {
            continue;
        }
        let mut ls = line_start(data, m.start).max(pos);
        number += bytes::count_byte(&data[pos..ls], b'\n') as u64;
        while ls <= last {
            let le = line_end(data, ls);
            while first < i && found[first].end <= ls {
                first += 1;
            }
            let mut spans = Vec::new();
            for m in &found[first..] {
                if m.start > le {
                    break;
                }
                let (a, b) = (m.start.max(ls), m.end.min(le));
                if b > a {
                    spans.push((a - ls, b - ls));
                }
            }
            out.push(LineHit {
                number,
                offset: ls as u64,
                text: strip_cr(&data[ls..le]).to_vec(),
                pooled: (0, 0),
                matched: true,
                spans,
                text_start: 0,
            });
            pos = le + 1;
            number += 1;
            ls = pos;
        }
    }
    out
}

/// How many lines the matches cover once each, for `-c`, which prints the number and never the
/// lines themselves.
fn matched_line_count(data: &[u8], found: &[Match]) -> u64 {
    let mut count = 0u64;
    let mut pos = 0usize;
    if data.is_empty() {
        return 0;
    }
    for m in found {
        let last = line_end(data, last_byte(data, m));
        if last < pos {
            continue;
        }
        let mut ls = line_start(data, m.start).max(pos);
        while ls <= last {
            count += 1;
            ls = line_end(data, ls) + 1;
        }
        pos = ls;
    }
    count
}

/// Every line of the file with what matched on it, for `--passthru -U`.
fn every_line(data: &[u8], found: &[Match]) -> Vec<LineHit> {
    let mut out: Vec<LineHit> = Vec::new();
    // Every match has ended before this index once the walk is past it, as they are in order.
    let mut first = 0usize;
    for (i, (ls, le)) in line_spans(data).into_iter().enumerate() {
        while first < found.len() && found[first].end <= ls {
            first += 1;
        }
        let mut spans = Vec::new();
        for m in &found[first..] {
            if m.start > le {
                break;
            }
            let (a, b) = (m.start.max(ls), m.end.min(le));
            if b > a {
                spans.push((a - ls, b - ls));
            }
        }
        out.push(LineHit {
            number: (i + 1) as u64,
            offset: ls as u64,
            text: strip_cr(&data[ls..le]).to_vec(),
            pooled: (0, 0),
            matched: !spans.is_empty(),
            spans,
            text_start: 0,
        });
    }
    out
}

fn plain_lines(data: &[u8]) -> Vec<LineHit> {
    line_spans(data)
        .into_iter()
        .enumerate()
        .map(|(i, (s, e))| LineHit {
            number: (i + 1) as u64,
            offset: s as u64,
            text: strip_cr(&data[s..e]).to_vec(),
            pooled: (0, 0),
            matched: false,
            spans: Vec::new(),
            text_start: 0,
        })
        .collect()
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
    /// Individual matches over the whole file (a line can hold several).
    total: u64,
    /// The text of the records that share one buffer (`-o`); handed to the file's results.
    pool: Vec<u8>,
    /// Byte offset of the current chunk within the file, and of the first byte the carry is
    /// holding.
    pos: u64,
    carry_pos: u64,
}

impl<'a> Scanner<'a> {
    fn new(o: &'a SearchOptions) -> Self {
        Scanner::with_lines(o, Lines::new())
    }

    /// A scanner on this thread's reusable buffers.
    fn pooled(o: &'a SearchOptions) -> Self {
        Scanner::with_lines(o, Lines::take())
    }

    fn with_lines(o: &'a SearchOptions, lines: Lines) -> Self {
        Scanner {
            o,
            carry: lines.carry,
            history: lines.history,
            hits: lines.hits,
            line_no: 0,
            matches: 0,
            remaining_after: 0,
            last_emitted: 0,
            want_lines: !o.count && !o.files_with_matches && !o.files_without_matches && !o.quiet,
            limit: o.max_count.unwrap_or(u64::MAX),
            stopped: false,
            line: lines.line,
            total: 0,
            pool: lines.pool,
            pos: 0,
            carry_pos: 0,
        }
    }

    /// Give the buffers back to the thread, so the next file on it allocates nothing.
    fn recycle(mut self) {
        let mut lines = Lines {
            carry: std::mem::take(&mut self.carry),
            line: std::mem::take(&mut self.line),
            history: std::mem::take(&mut self.history),
            hits: std::mem::take(&mut self.hits),
            pool: std::mem::take(&mut self.pool),
        };
        lines.put_back();
        SCAN_LINES.with(|l| *l.borrow_mut() = lines);
    }

    /// True once `--max-count` is reached: the file needs no more reading.
    fn done(&self) -> bool {
        self.stopped || self.matches >= self.limit
    }

    fn feed(&mut self, chunk: &[u8]) {
        if self.stopped || self.matches >= self.limit {
            return;
        }
        self.feed_chunk(chunk);
        self.pos += chunk.len() as u64;
    }

    fn feed_chunk(&mut self, chunk: &[u8]) {
        let base = self.pos;
        if self.block_scan(chunk) {
            return;
        }
        let mut start = 0;
        while let Some(pos) = bytes::find_byte(chunk, b'\n', start) {
            self.line_no += 1;
            // A line that sits entirely inside this block is matched in place; only a line
            // continued from the previous block needs the buffer.
            let stop = if self.carry.is_empty() {
                self.emit(&chunk[start..pos], base + start as u64)
            } else {
                let mut buf = std::mem::take(&mut self.line);
                buf.clear();
                buf.extend_from_slice(&self.carry);
                buf.extend_from_slice(&chunk[start..pos]);
                self.carry.clear();
                let stop = self.emit(&buf, base + start as u64);
                self.line = buf;
                stop
            };
            start = pos + 1;
            if stop {
                self.stopped = true;
                return;
            }
            if self.matches >= self.limit {
                return;
            }
        }
        if start < chunk.len() {
            if self.carry.is_empty() {
                self.carry_pos = base + start as u64;
            }
            self.carry.extend_from_slice(&chunk[start..]);
            self.flush_overlong();
        }
    }

    /// Find literal matches block-wise and expand only the lines that hold one, instead of
    /// splitting every line. This is the path ripgrep takes, and it is what makes a search
    /// for a rare word cost one scan of the data rather than one pass per line.
    ///
    /// Only for the modes where a match means "print or count this line" or, for `-o`, "report
    /// every match": context, inversion and `-m` need the line-at-a-time machinery and decline
    /// here.
    fn block_scan(&mut self, chunk: &[u8]) -> bool {
        if self.o.invert
            || self.o.before > 0
            || self.o.after > 0
            || self.o.passthru
            // The replacement rewrites the line, which this path does not do.
            || self.o.replace.is_some()
            || self.o.max_count.is_some()
            || self.o.stop_on_nonmatch
            || self.stopped
        {
            return false;
        }
        if !self.o.pattern.block_safe() {
            return false;
        }
        // A line continued from the previous block is finished the ordinary way first.
        let mut from = 0;
        if !self.carry.is_empty() {
            let Some(nl) = bytes::find_byte(chunk, b'\n', 0) else {
                self.carry.extend_from_slice(chunk);
                self.flush_overlong();
                return true;
            };
            let mut buf = std::mem::take(&mut self.line);
            buf.clear();
            buf.extend_from_slice(&self.carry);
            buf.extend_from_slice(&chunk[..nl]);
            self.carry.clear();
            self.line_no += 1;
            let stop = self.emit(&buf, self.carry_pos);
            self.line = buf;
            if stop || self.matches >= self.limit {
                self.stopped = stop;
                return true;
            }
            from = nl + 1;
        }
        // Everything after the last newline is a partial line: hand it to the next block.
        let body = &chunk[from..];
        let end = match bytes::rfind_byte(body, b'\n', body.len()) {
            Some(i) => i,
            None => {
                self.carry.extend_from_slice(body);
                self.flush_overlong();
                return true;
            }
        };
        let body = &body[..end];
        // The block ends right before a newline, so it holds one line more than its newlines.
        // Both walks count the lines they passed over the same way and hand back the same
        // number, so the next block starts on the right one.
        let base = self.line_no;
        self.line_no = if self.o.only_matching {
            self.block_matches(body, from, base)
        } else {
            self.block_lines(body, base)
        };
        if from + end + 1 < chunk.len() && self.carry.is_empty() {
            self.carry_pos = self.pos + (from + end + 1) as u64;
        }
        self.carry.extend_from_slice(&chunk[from + end + 1..]);
        true
    }

    /// The lines of one block that hold a match, each reported once with the spans of the
    /// matches on it. Returns the line number the block ends on.
    ///
    /// A line number costs a count of the bytes between one reported line and the next, since
    /// the block scan jumps over the lines it does not report. Only a mode that reads the
    /// number pays it (`-n`, `--vimgrep`, `--column`, `--json`, colour), and the count of the
    /// block it hands the next block is the sum of those counts plus the tail after the last
    /// reported line, rather than a second pass over the whole block.
    fn block_lines(&mut self, body: &[u8], base: u64) -> u64 {
        let numbers = self.numbered();
        // Newlines in `body[..counted_to]`. `base` is the number of the line before the
        // block's first one, so the line that begins at `counted_to` is `base + counted + 1`.
        let mut counted = 0u64;
        let mut counted_to = 0usize;
        let mut line_no = base;
        let mut line_start = 0;
        let mut pos = 0;
        // A block-safe regex is found by the forward automaton alone, which is a single pass
        // over bytes no match touches; the exact span of the line it lands on is worked out
        // afterwards, on that line only. A literal needs no such split.
        let regex_block = self.o.pattern.block_finder().is_none();
        while let Some((hit, stop_at)) = if regex_block {
            self.o
                .pattern
                .block_accept_end(body, pos)
                // The match ends just past its last byte, which is inside the line.
                .map(|e| (e.saturating_sub(1).max(pos), e))
        } else {
            self.o
                .pattern
                .block_match(body, pos)
                .map(|m| (m.start, m.end))
        } {
            // A match that runs across a newline is not a line match: the per-line semantics
            // never see the newline at all, so a class like `[^a-z]` must not match one.
            if bytes::find_byte(&body[hit..stop_at.max(hit)], b'\n', 0).is_some() {
                pos = hit + 1;
                line_start = match bytes::rfind_byte(body, b'\n', hit) {
                    Some(i) => i + 1,
                    None => 0,
                };
                continue;
            }
            let ls = match bytes::rfind_byte(body, b'\n', hit) {
                Some(i) => i + 1,
                None => line_start,
            };
            let le = match bytes::find_byte(body, b'\n', hit) {
                Some(i) => i,
                None => body.len(),
            };
            if numbers {
                counted += bytes::count_byte(&body[counted_to..ls], b'\n') as u64;
                counted_to = ls;
                line_no = base + counted + 1;
            }
            self.matches += 1;
            let line = strip_cr(&body[ls..le]);
            if self.o.count_matches {
                let mut at = 0;
                while let Some(cm) = self.o.pattern.block_match(line, at) {
                    self.total += 1;
                    at = cm.end.max(cm.start + 1);
                }
            }
            if self.want_lines {
                let offset = self.pos + ls as u64;
                let mut spans: Vec<(usize, usize)> = Vec::new();
                if self.o.paint {
                    let mut at = 0;
                    while let Some(cm) = self.o.pattern.block_match(line, at) {
                        spans.push((cm.start, cm.end));
                        at = cm.end.max(cm.start + 1);
                    }
                }
                push_pooled(
                    &mut self.hits,
                    &mut self.pool,
                    line_no,
                    offset,
                    line,
                    true,
                    spans,
                    0,
                );
            }
            // One line is reported once however many matches it holds.
            pos = le + 1;
            line_start = pos;
        }
        if !numbers {
            return base;
        }
        base + counted + bytes::count_byte(&body[counted_to..], b'\n') as u64 + 1
    }

    /// Every match of one block as its own record, for `-o`. The search continues from the
    /// previous match's end instead of restarting at the start of its line, which is what a
    /// line-at-a-time `-o` pays for once a file matches often. Returns the line number the
    /// block ends on.
    ///
    /// Line numbers cost a count of the bytes between one record and the next, since the scan
    /// jumps over the lines between them. Only `-n`, `--vimgrep` and the modes that print
    /// positions (`paint`: colour, `--json`, `--column`) read them, so the rest skip the count
    /// and their records carry the line number they arrived with; the block it hands the next
    /// one is the sum of those counts plus the tail, not a second pass over the whole block.
    fn block_matches(&mut self, body: &[u8], from: usize, base: u64) -> u64 {
        let numbers = self.numbered();
        // Absolute offset of `body`'s first byte within the file.
        let at = self.pos + from as u64;
        // Newlines in `body[..counted_to]`; `base` is the number of the line before the block's
        // first, so the line that begins at `counted_to` is `base + counted + 1`.
        let mut counted = 0u64;
        let mut line_no = base;
        // The line the last record came from: its first byte, the newline that ends it (or
        // the block end), where its text ends after a `\r\n`'s `\r` is stripped, and how
        // many records it has already reported. A line is capped at [`MAX_PER_LINE`] matches,
        // as the line path's `find_all` caps it.
        let mut line_start = 0;
        let mut line_end = 0;
        let mut content_end = 0;
        let mut counted_to = 0;
        let mut on_line = 0;
        let mut fresh = true;
        let mut pos = 0;
        while let Some(m) = self.o.pattern.block_match(body, pos) {
            // A match that runs across a newline is not a line match: the per-line semantics
            // never see the newline at all, so a class like `[^a-z]` must not match one.
            if bytes::find_byte(&body[m.start..m.end], b'\n', 0).is_some() {
                pos = m.start + 1;
                continue;
            }
            if fresh || m.start > line_end {
                line_start = match bytes::rfind_byte(body, b'\n', m.start) {
                    Some(i) => i + 1,
                    None => line_start,
                };
                line_end = match bytes::find_byte(body, b'\n', m.start) {
                    Some(i) => i,
                    None => body.len(),
                };
                if numbers {
                    counted += bytes::count_byte(&body[counted_to..line_start], b'\n') as u64;
                    counted_to = line_start;
                    line_no = base + counted + 1;
                }
                content_end = line_start + strip_cr(&body[line_start..line_end]).len();
                on_line = 0;
                fresh = false;
            }
            // The line path matches what is left after the `\r` of a `\r\n` is stripped, so
            // a match that reaches into it is not one.
            if m.end > content_end {
                pos = m.start + 1;
                continue;
            }
            // A line counts once however many matches it holds.
            if on_line == 0 {
                self.matches += 1;
            }
            if self.o.count_matches {
                self.total += 1;
            }
            if self.want_lines {
                let text = &body[m.start..m.end];
                let mut hit = LineHit {
                    number: line_no,
                    offset: at + m.start as u64,
                    text: Vec::new(),
                    pooled: (0, 0),
                    matched: true,
                    spans: Vec::new(),
                    text_start: m.start - line_start,
                };
                // One buffer for the whole file, while a `u32` can still address it: a match
                // otherwise costs an allocation and, later, a copy into the report.
                if self.pool.len() + text.len() <= POOL_MAX {
                    let from = self.pool.len() as u32;
                    self.pool.extend_from_slice(text);
                    hit.pooled = (from, self.pool.len() as u32);
                } else {
                    hit.text = text.to_vec();
                }
                self.hits.push(hit);
            }
            on_line += 1;
            if on_line >= MAX_PER_LINE {
                // Nothing more is reported from this line; the next one starts after it.
                pos = line_end + 1;
                continue;
            }
            pos = m.end.max(m.start + 1);
        }
        if !numbers {
            return base;
        }
        base + counted + bytes::count_byte(&body[counted_to..], b'\n') as u64 + 1
    }

    /// True when something will read the line number of a reported line: `-n` and `--vimgrep`
    /// print it, `--json` writes it into a field, and `paint` covers the modes that build one
    /// row per match (`--column`, `--vimgrep`, colour). A plain list of lines prints no
    /// number, so the block scan does not count the lines it skipped to print them.
    fn numbered(&self) -> bool {
        self.o.line_numbers || self.o.vimgrep || self.o.paint
    }

    /// Hand the carry over as a line once it grows past [`CARRY_MAX`], so a file that is not
    /// made of newlines cannot hold the whole of itself in memory. A match that would have
    /// straddled that boundary is not found, which is the price of a bounded line.
    fn flush_overlong(&mut self) {
        if self.carry.len() < CARRY_MAX || self.done() {
            return;
        }
        self.line.clear();
        std::mem::swap(&mut self.line, &mut self.carry);
        self.carry.clear();
        self.line_no += 1;
        let buf = std::mem::take(&mut self.line);
        self.emit(&buf, self.carry_pos);
        self.line = buf;
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
        let buf = std::mem::take(&mut self.line);
        self.emit(&buf, self.carry_pos);
        self.line = buf;
    }

    /// Run the matcher over one line; true means the file is finished (`-m`, or a line
    /// `--stop-on-nonmatch` ends the file at).
    fn emit(&mut self, line: &[u8], offset: u64) -> bool {
        // `--stop-on-nonmatch` ends a file at the first line the pattern does not select once
        // it has selected one. That line still goes out while its `-A` window lasts, which is
        // ripgrep's answer too.
        let stop_here = self.o.stop_on_nonmatch
            && self.matches > 0
            && self.o.pattern.find_at(strip_cr(line), 0).is_some() == self.o.invert;
        let done = handle_line(
            line,
            self.line_no,
            offset,
            self.o,
            &mut self.history,
            &mut self.hits,
            &mut self.pool,
            &mut self.matches,
            &mut self.total,
            &mut self.remaining_after,
            &mut self.last_emitted,
            self.want_lines,
            self.limit,
        );
        done || stop_here
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
    present(&mut sc.hits, o, &sc.pool);
    if sc.matches == 0 {
        // `--passthru` prints the file anyway, so its lines are kept even with no match.
        let wants_zero = o.files_without_matches || (o.include_zero && o.count) || (o.passthru);
        let hit = wants_zero.then(|| FileHits {
            path: path.to_string_lossy().into_owned(),
            hits: if o.passthru {
                std::mem::take(&mut sc.hits)
            } else {
                Vec::new()
            },
            matches: 0,
            total: 0,
            binary,
            pool: std::mem::take(&mut sc.pool),
        });
        sc.recycle();
        return Ok(hit);
    }
    stats.matches += sc.matches;
    let hits = std::mem::take(&mut sc.hits);
    let out = FileHits {
        path: path.to_string_lossy().into_owned(),
        hits,
        matches: sc.matches,
        total: sc.total,
        binary,
        pool: std::mem::take(&mut sc.pool),
    };
    sc.recycle();
    Ok(Some(out))
}

/// The lines as they print: `--trim` drops the leading whitespace and `--max-columns` cuts
/// what is left, with the spans shifted so colouring and the columns still line up.
///
/// A record whose text it shares with its file's `pool` is only materialised when one of the
/// two actually changes it, so a file's matches cost one buffer rather than one each.
fn present(hits: &mut [LineHit], o: &SearchOptions, pool: &[u8]) {
    if !o.trim && o.max_columns == 0 {
        return;
    }
    for h in hits {
        let cut = if o.trim {
            h.text(pool)
                .iter()
                .take_while(|b| b.is_ascii_whitespace())
                .count()
        } else {
            0
        };
        if cut > 0 || (o.max_columns > 0 && h.text(pool).len() > o.max_columns) {
            if h.pooled != (0, 0) {
                h.text = h.text(pool).to_vec();
                h.pooled = (0, 0);
            }
        } else {
            continue;
        }
        if cut > 0 {
            h.text.drain(..cut);
            h.text_start += cut;
            h.spans.retain_mut(|(s, e)| {
                *s = s.saturating_sub(cut);
                *e = e.saturating_sub(cut);
                *e > *s
            });
        }
        if o.max_columns > 0 && h.text.len() > o.max_columns {
            // Columns are characters, as in ripgrep, so a cut never lands inside a code
            // point; a line that is not UTF-8 is cut by bytes.
            let cut = match std::str::from_utf8(&h.text) {
                Ok(text) => text
                    .char_indices()
                    .nth(o.max_columns)
                    .map_or(h.text.len(), |(i, _)| i),
                Err(_) => o.max_columns,
            };
            h.text.truncate(cut);
            if o.max_columns_preview {
                h.text.extend_from_slice(TRUNCATED);
            }
            h.spans.retain(|(s, _)| *s < cut);
            for (_, e) in h.spans.iter_mut() {
                if *e > cut {
                    *e = cut;
                }
            }
        }
    }
}

/// Fold one file's counters into an aggregate.
fn merge_stats(into: &mut Stats, from: &Stats) {
    into.files_scanned += from.files_scanned;
    into.files_binary += from.files_binary;
    into.bytes_scanned += from.bytes_scanned;
    into.matches += from.matches;
    into.errors.add(&from.errors);
}

/// Handle one line. Returns true when the file should stop being read (`-m` reached).
#[allow(clippy::too_many_arguments)]
fn handle_line(
    line: &[u8],
    no: u64,
    offset: u64,
    o: &SearchOptions,
    history: &mut VecDeque<(u64, Vec<u8>)>,
    hits: &mut Vec<LineHit>,
    pool: &mut Vec<u8>,
    matches: &mut u64,
    total: &mut u64,
    remaining_after: &mut usize,
    last_emitted: &mut u64,
    want_lines: bool,
    limit: u64,
) -> bool {
    let text = strip_cr(line);
    let found = o.pattern.find_at(text, 0);
    if found.is_some() == o.invert {
        // `--passthru` prints the whole file, so a line the pattern did not select still goes
        // out, as plain context rather than a hit.
        if want_lines && o.passthru {
            push_pooled(hits, pool, no, offset, text, false, Vec::new(), 0);
            *last_emitted = no;
            return false;
        }
        // A context line after a match, while the window lasts.
        if want_lines && *remaining_after > 0 {
            push_pooled(hits, pool, no, offset, text, false, Vec::new(), 0);
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
    // `--count-matches` counts individual matches, so they are tallied even when no line is
    // kept for printing.
    if !o.invert && o.count_matches {
        *total += o.pattern.find_all(text, MAX_PER_LINE).len() as u64;
    }
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
                // Context lines above the block keep their text but not their position, which
                // only `-A`/`-B` reach and neither prints an offset for.
                offset,
                text: t,
                pooled: (0, 0),
                matched: false,
                spans: Vec::new(),
                text_start: 0,
            });
            *last_emitted = n;
        }
        history.clear();
    }
    let found: Vec<Match> = if o.invert {
        Vec::new()
    } else {
        o.pattern.find_all(text, MAX_PER_LINE)
    };
    // `-r` rewrites the line before it prints, so what is printed and what is highlighted are
    // both the replacement, not the match.
    if let Some(rep) = &o.replace {
        if o.only_matching {
            for m in &found {
                let mut out = Vec::new();
                expand(&mut out, rep, &o.pattern, text, m.start);
                hits.push(LineHit {
                    number: no,
                    offset: offset + m.start as u64,
                    text: out,
                    pooled: (0, 0),
                    matched: true,
                    spans: Vec::new(),
                    text_start: m.start,
                });
                *last_emitted = no;
            }
        } else {
            let (text, spans) = rewrite(text, &found, rep, &o.pattern);
            hits.push(LineHit {
                number: no,
                offset,
                text,
                pooled: (0, 0),
                matched: true,
                spans,
                text_start: 0,
            });
            *last_emitted = no;
        }
        *remaining_after = o.after;
        return *matches >= limit;
    }
    let spans: Vec<(usize, usize)> = found.iter().map(|m| (m.start, m.end)).collect();
    if o.only_matching && !spans.is_empty() {
        for (s, e) in spans {
            push_pooled(
                hits,
                pool,
                no,
                offset + s as u64,
                &text[s..e],
                true,
                Vec::new(),
                s,
            );
            *last_emitted = no;
        }
    } else {
        push_pooled(hits, pool, no, offset, text, true, spans, 0);
    }
    *remaining_after = o.after;
    *matches >= limit
}

/// Append the replacement for the match at `start` of `text`, group references resolved
/// Push one record whose text joins the file's `pool` instead of owning a buffer: a line of
/// a file that matches often then costs one allocation rather than one each, and a `u32`
/// offset addresses the pool while it still fits.
#[allow(clippy::too_many_arguments)]
fn push_pooled(
    hits: &mut Vec<LineHit>,
    pool: &mut Vec<u8>,
    number: u64,
    offset: u64,
    text: &[u8],
    matched: bool,
    spans: Vec<(usize, usize)>,
    text_start: usize,
) {
    let mut hit = LineHit {
        number,
        offset,
        text: Vec::new(),
        pooled: (0, 0),
        matched,
        spans,
        text_start,
    };
    if pool.len() + text.len() <= POOL_MAX {
        let from = pool.len() as u32;
        pool.extend_from_slice(text);
        hit.pooled = (from, pool.len() as u32);
    } else {
        hit.text = text.to_vec();
    }
    hits.push(hit);
}

/// Append the replacement for the match at `start` of `text`, group references resolved
/// through the capture engine.
fn expand(out: &mut Vec<u8>, rep: &[Segment], pattern: &Pattern, text: &[u8], start: usize) {
    let groups = pattern
        .captures_at(text, start)
        .unwrap_or_else(|| vec![Some((start, start))]);
    for seg in rep {
        match seg {
            Segment::Literal(bytes) => out.extend_from_slice(bytes),
            Segment::Group(i) => {
                if let Some(Some((s, e))) = groups.get(*i) {
                    out.extend_from_slice(&text[*s..*e]);
                }
            }
        }
    }
}

/// The whole line with every match replaced, and the byte ranges of the replacements inside
/// it, which is what colouring and `--column` report.
fn rewrite(
    text: &[u8],
    matches: &[Match],
    rep: &[Segment],
    pattern: &Pattern,
) -> (Vec<u8>, Vec<(usize, usize)>) {
    let mut out = Vec::with_capacity(text.len());
    let mut spans = Vec::with_capacity(matches.len());
    let mut last = 0;
    for m in matches {
        out.extend_from_slice(&text[last..m.start]);
        let from = out.len();
        expand(&mut out, rep, pattern, text, m.start);
        spans.push((from, out.len()));
        last = m.end;
    }
    out.extend_from_slice(&text[last..]);
    (out, spans)
}

/// What `--max-columns-preview` appends to a line it had to cut.
const TRUNCATED: &[u8] = b" [... omitted end of long line]";

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
    if o.count || o.count_matches || o.files_with_matches || o.files_without_matches {
        return !r.files.is_empty();
    }
    r.files.iter().any(|f| !f.hits.is_empty())
}

#[cfg(test)]
mod tests {
    /// The helper has to answer exactly what `Path::file_name` answers, because it replaced it:
    /// the cases that make that method return `None` are the ones a slice gets wrong.
    #[test]
    fn the_last_component_answers_what_file_name_answers() {
        let cases = [
            "a/b.rs",
            "/",
            "",
            ".",
            "..",
            "a/..",
            "a/.",
            "a/b/",
            "src/main.rs",
            "a",
            "//",
            "x/",
            "./a",
            "./",
            "a/./",
            "./.",
            "a/./b",
            "...",
            "a/...",
        ];
        for case in cases {
            let want = std::path::Path::new(case)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            let got = super::last_component(case).map(str::to_string);
            assert_eq!(want, got, "{case:?}");
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn open_scan_skips_a_device_node_and_takes_a_regular_file() {
        // `/dev/null` is a character device: opening it is not the problem, reading a FIFO
        // for ever is, so the guard rejects anything that is not a regular file.
        // An entry the walk could not type is the one that still asks the kernel, so that is
        // what a device node reaching here is: `Kind::File` is the walk having said so.
        assert!(
            super::open_scan(std::path::Path::new("/dev/null"), Kind::Other)
                .unwrap()
                .is_none()
        );
        let dir = std::env::temp_dir().join(format!("dusage_open_scan_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(super::open_scan(&file, Kind::Other).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// The walk reads an entry's kind from the directory record, and its size only when the
    /// expression asks for one: `-name`/`-type` cost no syscall per entry.
    #[test]
    fn facts_read_the_kind_from_the_record_and_the_rest_on_demand() {
        let f = Fixture::new("facts");
        let path = f.file("a.txt", "12345");
        let mut ents = Vec::new();
        enumerate::list(&f.0, &mut ents, &mut ErrorCounts::default()).unwrap();
        let e = ents.iter().find(|e| e.name == "a.txt").unwrap();
        let cheap = Facts::of_entry(&path, e, Needs::default()).unwrap();
        assert_eq!(cheap.st.kind, Kind::File);
        assert_eq!(cheap.st.size, 0, "size was never read");
        let full = Facts::of_entry(
            &path,
            e,
            Needs {
                stat: true,
                ..Needs::default()
            },
        )
        .unwrap();
        assert_eq!(full.st.size, 5);
        assert_eq!(full.st.kind, Kind::File);
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
                        String::from_utf8_lossy(h.text(&f.pool)).into_owned(),
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
        let mut o = opts(&f, "needle");
        o.paint = true;
        let r = search(&o).unwrap();
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

    /// `-U` hands the whole file to the pattern, so a match may cover several lines: each of
    /// them is reported once, in order, and the count a `-c` run prints is the number of those
    /// lines, not the number of matches.
    #[test]
    fn multiline_matches_report_every_line_they_cover() {
        let f = Fixture::new("ml-lines");
        f.file("a.txt", "one\nstart\nmid\nend\ntail\nstart x end\nlast\n");
        let mut o = opts(&f, r"(?s)start.*?end");
        o.multiline = true;
        o.line_numbers = true;
        let r = search(&o).unwrap();
        let got: Vec<(u64, String)> = lines(&r).into_iter().map(|(_, n, t, _)| (n, t)).collect();
        assert_eq!(
            got,
            [
                (2, "start".to_string()),
                (3, "mid".to_string()),
                (4, "end".to_string()),
                (6, "start x end".to_string()),
            ]
        );
        assert_eq!(
            r.files[0].matches, 4,
            "one report per covered line, once each"
        );
        assert_eq!(r.stats.matches, 2, "two matches over the whole file");
        let mut o = opts(&f, r"(?s)start.*?end");
        o.multiline = true;
        o.count = true;
        assert_eq!(search(&o).unwrap().files[0].matches, 4, "-c counts lines");
        let mut o = opts(&f, r"(?s)start.*?end");
        o.multiline = true;
        o.only_matching = true;
        o.line_numbers = true;
        let starts: Vec<(u64, String)> = lines(&search(&o).unwrap())
            .into_iter()
            .map(|(_, n, t, _)| (n, t))
            .collect();
        assert_eq!(
            starts,
            [
                (2, "start\nmid\nend".to_string()),
                (6, "start x end".to_string())
            ],
            "-o reports the match whole, numbered by the line it starts on"
        );
    }

    /// The lines a match covers are counted the same way whether they are built or counted:
    /// one line each, the last line of the file when the match ends past its final newline,
    /// and none for an empty file, which has no lines at all.
    #[test]
    fn multiline_line_count_matches_the_lines_built() {
        let cases: [&[u8]; 7] = [
            b"",
            b"a",
            b"a\n",
            b"a\nb",
            b"a\nb\n",
            b"\n\n",
            b"start\nmid\nend",
        ];
        for data in cases {
            for src in [r"a", r"(?s)start.*?end", r"(?:)", r"(?m)^$", r"\w"] {
                let found = Pattern::new(src, false, false, false)
                    .unwrap()
                    .find_all(data, usize::MAX);
                assert_eq!(
                    matched_line_count(data, &found),
                    matched_lines(data, &found).len() as u64,
                    "{src} over {data:?}"
                );
            }
        }
        let empty = Pattern::new(r"^$", false, false, false).unwrap();
        assert_eq!(
            matched_lines(b"", &empty.find_all(b"", usize::MAX)).len(),
            0
        );
        let end = Pattern::new(r"(?:)", false, false, false).unwrap();
        let trailing = end.find_all(b"a\n", usize::MAX);
        assert_eq!(trailing.len(), 3);
        assert_eq!(
            matched_lines(b"a\n", &trailing).len(),
            1,
            "the empty tail after the final newline is not a line"
        );
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
        o.paint = true;
        let r = search(&o).unwrap();
        assert_eq!(r.files[0].hits[0].spans, vec![(2, 5), (8, 11)]);
    }

    #[test]
    fn only_matching_walks_a_whole_block() {
        // `-o` reads a block in one pass and continues from each match's end, so a line with
        // many matches in it, a match at the very start and one at the very end all have to
        // come out as the line path would print them.
        let f = Fixture::new("only-block");
        f.file(
            "a.txt",
            "needle one\n\n  needle needle needle  \nno\nneedle\n",
        );
        let mut o = opts(&f, "needle");
        o.only_matching = true;
        o.line_numbers = true;
        let r = search(&o).unwrap();
        let got: Vec<(u64, String)> = r.files[0]
            .hits
            .iter()
            .map(|h| {
                (
                    h.number,
                    String::from_utf8_lossy(h.text(&r.files[0].pool)).into_owned(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (1, "needle".to_string()),
                (3, "needle".to_string()),
                (3, "needle".to_string()),
                (3, "needle".to_string()),
                (5, "needle".to_string()),
            ]
        );
        assert_eq!(r.stats.matches, 3, "matching lines, not matches");
        assert_eq!(r.files[0].total, 0, "`--count-matches` was not asked for");
    }

    #[test]
    fn only_matching_counts_lines_across_blocks() {
        // A file several blocks long: each block picks up where the last one stopped, and the
        // line numbers still name the line the match is on.
        let f = Fixture::new("only-blocks");
        let mut text = String::new();
        let mut want = Vec::new();
        for line in 1..=60_000u64 {
            if line % 7 == 0 {
                want.push(line);
                text.push_str("a needle here\n");
            } else {
                text.push_str("nothing on this line at all\n");
            }
        }
        f.file("big.txt", &text);
        let mut o = opts(&f, "needle");
        o.only_matching = true;
        o.line_numbers = true;
        let r = search(&o).unwrap();
        let got: Vec<u64> = r.files[0].hits.iter().map(|h| h.number).collect();
        assert_eq!(got, want);
        let text: Vec<&[u8]> = r.files[0]
            .hits
            .iter()
            .map(|h| h.text(&r.files[0].pool))
            .collect();
        assert!(text.iter().all(|t| *t == b"needle"));
    }

    #[test]
    fn only_matching_caps_a_line() {
        // A pathological line cannot grow the output without bound, and the matches past the
        // cap are dropped rather than stopping the file.
        let f = Fixture::new("only-cap");
        let mut line = String::new();
        for _ in 0..(MAX_PER_LINE + 100) {
            line.push_str("ab ");
        }
        f.file("a.txt", &format!("{line}\nafter ab\n"));
        let mut o = opts(&f, "ab");
        o.only_matching = true;
        let r = search(&o).unwrap();
        let capped = r.files[0].hits.len();
        assert_eq!(capped, MAX_PER_LINE + 1, "the cap plus the next line's one");
        assert_eq!(r.stats.matches, 2, "both lines still count as matching");
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

    /// A tree wide enough that the collecting walk would hold every result, walked in the
    /// order `--sort path` prints: the streaming walk has to answer the same list, and it
    /// has to hand it over one result at a time.
    #[test]
    fn streaming_walk_lists_every_path_in_sort_order() {
        let f = Fixture::new("stream");
        for d in ["a", "b", "a-b", "a/deep", "b/deep", "zz"] {
            for n in 0..40 {
                f.file(&format!("{d}/f{n:02}.txt"), "");
            }
        }
        // Path scope does not match contents, so the pattern is never consulted.
        let mut o = opts(&f, "x");
        o.scope = Scope::Paths;
        let mut streamed: Vec<String> = Vec::new();
        let stats = walk_paths(&o, AbsPath::Fill, &mut |c| {
            streamed.push(c.rel.clone());
            true
        });
        assert_eq!(stats.errors.total(), 0);

        // The same set the collecting walk produces, in the same order, which is the order
        // `--sort path` prints and the order the listings have always come out in.
        let (collected, _) = walk(&o, 0);
        let expected: Vec<String> = collected.into_iter().map(|c| c.rel).collect();
        assert_eq!(streamed.len(), expected.len());
        assert_eq!(streamed, expected);
        let mut sorted = streamed.clone();
        sorted.sort();
        assert_eq!(sorted, streamed, "the stream is the sorted listing");
    }

    /// The visitor's `false` stops the walk where it stands: that is what a mode that has
    /// seen enough results relies on, and it is why the walk does not read the whole tree
    /// before the first line prints.
    #[test]
    fn streaming_walk_stops_when_the_visitor_says_so() {
        let f = Fixture::new("streamstop");
        for d in ["a", "b", "c", "d"] {
            for n in 0..10 {
                f.file(&format!("{d}/f{n}.txt"), "");
            }
        }
        let mut o = opts(&f, "x");
        o.scope = Scope::Paths;
        let mut seen = 0;
        let partial = walk_paths(&o, AbsPath::Skip, &mut |_| {
            seen += 1;
            seen < 12
        });
        assert_eq!(seen, 12, "the visitor decided the count");
        let mut all = 0;
        let full = walk_paths(&o, AbsPath::Skip, &mut |_| {
            all += 1;
            true
        });
        assert_eq!(all, 44, "four directories and forty files");
        assert!(
            partial.dirs_walked < full.dirs_walked,
            "stopped after {} of {} directories",
            partial.dirs_walked,
            full.dirs_walked
        );
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
        let f = &r.files[0];
        assert_eq!(f.hits[0].text(&f.pool), b"needle");
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
    fn a_read_boundary_inside_a_line_keeps_numbers_and_columns() {
        // Every line here is longer than one read, so each match is found by a block whose
        // boundary falls in the middle of a line and the line itself arrives in two pieces.
        // The number and the column must still name the line and the byte the text has them
        // at, the same answer the line-at-a-time path gives.
        let f = Fixture::new("wide-lines");
        let mut text = String::new();
        text.push_str(&"a".repeat(70_000));
        text.push('\n');
        text.push_str("needle one\n");
        text.push_str(&"b".repeat(70_000));
        text.push('\n');
        text.push_str("x needle y needle\n");
        text.push_str("needle\n");
        f.file("wide.txt", &text);
        let mut o = opts(&f, "needle");
        o.line_numbers = true;
        o.column = true;
        o.paint = true;
        let r = search(&o).unwrap();
        let got: Vec<(u64, Vec<(usize, usize)>)> = r.files[0]
            .hits
            .iter()
            .map(|h| (h.number, h.spans.clone()))
            .collect();
        // Columns are one past the span, so the second needle on line 4 is column 12.
        assert_eq!(
            got,
            [
                (2, vec![(0, 6)]),
                (4, vec![(2, 8), (11, 17)]),
                (5, vec![(0, 6)]),
            ]
        );
        // The same lines, one record per match, numbered the same way.
        let mut o = opts(&f, "needle");
        o.only_matching = true;
        o.line_numbers = true;
        let r = search(&o).unwrap();
        let numbers: Vec<u64> = r.files[0].hits.iter().map(|h| h.number).collect();
        assert_eq!(numbers, [2, 4, 4, 5]);
    }

    #[test]
    fn a_crlf_split_across_reads_shifts_no_number() {
        // A read ends between the `\r` and the `\n` of one line, and another `\r\n` sits
        // across the boundary of the next: neither may shift a number, and the `\r` must not
        // reach the printed line.
        let f = Fixture::new("crlf-split");
        let mut text = String::new();
        text.push_str(&"a".repeat(65_535));
        text.push_str("\r\n");
        text.push_str("needle two\r\n");
        text.push_str(&"c".repeat(65_000));
        text.push_str("\r\n");
        text.push_str("needle four\r\n");
        f.file("crlf.txt", &text);
        let mut o = opts(&f, "needle");
        o.line_numbers = true;
        let r = search(&o).unwrap();
        let got: Vec<(u64, String)> = r.files[0]
            .hits
            .iter()
            .map(|h| {
                (
                    h.number,
                    String::from_utf8_lossy(h.text(&r.files[0].pool)).into_owned(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (2, "needle two".to_string()),
                (4, "needle four".to_string()),
            ]
        );
        // And the count of text after the split is the file's own line count.
        assert_eq!(r.stats.matches, 2);
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
        // The number is what this test is about, so it asks for it the way `-n` does: a mode
        // that prints no number does not count the lines it skipped to print them.
        let mut o = opts(&f, "needle");
        o.line_numbers = true;
        let r = search(&o).unwrap();
        assert_eq!(r.stats.matches, 1);
        assert_eq!(r.files[0].hits[0].number, 1);
    }
}

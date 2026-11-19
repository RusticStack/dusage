//! Content and path search behind `dusage search` (a grep/rg replacement) and
//! `dusage find` (a name/path walker).
//!
//! The walk is single-threaded and deterministic — it lists candidates in one pass,
//! applying hidden, ignore-file and `--glob` rules — and the expensive part, reading and
//! matching file contents, runs on all worker threads through [`par_for_each`].

use crate::bytes;
use crate::encodings::{self, Encoding};
use crate::findexpr::{self, Ctx, Expr, Kind, Sink};
use crate::ignore::IgnoreRules;
use crate::model::ErrorCounts;
use crate::native::enumerate;
use crate::par_for_each;
use crate::pattern::{Match, Pattern};
use std::collections::{HashMap, VecDeque};
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
    /// Line bytes without the newline (a trailing `\r` is removed for `\r\n` files).
    pub text: Vec<u8>,
    /// False for context lines.
    pub matched: bool,
    /// Byte spans inside `text` for `-o` and colouring; empty on context lines.
    pub spans: Vec<(usize, usize)>,
    /// Where `text` starts within its line: 0 for a whole line, the match's offset for `-o`.
    /// `--column` and `--vimgrep` count a column from here.
    pub text_start: usize,
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
    pub path: PathBuf,
    pub rel: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub is_dir: bool,
    pub is_link: bool,
}

fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

/// One directory to read. The rules are this directory's ancestors; the directory's own
/// ignore files are pushed when the job runs, so a snapshot can be shared across threads.
struct WalkJob {
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
    cands: Vec<Candidate>,
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
fn read_job(job: &WalkJob, o: &SearchOptions, want_dirs: bool) -> JobOut {
    let mut out = JobOut {
        cands: Vec::new(),
        stats: Stats::default(),
        children: Vec::new(),
    };
    let mut rules = (*job.rules).clone();
    if !o.no_ignore {
        rules.push_dir(&job.dir, &job.rel);
    }
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
        let name = e.name.to_string_lossy();
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
            let child = job.dir.join(&e.name);
            if o.one_filesystem && enumerate::dev_of(&child).is_ok_and(|dev| dev != job.dev) {
                continue;
            }
            if want_dirs {
                out.cands.push(Candidate {
                    path: child.clone(),
                    rel: child_rel.clone(),
                    size: 0,
                    modified: None,
                    is_dir: true,
                    is_link: false,
                });
            }
            if o.max_depth.is_none_or(|m| job.depth + 1 < m) {
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
                    dir: child,
                    rel: child_rel,
                    depth: job.depth + 1,
                    rules: Arc::new(rules.clone()),
                    dev: job.dev,
                    chain,
                });
            }
        } else {
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
                let path = job.dir.join(&e.name);
                match e.stat(&path) {
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
            out.cands.push(Candidate {
                path: job.dir.join(&e.name),
                rel: child_rel,
                size,
                modified,
                is_dir: false,
                is_link: false,
            });
        }
    }
    out
}

/// List every candidate under the roots, in a deterministic order. Directories are read by
/// every worker thread at once, so the walk costs a fraction of its sequential time on a
/// tree with many directories.
/// The jobs a walk starts from: the roots that are directories, plus the files named directly
/// on the command line, which are taken as given.
fn root_jobs(o: &SearchOptions, root_dev: u64) -> (Vec<Candidate>, Vec<WalkJob>, Stats) {
    let mut all: Vec<Candidate> = Vec::new();
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
                all.push(Candidate {
                    path: root.clone(),
                    rel,
                    size: md.len(),
                    modified: md.modified().ok(),
                    is_dir: false,
                    is_link: false,
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

fn walk(o: &SearchOptions, root_dev: u64) -> (Vec<Candidate>, Stats) {
    let (mut all, jobs, mut stats) = root_jobs(o, root_dev);
    if !jobs.is_empty() {
        let want_dirs = o.scope == Scope::Paths;
        let threads = o.threads.max(1);
        let shared = Mutex::new((Vec::<Candidate>::new(), Stats::default()));
        par_for_each(jobs, threads, |job, spawn| {
            let out = read_job(job, o, want_dirs);
            for child in out.children {
                spawn(child);
            }
            let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
            guard.0.extend(out.cands);
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

/// Order the walk's results, and reverse them when `--sortr` asked.
fn sort_paths(v: &mut Vec<Candidate>, o: &SearchOptions) {
    match o.sort {
        SortKey::None => {}
        SortKey::Path => v.sort_by(|a, b| a.rel.cmp(&b.rel).then_with(|| a.path.cmp(&b.path))),
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

/// What an expression walk produced, for the front end's exit status.
#[derive(Debug, Default)]
pub struct ExprOutcome {
    /// Entries whose expression evaluated true.
    pub matched: u64,
    pub stats: Stats,
}

/// One entry's facts, in the shape [`findexpr::Entry`] wants them. The platforms differ in
/// which of them exist, so they are filled in one place per platform below.
struct Facts {
    path: PathBuf,
    kind: Kind,
    size: u64,
    blocks: u64,
    mtime: SystemTime,
    atime: SystemTime,
    ctime: SystemTime,
    links: u64,
    inode: u64,
    dev: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    /// A symlink's target, and what the target is.
    target: Option<String>,
    target_kind: Option<Kind>,
}

impl Facts {
    fn of(path: &Path) -> io::Result<Facts> {
        let md = std::fs::symlink_metadata(path)?;
        let mut f = Facts {
            path: path.to_path_buf(),
            kind: kind_of(&md),
            ..fields(&md)
        };
        if f.kind == Kind::Symlink {
            f.target = std::fs::read_link(path)
                .ok()
                .map(|t| t.to_string_lossy().into_owned());
            f.target_kind = std::fs::metadata(path).ok().map(|md| kind_of(&md));
        }
        Ok(f)
    }

    /// The entry described by its target instead of the link, which is what `-follow` turns
    /// a symlink into; the link's own name and target stay for `%l` and `-xtype`.
    fn followed(self, target: &Path) -> Facts {
        let Ok(md) = std::fs::metadata(target) else {
            return self;
        };
        let kind = match kind_of(&md) {
            Kind::Other => self.kind,
            k => k,
        };
        Facts {
            path: self.path,
            kind,
            target: self.target,
            target_kind: self.target_kind.or(Some(kind)),
            ..fields(&md)
        }
    }
}

/// The `lstat` fields both platforms have some of. `path` and `kind` are placeholders here:
/// every caller fills them in from the metadata it already parsed.
#[cfg(unix)]
fn fields(md: &std::fs::Metadata) -> Facts {
    use std::os::unix::fs::MetadataExt;
    Facts {
        path: PathBuf::new(),
        kind: Kind::Other,
        size: md.size(),
        blocks: md.blocks(),
        mtime: stamp(md.mtime()),
        atime: stamp(md.atime()),
        ctime: stamp(md.ctime()),
        links: md.nlink(),
        inode: md.ino(),
        dev: md.dev(),
        mode: md.mode(),
        uid: md.uid(),
        gid: md.gid(),
        target: None,
        target_kind: None,
    }
}

#[cfg(not(unix))]
fn fields(md: &std::fs::Metadata) -> Facts {
    use std::os::windows::fs::MetadataExt;
    Facts {
        path: PathBuf::new(),
        kind: Kind::Other,
        size: md.file_size(),
        // Windows has no `st_blocks`, and find's `%b` is a POSIX idea; 0 says "unknown".
        blocks: 0,
        mtime: tick(md.last_write_time()),
        atime: tick(md.last_access_time()),
        ctime: tick(md.creation_time()),
        // No link count, inode, mode bits or owner the walk can read without opening the
        // file, so `-links`, `-perm`, `-uid`, `-user` answer from zeros (see SEARCH.md).
        links: 0,
        inode: 0,
        dev: 0,
        mode: 0,
        uid: 0,
        gid: 0,
        target: None,
        target_kind: None,
    }
}

/// What kind of entry this is, from the `lstat` the walk already has.
#[cfg(unix)]
fn kind_of(md: &std::fs::Metadata) -> Kind {
    use std::os::unix::fs::FileTypeExt;
    let t = md.file_type();
    if t.is_dir() {
        Kind::Dir
    } else if t.is_symlink() {
        Kind::Symlink
    } else if t.is_file() {
        Kind::File
    } else if t.is_fifo() {
        Kind::Fifo
    } else if t.is_socket() {
        Kind::Socket
    } else if t.is_char_device() {
        Kind::Char
    } else if t.is_block_device() {
        Kind::Block
    } else {
        Kind::Other
    }
}

#[cfg(not(unix))]
fn kind_of(md: &std::fs::Metadata) -> Kind {
    let t = md.file_type();
    if t.is_symlink() {
        Kind::Symlink
    } else if t.is_dir() {
        Kind::Dir
    } else if t.is_file() {
        Kind::File
    } else {
        // Nothing reports a fifo, socket or device node here, and the walk never opens a
        // file to find out.
        Kind::Other
    }
}

#[cfg(unix)]
fn stamp(secs: i64) -> SystemTime {
    if secs >= 0 {
        std::time::UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        std::time::UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

/// A FILETIME is 100 ns ticks since 1601-01-01, 11644473600 s before the Unix epoch.
#[cfg(not(unix))]
fn tick(ticks: u64) -> SystemTime {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    let secs = ticks / 10_000_000;
    let nanos = (ticks % 10_000_000) * 100;
    match secs.checked_sub(EPOCH_DIFF) {
        Some(s) => std::time::UNIX_EPOCH + Duration::new(s, nanos as u32),
        None => std::time::UNIX_EPOCH,
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
    let mut w = ExprWalk {
        o,
        expr,
        ctx,
        sink,
        want_empty,
        out: ExprOutcome::default(),
        fs_names: filesystem_types(),
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
        if self.follow() && facts.kind == Kind::Symlink {
            facts = facts.followed(root);
        }
        let settings = self.expr.settings();
        let mut descend = facts.kind == Kind::Dir;
        // `-depth` reports a directory after its contents, the starting point included.
        let visit_now = !settings.depth_first;
        if visit_now && settings.mindepth.is_none_or(|m| m == 0) {
            descend &= self.visit(&facts, "", 0)?;
        }
        if descend && settings.maxdepth != Some(0) {
            let mut chain = vec![real_path(root)];
            self.dir(root, "", 0, rules, facts.dev, &mut chain)?;
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
                match std::fs::metadata(&child) {
                    Ok(md) => is_dir = md.is_dir(),
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
            let mut facts = match Facts::of(&child) {
                Ok(f) => f,
                Err(err) => {
                    self.out.stats.errors.record(&err);
                    continue;
                }
            };
            if self.follow() && facts.kind == Kind::Symlink {
                facts = facts.followed(&child);
            }
            // `-maxdepth` bounds what is walked as well as what is reported: the entry at
            // the limit is still evaluated, but its directory is never read.
            let mut descend =
                facts.kind == Kind::Dir && settings.maxdepth.is_none_or(|m| (depth as u64) < m);
            if descend && self.one_filesystem() && facts.dev != root_dev {
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
        let name = facts
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        let empty = self.want_empty
            && match facts.kind {
                Kind::File => facts.size == 0,
                Kind::Dir => std::fs::read_dir(&facts.path).is_ok_and(|mut d| d.next().is_none()),
                _ => false,
            };
        let entry = findexpr::Entry {
            size: facts.size,
            blocks: facts.blocks,
            mtime: facts.mtime,
            atime: facts.atime,
            ctime: facts.ctime,
            links: facts.links,
            inode: facts.inode,
            dev: facts.dev,
            mode: facts.mode,
            uid: facts.uid,
            gid: facts.gid,
            depth,
            empty,
            fs: self.fs_names.get(&facts.dev).map(String::as_str),
            target: facts.target.as_deref(),
            target_kind: facts.target_kind,
            ..findexpr::Entry::new(&path, &name, sub, facts.kind)
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

/// The real path of a directory, for the `--follow` cycle check: the path itself when it
/// cannot be resolved, so the check fails closed rather than skipping the entry.
fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Filesystem type names by device, best effort, for `-fstype`. Linux records which type is
/// mounted where in `/proc/self/mountinfo`; Windows leaves every name unknown.
#[cfg(unix)]
fn filesystem_types() -> HashMap<u64, String> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for line in text.lines() {
        // device mount-point options [optional...] `-` fstype source super-options
        let mut parts = line.split(' ');
        let dev = parts.nth(2).and_then(|d| {
            let (major, minor) = d.split_once(':')?;
            Some(makedev(major.parse().ok()?, minor.parse().ok()?))
        });
        let Some(dev) = dev else { continue };
        let name = parts.skip_while(|f| *f != "-").nth(1);
        if let Some(name) = name {
            out.insert(dev, name.to_string());
        }
    }
    out
}

/// glibc's `makedev`, which is the number `st_dev` holds for the same device.
#[cfg(unix)]
fn makedev(major: u64, minor: u64) -> u64 {
    (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
}

#[cfg(not(unix))]
fn filesystem_types() -> HashMap<u64, String> {
    HashMap::new()
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
    let root_dev = if o.one_filesystem {
        o.roots
            .first()
            .and_then(|r| enumerate::dev_of(r).ok())
            .unwrap_or(0)
    } else {
        0
    };
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
    let (direct, jobs, mut stats) = root_jobs(o, root_dev);
    let shared: Mutex<(Vec<FileHits>, Stats)> = Mutex::new((Vec::new(), Stats::default()));
    // `-q` answers the first match it sees and asks the rest of the walk to stop early.
    let found_one = std::sync::atomic::AtomicBool::new(false);
    let scan_one = |path: &Path, shared: &Mutex<(Vec<FileHits>, Stats)>, early: bool| {
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
        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = got {
            if early && hit.matches > 0 {
                found_one.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            guard.0.push(hit);
        }
        merge_stats(&mut guard.1, &local);
    };
    for c in &direct {
        scan_one(&c.path, &shared, o.quiet);
    }
    if !jobs.is_empty() {
        let early = o.quiet;
        par_for_each(jobs, o.threads, |job, spawn| {
            if found_one.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let out = read_job(job, o, false);
            for child in out.children {
                spawn(child);
            }
            let mut local = out.stats;
            for c in &out.cands {
                if found_one.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let mut one = Stats::default();
                let got = match scan_file(&c.path, o, &mut one) {
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
}

impl Lines {
    fn new() -> Lines {
        Lines {
            carry: Vec::new(),
            line: Vec::new(),
            history: VecDeque::new(),
            hits: Vec::new(),
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
fn scan_file(path: &Path, o: &SearchOptions, stats: &mut Stats) -> io::Result<Option<FileHits>> {
    // One scan buffer per thread: a fresh `vec![0u8; BLOCK]` per file zeroes 64 KiB that the
    // read is about to overwrite, which on a 162k-file tree is ten gigabytes of memset.
    let mut buf = scan_buffer();
    let res = scan_file_with(path, o, stats, &mut buf);
    recycle_buffer(buf);
    res
}

fn scan_file_with(
    path: &Path,
    o: &SearchOptions,
    stats: &mut Stats,
    buf: &mut Vec<u8>,
) -> io::Result<Option<FileHits>> {
    // `-m 0` asks for no match at all: never open the file.
    if o.max_count == Some(0) {
        return Ok(None);
    }
    if o.multiline {
        return scan_multiline(path, o, stats);
    }
    if o.null_data {
        return scan_records(path, o, stats);
    }
    // `--pre` hands the file to a command and searches what it prints, which is how a PDF or
    // an office document becomes text without this tool knowing anything about the format.
    if let Some(cmd) = &o.pre {
        if o.pre_globs.is_empty() || pre_matches(path, &o.pre_globs) {
            return scan_prepared(path, cmd, o, stats);
        }
    }
    let mut f = File::open(path)?;
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
        while !found {
            let n = read_chunk(&mut f, buf, true)?;
            if n == 0 {
                break;
            }
            stats.bytes_scanned += n as u64;
            found = o.pattern.find_at(&buf[..n.min(BIG_BLOCK)], 0).is_some();
        }
        return Ok(binary_hit(path, o, stats, found));
    }
    let mut sc = Scanner::pooled(o);
    if binary {
        stats.files_binary += 1;
    }
    stats.bytes_scanned += n as u64;
    sc.feed(&buf[..n]);
    while !sc.done() {
        let n = read_chunk(&mut f, buf, true)?;
        if n == 0 {
            break;
        }
        stats.bytes_scanned += n as u64;
        sc.feed(&buf[..n]);
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
    o: &SearchOptions,
    stats: &mut Stats,
) -> io::Result<Option<FileHits>> {
    let mut f = File::open(path)?;
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
    let found = o.pattern.find_all(&data, usize::MAX);
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
        }));
    }
    stats.matches += found.len() as u64;
    let mut hits = if o.passthru {
        every_line(&data, &found)
    } else if o.only_matching {
        found
            .iter()
            .map(|m| LineHit {
                number: line_number(&data, m.start),
                offset: m.start as u64,
                text: data[m.start..m.end].to_vec(),
                matched: true,
                spans: vec![(0, m.end - m.start)],
                text_start: m.start - line_start(&data, m.start),
            })
            .collect()
    } else if o.count || o.files_with_matches || o.files_without_matches || o.quiet {
        Vec::new()
    } else {
        matched_lines(&data, &found)
    };
    // `-c` counts the lines the matches cover, which is what a per-line mode would count.
    let lines = if hits.is_empty() {
        matched_lines(&data, &found).len() as u64
    } else {
        hits.iter().filter(|h| h.matched).count() as u64
    };
    present(&mut hits, o);
    let out = FileHits {
        path: name(),
        hits,
        matches: lines,
        total: found.len() as u64,
        binary: false,
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
fn scan_records(path: &Path, o: &SearchOptions, stats: &mut Stats) -> io::Result<Option<FileHits>> {
    let mut f = File::open(path)?;
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
        }));
    }
    stats.matches += matches;
    let keep =
        o.count || o.count_matches || o.files_with_matches || o.files_without_matches || o.quiet;
    if !keep {
        present(&mut hits, o);
    }
    Ok(Some(FileHits {
        path: path.to_string_lossy().into_owned(),
        hits: if keep { Vec::new() } else { hits },
        matches,
        total,
        binary: false,
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

fn line_number(data: &[u8], at: usize) -> u64 {
    bytes::count_byte(&data[..at.min(data.len())], b'\n') as u64 + 1
}

/// The offset at which the line holding `at` starts.
fn line_start(data: &[u8], at: usize) -> usize {
    match data[..at.min(data.len())].iter().rposition(|b| *b == b'\n') {
        Some(p) => p + 1,
        None => 0,
    }
}

/// The lines the matches cover, once each, with each line's own share of every match on it.
fn matched_lines(data: &[u8], found: &[Match]) -> Vec<LineHit> {
    let lines = line_spans(data);
    let mut out: Vec<LineHit> = Vec::new();
    let mut next = 0usize;
    for m in found {
        let first = lines
            .partition_point(|(s, _)| *s <= m.start)
            .saturating_sub(1);
        let last = lines
            .partition_point(|(s, _)| *s <= m.end.saturating_sub(1).max(m.start))
            .saturating_sub(1);
        for (i, (s, e)) in lines
            .iter()
            .enumerate()
            .take(last + 1)
            .skip(first.max(next))
        {
            let (s, e) = (*s, *e);
            let mut spans = Vec::new();
            for m in found.iter().filter(|m| m.start <= e && m.end > s) {
                let (a, b) = (m.start.max(s), m.end.min(e));
                if b > a {
                    spans.push((a - s, b - s));
                }
            }
            out.push(LineHit {
                number: (i + 1) as u64,
                offset: s as u64,
                text: strip_cr(&data[s..e]).to_vec(),
                matched: true,
                spans,
                text_start: 0,
            });
        }
        next = next.max(last + 1);
    }
    out
}

/// Every line of the file with what matched on it, for `--passthru -U`.
fn every_line(data: &[u8], found: &[Match]) -> Vec<LineHit> {
    line_spans(data)
        .into_iter()
        .enumerate()
        .map(|(i, (s, e))| {
            let mut spans = Vec::new();
            for m in found.iter().filter(|m| m.start <= e && m.end > s) {
                let (a, b) = (m.start.max(s), m.end.min(e));
                if b > a {
                    spans.push((a - s, b - s));
                }
            }
            LineHit {
                number: (i + 1) as u64,
                offset: s as u64,
                text: strip_cr(&data[s..e]).to_vec(),
                matched: !spans.is_empty(),
                spans,
                text_start: 0,
            }
        })
        .collect()
}

fn plain_lines(data: &[u8]) -> Vec<LineHit> {
    line_spans(data)
        .into_iter()
        .enumerate()
        .map(|(i, (s, e))| LineHit {
            number: (i + 1) as u64,
            offset: s as u64,
            text: strip_cr(&data[s..e]).to_vec(),
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
    /// Only for the modes where a match means "print or count this line": context, inversion,
    /// `-o` and `-m` need the line-at-a-time machinery and decline here.
    fn block_scan(&mut self, chunk: &[u8]) -> bool {
        if self.o.invert
            || self.o.before > 0
            || self.o.after > 0
            || self.o.only_matching
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
        // Line numbers are carried forward from match to match, so each byte of the block is
        // counted once rather than once per match.
        let base = self.line_no;
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
            line_no += bytes::count_byte(&body[line_start..ls], b'\n') as u64 + 1;
            self.line_no = line_no;
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
                let text = line.to_vec();
                let offset = self.pos + ls as u64;
                let mut spans: Vec<(usize, usize)> = Vec::new();
                if self.o.paint {
                    let mut at = 0;
                    while let Some(cm) = self.o.pattern.block_match(&text, at) {
                        spans.push((cm.start, cm.end));
                        at = cm.end.max(cm.start + 1);
                    }
                }
                self.hits.push(LineHit {
                    number: line_no,
                    offset,
                    text,
                    matched: true,
                    spans,
                    text_start: 0,
                });
            }
            if self.matches >= self.limit {
                break;
            }
            // One line is reported once however many matches it holds.
            pos = le + 1;
            line_start = pos;
        }
        // The block ends right before a newline, so it holds one line more than its newlines.
        self.line_no = base + bytes::count_byte(body, b'\n') as u64 + 1;
        if from + end + 1 < chunk.len() && self.carry.is_empty() {
            self.carry_pos = self.pos + (from + end + 1) as u64;
        }
        self.carry.extend_from_slice(&chunk[from + end + 1..]);
        true
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
    present(&mut sc.hits, o);
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
    };
    sc.recycle();
    Ok(Some(out))
}

/// The lines as they print: `--trim` drops the leading whitespace and `--max-columns` cuts
/// what is left, with the spans shifted so colouring and the columns still line up.
fn present(hits: &mut [LineHit], o: &SearchOptions) {
    for h in hits {
        if o.trim {
            let cut = h
                .text
                .iter()
                .take_while(|b| b.is_ascii_whitespace())
                .count();
            if cut > 0 {
                h.text.drain(..cut);
                h.text_start += cut;
                h.spans.retain_mut(|(s, e)| {
                    *s = s.saturating_sub(cut);
                    *e = e.saturating_sub(cut);
                    *e > *s
                });
            }
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
            hits.push(LineHit {
                number: no,
                offset,
                text: text.to_vec(),
                matched: false,
                spans: Vec::new(),
                text_start: 0,
            });
            *last_emitted = no;
            return false;
        }
        // A context line after a match, while the window lasts.
        if want_lines && *remaining_after > 0 {
            hits.push(LineHit {
                number: no,
                offset,
                text: text.to_vec(),
                matched: false,
                spans: Vec::new(),
                text_start: 0,
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
                let len = out.len();
                hits.push(LineHit {
                    number: no,
                    offset: offset + m.start as u64,
                    text: out,
                    matched: true,
                    spans: vec![(0, len)],
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
            hits.push(LineHit {
                number: no,
                offset: offset + s as u64,
                text: text[s..e].to_vec(),
                matched: true,
                spans: vec![(0, e - s)],
                text_start: s,
            });
            *last_emitted = no;
        }
    } else {
        hits.push(LineHit {
            number: no,
            offset,
            text: text.to_vec(),
            matched: true,
            spans,
            text_start: 0,
        });
        *last_emitted = no;
    }
    *remaining_after = o.after;
    *matches >= limit
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

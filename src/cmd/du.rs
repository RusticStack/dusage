//! `dusage du`: a du-compatible per-directory usage listing.
//!
//! The walk is the scan engine's: one work item per directory through
//! [`par_for_each`], listed with the batched enumerator, so every entry arrives with
//! its size, device and inode identity from one syscall. What `scan` throws away and
//! this keeps is the shape of the tree: one small node per directory holding its own
//! totals, its subdirectory totals and the listing order of its entries. Nothing is
//! printed during the pass; the tree is walked once more afterwards, in listing order,
//! post-order, which is the order GNU du prints in. Thread count therefore moves the
//! clock and never the output.
//!
//! # Sizes
//!
//! The default is allocation, which is what GNU du reports: the bytes a file really occupies
//! (`st_blocks * 512` on Linux, the allocation size the Windows enumerator returns), so a
//! sparse, reflinked or compressed file counts what the disk holds and not the length it
//! claims. A directory's own allocated blocks are counted too, as du counts them.
//! `-b`/`--bytes` and `--apparent-size` switch to the apparent size (`st_size`), where a
//! directory contributes nothing, which is GNU's rule there. Either base is then divided by
//! the unit exactly once per printed line, so each line converts its own subtree total and a
//! file's line and the directory above it round independently, as du's do: `-k` is 1 KiB and
//! the default, `-m` is 1 MiB, `--block-size=SIZE` is any size from one byte up (where `K`
//! and `KiB` are 1024 and `MB` 1000000), and `-h`/`--si` scale the base instead of dividing
//! it.
//!
//! An entry whose allocated size the enumerator cannot report is charged its apparent size
//! instead. That is decided by the backend and not by a zero: the Windows `readdir` fallback
//! has no allocation size (`std`'s metadata stops at EndOfFile) while the batched query
//! always carries one, and on Linux both backends read `st_blocks`, so there a file that
//! really occupies nothing — a hole, an empty file — is charged nothing, exactly as du
//! charges it.
//!
//! # Links, hardlinks and errors
//!
//! A symlink is counted as the entry it is — its own blocks by default, its own `st_size`
//! (the length of the target) under `-b` — and never followed; on Windows every reparse
//! point is treated the same way, whether or not it points at a directory. Hardlinked files are counted once per run
//! through a sharded set of `dev << 64 | ino` identities, and the names that lost are
//! dropped from both the totals and the `-a` listing, exactly as GNU du does.
//!
//! An unreadable directory is reported once on stderr, counted as empty, and makes the
//! run exit 1; a start point that is missing or cannot be read is reported and skipped
//! the same way.
//!
//! # Memory
//!
//! One node per directory (a parent index, a name, two sets of three counters, the entry
//! order) plus, with `-a`, one record per printed file. The hardlink set holds one 16-byte key per
//! distinct inode and is capped at [`MAX_SEEN`] keys, about 100 MB worst case; past the
//! cap the set stops taking keys, links are counted per name again, and `--stats` says
//! so.

use super::Fail;
use crate::cli::{parse_size, parse_threads, Args};
use crate::glob::Glob;
use crate::model::ErrorCounts;
use crate::native::enumerate::{self, DirReader, Entry};
use crate::queue::par_for_each;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DU_HELP: &str = "dusage du - disk usage per directory, the way du prints it

USAGE:
    dusage du [PATH...] [OPTIONS]

Each start point is walked in parallel and then printed deepest first, one directory
per line, with the start point's own line last. Sizes are the allocated size of each
entry (the blocks it occupies, as du reports), in 1 KiB blocks rounded up per line;
-b, -h, --si, --block-size and -m change the base or the unit.

OPTIONS:
    -a, --all             print every file as well as every directory
    -s, --summarize       print only the start points (the same as -d 0)
    -c, --total           add a final line labelled `total`
    -d, --max-depth N     print entries at depth <= N; totals still cover the subtree
    -b, --bytes           apparent sizes (st_size) in bytes, not allocated blocks
        --apparent-size   apparent sizes, in the default 1 KiB blocks
    -k                    blocks of 1 KiB of the base in force (the default)
    -m                    blocks of 1 MiB of the base in force
        --block-size SIZE divide the base by SIZE before printing: 1, 4096, K (1024),
                          KiB (1024), MB (1000000), M (1048576), ...
    -h, --human-readable  powers of 1024 of the base, one decimal below 10 (4.0K, 13K)
        --si              powers of 1000 (4.1k, 13k)
        --inodes          count entries instead of bytes
    -S, --separate-dirs   a directory's own size excludes its subdirectories
    -t, --threshold SIZE  print only entries >= SIZE, or <= SIZE with a leading `-`
    -x, --one-file-system
                          do not descend into a directory on another filesystem
        --exclude PATTERN     skip entries matching a gitignore-style glob (repeatable),
                              matched against the path relative to the start point, so
                              `*.log` hits at any depth and `build/**` a whole subtree
        --exclude-from FILE   read patterns from FILE, one per line (`#` comment lines
                              and blank lines are skipped)
        --time            print the modification time between size and path
        --time-style S    `long-iso` (default), `iso`, or `+FORMAT` built from %Y %m %d
                          %H %M %S and %% (times are UTC)
        --threads N       worker threads (default: one per core, 4 to 16)
        --stats           a summary line on stderr
    -P, --no-dereference  never follow links (the default; accepted for compatibility)
    -L, --dereference     refused: the walk never follows links
        --help            show this help (there is no short help flag: -h is human-readable)
    -V, --version         print the version

SIZES:
    The default is the allocated size, as du's is: st_blocks * 512 on Linux and the
    allocation size on Windows, so a sparse, reflinked or compressed file counts what the
    disk holds and a directory counts its own blocks. -b/--bytes and --apparent-size count
    st_size instead, where a directory contributes nothing. The base is divided by the unit
    once per printed line, so every line converts its own subtree total: 1 KiB by default and
    with -k, 1 MiB with -m, whatever --block-size asks for, and -h/--si scale the base to
    powers of 1024 or 1000. -t compares the same figure the line prints.

DIFFERENCES FROM GNU du:
    -L/--dereference is refused: the walk never follows links. A start point's own blocks
    are counted on Linux; on Windows std's metadata carries no allocation size for the path
    itself, so a start point's own blocks count 0 while the directories inside it are
    counted.

ORDER:
    Lines come out post-order: a directory's entries in listing order, each subdirectory
    before its parent, and the start point's own total last. A directory's entries keep
    the order the filesystem lists them, so a run is reproducible on a given tree but not
    sorted; --threads never changes it.

LINKS AND ERRORS:
    A symlink (on Windows, any reparse point) is counted as the entry it is and never
    followed. Hardlinked files count once per run. An unreadable directory is reported
    once on stderr, counted as empty, and the run exits 1.

STATS:
    --stats writes one line on stderr: entries and directories counted, hardlink names
    skipped and the bytes they carried, errors, threads, elapsed time, and whether the
    hardlink set hit its cap (after which links count per name).
";

/// Cap on distinct inodes held for hardlink dedup: 4M keys of 16 bytes plus hash
/// overhead, about 100 MB. Past it the set stops taking keys rather than grow without
/// bound, and `--stats` reports it.
const MAX_SEEN: usize = 4 << 20;

thread_local! {
    static READER: RefCell<DirReader> = RefCell::new(DirReader::new());
}

/// Read a directory into `out`, falling back to the portable `readdir` reader for the
/// rest of the run when the filesystem rejects the batched query.
fn read_dir(
    plain: &AtomicBool,
    dir: &Path,
    out: &mut Vec<Entry>,
    errs: &mut ErrorCounts,
) -> io::Result<()> {
    if plain.load(Ordering::Relaxed) {
        return enumerate::read_plain(dir, out, errs);
    }
    match READER.with(|r| r.borrow_mut().read(dir, out, errs)) {
        Err(e) if enumerate::is_unsupported(&e) => {
            plain.store(true, Ordering::Relaxed);
            out.clear();
            enumerate::read_plain(dir, out, errs)
        }
        r => r,
    }
}

/// Sharded set of seen inode identities: `dev << 64 | ino`, the same key the scanner
/// uses, so a hardlink in two subtrees is charged once.
struct Seen {
    shards: Vec<Mutex<HashSet<u128>>>,
    keys: AtomicUsize,
    capped: AtomicBool,
}

impl Seen {
    const SHARDS: usize = 64;

    fn new() -> Self {
        Seen {
            shards: (0..Self::SHARDS)
                .map(|_| Mutex::new(HashSet::new()))
                .collect(),
            keys: AtomicUsize::new(0),
            capped: AtomicBool::new(false),
        }
    }

    /// True the first time `id` is seen. Id 0 (unknown) is never a duplicate.
    fn first_seen(&self, id: u128) -> bool {
        if id == 0 || self.capped.load(Ordering::Relaxed) {
            return true;
        }
        if self.keys.load(Ordering::Relaxed) >= MAX_SEEN {
            self.capped.store(true, Ordering::Relaxed);
            return true;
        }
        let h = ((id as u64) ^ ((id >> 64) as u64)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let shard = &self.shards[(h >> 58) as usize % Self::SHARDS];
        let mut s = shard.lock().unwrap_or_else(|e| e.into_inner());
        if s.insert(id) {
            self.keys.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

/// One printed file, kept only under `-a`.
struct FileRec {
    name: String,
    apparent: u64,
    alloc: u64,
    mtime: SystemTime,
}

/// One directory's worth of bytes and entries, in both bases: the apparent size the files
/// claim, the size they occupy, and how many entries there are (`--inodes`).
#[derive(Clone, Copy, Default)]
struct Counters {
    apparent: u64,
    alloc: u64,
    count: u64,
}

impl Counters {
    fn add(&mut self, o: &Counters) {
        self.apparent += o.apparent;
        self.alloc += o.alloc;
        self.count += o.count;
    }
}

/// One directory: its own files, its subdirectories, and its totals.
struct Node {
    parent: Option<usize>,
    name: String,
    depth: usize,
    /// Entries of this directory in listing order, for the deterministic print pass.
    kids: Vec<Kid>,
    /// This directory's own files plus the directory's own inode (which is why `alloc` is
    /// seeded from the entry above and `count` from 1).
    own: Counters,
    /// Totals of the subdirectories that have already finished.
    sub: Counters,
    /// Subdirectories created and not yet rolled up into `sub`.
    pending: usize,
    /// The listing is finished, so `own` is final.
    listed: bool,
    /// Already rolled up into the parent.
    done: bool,
    mtime: SystemTime,
}

enum Kid {
    Dir(usize),
    File(FileRec),
}

/// An entry of one directory, before its child node exists.
enum Pending {
    Dir {
        name: String,
        path: PathBuf,
        /// The directory's own allocated bytes, from this listing's `statx`.
        alloc: u64,
        mtime: SystemTime,
    },
    File(FileRec),
}

struct Work {
    node: usize,
    path: PathBuf,
    /// Path relative to the start point, `/`-separated, for `--exclude`.
    rel: String,
}

/// The shared state of one walk: the node arena under one lock, plus counters touched
/// once per directory.
struct State {
    nodes: Vec<Node>,
    errors: Vec<String>,
    errs: ErrorCounts,
    files: u64,
    dirs: u64,
    dupes: u64,
    dupe_bytes: u64,
}

struct Ctx<'a> {
    o: &'a Options,
    plain: AtomicBool,
    root_dev: Option<u64>,
    seen: &'a Seen,
    state: Mutex<State>,
}

/// The finished walk of one start point.
struct Tree {
    nodes: Vec<Node>,
    errors: Vec<String>,
    errs: ErrorCounts,
    files: u64,
    dirs: u64,
    dupes: u64,
    dupe_bytes: u64,
}

/// Which figure of an entry the sizes come from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Base {
    /// What the entry occupies: `st_blocks * 512` on Linux, the allocation size on Windows.
    Alloc,
    /// The bytes the entry claims (`st_size`); `-b`/`--bytes`, `--apparent-size`.
    Apparent,
}

/// How sizes are printed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scale {
    /// The base divided into `block`-byte units, rounded up per printed line.
    Blocks,
    /// The base scaled to powers of `base` with `K`/`M`/... suffixes.
    Human(u64),
    /// Entry counts, `--inodes`.
    Inodes,
}

#[derive(Clone)]
enum TimeStyle {
    Long,
    Iso,
    Custom(String),
}

struct Options {
    threads: usize,
    all: bool,
    max_depth: usize,
    total: bool,
    separate: bool,
    scale: Scale,
    base: Base,
    block: u64,
    /// Unit label a bare `--block-size=KiB` prints, as `du -B KiB` does.
    block_label: Option<&'static str>,
    threshold: Option<i64>,
    one_filesystem: bool,
    excludes: Vec<Glob>,
    time: Option<TimeStyle>,
    stats: bool,
}

impl Options {
    /// The counted figure of one entry: the base the caller asked for, or an entry count.
    fn figure(&self, c: Counters) -> u64 {
        match self.scale {
            Scale::Inodes => c.count,
            _ => match self.base {
                Base::Alloc => c.alloc,
                Base::Apparent => c.apparent,
            },
        }
    }

    /// GNU du's threshold rule: a positive threshold keeps entries at least that size,
    /// a negative one keeps entries at most that size, compared before conversion.
    fn wanted(&self, bytes: u64) -> bool {
        match self.threshold {
            Some(t) if t > 0 => bytes >= t as u64,
            Some(t) if t < 0 => bytes <= t.unsigned_abs(),
            _ => true,
        }
    }

    fn excluded(&self, rel: &str, is_dir: bool) -> bool {
        self.excludes.iter().any(|g| g.matches(rel, is_dir))
    }

    /// The printed form of one entry's figure.
    fn size_text(&self, value: u64) -> String {
        match self.scale {
            Scale::Human(base) => human(value, base),
            Scale::Inodes => value.to_string(),
            Scale::Blocks => match self.block_label {
                Some(unit) => format!("{}{unit}", value.div_ceil(self.block)),
                None => value.div_ceil(self.block).to_string(),
            },
        }
    }
}

/// Post-order printing of a finished tree: the order GNU du prints in.
struct Pass<'a> {
    o: &'a Options,
    root_name: &'a str,
    nodes: &'a [Node],
}

impl Pass<'_> {
    fn total(&self) -> Counters {
        let n = &self.nodes[0];
        let mut c = n.own;
        c.add(&n.sub);
        c
    }

    /// The printed size of a directory: its own size only under `-S`.
    fn dir_value(&self, n: &Node) -> Counters {
        if self.o.separate {
            return n.own;
        }
        let mut c = n.own;
        c.add(&n.sub);
        c
    }

    fn emit(&self, out: &mut impl Write) -> io::Result<()> {
        self.emit_dir(0, 0, self.root_name, out)
    }

    fn emit_dir(
        &self,
        idx: usize,
        depth: usize,
        path: &str,
        out: &mut impl Write,
    ) -> io::Result<()> {
        let n = &self.nodes[idx];
        for kid in &n.kids {
            match kid {
                Kid::Dir(c) => {
                    let p = join(path, &self.nodes[*c].name);
                    self.emit_dir(*c, depth + 1, &p, out)?;
                }
                Kid::File(f) => {
                    let c = Counters {
                        apparent: f.apparent,
                        alloc: f.alloc,
                        count: 1,
                    };
                    let v = self.o.figure(c);
                    if depth < self.o.max_depth && self.o.wanted(v) {
                        let p = join(path, &f.name);
                        line(self.o, out, v, &p, f.mtime)?;
                    }
                }
            }
        }
        let v = self.o.figure(self.dir_value(n));
        if depth <= self.o.max_depth && self.o.wanted(v) {
            line(self.o, out, v, path, n.mtime)?;
        }
        Ok(())
    }
}

/// One output line: the size, the time under `--time`, then the path.
fn line(
    o: &Options,
    out: &mut impl Write,
    value: u64,
    path: &str,
    mtime: SystemTime,
) -> io::Result<()> {
    let size = o.size_text(value);
    match &o.time {
        None => writeln!(out, "{size}\t{path}"),
        Some(style) => writeln!(out, "{size}\t{}\t{path}", stamp(mtime, style)),
    }
}

fn join(base: &str, name: &str) -> String {
    let mut s = String::with_capacity(base.len() + 1 + name.len());
    s.push_str(base);
    if !base.ends_with(std::path::MAIN_SEPARATOR) {
        s.push(std::path::MAIN_SEPARATOR);
    }
    s.push_str(name);
    s
}

fn relative(base: &str, name: &OsStr) -> String {
    let n = name.to_string_lossy();
    if base.is_empty() {
        n.into_owned()
    } else {
        format!("{base}/{n}")
    }
}

fn display(name: &OsStr) -> String {
    name.to_string_lossy().into_owned()
}

/// Roll one finished directory up into its ancestors, stopping at the first ancestor
/// still waiting for a sibling.
fn roll(st: &mut State, mut idx: usize) {
    loop {
        let (own, sub, parent) = {
            let n = &mut st.nodes[idx];
            if !n.listed || n.pending != 0 || n.done {
                return;
            }
            n.done = true;
            (n.own, n.sub, n.parent)
        };
        let Some(p) = parent else { return };
        let mut sum = own;
        sum.add(&sub);
        let pn = &mut st.nodes[p];
        pn.sub.add(&sum);
        pn.pending -= 1;
        idx = p;
    }
}

/// Allocated bytes charged for one entry: what the backend reported, or the apparent size
/// when it reported nothing. A backend without allocations hands back 0 for every entry,
/// which is "unknown" and not "empty" — a hole only counts as nothing where the filesystem
/// reports blocks, and it does on Linux on both backends.
fn charge(apparent: u64, alloc: u64, known: bool) -> u64 {
    if known {
        alloc
    } else {
        apparent
    }
}

/// The charged size of one entry of a listed directory.
fn alloc_of(e: &Entry, known: bool) -> u64 {
    charge(e.size, e.alloc, known)
}

/// Whether a directory's entries carry allocated sizes. The batched enumerator always does;
/// the `readdir` fallback does on Linux (`lstat` gives `st_blocks`) but not on Windows,
/// where `std`'s metadata stops at EndOfFile.
fn allocations_known(plain: bool) -> bool {
    !plain || !cfg!(windows)
}

fn walk(c: &Ctx, w: &Work, push: &dyn Fn(Work)) {
    let o = c.o;
    let mut errs = ErrorCounts::default();
    let mut ents = Vec::new();
    let mut errors = Vec::new();
    if let Err(e) = read_dir(&c.plain, &w.path, &mut ents, &mut errs) {
        errs.record(&e);
        errors.push(format!("cannot read directory '{}': {e}", w.path.display()));
    }
    let alloc_known = allocations_known(c.plain.load(Ordering::Relaxed));
    let mut pending: Vec<Pending> = Vec::with_capacity(ents.len());
    let mut files = 0u64;
    let mut dirs = 0u64;
    let mut dupes = 0u64;
    let mut dupe_bytes = 0u64;
    // This directory's own files. The directory's own entry — its `count` and its blocks —
    // is seeded when its node is created, so it is counted once.
    let mut own = Counters::default();
    for e in &ents {
        let reparse = e.is_reparse();
        let is_dir = e.is_dir() && !reparse;
        if !o.excludes.is_empty() && o.excluded(&relative(&w.rel, &e.name), is_dir) {
            continue;
        }
        if is_dir {
            if c.root_dev
                .is_some_and(|d| enumerate::other_filesystem(e, d))
            {
                continue;
            }
            dirs += 1;
            pending.push(Pending::Dir {
                name: display(&e.name),
                path: w.path.join(&e.name),
                alloc: if alloc_known { e.alloc } else { 0 },
                mtime: e.modified(),
            });
            continue;
        }
        if !c.seen.first_seen(e.file_id) {
            dupes += 1;
            dupe_bytes += e.size;
            continue;
        }
        files += 1;
        own.apparent += e.size;
        own.alloc += alloc_of(e, alloc_known);
        own.count += 1;
        if o.all {
            pending.push(Pending::File(FileRec {
                name: display(&e.name),
                apparent: e.size,
                alloc: alloc_of(e, alloc_known),
                mtime: e.modified(),
            }));
        }
    }

    let mut work = Vec::new();
    let mut st = c.state.lock().unwrap_or_else(|e| e.into_inner());
    for p in pending {
        match p {
            Pending::Dir {
                name,
                path,
                alloc,
                mtime,
            } => {
                let rel = if w.rel.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{}", w.rel, name)
                };
                let idx = st.nodes.len();
                let depth = st.nodes[w.node].depth + 1;
                st.nodes.push(Node {
                    parent: Some(w.node),
                    name,
                    depth,
                    kids: Vec::new(),
                    own: Counters {
                        apparent: 0,
                        alloc,
                        count: 1,
                    },
                    sub: Counters::default(),
                    pending: 0,
                    listed: false,
                    done: false,
                    mtime,
                });
                st.nodes[w.node].kids.push(Kid::Dir(idx));
                st.nodes[w.node].pending += 1;
                work.push(Work {
                    node: idx,
                    path,
                    rel,
                });
            }
            Pending::File(f) => st.nodes[w.node].kids.push(Kid::File(f)),
        }
    }
    {
        let n = &mut st.nodes[w.node];
        n.own.add(&own);
        n.listed = true;
    }
    st.files += files;
    st.dirs += dirs;
    st.dupes += dupes;
    st.dupe_bytes += dupe_bytes;
    st.errs.add(&errs);
    st.errors.extend(errors);
    roll(&mut st, w.node);
    drop(st);
    for w in work {
        push(w);
    }
}

/// Walk one start point and return the finished tree. `root_alloc` is the start point's own
/// allocated size, which no listing above it can supply.
fn walk_root(o: &Options, spec: &str, mtime: SystemTime, root_alloc: u64, seen: &Seen) -> Tree {
    let path = PathBuf::from(spec);
    let root_dev = o
        .one_filesystem
        .then(|| enumerate::dev_of(&path).ok())
        .flatten();
    let root = Node {
        parent: None,
        name: spec.to_string(),
        depth: 0,
        kids: Vec::new(),
        own: Counters {
            apparent: 0,
            alloc: root_alloc,
            count: 1,
        },
        sub: Counters::default(),
        pending: 0,
        listed: false,
        done: false,
        mtime,
    };
    let ctx = Ctx {
        o,
        plain: AtomicBool::new(false),
        root_dev,
        seen,
        state: Mutex::new(State {
            nodes: vec![root],
            errors: Vec::new(),
            errs: ErrorCounts::default(),
            files: 0,
            dirs: 0,
            dupes: 0,
            dupe_bytes: 0,
        }),
    };
    par_for_each(
        vec![Work {
            node: 0,
            path,
            rel: String::new(),
        }],
        o.threads,
        |w, push| walk(&ctx, w, push),
    );
    let st = ctx.state.into_inner().unwrap_or_else(|e| e.into_inner());
    Tree {
        nodes: st.nodes,
        errors: st.errors,
        errs: st.errs,
        files: st.files,
        dirs: st.dirs,
        dupes: st.dupes,
        dupe_bytes: st.dupe_bytes,
    }
}

/// Read `text` as a `--time-style` value.
fn time_style(s: &str) -> Result<TimeStyle, String> {
    if let Some(f) = s.strip_prefix('+') {
        return Ok(TimeStyle::Custom(f.to_string()));
    }
    match s {
        "long-iso" => Ok(TimeStyle::Long),
        "iso" => Ok(TimeStyle::Iso),
        _ => Err(format!(
            "unknown time style {s:?} (use long-iso, iso, or +FORMAT)"
        )),
    }
}

/// Seconds since the epoch as (year, month, day, hour, minute, second), UTC.
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let rem = secs.rem_euclid(86_400) as u32;
    (
        if m <= 2 { y + 1 } else { y },
        m,
        d,
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60,
    )
}

fn stamp(t: SystemTime, style: &TimeStyle) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil(secs);
    match style {
        TimeStyle::Long => format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}"),
        // GNU du's `iso` is the date alone; `long-iso` is this plus the minutes.
        TimeStyle::Iso => format!("{y:04}-{mo:02}-{d:02}"),
        TimeStyle::Custom(f) => {
            let mut out = String::with_capacity(f.len() + 8);
            let mut it = f.chars();
            while let Some(ch) = it.next() {
                if ch != '%' {
                    out.push(ch);
                    continue;
                }
                match it.next() {
                    Some('Y') => out.push_str(&format!("{y:04}")),
                    Some('m') => out.push_str(&format!("{mo:02}")),
                    Some('d') => out.push_str(&format!("{d:02}")),
                    Some('H') => out.push_str(&format!("{h:02}")),
                    Some('M') => out.push_str(&format!("{mi:02}")),
                    Some('S') => out.push_str(&format!("{s:02}")),
                    Some('%') => out.push('%'),
                    Some(c) => {
                        out.push('%');
                        out.push(c);
                    }
                    None => out.push('%'),
                }
            }
            out
        }
    }
}

/// Powers of `base`, rounded up at the printed precision the way `du -h` prints:
/// `4.0K`, `13K`, `7.0K` for 7095 bytes, `512` for less than a unit (and `4.1k` for
/// `--si`). A carry that would reach the base promotes the unit, so 1048575 bytes is
/// `1.0M` rather than `1024K`.
fn human(bytes: u64, base: u64) -> String {
    const BIN: [&str; 7] = ["", "K", "M", "G", "T", "P", "E"];
    const SI: [&str; 7] = ["", "k", "M", "G", "T", "P", "E"];
    let units = if base == 1000 { SI } else { BIN };
    if bytes < base {
        return bytes.to_string();
    }
    let mut idx = 0;
    let mut v = bytes as f64;
    while v >= base as f64 && idx < units.len() - 1 {
        v /= base as f64;
        idx += 1;
    }
    // One decimal below ten, none above, always rounded up.
    let mut left = if v < 10.0 {
        (v * 10.0).ceil() / 10.0
    } else {
        v.ceil()
    };
    if left >= base as f64 && idx < units.len() - 1 {
        left /= base as f64;
        idx += 1;
    }
    if left < 10.0 {
        format!("{left:.1}{}", units[idx])
    } else {
        format!("{left:.0}{}", units[idx])
    }
}

fn parse_threshold(s: &str) -> Result<i64, String> {
    let t = s.trim();
    let (sign, body) = match t.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, t.strip_prefix('+').unwrap_or(t)),
    };
    let n = parse_size(body)?;
    if n > i64::MAX as u64 {
        return Err(format!("threshold too large: {s:?}"));
    }
    Ok(sign * n as i64)
}

fn parse_depth(s: &str) -> Result<usize, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("invalid depth {s:?} (must be an integer >= 0)"))
}

fn excludes_of(a: &Args) -> Result<Vec<Glob>, Fail> {
    let mut out = Vec::new();
    for p in a.values("--exclude") {
        out.push(Glob::new(p).map_err(|e| Fail(2, format!("--exclude {p:?}: {e}")))?);
    }
    for file in a.values("--exclude-from") {
        let text = fs::read_to_string(file).map_err(|e| Fail(2, format!("{file}: {e}")))?;
        for line in text.lines() {
            let p = line.trim();
            if p.is_empty() || p.starts_with('#') {
                continue;
            }
            out.push(Glob::new(p).map_err(|e| Fail(2, format!("{file}: {p:?}: {e}")))?);
        }
    }
    Ok(out)
}

/// Allocated bytes of one path, from metadata that is already in hand: `st_blocks * 512` on
/// Linux. Windows' `std` metadata carries the EndOfFile size and not AllocationSize (only the
/// batched directory query has that), so a start point's own blocks are `None` there.
#[cfg(target_os = "linux")]
fn path_alloc(md: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(md.blocks() * 512)
}

#[cfg(windows)]
fn path_alloc(_md: &fs::Metadata) -> Option<u64> {
    None
}

/// `--block-size=SIZE`, GNU's meaning: a number and an optional unit, where `K` and `KiB`
/// are 1024 and `KB` 1000. A SIZE that is a bare unit also labels every number it prints, as
/// `du --block-size=KiB` prints `23KiB`; one with a number in it prints a plain number.
fn block_size(s: &str) -> Result<(u64, Option<&'static str>), String> {
    let t = s.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let (mult, label): (u64, Option<&'static str>) = match unit {
        "" => (1, None),
        "k" | "K" | "Ki" => (1 << 10, Some("K")),
        "KiB" => (1 << 10, Some("KiB")),
        "kB" | "KB" => (1_000, Some("kB")),
        "M" | "Mi" => (1 << 20, Some("M")),
        "MiB" => (1 << 20, Some("MiB")),
        "MB" => (1_000_000, Some("MB")),
        "G" | "Gi" => (1 << 30, Some("G")),
        "GiB" => (1 << 30, Some("GiB")),
        "GB" => (1_000_000_000, Some("GB")),
        "T" | "Ti" => (1 << 40, Some("T")),
        "TiB" => (1 << 40, Some("TiB")),
        "TB" => (1_000_000_000_000, Some("TB")),
        _ => {
            return Err(format!(
                "invalid --block-size unit in {s:?} (use e.g. 1, 4096, K, KiB, M, MB)"
            ))
        }
    };
    let base = if num.is_empty() {
        1
    } else {
        parse_size(num).map_err(|_| format!("invalid --block-size {s:?}"))?
    };
    let block = base
        .checked_mul(mult)
        .ok_or_else(|| format!("--block-size too large: {s:?}"))?;
    if block == 0 {
        return Err(format!(
            "invalid --block-size {s:?}: a block is at least one byte"
        ));
    }
    Ok((block, if num.is_empty() { label } else { None }))
}

fn options(a: &Args) -> Result<Options, Fail> {
    if a.flag("-L") || a.flag("--dereference") {
        return Err(Fail(
            2,
            "-L/--dereference is not supported: this walk never follows links. \
             To list what links point at, use 'dusage find --follow'"
                .to_string(),
        ));
    }
    let summarize = a.flag("-s") || a.flag("--summarize");
    let all = a.flag("-a") || a.flag("--all");
    if summarize && all {
        return Err(Fail(
            2,
            "cannot both summarize (-s) and show all entries (-a)".to_string(),
        ));
    }
    let scale = if a.flag("--inodes") {
        Scale::Inodes
    } else if a.flag("-h") || a.flag("--human-readable") {
        Scale::Human(1024)
    } else if a.flag("--si") {
        Scale::Human(1000)
    } else {
        Scale::Blocks
    };
    let bytes = a.flag("-b") || a.flag("--bytes");
    let base = if bytes || a.flag("--apparent-size") {
        Base::Apparent
    } else {
        Base::Alloc
    };
    let (block, block_label) = match a.value("--block-size") {
        Some(v) => block_size(v).map_err(Fail::from)?,
        // `-b` is `--apparent-size --block-size=1`; `--apparent-size` alone keeps 1 KiB.
        None if bytes => (1, None),
        None if a.flag("-k") => (1 << 10, None),
        None if a.flag("-m") => (1 << 20, None),
        None => (1 << 10, None),
    };
    let threshold = match a.value("--threshold").or(a.value("-t")) {
        Some(v) => Some(parse_threshold(v)?),
        None => None,
    };
    let max_depth = match a.value("--max-depth").or(a.value("-d")) {
        Some(v) => parse_depth(v)?,
        None if summarize => 0,
        None => usize::MAX,
    };
    let time = match a.value("--time-style") {
        Some(v) => Some(time_style(v)?),
        None if a.flag("--time") => Some(TimeStyle::Long),
        None => None,
    };
    Ok(Options {
        threads: match a.value("--threads") {
            Some(v) => parse_threads(v)?,
            None => crate::cli::default_threads(),
        },
        all,
        max_depth,
        total: a.flag("-c") || a.flag("--total"),
        separate: a.flag("-S") || a.flag("--separate-dirs"),
        scale,
        base,
        block,
        block_label,
        threshold,
        one_filesystem: a.flag("-x") || a.flag("--one-file-system"),
        excludes: excludes_of(a)?,
        time,
        stats: a.flag("--stats"),
    })
}

struct Run {
    o: Options,
    seen: Seen,
    totals: Counters,
    files: u64,
    dirs: u64,
    dupes: u64,
    dupe_bytes: u64,
    errs: ErrorCounts,
    failed: bool,
}

impl Run {
    fn new(o: Options) -> Self {
        Run {
            o,
            seen: Seen::new(),
            totals: Counters::default(),
            files: 0,
            dirs: 0,
            dupes: 0,
            dupe_bytes: 0,
            errs: ErrorCounts::default(),
            failed: false,
        }
    }

    fn error(&mut self, msg: String) {
        eprintln!("dusage du: {msg}");
        self.failed = true;
    }

    fn one(&mut self, spec: &str, out: &mut impl Write) -> io::Result<()> {
        let path = Path::new(spec);
        let md = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                self.error(format!("cannot access '{spec}': {e}"));
                return Ok(());
            }
        };
        if let Err(e) = enumerate::refuse_reparse_root(path) {
            self.error(e.to_string());
            return Ok(());
        }
        let mtime = md.modified().unwrap_or(UNIX_EPOCH);
        if !md.is_dir() {
            let size = md.len();
            let c = Counters {
                apparent: size,
                alloc: path_alloc(&md).unwrap_or(size),
                count: 1,
            };
            self.files += 1;
            self.totals.add(&c);
            let v = self.o.figure(c);
            if self.o.wanted(v) {
                line(&self.o, out, v, spec, mtime)?;
            }
            return Ok(());
        }
        let root_alloc = path_alloc(&md).unwrap_or(0);
        let tree = walk_root(&self.o, spec, mtime, root_alloc, &self.seen);
        self.dirs += 1;
        for m in &tree.errors {
            self.error(m.clone());
        }
        self.errs.add(&tree.errs);
        self.files += tree.files;
        self.dirs += tree.dirs;
        self.dupes += tree.dupes;
        self.dupe_bytes += tree.dupe_bytes;
        let pass = Pass {
            o: &self.o,
            root_name: spec,
            nodes: &tree.nodes,
        };
        pass.emit(out)?;
        self.totals.add(&pass.total());
        Ok(())
    }
}

pub fn run(raw: Vec<String>) -> Result<(), Fail> {
    let a = Args::parse(raw, VALUE_FLAGS, BOOL_FLAGS)?;
    if a.flag("--help") {
        print!("{DU_HELP}");
        return Ok(());
    }
    if a.flag("-V") || a.flag("--version") {
        println!("dusage {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let o = options(&a)?;
    let roots: Vec<String> = if a.positionals().is_empty() {
        vec![".".to_string()]
    } else {
        a.positionals().to_vec()
    };
    let started = Instant::now();
    let mut run = Run::new(o);
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    for spec in &roots {
        run.one(spec, &mut out).map_err(Fail::output)?;
    }
    let total = run.totals;
    if run.o.total && run.o.wanted(run.o.figure(total)) {
        line(&run.o, &mut out, run.o.figure(total), "total", UNIX_EPOCH).map_err(Fail::output)?;
    }
    out.flush().map_err(Fail::output)?;
    let elapsed = started.elapsed();
    if run.o.stats {
        report_stats(&run, elapsed);
    }
    if run.failed || run.errs.total() > 0 {
        return Err(Fail(1, String::new()));
    }
    Ok(())
}

fn report_stats(run: &Run, elapsed: Duration) {
    let mut line = format!(
        "dusage du: {} entries ({} files, {} directories), {} hardlink names already \
         counted ({} bytes), {} errors, {} threads, {:.1} ms",
        run.files + run.dirs,
        run.files,
        run.dirs,
        run.dupes,
        run.dupe_bytes,
        run.errs.total(),
        run.o.threads,
        elapsed.as_secs_f64() * 1000.0
    );
    if run.seen.capped.load(Ordering::Relaxed) {
        line.push_str(", hardlink set capped: links past the cap count per name");
    }
    eprintln!("{line}");
}

/// The flags `du` takes, for [`super::accepted_flags`].
pub(super) const VALUE_FLAGS: &[&str] = &[
    "--max-depth",
    "-d",
    "--threshold",
    "-t",
    "--block-size",
    "--exclude",
    "--exclude-from",
    "--time-style",
    "--threads",
];

pub(super) const BOOL_FLAGS: &[&str] = &[
    "-a",
    "--all",
    "-b",
    "--bytes",
    "--apparent-size",
    "-c",
    "--total",
    "-k",
    "-m",
    "-h",
    "--human-readable",
    "--si",
    "--inodes",
    "-s",
    "--summarize",
    "-S",
    "--separate-dirs",
    "-x",
    "--one-file-system",
    "--time",
    "--stats",
    "-L",
    "--dereference",
    "-P",
    "--no-dereference",
    "--help",
    "-V",
    "--version",
];

/// The du help text, for `dusage --help`.
pub fn help() -> &'static str {
    DU_HELP
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry whose allocated size the backend cannot report is charged its apparent size;
    /// a reported 0 — a hole, on a filesystem that does count blocks — stays 0.
    #[test]
    fn an_unknown_allocation_is_charged_its_apparent_size() {
        assert_eq!(charge(1 << 20, 0, false), 1 << 20);
        assert_eq!(charge(1 << 20, 4096, false), 1 << 20);
        assert_eq!(charge(1 << 20, 0, true), 0);
        assert_eq!(charge(1 << 20, 4096, true), 4096);
        // Only Windows' readdir backend loses allocations; Linux reads st_blocks both ways.
        assert_eq!(allocations_known(true), cfg!(not(windows)));
        assert!(allocations_known(false));
    }

    #[test]
    fn block_size_reads_gnu_units() {
        assert_eq!(block_size("1"), Ok((1, None)));
        assert_eq!(block_size("4096"), Ok((4096, None)));
        assert_eq!(block_size("1K"), Ok((1024, None)));
        assert_eq!(block_size("K"), Ok((1024, Some("K"))));
        assert_eq!(block_size("k"), Ok((1024, Some("K"))));
        assert_eq!(block_size("KiB"), Ok((1024, Some("KiB"))));
        assert_eq!(block_size("KB"), Ok((1000, Some("kB"))));
        assert_eq!(block_size("M"), Ok((1 << 20, Some("M"))));
        assert_eq!(block_size("MB"), Ok((1_000_000, Some("MB"))));
        assert_eq!(block_size("2K"), Ok((2048, None)));
        assert!(block_size("0").is_err());
        assert!(block_size("nonsense").is_err());
    }

    /// Every line converts its own total once, in whichever base and unit it prints.
    #[test]
    fn a_line_costs_one_conversion() {
        let opts = |scale, base, block, block_label| Options {
            threads: 1,
            all: false,
            max_depth: usize::MAX,
            total: false,
            separate: false,
            scale,
            base,
            block,
            block_label,
            threshold: None,
            one_filesystem: false,
            excludes: Vec::new(),
            time: None,
            stats: false,
        };
        let sparse = Counters {
            apparent: 1 << 20,
            alloc: 8192,
            count: 1,
        };
        let alloc = opts(Scale::Blocks, Base::Alloc, 1024, None);
        assert_eq!(alloc.size_text(alloc.figure(sparse)), "8");
        let apparent = opts(Scale::Blocks, Base::Apparent, 1024, None);
        assert_eq!(apparent.size_text(apparent.figure(sparse)), "1024");
        let label = opts(Scale::Blocks, Base::Alloc, 1 << 10, Some("KiB"));
        assert_eq!(label.size_text(label.figure(sparse)), "8KiB");
        let human = opts(Scale::Human(1024), Base::Alloc, 1, None);
        assert_eq!(human.size_text(human.figure(sparse)), "8.0K");
    }
}

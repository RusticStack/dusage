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
//! A size is the file's own byte count (`st_size`), in 1 KiB blocks rounded up per
//! file: 1000 bytes is one block, 5000 is five. `-b`/`--apparent-size` switch to exact
//! bytes, `-m` to MiB blocks, `-h`/`--si` to scaled units. GNU du's default is allocated
//! blocks (`st_blocks`), which counts what a sparse or compressed file really occupies;
//! this one reports the data itself, so totals do not move with the filesystem, and
//! `du --apparent-size` is the closest GNU comparison. `-m` keeps du's own rule and
//! converts a subtree's byte total once, because one megabyte per small file would
//! report a directory of a thousand 1 KiB files as a gigabyte. A directory inode contributes
//! nothing to its own size, as with `du --apparent-size`: a directory's `st_size` is
//! filesystem bookkeeping, not data.
//!
//! # Links, hardlinks and errors
//!
//! A symlink is counted as the entry it is (its own `st_size`, the length of the
//! target) and never followed; on Windows every reparse point is treated the same way,
//! whether or not it points at a directory. Hardlinked files are counted once per run
//! through a sharded set of `dev << 64 | ino` identities, and the names that lost are
//! dropped from both the totals and the `-a` listing, exactly as GNU du does.
//!
//! An unreadable directory is reported once on stderr, counted as empty, and makes the
//! run exit 1; a start point that is missing or cannot be read is reported and skipped
//! the same way.
//!
//! # Memory
//!
//! One node per directory (a parent index, a name, four counters, the entry order) plus,
//! with `-a`, one record per printed file. The hardlink set holds one 16-byte key per
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
per line, with the start point's own line last. Sizes are the apparent size of the
files, in 1 KiB blocks rounded up per file; -b, -h, --si and -m change the unit.

OPTIONS:
    -a, --all             print every file as well as every directory
    -s, --summarize       print only the start points (the same as -d 0)
    -c, --total           add a final line labelled `total`
    -d, --max-depth N     print entries at depth <= N; totals still cover the subtree
    -b, --bytes           sizes in bytes, not blocks
        --apparent-size   the same as -b
    -k                    sizes in 1 KiB blocks (the default)
    -m                    sizes in 1 MiB blocks, the subtree total converted once
    -h, --human-readable  powers of 1024, one decimal below 10 (4.0K, 13K); sizes in bytes
        --si              powers of 1000 (4.1k, 13k); sizes in bytes
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
    Apparent size (st_size), rounded up per file into the unit: 1000 bytes is one 1 KiB
    block and 5000 is five, so a file's own line and the directory above it agree. GNU du
    defaults to allocated blocks (st_blocks), which is what a sparse or compressed file
    really occupies, and converts its subtree total once; `du --apparent-size` is the
    closest GNU mode to this one. Directory inodes contribute nothing to their own size,
    as with `du --apparent-size`. -m keeps du's total-conversion rule because a per-file
    megabyte would count every small file as a whole one.

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
    bytes: u64,
    units: u64,
    mtime: SystemTime,
}

/// One directory: its own files, its subdirectories, and its totals.
struct Node {
    parent: Option<usize>,
    name: String,
    depth: usize,
    /// Entries of this directory in listing order, for the deterministic print pass.
    kids: Vec<Kid>,
    /// Bytes and units of this directory's own files (and, with `--inodes`, itself).
    own_bytes: u64,
    own_units: u64,
    /// Totals of the subdirectories that have already finished.
    sub_bytes: u64,
    sub_units: u64,
    /// Subdirectories created and not yet rolled up into `sub_*`.
    pending: usize,
    /// The listing is finished, so `own_*` are final.
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

/// How sizes are printed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scale {
    /// Blocks of `block` bytes, rounded up per file.
    Blocks,
    /// Bytes, scaled to powers of `base` with `K`/`M`/... suffixes.
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
    block: u64,
    /// Round each file up into its block; false only for `-m` (subtree totals).
    per_file: bool,
    threshold: Option<i64>,
    one_filesystem: bool,
    excludes: Vec<Glob>,
    time: Option<TimeStyle>,
    stats: bool,
}

impl Options {
    /// Output units for one file of `bytes`: one block at least, rounded up. Only at
    /// 1 KiB and below, where du's own block counts come out the same way; `-m` converts
    /// a subtree's byte total once, because a per-file megabyte would count every small
    /// file as a whole one.
    fn units(&self, bytes: u64) -> u64 {
        match self.scale {
            Scale::Inodes => 1,
            _ if self.per_file => bytes.div_ceil(self.block),
            _ => 0,
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

    /// The printed form of one entry's total.
    fn size_text(&self, bytes: u64, units: u64) -> String {
        match self.scale {
            Scale::Human(base) => human(bytes, base),
            Scale::Blocks if !self.per_file => bytes.div_ceil(self.block).to_string(),
            _ => units.to_string(),
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
    fn total(&self) -> (u64, u64) {
        let n = &self.nodes[0];
        (n.own_bytes + n.sub_bytes, n.own_units + n.sub_units)
    }

    /// The printed size of a directory: its own size only under `-S`.
    fn dir_value(&self, n: &Node) -> (u64, u64) {
        if self.o.separate {
            (n.own_bytes, n.own_units)
        } else {
            (n.own_bytes + n.sub_bytes, n.own_units + n.sub_units)
        }
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
                    if depth < self.o.max_depth && self.o.wanted(f.bytes) {
                        let p = join(path, &f.name);
                        line(self.o, out, f.bytes, f.units, &p, f.mtime)?;
                    }
                }
            }
        }
        let (bytes, units) = self.dir_value(n);
        if depth <= self.o.max_depth && self.o.wanted(bytes) {
            line(self.o, out, bytes, units, path, n.mtime)?;
        }
        Ok(())
    }
}

/// One output line: the size, the time under `--time`, then the path.
fn line(
    o: &Options,
    out: &mut impl Write,
    bytes: u64,
    units: u64,
    path: &str,
    mtime: SystemTime,
) -> io::Result<()> {
    let size = o.size_text(bytes, units);
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
        let (own_bytes, own_units, sub_bytes, sub_units, parent) = {
            let n = &mut st.nodes[idx];
            if !n.listed || n.pending != 0 || n.done {
                return;
            }
            n.done = true;
            (n.own_bytes, n.own_units, n.sub_bytes, n.sub_units, n.parent)
        };
        let Some(p) = parent else { return };
        let pn = &mut st.nodes[p];
        pn.sub_bytes += own_bytes + sub_bytes;
        pn.sub_units += own_units + sub_units;
        pn.pending -= 1;
        idx = p;
    }
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
    let inode = o.scale == Scale::Inodes;
    let mut pending: Vec<Pending> = Vec::with_capacity(ents.len());
    let mut files = 0u64;
    let mut dirs = 0u64;
    let mut dupes = 0u64;
    let mut dupe_bytes = 0u64;
    let mut bytes = u64::from(inode);
    let mut units = u64::from(inode);
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
        bytes += e.size;
        units += o.units(e.size);
        if o.all {
            pending.push(Pending::File(FileRec {
                name: display(&e.name),
                bytes: e.size,
                units: o.units(e.size),
                mtime: e.modified(),
            }));
        }
    }

    let mut work = Vec::new();
    let mut st = c.state.lock().unwrap_or_else(|e| e.into_inner());
    for p in pending {
        match p {
            Pending::Dir { name, path, mtime } => {
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
                    own_bytes: 0,
                    own_units: 0,
                    sub_bytes: 0,
                    sub_units: 0,
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
        n.own_bytes += bytes;
        n.own_units += units;
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

/// Walk one start point and return the finished tree.
fn walk_root(o: &Options, spec: &str, mtime: SystemTime, seen: &Seen) -> Tree {
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
        own_bytes: 0,
        own_units: 0,
        sub_bytes: 0,
        sub_units: 0,
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
        TimeStyle::Iso => format!("{mo:02}-{d:02} {h:02}:{mi:02}"),
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
    let (scale, block) = if a.flag("--inodes") {
        (Scale::Inodes, 1)
    } else if a.flag("-h") || a.flag("--human-readable") {
        (Scale::Human(1024), 1)
    } else if a.flag("--si") {
        (Scale::Human(1000), 1)
    } else if a.flag("-b") || a.flag("--bytes") || a.flag("--apparent-size") {
        (Scale::Blocks, 1)
    } else if a.flag("-m") {
        (Scale::Blocks, 1 << 20)
    } else {
        (Scale::Blocks, 1 << 10)
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
        per_file: block <= 1 << 10,
        threads: match a.value("--threads") {
            Some(v) => parse_threads(v)?,
            None => crate::cli::default_threads(),
        },
        all,
        max_depth,
        total: a.flag("-c") || a.flag("--total"),
        separate: a.flag("-S") || a.flag("--separate-dirs"),
        scale,
        block,
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
    bytes: u64,
    units: u64,
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
            bytes: 0,
            units: 0,
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
        if let Err(e) = enumerate::refuse_reparse_root(path) {
            self.error(e.to_string());
            return Ok(());
        }
        let md = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                self.error(format!("cannot access '{spec}': {e}"));
                return Ok(());
            }
        };
        let mtime = md.modified().unwrap_or(UNIX_EPOCH);
        if !md.is_dir() {
            let size = md.len();
            self.files += 1;
            self.bytes += size;
            self.units += self.o.units(size);
            if self.o.wanted(size) {
                line(&self.o, out, size, self.o.units(size), spec, mtime)?;
            }
            return Ok(());
        }
        let tree = walk_root(&self.o, spec, mtime, &self.seen);
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
        let (bytes, units) = pass.total();
        self.bytes += bytes;
        self.units += units;
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
    if run.o.total && run.o.wanted(run.bytes) {
        line(&run.o, &mut out, run.bytes, run.units, "total", UNIX_EPOCH).map_err(Fail::output)?;
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

const VALUE_FLAGS: &[&str] = &[
    "--max-depth",
    "-d",
    "--threshold",
    "-t",
    "--exclude",
    "--exclude-from",
    "--time-style",
    "--threads",
];

const BOOL_FLAGS: &[&str] = &[
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

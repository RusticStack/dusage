//! Scanner engine behind `dusage`. Owned by wave-2 lane `scan`.
//!
//! Every directory is a work item tagged with the index of the top-level entry it
//! belongs to. A worker accumulates one directory locally and merges it into the
//! per-top aggregate under one lock, so there is no lock per entry.

use crate::cli::default_threads;
use crate::model::{BigFile, ErrorCounts, ScanResult, Source, TopEntry, Totals};
use crate::native::enumerate::{self, DirReader, Entry};
use crate::native::watch::ChangeSource;
use crate::queue::par_for_each;
#[cfg(windows)]
use crate::win::mft;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
#[cfg(windows)]
use std::path::Prefix;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{fs, io};

#[cfg(windows)]
const MFT_INDEX_MASK: u64 = 0xFFFF_FFFF_FFFF;
#[cfg(windows)]
const MFT_ROOT_RECORD: u64 = 5;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Batched, falling back to `ReadDir` when the filesystem rejects it.
    #[default]
    Auto,
    ReadDir,
    Batched,
    /// Raw MFT read; needs admin and a local NTFS drive letter.
    Mft,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub root: PathBuf,
    pub threads: usize,
    /// Individual files at least this large are reported.
    pub min_big: u64,
    pub top_n: usize,
    pub backend: Backend,
    /// Count each file id once (Batched only). A link whose other name lies outside
    /// the root is still counted once.
    pub dedup_hardlinks: bool,
    /// Sum named alternate data streams into `stream_bytes` (NTFS only).
    pub streams: bool,
    pub by_ext: bool,
    /// Do not descend into a directory on another filesystem (a mount point).
    pub one_filesystem: bool,
}

impl ScanOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ScanOptions {
            root: root.into(),
            threads: default_threads(),
            min_big: 1 << 30,
            top_n: 60,
            backend: Backend::Auto,
            dedup_hardlinks: false,
            streams: false,
            by_ext: false,
            one_filesystem: false,
        }
    }
}

/// Everything one top-level entry contributes; results are assembled from these.
#[derive(Default)]
struct TopData {
    name: OsString,
    is_dir: bool,
    totals: Totals,
    big: Vec<BigFile>,
    ext: HashMap<String, Totals>,
    skipped: u64,
    mounts: u64,
    errors: ErrorCounts,
    dupes: u64,
    dupe_bytes: u64,
    streams: u64,
}

fn trim_big(v: &mut Vec<BigFile>, n: usize) {
    v.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    v.truncate(n);
}

impl TopData {
    fn absorb(&mut self, o: TopData, top_n: usize) {
        self.totals.add(&o.totals);
        self.big.extend(o.big);
        if self.big.len() > top_n * 2 + 64 {
            trim_big(&mut self.big, top_n);
        }
        for (k, v) in o.ext {
            self.ext.entry(k).or_default().add(&v);
        }
        self.skipped += o.skipped;
        self.mounts += o.mounts;
        self.errors.add(&o.errors);
        self.dupes += o.dupes;
        self.dupe_bytes += o.dupe_bytes;
        self.streams += o.streams;
    }
}

struct Work {
    path: PathBuf,
    top: usize,
}

/// Sharded set of seen file ids.
struct Dedup(Vec<Mutex<HashSet<u128>>>);

impl Dedup {
    const SHARDS: usize = 64;

    fn new() -> Self {
        Dedup((0..Self::SHARDS).map(|_| Mutex::default()).collect())
    }

    /// True the first time `id` is seen. Id 0 (unknown) is never a duplicate.
    fn first_seen(&self, id: u128) -> bool {
        if id == 0 {
            return true;
        }
        let h = ((id as u64) ^ ((id >> 64) as u64)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let shard = &self.0[(h >> 58) as usize % Self::SHARDS];
        shard.lock().unwrap_or_else(|e| e.into_inner()).insert(id)
    }
}

/// Lowercase text after the last dot of a file name, `""` when there is none.
fn ext_of(name: &OsStr) -> String {
    let n = name.to_string_lossy();
    n.rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default()
}

struct FileInfo<'a> {
    name: &'a OsStr,
    size: u64,
    alloc: u64,
    id: u128,
    /// Eligible for the (opening) alternate-stream probe.
    plain: bool,
}

fn count_file(o: &ScanOptions, dedup: Option<&Dedup>, acc: &mut TopData, dir: &Path, f: &FileInfo) {
    if let Some(d) = dedup {
        if !d.first_seen(f.id) {
            acc.dupes += 1;
            acc.dupe_bytes += f.size;
            return;
        }
    }
    acc.totals.bytes += f.size;
    acc.totals.alloc += f.alloc;
    acc.totals.files += 1;
    let big = f.size >= o.min_big;
    let streams = o.streams && f.plain;
    if big || streams {
        let path = dir.join(f.name);
        if streams {
            match enumerate::named_stream_bytes(&path) {
                Ok(n) => acc.streams += n,
                Err(e) => {
                    acc.errors.record(&e);
                }
            }
        }
        if big {
            acc.big.push(BigFile {
                path: path.to_string_lossy().into_owned(),
                bytes: f.size,
                alloc: f.alloc,
            });
        }
    }
    if o.by_ext {
        let t = acc.ext.entry(ext_of(f.name)).or_default();
        t.bytes += f.size;
        t.alloc += f.alloc;
        t.files += 1;
    }
}

thread_local! {
    static READER: RefCell<DirReader> = RefCell::new(DirReader::new());
}

fn list_dir(
    src: Source,
    dir: &Path,
    out: &mut Vec<Entry>,
    errs: &mut ErrorCounts,
) -> io::Result<()> {
    match src {
        Source::Batched => READER.with(|r| r.borrow_mut().read(dir, out, errs)),
        _ => enumerate::read_plain(dir, out, errs),
    }
}

/// Classify one listed entry: a directory to descend into, a file to count, or a link
/// that is never followed.
enum Kind {
    Dir,
    File,
    Reparse,
}

fn kind_of(e: &Entry) -> Kind {
    if enumerate::is_skipped(e) {
        Kind::Reparse
    } else if e.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    }
}

fn file_info(e: &Entry) -> FileInfo<'_> {
    FileInfo {
        name: &e.name,
        size: e.size,
        alloc: e.alloc,
        id: e.file_id,
        plain: !e.is_reparse(),
    }
}

struct Scanned {
    /// Index 0 is the misc bucket (root-level skips/errors, empty name).
    tops: Vec<TopData>,
    src: Source,
    /// Lowercased names of all root entries (used by `watch` to drop vanished tops).
    listed: HashSet<String>,
}

fn key(name: &OsStr) -> String {
    name.to_string_lossy().to_lowercase()
}

struct Ctx<'a> {
    o: &'a ScanOptions,
    src: Source,
    dedup: Option<Dedup>,
    /// Device of the scan root, when `--one-filesystem` restricts the walk.
    root_dev: Option<u64>,
    agg: Mutex<Vec<TopData>>,
}

/// The device to stay on for `--one-filesystem`, or `None` when mounts are followed.
fn root_dev(o: &ScanOptions) -> Option<u64> {
    o.one_filesystem
        .then(|| enumerate::dev_of(&o.root).ok())
        .flatten()
}

fn walk(c: &Ctx, w: &Work, push: &dyn Fn(Work)) {
    let mut local = TopData::default();
    local.totals.dirs = 1;
    let mut ents = Vec::new();
    if let Err(e) = list_dir(c.src, &w.path, &mut ents, &mut local.errors) {
        local.errors.record(&e);
    }
    for e in &ents {
        match kind_of(e) {
            Kind::Dir
                if c.root_dev
                    .is_some_and(|d| enumerate::other_filesystem(e, d)) =>
            {
                local.mounts += 1;
            }
            Kind::Dir => push(Work {
                path: w.path.join(&e.name),
                top: w.top,
            }),
            Kind::File => count_file(c.o, c.dedup.as_ref(), &mut local, &w.path, &file_info(e)),
            Kind::Reparse => local.skipped += 1,
        }
    }
    let mut agg = c.agg.lock().unwrap_or_else(|e| e.into_inner());
    agg[w.top].absorb(local, c.o.top_n);
}

fn check_root(o: &ScanOptions) -> io::Result<()> {
    enumerate::refuse_reparse_root(&o.root)?;
    if fs::metadata(&o.root)?.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", o.root.display()),
        ))
    }
}

/// Read the root, create one `TopData` per entry and the first work items. With
/// `only`, directories not named there are listed but not scanned.
fn seed(
    o: &ScanOptions,
    dedup: Option<&Dedup>,
    only: Option<&HashSet<String>>,
) -> io::Result<(Scanned, Vec<Work>)> {
    let mut src = if o.backend == Backend::ReadDir {
        Source::ReadDir
    } else {
        Source::Batched
    };
    let mut misc = TopData::default();
    let mut ents = Vec::new();
    if let Err(e) = list_dir(src, &o.root, &mut ents, &mut misc.errors) {
        let unsupported = enumerate::is_unsupported(&e);
        if o.backend != Backend::Auto || !unsupported {
            return Err(e);
        }
        src = Source::ReadDir;
        ents.clear();
        list_dir(src, &o.root, &mut ents, &mut misc.errors)?;
    }
    let dev = root_dev(o);
    let mut tops = vec![misc];
    let mut work = Vec::new();
    let mut listed = HashSet::new();
    for e in &ents {
        let k = key(&e.name);
        let kind = kind_of(e);
        listed.insert(k.clone());
        let is_dir = matches!(kind, Kind::Dir);
        if matches!(kind, Kind::Reparse) {
            tops[0].skipped += 1;
            continue;
        }
        if is_dir && dev.is_some_and(|d| enumerate::other_filesystem(e, d)) {
            tops[0].mounts += 1;
            continue;
        }
        if is_dir && only.is_some_and(|s| !s.contains(&k)) {
            continue;
        }
        let mut t = TopData {
            name: e.name.clone(),
            is_dir,
            ..Default::default()
        };
        if is_dir {
            work.push(Work {
                path: o.root.join(&e.name),
                top: tops.len(),
            });
        } else {
            count_file(o, dedup, &mut t, &o.root, &file_info(e));
        }
        tops.push(t);
    }
    Ok((Scanned { tops, src, listed }, work))
}

fn run(o: &ScanOptions, only: Option<&HashSet<String>>) -> io::Result<Scanned> {
    check_root(o)?;
    if o.backend == Backend::Mft {
        return run_mft(o);
    }
    let dedup = o.dedup_hardlinks.then(Dedup::new);
    let (s, work) = seed(o, dedup.as_ref(), only)?;
    let ctx = Ctx {
        o,
        src: s.src,
        dedup,
        root_dev: root_dev(o),
        agg: Mutex::new(s.tops),
    };
    par_for_each(work, o.threads, |w, push| walk(&ctx, w, push));
    Ok(Scanned {
        tops: ctx.agg.into_inner().unwrap_or_else(|e| e.into_inner()),
        src: s.src,
        listed: s.listed,
    })
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

#[cfg(windows)]
fn run_mft(o: &ScanOptions) -> io::Result<Scanned> {
    let abs = std::path::absolute(&o.root)?;
    let letter = match abs.components().next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => d as char,
            _ => return Err(invalid("--backend mft needs a local drive-letter path")),
        },
        _ => return Err(invalid("--backend mft needs a local drive-letter path")),
    };
    let root_idx = if abs.components().count() <= 2 {
        MFT_ROOT_RECORD
    } else {
        enumerate::file_id_of(&abs)? as u64 & MFT_INDEX_MASK
    };
    let mut ents: Vec<mft::MftEntry> = Vec::new();
    mft::scan_volume(letter, &mut |e| ents.push(e)).map_err(|e| {
        if enumerate::is_denied(&e) {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "--backend mft needs an elevated (admin) shell",
            )
        } else {
            e
        }
    })?;
    let approx = ents.iter().filter(|e| e.approx).count();
    if approx > 0 {
        eprintln!(
            "dusage: note: {approx} MFT records have approximate sizes (attribute lists); counted as read"
        );
    }
    // One link per (parent, name): a file with several hardlinks appears under every
    // directory that names it, like a directory walk sees it; --dedup-hardlinks then
    // counts it once, keyed by its record number.
    struct Link<'a> {
        parent: u64,
        name: &'a OsStr,
        e: &'a mft::MftEntry,
    }
    let mut links: Vec<Link> = Vec::with_capacity(ents.len());
    for e in &ents {
        links.push(Link {
            parent: e.parent & MFT_INDEX_MASK,
            name: &e.name,
            e,
        });
        for (p, n) in &e.other_names {
            links.push(Link {
                parent: p & MFT_INDEX_MASK,
                name: n,
                e,
            });
        }
    }
    links.sort_unstable_by_key(|l| l.parent);
    let children = |r: u64| -> &[Link] {
        let lo = links.partition_point(|l| l.parent < r);
        let hi = links.partition_point(|l| l.parent <= r);
        &links[lo..hi]
    };
    let dedup = o.dedup_hardlinks.then(Dedup::new);
    let mut tops = vec![TopData::default()];
    let mut listed = HashSet::new();
    let mut stack: Vec<(u64, PathBuf, usize)> = Vec::new();
    let mut visit =
        |tops: &mut Vec<TopData>, stack: &mut Vec<_>, dir: &Path, top: Option<usize>, l: &Link| {
            let e = l.e;
            if e.frn & MFT_INDEX_MASK == l.parent {
                return; // the root record is its own parent
            }
            let t = top.unwrap_or(tops.len());
            if e.is_dir && e.reparse {
                tops[0].skipped += 1;
                return;
            }
            if top.is_none() {
                listed.insert(key(l.name));
                tops.push(TopData {
                    name: l.name.to_os_string(),
                    is_dir: e.is_dir,
                    ..Default::default()
                });
            }
            if e.is_dir {
                stack.push((e.frn & MFT_INDEX_MASK, dir.join(l.name), t));
            } else {
                let id = if e.other_names.is_empty() {
                    0 // single link: nothing to dedup
                } else {
                    (e.frn & MFT_INDEX_MASK) as u128 + 1
                };
                let info = FileInfo {
                    name: l.name,
                    size: e.size,
                    alloc: e.alloc,
                    id,
                    plain: false,
                };
                count_file(o, dedup.as_ref(), &mut tops[t], dir, &info);
            }
        };
    for e in children(root_idx) {
        visit(&mut tops, &mut stack, &o.root, None, e);
    }
    while let Some((r, path, t)) = stack.pop() {
        tops[t].totals.dirs += 1;
        for e in children(r) {
            visit(&mut tops, &mut stack, &path, Some(t), e);
        }
    }
    Ok(Scanned {
        tops,
        src: Source::Mft,
        listed,
    })
}

#[cfg(not(windows))]
fn run_mft(_o: &ScanOptions) -> io::Result<Scanned> {
    Err(invalid(
        "--backend mft reads the NTFS master file table and needs Windows; \
         use --backend auto or batched on this platform",
    ))
}

fn assemble(o: &ScanOptions, tops: &[TopData], src: Source, elapsed: Duration) -> ScanResult {
    let mut r = ScanResult {
        root: o.root.to_string_lossy().into_owned(),
        elapsed,
        threads: o.threads,
        source: src,
        ..Default::default()
    };
    let mut ext: HashMap<&str, Totals> = HashMap::new();
    for t in tops {
        if !t.name.is_empty() {
            r.tops.push(TopEntry {
                name: t.name.to_string_lossy().into_owned(),
                is_dir: t.is_dir,
                totals: t.totals,
            });
            r.total.add(&t.totals);
        }
        r.big_files.extend(t.big.iter().cloned());
        for (k, v) in &t.ext {
            ext.entry(k).or_default().add(v);
        }
        r.skipped_reparse += t.skipped;
        r.skipped_mounts += t.mounts;
        r.errors.add(&t.errors);
        r.hardlink_dupes += t.dupes;
        r.hardlink_dupe_bytes += t.dupe_bytes;
        r.stream_bytes += t.streams;
    }
    r.tops.sort_by(|a, b| {
        b.totals
            .bytes
            .cmp(&a.totals.bytes)
            .then_with(|| a.name.cmp(&b.name))
    });
    trim_big(&mut r.big_files, o.top_n);
    r.by_ext = ext.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    r.by_ext
        .sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then_with(|| a.0.cmp(&b.0)));
    r
}

/// Scan `o.root`. Errors only if the root itself is missing, unreadable or not a
/// directory; everything below it is counted in `ScanResult::errors` instead.
pub fn scan(o: &ScanOptions) -> io::Result<ScanResult> {
    let t0 = Instant::now();
    let s = run(o, None)?;
    Ok(assemble(o, &s.tops, s.src, t0.elapsed()))
}

/// Comparable form of one path component: verbatim and plain drive/UNC prefixes map
/// to the same key, and components compare case-insensitively on Windows.
fn comp_key(c: Component<'_>) -> String {
    match c {
        #[cfg(windows)]
        Component::Prefix(p) => {
            use std::path::Prefix;
            let lower = |s: &OsStr| s.to_string_lossy().to_ascii_lowercase();
            match p.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                    format!("{}:", (d as char).to_ascii_lowercase())
                }
                Prefix::UNC(s, sh) | Prefix::VerbatimUNC(s, sh) => {
                    format!("\\\\{}\\{}", lower(s), lower(sh))
                }
                Prefix::Verbatim(s) | Prefix::DeviceNS(s) => lower(s),
            }
        }
        #[cfg(not(windows))]
        Component::Prefix(_) => String::new(),
        Component::RootDir => "/".to_string(),
        Component::CurDir => String::new(),
        Component::ParentDir => "..".to_string(),
        Component::Normal(s) => {
            #[cfg(windows)]
            {
                s.to_string_lossy().to_ascii_lowercase()
            }
            #[cfg(not(windows))]
            {
                s.to_string_lossy().into_owned()
            }
        }
    }
}

fn comps(p: &Path) -> Vec<Component<'_>> {
    p.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect()
}

/// Which top-level entry of `root` does a change to `name` inside directory
/// `parent` affect? `None` when it lies outside `root` or is `root`'s own parent chain.
fn top_for(root: &Path, parent: &Path, name: &OsStr) -> Option<String> {
    let r = comps(root);
    let p = comps(parent);
    if p.len() < r.len() || r.iter().zip(&p).any(|(a, b)| comp_key(*a) != comp_key(*b)) {
        return None;
    }
    if p.len() == r.len() {
        return Some(name.to_string_lossy().into_owned());
    }
    match p[r.len()] {
        Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
        _ => None,
    }
}

/// Hardlink dedup state is per `run`, so a partial rescan would count a link whose other name
/// sits in a kept top a second time: with `--dedup-hardlinks` every rescan is a full one.
fn rescan_scope(
    dedup_hardlinks: bool,
    only: Option<HashSet<String>>,
    what: String,
) -> (Option<HashSet<String>>, String) {
    match only {
        Some(_) if dedup_hardlinks => (None, format!("{what} (full rescan: --dedup-hardlinks)")),
        other => (other, what),
    }
}

/// The top-level entries (lowercase key -> display name) touched by `changed` paths.
fn changed_tops(root: &Path, changed: &[PathBuf]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for p in changed {
        let Some(name) = p.file_name() else { continue };
        let Some(parent) = p.parent() else { continue };
        if let Some(t) = top_for(root, parent, name) {
            out.insert(t.to_lowercase(), t);
        }
    }
    out
}

/// Scan, then rescan every `interval`, reporting through `on_update` (return false
/// to stop). The platform change feed (the NTFS USN journal on Windows, inotify on
/// Linux) says which top-level entries changed and only those are rescanned; when it
/// is unavailable every tick is a full rescan. Read-only: the feed is only read.
pub fn watch(
    o: &ScanOptions,
    interval: Duration,
    on_update: &mut dyn FnMut(&ScanResult, &str) -> bool,
) -> io::Result<()> {
    let root = crate::native::watch::resolve_root(&std::path::absolute(&o.root)?);
    // Open the change feed before scanning so changes made during the scan are not lost.
    let mut changes = ChangeSource::open(&root);
    let t0 = Instant::now();
    let first = run(o, None)?;
    let mut o2 = o.clone();
    o2.backend = match first.src {
        Source::ReadDir => Backend::ReadDir,
        Source::Batched => Backend::Batched,
        Source::Mft => Backend::Mft,
    };
    let src = first.src;
    let mut tops = first.tops;
    if !on_update(&assemble(&o2, &tops, src, t0.elapsed()), "initial scan") {
        return Ok(());
    }
    loop {
        let (only, what) = match changes.as_mut().map(|c| c.poll(interval)) {
            Some(Ok(changed)) if changed.is_empty() => continue,
            Some(Ok(changed)) => {
                let set = changed_tops(&root, &changed);
                if set.is_empty() {
                    continue;
                }
                let mut names: Vec<&str> = set.values().map(String::as_str).collect();
                names.sort_unstable();
                let what = format!("changed: {}", names.join(", "));
                if src == Source::Mft {
                    (None, format!("{what} (full MFT rescan)"))
                } else {
                    (Some(set.keys().cloned().collect()), what)
                }
            }
            Some(Err(_)) => {
                changes = ChangeSource::open(&root);
                (None, "change journal unavailable; full rescan".to_string())
            }
            None => {
                changes = ChangeSource::open(&root);
                (None, "change journal unavailable; full rescan".to_string())
            }
        };
        let (only, what) = rescan_scope(o2.dedup_hardlinks, only, what);
        let t = Instant::now();
        let s = run(&o2, only.as_ref())?;
        tops = match &only {
            None => s.tops,
            Some(only) => {
                let mut keep: Vec<TopData> = tops
                    .into_iter()
                    .filter(|t| {
                        let k = key(&t.name);
                        t.is_dir
                            && !t.name.is_empty()
                            && s.listed.contains(&k)
                            && !only.contains(&k)
                    })
                    .collect();
                keep.extend(s.tops);
                keep
            }
        };
        if !on_update(&assemble(&o2, &tops, src, t.elapsed()), &what) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "dusage_scan_{tag}_{}_{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Fixture(p)
        }
        fn file(&self, rel: &str, n: usize) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, vec![b'x'; n]).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir(self.0.join("junc"));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn opts(f: &Fixture, b: Backend) -> ScanOptions {
        ScanOptions {
            threads: 4,
            min_big: 1000,
            backend: b,
            by_ext: true,
            ..ScanOptions::new(&f.0)
        }
    }

    fn build(f: &Fixture) {
        f.file("a/one.TXT", 1000);
        f.file("a/deep/two.bin", 5000);
        f.file("b/three.txt", 200);
        f.file("root.dat", 70);
        f.file("noext", 3);
    }

    /// Create `link` as a directory link to `target` (junction on Windows, symlink on
    /// Linux). Returns false when the platform refuses.
    fn link_dir(link: &Path, target: &Path) -> bool {
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        #[cfg(target_os = "linux")]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }

    fn top<'a>(r: &'a ScanResult, n: &str) -> &'a TopEntry {
        r.tops.iter().find(|t| t.name == n).unwrap()
    }

    #[test]
    fn ext_names() {
        assert_eq!(ext_of(OsStr::new("A.TXT")), "txt");
        assert_eq!(ext_of(OsStr::new("a.tar.GZ")), "gz");
        assert_eq!(ext_of(OsStr::new("noext")), "");
        assert_eq!(ext_of(OsStr::new("trail.")), "");
        assert_eq!(ext_of(OsStr::new(".gitignore")), "gitignore");
    }

    #[cfg(windows)]
    #[test]
    fn top_mapping() {
        let root = Path::new(r"C:\data");
        let t = |p: &str, n: &str| top_for(root, Path::new(p), OsStr::new(n));
        assert_eq!(t(r"C:\data", "Foo"), Some("Foo".into()));
        assert_eq!(t(r"\\?\C:\DATA", "x"), Some("x".into()));
        assert_eq!(t(r"C:\data\sub", "f"), Some("sub".into()));
        assert_eq!(t(r"\\?\c:\Data\sub\deep\er", "f"), Some("sub".into()));
        assert_eq!(t(r"C:\database", "f"), None);
        assert_eq!(t(r"C:\other\data", "f"), None);
        assert_eq!(t(r"C:\", "f"), None);
        let vol = Path::new(r"C:\");
        assert_eq!(
            top_for(vol, Path::new(r"C:\"), OsStr::new("x")),
            Some("x".into())
        );
        assert_eq!(
            top_for(vol, Path::new(r"C:\Users\me"), OsStr::new("x")),
            Some("Users".into())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn top_mapping() {
        let root = Path::new("/data");
        let t = |p: &str| top_for(root, Path::new(p), OsStr::new("f"));
        assert_eq!(t("/data"), Some("f".into()));
        assert_eq!(t("/data/./"), Some("f".into()));
        assert_eq!(t("/data/sub"), Some("sub".into()));
        assert_eq!(t("/data/sub/deep/er"), Some("sub".into()));
        assert_eq!(t("/database"), None);
        assert_eq!(t("/other/data"), None);
        assert_eq!(t("/"), None);
        assert_eq!(
            top_for(Path::new("/"), Path::new("/"), OsStr::new("x")),
            Some("x".into())
        );
        assert_eq!(
            top_for(Path::new("/"), Path::new("/home/me"), OsStr::new("x")),
            Some("home".into())
        );
    }

    #[test]
    fn dedup_hardlinks_forces_full_rescan() {
        let set: HashSet<String> = ["b".to_string()].into();
        let (only, what) = rescan_scope(true, Some(set.clone()), "changed: b".into());
        assert!(only.is_none() && what.contains("full rescan"), "{what}");
        let (only, what) = rescan_scope(false, Some(set.clone()), "changed: b".into());
        assert_eq!(only, Some(set));
        assert_eq!(what, "changed: b");
        assert!(rescan_scope(true, None, "x".into()).0.is_none());
    }

    #[test]
    fn change_feed_resolves_junction_parent() {
        let f = Fixture::new("jroot");
        f.file("real/sub/x.bin", 10);
        if !link_dir(&f.0.join("junc"), &f.0.join("real")) {
            eprintln!("skipped: could not create link");
            return;
        }
        let via_link = f.0.join("junc").join("sub");
        let jr = crate::native::watch::resolve_root(&via_link);
        // A journal record for the real path must map to a top of the resolved root.
        let real_parent = fs::canonicalize(f.0.join("real").join("sub")).unwrap();
        assert_eq!(
            top_for(&jr, &real_parent, OsStr::new("x.bin")),
            Some("x.bin".into())
        );
        assert_eq!(top_for(&via_link, &real_parent, OsStr::new("x.bin")), None);
    }

    #[test]
    fn root_reparse_point_refused() {
        let f = Fixture::new("rroot");
        f.file("real/a.bin", 5000);
        if !link_dir(&f.0.join("junc"), &f.0.join("real")) {
            eprintln!("skipped: could not create link");
            return;
        }
        for b in [Backend::Auto, Backend::ReadDir, Backend::Batched] {
            let o = ScanOptions {
                backend: b,
                ..ScanOptions::new(f.0.join("junc"))
            };
            let e = scan(&o).unwrap_err();
            assert!(e.to_string().contains("reparse point"), "{b:?}: {e}");
        }
        // The real directory still scans.
        let r = scan(&opts(&f, Backend::Auto)).unwrap();
        assert_eq!(r.total.bytes, 5000);
    }

    #[test]
    fn dedup_shards() {
        let d = Dedup::new();
        assert!(d.first_seen(7));
        assert!(!d.first_seen(7));
        assert!(d.first_seen(7 << 70));
        assert!(d.first_seen(0));
        assert!(d.first_seen(0));
    }

    #[test]
    fn backends_agree() {
        let f = Fixture::new("agree");
        build(&f);
        for b in [Backend::ReadDir, Backend::Batched, Backend::Auto] {
            let r = scan(&opts(&f, b)).unwrap();
            assert_eq!(r.total.bytes, 1000 + 5000 + 200 + 70 + 3, "{b:?}");
            assert_eq!(r.total.files, 5);
            assert_eq!(top(&r, "a").totals.bytes, 6000);
            assert_eq!(top(&r, "a").totals.dirs, 2);
            assert!(top(&r, "a").is_dir);
            assert!(!top(&r, "root.dat").is_dir);
            assert_eq!(r.tops[0].name, "a");
            let paths: Vec<_> = r.big_files.iter().map(|b| b.bytes).collect();
            assert_eq!(paths, [5000, 1000]);
            let ext: HashMap<_, _> = r.by_ext.iter().cloned().collect();
            assert_eq!(ext["txt"].bytes, 1200);
            assert_eq!(ext[""].bytes, 3);
            assert_eq!(r.by_ext[0].0, "bin");
            if b == Backend::ReadDir {
                assert_eq!(r.source, Source::ReadDir);
                assert_eq!(r.total.alloc, 0);
            } else {
                assert_eq!(r.source, Source::Batched);
                assert!(r.total.alloc >= r.total.bytes);
            }
        }
    }

    #[test]
    fn junction_not_followed() {
        let f = Fixture::new("junc");
        build(&f);
        let target = Fixture::new("junc_target");
        target.file("huge.bin", 100_000);
        assert!(
            link_dir(&f.0.join("junc"), &target.0),
            "link creation failed"
        );
        for b in [Backend::ReadDir, Backend::Batched] {
            let r = scan(&opts(&f, b)).unwrap();
            assert_eq!(r.total.bytes, 6273, "{b:?}");
            assert_eq!(r.skipped_reparse, 1);
            assert!(r.tops.iter().all(|t| t.name != "junc"));
        }
    }

    #[test]
    fn hardlinks_dedup() {
        let f = Fixture::new("hl");
        f.file("a/orig.bin", 4000);
        fs::hard_link(f.0.join("a/orig.bin"), f.0.join("link.bin")).unwrap();
        let mut o = opts(&f, Backend::Batched);
        let plain = scan(&o).unwrap();
        assert_eq!(plain.total.bytes, 8000);
        assert_eq!(plain.hardlink_dupes, 0);
        o.dedup_hardlinks = true;
        let r = scan(&o).unwrap();
        assert_eq!(r.total.bytes, 4000);
        assert_eq!(r.hardlink_dupes, 1);
        assert_eq!(r.hardlink_dupe_bytes, 4000);
    }

    #[cfg(windows)]
    #[test]
    fn streams_counted() {
        let f = Fixture::new("ads");
        f.file("a.txt", 10);
        if fs::write(f.0.join("a.txt:side"), vec![0u8; 321]).is_err() {
            return; // not NTFS
        }
        let mut o = opts(&f, Backend::Batched);
        o.streams = true;
        let r = scan(&o).unwrap();
        assert_eq!(r.stream_bytes, 321);
        assert_eq!(r.total.bytes, 10);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn streams_are_zero_on_linux() {
        let f = Fixture::new("ads");
        f.file("a.txt", 10);
        let mut o = opts(&f, Backend::Batched);
        o.streams = true;
        let r = scan(&o).unwrap();
        assert_eq!(r.stream_bytes, 0);
        assert_eq!(r.total.bytes, 10);
    }

    #[test]
    fn bad_roots() {
        let f = Fixture::new("bad");
        f.file("x", 1);
        assert!(scan(&ScanOptions::new(f.0.join("missing"))).is_err());
        assert!(scan(&ScanOptions::new(f.0.join("x"))).is_err());
    }

    #[test]
    fn top_n_truncates() {
        let f = Fixture::new("topn");
        for i in 0..5 {
            f.file(&format!("d/f{i}"), 2000 + i);
        }
        let mut o = opts(&f, Backend::Batched);
        o.top_n = 2;
        let r = scan(&o).unwrap();
        assert_eq!(r.big_files.len(), 2);
        assert_eq!(r.big_files[0].bytes, 2004);
    }

    #[test]
    fn watch_sees_new_file() {
        let f = Fixture::new("watch");
        build(&f);
        let o = opts(&f, Backend::Batched);
        let mut seen = Vec::new();
        watch(&o, Duration::from_millis(20), &mut |r, what| {
            seen.push((r.total.bytes, what.to_string()));
            if seen.len() == 1 {
                f.file("b/new.bin", 100);
            }
            seen.len() < 2
        })
        .unwrap();
        assert_eq!(seen[0].1, "initial scan");
        assert_eq!(seen[1].0, seen[0].0 + 100, "{:?}", seen[1].1);
    }
}

//! Directory enumeration for Linux, in two layers.
//!
//! [`list`] is the primitive: one `getdents64` batch sequence through `std::fs::read_dir`,
//! a name copy per entry, and a `dirent` type that costs nothing when the filesystem fills
//! `d_type` (std falls back to one `lstat` only on filesystems that leave it `DT_UNKNOWN`).
//! Anything that needs the inode — size, allocation, mtime, device, file id — is
//! [`Lite::stat`], one `lstat` on demand, cached in the entry. A walk that only reads names
//! and types therefore never stats.
//!
//! [`DirReader::read`] keeps the eager contract its other callers rely on (every entry
//! carries its `lstat` data, exactly as before) and is built on a single `readdir` loop.
//!
//! Symlinks take the place of Windows reparse points: they are reported and never
//! followed. Per-entry failures are recorded in the caller's `ErrorCounts` and the walk
//! continues; only an unreadable directory is returned as `Err`.

use crate::model::ErrorCounts;
use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// One directory entry: the Unix shape of `win::enumerate::Entry`.
pub struct Entry {
    pub name: OsString,
    pub is_dir: bool,
    /// A symlink (the only "do not follow" entry on Linux).
    pub is_link: bool,
    pub size: u64,
    /// Allocated bytes (`st_blocks * 512`); 0 on the `readdir` backend.
    pub alloc: u64,
    /// `dev << 64 | ino`, the same value for every hardlink to one inode; 0 when unknown.
    pub file_id: u128,
    pub dev: u64,
    modified: std::time::SystemTime,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_reparse(&self) -> bool {
        self.is_link
    }

    pub fn modified(&self) -> std::time::SystemTime {
        self.modified
    }
}

/// The `dirent` type, which is exact and free: `getdents64` reports it, so neither `list`
/// nor a walk that only needs names pays a syscall for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Dir,
    File,
    Link,
    Other,
}

impl Kind {
    fn of(t: fs::FileType) -> Kind {
        if t.is_dir() {
            Kind::Dir
        } else if t.is_symlink() {
            Kind::Link
        } else if t.is_file() {
            Kind::File
        } else {
            Kind::Other
        }
    }
}

/// What one `lstat` reports, as [`Lite::stat`] hands it over.
#[derive(Clone, Copy)]
pub struct Stat {
    pub size: u64,
    pub alloc: u64,
    pub file_id: u128,
    pub dev: u64,
    modified: SystemTime,
}

impl Stat {
    pub fn modified(&self) -> SystemTime {
        self.modified
    }
}

fn stat_of(p: &Path) -> io::Result<Stat> {
    let md = fs::symlink_metadata(p)?;
    Ok(Stat {
        size: if md.is_dir() { 0 } else { md.len() },
        alloc: md.blocks() * 512,
        file_id: (md.dev() as u128) << 64 | md.ino() as u128,
        dev: md.dev(),
        modified: md.modified().unwrap_or(UNIX_EPOCH),
    })
}

/// One name from a directory, as cheaply as the kernel gives it up: a name, a type that
/// came with the `dirent`, and an empty slot for the `lstat` data.
///
/// `stat` is the only thing that costs a syscall, and it runs once: the entry remembers the
/// answer, so a size asked for in a filter and again in the output is one `lstat`.
pub struct Lite {
    pub name: OsString,
    kind: Kind,
    stat: Cell<Option<Stat>>,
}

impl Lite {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn is_reparse(&self) -> bool {
        self.kind == Kind::Link
    }

    /// `lstat` this entry at `path`, its full path in the directory just listed. The result
    /// is cached: a second call is free. Exact, never a shortcut — the scan and prune paths
    /// decide on size and mtime from it.
    pub fn stat(&self, path: &Path) -> io::Result<Stat> {
        match self.stat.get() {
            Some(s) => Ok(s),
            None => {
                let s = stat_of(path)?;
                self.stat.set(Some(s));
                Ok(s)
            }
        }
    }
}

/// List one directory into `out`: names and types only, no `stat`.
///
/// `out` is cleared and its allocation reused, so a walk keeps one entry vector instead of
/// allocating one per directory. An entry that cannot be typed is recorded in `errs` and
/// dropped, like an entry that vanishes between `getdents` and `lstat` always was.
pub fn list(dir: &Path, out: &mut Vec<Lite>, errs: &mut ErrorCounts) -> io::Result<()> {
    out.clear();
    for e in fs::read_dir(dir)? {
        let e = match e {
            Ok(e) => e,
            Err(err) => {
                errs.record(&err);
                continue;
            }
        };
        // `file_type` is the `dirent`'s own `d_type`; std stats only when the filesystem
        // reports `DT_UNKNOWN`.
        match e.file_type() {
            Ok(t) => out.push(Lite {
                name: e.file_name(),
                kind: Kind::of(t),
                stat: Cell::new(None),
            }),
            Err(err) => {
                errs.record(&err);
            }
        }
    }
    Ok(())
}

/// True for entries that are counted as links and never descended into.
pub fn is_skipped(e: &Entry) -> bool {
    e.is_link
}

fn entry(name: OsString, md: &fs::Metadata, detail: bool) -> Entry {
    let id = if detail {
        (md.dev() as u128) << 64 | md.ino() as u128
    } else {
        0
    };
    Entry {
        name,
        is_dir: md.is_dir(),
        is_link: md.file_type().is_symlink(),
        size: if md.is_dir() { 0 } else { md.len() },
        alloc: if detail { md.blocks() * 512 } else { 0 },
        file_id: id,
        dev: if detail { md.dev() } else { 0 },
        modified: md.modified().unwrap_or(UNIX_EPOCH),
    }
}

/// List one directory with an `lstat` per entry. `detail` fills allocation size, device and
/// file id, which costs nothing extra here (one `lstat` per entry either way) but keeps the
/// `readdir` backend's documented "on-disk unknown, no file ids" behaviour.
///
/// This keeps the `DirEntry::metadata` call rather than rebuilding a path and statting it:
/// it is `fstatat(dirfd, name)`, one syscall with no path to resolve, which is what the
/// eager callers (scan, prune, delete, watch) want for every single entry.
fn read_with(
    dir: &Path,
    out: &mut Vec<Entry>,
    errs: &mut ErrorCounts,
    detail: bool,
) -> io::Result<()> {
    for e in fs::read_dir(dir)? {
        let e = match e {
            Ok(e) => e,
            Err(err) => {
                errs.record(&err);
                continue;
            }
        };
        // `DirEntry::metadata` is `lstat` on Linux: a symlink is described, not followed.
        match e.metadata() {
            Ok(md) => out.push(entry(e.file_name(), &md, detail)),
            Err(err) => {
                errs.record(&err);
            }
        }
    }
    Ok(())
}

/// Enumerator with the batched backend's contract: every entry carries its
/// allocated size and file id.
pub struct DirReader;

impl DirReader {
    pub fn new() -> Self {
        DirReader
    }

    pub fn read(
        &mut self,
        dir: &Path,
        out: &mut Vec<Entry>,
        errs: &mut ErrorCounts,
    ) -> io::Result<()> {
        read_with(dir, out, errs, true)
    }
}

impl Default for DirReader {
    fn default() -> Self {
        Self::new()
    }
}

/// The `readdir` backend: names and sizes only, no allocation sizes or file ids.
pub fn read_plain(dir: &Path, out: &mut Vec<Entry>, errs: &mut ErrorCounts) -> io::Result<()> {
    read_with(dir, out, errs, false)
}

/// Err when `path` is itself a symlink: scanning or pruning it would silently act on
/// its target instead.
pub fn refuse_reparse_root(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is a reparse point (symlink); pass its target",
                path.display()
            ),
        )),
        _ => Ok(()),
    }
}

/// Identity of one inode, stable across hardlinks: `dev << 64 | ino`.
pub fn file_id_of(path: &Path) -> io::Result<u128> {
    let md = fs::symlink_metadata(path)?;
    Ok((md.dev() as u128) << 64 | md.ino() as u128)
}

/// Unix has no named data streams. `--streams` therefore reports 0 bytes.
pub fn named_stream_bytes(_path: &Path) -> io::Result<u64> {
    Ok(0)
}

/// Device id of a directory, used to recognise mount points.
pub fn dev_of(path: &Path) -> io::Result<u64> {
    Ok(fs::metadata(path)?.dev())
}

/// True when `e` lives on a different filesystem than `root_dev` (a mount point).
pub fn other_filesystem(e: &Entry, root_dev: u64) -> bool {
    root_dev != 0 && e.dev != 0 && e.dev != root_dev
}

/// A mount point is the Unix analogue of a Windows volume root: its own device.
pub fn same_filesystem(e: &Entry, parent_dev: u64) -> bool {
    parent_dev == 0 || e.dev == 0 || e.dev == parent_dev
}

pub fn is_unsupported(e: &io::Error) -> bool {
    super::sys::is_unsupported(e)
}

pub fn is_denied(e: &io::Error) -> bool {
    super::sys::is_denied(e)
}

/// Bytes of one file name as an `OsStr` (used by the inotify watcher).
pub fn path_bytes(p: &Path) -> &[u8] {
    p.as_os_str().as_bytes()
}

pub fn os_from_bytes(b: &[u8]) -> OsString {
    OsStr::from_bytes(b).to_os_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "dusage_unix_enum_{tag}_{}_{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Fixture(p)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn names(v: &[Entry]) -> Vec<String> {
        let mut n: Vec<String> = v
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        n.sort();
        n
    }

    #[test]
    fn lists_names_sizes_and_links() {
        let f = Fixture::new("list");
        fs::create_dir(f.0.join("sub")).unwrap();
        fs::write(f.0.join("a.bin"), vec![0u8; 4096]).unwrap();
        std::os::unix::fs::symlink(f.0.join("a.bin"), f.0.join("link")).unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        DirReader::new().read(&f.0, &mut v, &mut errs).unwrap();
        assert_eq!(names(&v), ["a.bin", "link", "sub"]);
        assert_eq!(errs.total(), 0);
        let a = v.iter().find(|e| e.name == "a.bin").unwrap();
        assert_eq!(a.size, 4096);
        assert!(a.alloc >= 4096, "alloc {}", a.alloc);
        assert!(a.file_id != 0 && !a.is_dir && !a.is_link);
        assert!(a.modified().duration_since(UNIX_EPOCH).unwrap().as_secs() > 0);
        let link = v.iter().find(|e| e.name == "link").unwrap();
        assert!(link.is_link && is_skipped(link) && !link.is_dir);
        let sub = v.iter().find(|e| e.name == "sub").unwrap();
        assert!(sub.is_dir && !is_skipped(sub));
        // Both hardlink names of one inode share a file id.
        fs::hard_link(f.0.join("a.bin"), f.0.join("b.bin")).unwrap();
        let mut w = Vec::new();
        DirReader::new().read(&f.0, &mut w, &mut errs).unwrap();
        let id = |n: &str| w.iter().find(|e| e.name == n).unwrap().file_id;
        assert_eq!(id("a.bin"), id("b.bin"));
    }

    #[test]
    fn plain_backend_has_no_alloc_or_ids() {
        let f = Fixture::new("plain");
        fs::write(f.0.join("a"), b"xyz").unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        read_plain(&f.0, &mut v, &mut errs).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].size, 3);
        assert_eq!((v[0].alloc, v[0].file_id, v[0].dev), (0, 0, 0));
    }

    #[test]
    fn missing_directory_is_an_error() {
        let f = Fixture::new("missing");
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        assert!(DirReader::new()
            .read(&f.0.join("nope"), &mut v, &mut errs)
            .is_err());
    }

    #[test]
    fn link_root_refused_but_target_scans() {
        let f = Fixture::new("root");
        fs::create_dir(f.0.join("real")).unwrap();
        fs::write(f.0.join("real/x"), b"x").unwrap();
        std::os::unix::fs::symlink(f.0.join("real"), f.0.join("lnk")).unwrap();
        refuse_reparse_root(&f.0.join("real")).unwrap();
        let e = refuse_reparse_root(&f.0.join("lnk")).unwrap_err();
        assert!(e.to_string().contains("reparse point"), "{e}");
    }

    #[test]
    fn streams_are_zero_and_ids_are_128_bit() {
        let f = Fixture::new("misc");
        fs::write(f.0.join("a"), b"x").unwrap();
        assert_eq!(named_stream_bytes(&f.0.join("a")).unwrap(), 0);
        let id = file_id_of(&f.0.join("a")).unwrap();
        assert!(id >> 64 != 0, "device id must occupy the high half: {id}");
        assert_eq!(dev_of(&f.0).unwrap(), dev_of(&f.0.join("a")).unwrap());
    }

    #[test]
    fn mount_boundary_helpers() {
        let f = Fixture::new("dev");
        fs::write(f.0.join("a"), b"x").unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        DirReader::new().read(&f.0, &mut v, &mut errs).unwrap();
        let dev = dev_of(&f.0).unwrap();
        assert!(!other_filesystem(&v[0], dev));
        assert!(other_filesystem(&v[0], dev + 1));
        assert!(same_filesystem(&v[0], dev));
        assert!(!same_filesystem(&v[0], dev + 1));
        assert!(!other_filesystem(&v[0], 0), "unknown root device");
        assert!(same_filesystem(&v[0], 0), "unknown parent dev accepts");
    }

    #[test]
    fn lite_types_come_from_the_dirent() {
        let f = Fixture::new("lite");
        fs::create_dir(f.0.join("sub")).unwrap();
        fs::write(f.0.join("a.bin"), vec![0u8; 4096]).unwrap();
        std::os::unix::fs::symlink(f.0.join("a.bin"), f.0.join("link")).unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        let mut got: Vec<String> = v
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, ["a.bin", "link", "sub"]);
        assert_eq!(errs.total(), 0);
        let dir = |n: &str| v.iter().find(|e| e.name == n).unwrap().is_dir();
        assert!(dir("sub") && !dir("a.bin") && !dir("link"));
        let link = v.iter().find(|e| e.name == "link").unwrap();
        assert!(link.is_reparse());
        // `list` clears and reuses the buffer.
        list(&f.0, &mut v, &mut errs).unwrap();
        assert_eq!(v.len(), 3);
        list(&f.0.join("sub"), &mut v, &mut errs).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn lite_stat_is_exact_and_cached() {
        let f = Fixture::new("stat");
        fs::write(f.0.join("a"), vec![0u8; 5000]).unwrap();
        std::os::unix::fs::symlink(f.0.join("a"), f.0.join("lnk")).unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        for e in &v {
            let p = f.0.join(&e.name);
            let s = e.stat(&p).unwrap();
            let md = fs::symlink_metadata(&p).unwrap();
            assert_eq!(
                s.size,
                if md.is_dir() { 0 } else { md.len() },
                "{:?}",
                e.name
            );
            assert_eq!(s.alloc, md.blocks() * 512, "{:?}", e.name);
            assert_eq!(s.dev, md.dev(), "{:?}", e.name);
            assert_eq!(s.file_id, (md.dev() as u128) << 64 | md.ino() as u128);
            assert_eq!(s.modified(), md.modified().unwrap(), "{:?}", e.name);
            // Cached: the same answer, and the same copy, on a second call.
            let again = e.stat(&f.0.join("does-not-exist")).unwrap();
            assert_eq!(again.size, s.size);
        }
        // A link stats as itself: its own length, not its target's 5000 bytes.
        let lone = v.iter().find(|e| e.name == "lnk").unwrap();
        assert_ne!(lone.stat(&f.0.join("lnk")).unwrap().size, 5000);
    }

    #[test]
    fn lite_stat_reports_a_vanished_entry() {
        let f = Fixture::new("gone");
        fs::write(f.0.join("a"), b"x").unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        let gone = f.0.join("vanished");
        assert!(v[0].stat(&gone).is_err());
    }

    /// Enumeration cost over a real tree, eager versus cheap. Ignored: it needs a big
    /// directory. `DUSAGE_ENUM_BENCH=/usr cargo test --release -- --ignored --nocapture
    /// bench_enumeration`.
    #[test]
    #[ignore]
    fn bench_enumeration() {
        let root = std::env::var_os("DUSAGE_ENUM_BENCH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr"));
        let mut stack = vec![root.clone()];
        let mut dirs = Vec::new();
        while let Some(d) = stack.pop() {
            let mut ents = Vec::new();
            if list(&d, &mut ents, &mut ErrorCounts::default()).is_ok() {
                dirs.push(d.clone());
                for e in &ents {
                    if e.is_dir() {
                        stack.push(d.join(&e.name));
                    }
                }
            }
        }
        let t0 = Instant::now();
        let mut n = 0usize;
        let mut buf = Vec::new();
        for d in &dirs {
            if list(d, &mut buf, &mut ErrorCounts::default()).is_ok() {
                n += buf.len();
            }
        }
        let cheap = t0.elapsed();

        let t0 = Instant::now();
        let mut m = 0usize;
        let mut reader = DirReader::new();
        let mut out = Vec::new();
        for d in &dirs {
            out.clear();
            if reader
                .read(d, &mut out, &mut ErrorCounts::default())
                .is_ok()
            {
                m += out.len();
            }
        }
        let eager = t0.elapsed();

        let t0 = Instant::now();
        let mut k = 0usize;
        let mut st = PathBuf::with_capacity(256);
        for d in &dirs {
            if list(d, &mut buf, &mut ErrorCounts::default()).is_ok() {
                for e in &buf {
                    st.clear();
                    st.push(d);
                    st.push(&e.name);
                    if e.stat(&st).is_ok() {
                        k += 1;
                    }
                }
            }
        }
        let lazy = t0.elapsed();

        println!(
            "enum bench {}: {} dirs, {} entries",
            root.display(),
            dirs.len(),
            n
        );
        println!(
            "  list (names + types only)         {:>8.1} ms  {n} entries",
            ms(cheap)
        );
        println!(
            "  read (names + one lstat each)     {:>8.1} ms  {m} entries",
            ms(eager)
        );
        println!(
            "  list + stat every entry           {:>8.1} ms  {k} entries",
            ms(lazy)
        );
    }

    fn ms(d: std::time::Duration) -> f64 {
        d.as_secs_f64() * 1e3
    }
}

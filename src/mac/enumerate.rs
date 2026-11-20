//! Directory enumeration for macOS.
//!
//! Slice 1 of the port: names, kinds, sizes, allocation, file id and device all come
//! from `std::fs`, which on Darwin is `getdirentries64` under `read_dir` and `fstatat`
//! under `symlink_metadata`/`metadata`. The shapes are the ones `src/unix/enumerate.rs`
//! established, so `scan`, `du`, `prune`, `search` and `find` need no platform branch:
//!
//! - a symlink is the "do not follow" class, reported as `Kind::Symlink` and never
//!   descended into;
//! - `alloc` is `st_blocks * 512` and `file_id` is `dev << 64 | ino`, so
//!   `--dedup-hardlinks`, `-size` and the on-disk columns mean the same as on Linux;
//! - a directory's size is reported as 0 by the eager `Entry`, because that is its own
//!   record and not its contents, while `lstat` reports the record itself;
//! - `read_plain` leaves `alloc`, `file_id` and `dev` at zero.
//!
//! [`list`] is the names-and-kinds layer [`Lite`] walks, and [`DirReader::read`] adds the
//! `lstat`; [`Lite::stat`] defers it until asked and caches the answer. Per-entry
//! failures are recorded in the caller's `ErrorCounts` and the walk continues; only an
//! unreadable directory is returned as `Err`.

use super::sys;
use crate::model::{ErrorCounts, Kind, Stat};
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One directory entry: the macOS shape of `win::enumerate::Entry` and
/// `unix::enumerate::Entry`.
pub struct Entry {
    pub name: OsString,
    pub is_dir: bool,
    /// A symlink (the only "do not follow" entry on macOS).
    pub is_link: bool,
    pub size: u64,
    /// Allocated bytes (`st_blocks * 512`); 0 on the `readdir` backend.
    pub alloc: u64,
    /// `dev << 64 | ino`, the same value for every hardlink to one inode; 0 when unknown.
    pub file_id: u128,
    pub dev: u64,
    modified: SystemTime,
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

/// macOS has the POSIX owner, mode and link-count fields, so `find -perm`, `-uid`,
/// `-gid` and `-links` answer here the way they do on Linux.
pub const POSIX_FIELDS: bool = true;

fn kind_of_type(t: &fs::FileType) -> Kind {
    if t.is_symlink() {
        Kind::Symlink
    } else if t.is_dir() {
        Kind::Dir
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

fn mode_kind(mode: u32) -> Kind {
    match mode & 0o170_000 {
        0o040_000 => Kind::Dir,
        0o100_000 => Kind::File,
        0o120_000 => Kind::Symlink,
        0o010_000 => Kind::Fifo,
        0o140_000 => Kind::Socket,
        0o020_000 => Kind::Char,
        0o060_000 => Kind::Block,
        _ => Kind::Other,
    }
}

fn time_of(secs: i64, nsec: i64) -> SystemTime {
    if secs < 0 && nsec > 0 && nsec < 1_000_000_000 {
        // `tv_sec` floors: -0.5 s arrives as `tv_sec = -1, tv_nsec = 500_000_000`.
        UNIX_EPOCH - Duration::new(secs.unsigned_abs() - 1, 1_000_000_000 - nsec as u32)
    } else {
        let span = Duration::new(secs.unsigned_abs(), nsec.clamp(0, 1_999_999_999) as u32);
        if secs < 0 {
            UNIX_EPOCH - span
        } else {
            UNIX_EPOCH + span
        }
    }
}

fn stat_of(md: &fs::Metadata) -> Stat {
    Stat {
        kind: mode_kind(md.mode()),
        size: md.size(),
        blocks: md.blocks(),
        mtime: time_of(md.mtime(), md.mtime_nsec()),
        atime: time_of(md.atime(), md.atime_nsec()),
        ctime: time_of(md.ctime(), md.ctime_nsec()),
        links: md.nlink(),
        inode: md.ino(),
        dev: md.dev(),
        mode: md.mode(),
        uid: md.uid(),
        gid: md.gid(),
    }
}

/// `lstat` one path: the root, a `--follow` target, or a `-newerXY` reference file.
pub fn lstat(p: &Path) -> io::Result<Stat> {
    fs::symlink_metadata(p).map(|m| stat_of(&m))
}

/// `stat` one path, following a symlink (what `-follow` and `-L` turn an entry into).
pub fn stat(p: &Path) -> io::Result<Stat> {
    fs::metadata(p).map(|m| stat_of(&m))
}

/// Every name of one directory, back to back: the entries listed with it index into this
/// one allocation instead of owning a name each.
type Names = Rc<Vec<u8>>;

/// One entry's name: a byte range in its directory's [`Names`], which is why a name is
/// only readable once [`list`] has returned.
#[derive(Clone)]
pub struct Name {
    names: Option<Names>,
    at: usize,
    len: usize,
}

impl Name {
    fn slice(&self) -> &[u8] {
        let names = self
            .names
            .as_ref()
            .expect("a name is readable once its directory has been read");
        &names[self.at..self.at + self.len]
    }

    fn os(&self) -> &OsStr {
        OsStr::from_bytes(self.slice())
    }

    /// A name in an arena of its own, for a caller that has the bytes and no directory read
    /// behind them.
    #[cfg(test)]
    fn owned(bytes: &[u8]) -> Name {
        Name {
            names: Some(Rc::new(bytes.to_vec())),
            at: 0,
            len: bytes.len(),
        }
    }
}

impl std::fmt::Debug for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.os(), f)
    }
}

impl std::ops::Deref for Name {
    type Target = OsStr;

    fn deref(&self) -> &OsStr {
        self.os()
    }
}

impl AsRef<OsStr> for Name {
    fn as_ref(&self) -> &OsStr {
        self.os()
    }
}

impl AsRef<Path> for Name {
    fn as_ref(&self) -> &Path {
        Path::new(self.os())
    }
}

impl PartialEq for Name {
    fn eq(&self, other: &Name) -> bool {
        self.slice() == other.slice()
    }
}

impl Eq for Name {}

impl Ord for Name {
    fn cmp(&self, other: &Name) -> std::cmp::Ordering {
        self.slice().cmp(other.slice())
    }
}

impl PartialOrd for Name {
    fn partial_cmp(&self, other: &Name) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.slice() == other.as_bytes()
    }
}

/// One name from a directory: the name, the type that came with the directory record,
/// the directory it was read from, and an empty slot for its `lstat` data.
pub struct Lite {
    pub name: Name,
    kind: Kind,
    /// The directory this name came from, alive as long as any entry listed with it.
    dir: Rc<PathBuf>,
    stat: Cell<Option<Stat>>,
}

impl Lite {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn is_reparse(&self) -> bool {
        self.kind == Kind::Symlink
    }

    /// The kind the directory record reported: exact, and free.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// `lstat` this entry. The result is cached: a second call is free.
    pub fn stat(&self) -> io::Result<Stat> {
        if let Some(s) = self.stat.get() {
            return Ok(s);
        }
        let s = lstat(&self.dir.join(self.name.os()))?;
        self.stat.set(Some(s));
        Ok(s)
    }
}

/// List one directory into `out`: names and kinds only, no `lstat` per entry.
pub fn list(dir: &Path, out: &mut Vec<Lite>, errs: &mut ErrorCounts) -> io::Result<()> {
    out.clear();
    let dir = Rc::new(dir.to_path_buf());
    let mut names: Vec<u8> = Vec::new();
    for e in fs::read_dir(&*dir)? {
        let e = match e {
            Ok(e) => e,
            Err(err) => {
                errs.record(&err);
                continue;
            }
        };
        let kind = match e.file_type() {
            Ok(t) => kind_of_type(&t),
            Err(err) => {
                errs.record(&err);
                continue;
            }
        };
        let name = e.file_name();
        let bytes = name.as_bytes();
        let at = names.len();
        names.extend_from_slice(bytes);
        out.push(Lite {
            name: Name {
                names: None,
                at,
                len: bytes.len(),
            },
            kind,
            dir: Rc::clone(&dir),
            stat: Cell::new(None),
        });
    }
    let names: Names = Rc::new(names);
    for e in out.iter_mut() {
        e.name.names = Some(Rc::clone(&names));
    }
    Ok(())
}

/// True for entries that are counted as links and never descended into.
pub fn is_skipped(e: &Entry) -> bool {
    e.is_link
}

/// Enumerator with the batched backend's contract: every entry carries its allocated
/// size and file id.
pub struct DirReader;

impl Default for DirReader {
    fn default() -> Self {
        Self::new()
    }
}

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

/// List one directory with an `lstat` per entry. `detail` fills allocation size, device
/// and file id, which costs nothing extra here but keeps the `readdir` backend's
/// documented "on-disk unknown, no file ids" behaviour.
fn read_with(
    dir: &Path,
    out: &mut Vec<Entry>,
    errs: &mut ErrorCounts,
    detail: bool,
) -> io::Result<()> {
    out.clear();
    for e in fs::read_dir(dir)? {
        let (e, md) = match e.and_then(|e| e.metadata().map(|m| (e, m))) {
            Ok(x) => x,
            Err(err) => {
                errs.record(&err);
                continue;
            }
        };
        let kind = mode_kind(md.mode());
        out.push(Entry {
            name: e.file_name(),
            is_dir: kind == Kind::Dir,
            is_link: kind == Kind::Symlink,
            // A directory's size is its own bookkeeping, not its contents.
            size: if kind == Kind::Dir { 0 } else { md.size() },
            alloc: if detail { md.blocks() * 512 } else { 0 },
            file_id: if detail {
                (md.dev() as u128) << 64 | md.ino() as u128
            } else {
                0
            },
            dev: if detail { md.dev() } else { 0 },
            modified: time_of(md.mtime(), md.mtime_nsec()),
        });
    }
    Ok(())
}

/// The `readdir` backend: names and sizes only, no allocation sizes or file ids.
pub fn read_plain(dir: &Path, out: &mut Vec<Entry>, errs: &mut ErrorCounts) -> io::Result<()> {
    read_with(dir, out, errs, false)
}

/// Err when `path` is itself a symlink: scanning or pruning it would silently act on
/// its target instead. The wording is Linux's, because the callers and the tests match
/// on it.
pub fn refuse_reparse_root(path: &Path) -> io::Result<()> {
    match lstat(path) {
        Ok(s) if s.kind == Kind::Symlink => Err(io::Error::new(
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
    Ok(lstat(path)?.file_id())
}

/// macOS has no named data streams. `--streams` therefore reports 0 bytes.
pub fn named_stream_bytes(_path: &Path) -> io::Result<u64> {
    Ok(0)
}

/// Device id of a directory, used to recognise mount points.
pub fn dev_of(path: &Path) -> io::Result<u64> {
    Ok(stat(path)?.dev)
}

/// Filesystem type names by device, for `-fstype`. Darwin answers this from
/// `getmntinfo`, which slice 1 does not carry yet: an empty map means the walk has no
/// names to compare against, so `-fstype` matches nothing rather than lying.
pub fn filesystem_types() -> HashMap<u64, String> {
    HashMap::new()
}

/// True when `e` lives on a different filesystem than `root_dev` (a mount point).
pub fn other_filesystem(e: &Entry, root_dev: u64) -> bool {
    root_dev != 0 && e.dev != 0 && e.dev != root_dev
}

/// A mount point is the macOS analogue of a Windows volume root: its own device.
pub fn same_filesystem(e: &Entry, parent_dev: u64) -> bool {
    parent_dev == 0 || e.dev == 0 || e.dev == parent_dev
}

pub fn is_unsupported(e: &io::Error) -> bool {
    sys::is_unsupported(e)
}

pub fn is_denied(e: &io::Error) -> bool {
    sys::is_denied(e)
}

/// Bytes of one file name as an `OsStr` (used by the change feed).
pub fn path_bytes(p: &Path) -> &[u8] {
    p.as_os_str().as_bytes()
}

pub fn os_from_bytes(b: &[u8]) -> OsString {
    OsStr::from_bytes(b).to_os_string()
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
                "dusage_mac_enum_{tag}_{}_{}",
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
        let link = v.iter().find(|e| e.name == "link").unwrap();
        assert!(link.is_link && is_skipped(link) && !link.is_dir);
        let sub = v.iter().find(|e| e.name == "sub").unwrap();
        assert!(sub.is_dir && !is_skipped(sub) && sub.size == 0);
    }

    #[test]
    fn hardlinks_share_one_file_id() {
        let f = Fixture::new("hl");
        fs::write(f.0.join("a.bin"), b"x").unwrap();
        fs::hard_link(f.0.join("a.bin"), f.0.join("b.bin")).unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        DirReader::new().read(&f.0, &mut v, &mut errs).unwrap();
        let id = |n: &str| v.iter().find(|e| e.name == n).unwrap().file_id;
        assert_eq!(id("a.bin"), id("b.bin"));
        assert_eq!(id("a.bin"), file_id_of(&f.0.join("a.bin")).unwrap());
        assert!(id("a.bin") >> 64 != 0, "device id occupies the high half");
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
    fn streams_are_zero_and_mounts_compare_by_device() {
        let f = Fixture::new("misc");
        fs::write(f.0.join("a"), b"x").unwrap();
        assert_eq!(named_stream_bytes(&f.0.join("a")).unwrap(), 0);
        assert_eq!(dev_of(&f.0).unwrap(), dev_of(&f.0.join("a")).unwrap());
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
    fn lite_names_come_from_the_record_and_stat_is_cached() {
        let f = Fixture::new("lite");
        fs::create_dir(f.0.join("sub")).unwrap();
        fs::write(f.0.join("a.bin"), vec![0u8; 5000]).unwrap();
        std::os::unix::fs::symlink(f.0.join("a.bin"), f.0.join("link")).unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        assert_eq!(errs.total(), 0);
        let mut got: Vec<String> = v
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, ["a.bin", "link", "sub"]);
        assert!(!v.iter().find(|e| e.name == "link").unwrap().is_dir());
        assert!(v.iter().find(|e| e.name == "sub").unwrap().is_dir());
        let a = v.iter().find(|e| e.name == "a.bin").unwrap();
        let s = a.stat().unwrap();
        assert_eq!(s.size, 5000);
        assert!(s.alloc() >= 5000, "on-disk {} for 5000 bytes", s.alloc());
        assert_eq!(a.stat().unwrap(), s, "cached, so the answer is stable");
        // A link stats as itself, not as its target.
        let link = v.iter().find(|e| e.name == "link").unwrap();
        assert_eq!(link.stat().unwrap().kind, Kind::Symlink);
        assert_ne!(link.stat().unwrap().size, 5000);
        // `list` clears and reuses the caller's vector.
        list(&f.0.join("sub"), &mut v, &mut errs).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn lite_stat_reports_a_vanished_entry() {
        let f = Fixture::new("gone");
        fs::write(f.0.join("a"), b"x").unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        fs::remove_file(f.0.join("a")).unwrap();
        assert!(v[0].stat().is_err());
    }

    #[test]
    fn posix_fields_are_present() {
        let f = Fixture::new("posix");
        fs::write(f.0.join("a"), vec![0u8; 7000]).unwrap();
        fs::hard_link(f.0.join("a"), f.0.join("b")).unwrap();
        let md = fs::symlink_metadata(f.0.join("a")).unwrap();
        let s = lstat(&f.0.join("a")).unwrap();
        assert_eq!(s.mode, md.mode());
        assert_eq!((s.uid, s.gid), (md.uid(), md.gid()));
        assert_eq!(s.links, md.nlink());
        assert_eq!(s.inode, md.ino());
        assert_eq!(s.blocks, md.blocks());
        assert_eq!(s.file_id(), lstat(&f.0.join("b")).unwrap().file_id());
        // `stat` follows a link, `lstat` describes it.
        std::os::unix::fs::symlink(f.0.join("a"), f.0.join("lnk")).unwrap();
        assert_eq!(stat(&f.0.join("lnk")).unwrap().size, 7000);
        assert_eq!(lstat(&f.0.join("lnk")).unwrap().kind, Kind::Symlink);
        assert_eq!(
            lstat(&f.0.join("nope")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn a_name_that_is_not_valid_utf8_keeps_its_bytes() {
        let f = Fixture::new("nonutf8");
        let dir = OsStr::from_bytes(b"bad\xff\xfedir");
        fs::create_dir(f.0.join(dir)).unwrap();
        fs::write(f.0.join(dir).join(OsStr::from_bytes(b"in\xff.go")), b"1").unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        let e = v
            .iter()
            .find(|e| e.name.as_bytes() == b"bad\xff\xfedir")
            .expect("the name reaches the caller exactly as the kernel gave it");
        assert!(e.is_dir());
        assert_eq!(e.stat().unwrap().kind, Kind::Dir);
    }

    #[test]
    fn kinds_come_from_the_mode() {
        assert_eq!(mode_kind(0o040_755), Kind::Dir);
        assert_eq!(mode_kind(0o100_644), Kind::File);
        assert_eq!(mode_kind(0o120_777), Kind::Symlink);
        assert_eq!(mode_kind(0o010_644), Kind::Fifo);
        assert_eq!(mode_kind(0o140_755), Kind::Socket);
        assert_eq!(mode_kind(0o020_600), Kind::Char);
        assert_eq!(mode_kind(0o060_600), Kind::Block);
        assert_eq!(mode_kind(0), Kind::Other);
    }

    #[test]
    fn timestamps_before_the_epoch_keep_their_nanoseconds() {
        assert_eq!(time_of(0, 0), UNIX_EPOCH);
        assert_eq!(time_of(2, 500), UNIX_EPOCH + Duration::new(2, 500));
        assert_eq!(
            time_of(-1, 500_000_000),
            UNIX_EPOCH - Duration::new(0, 500_000_000)
        );
        assert_eq!(
            time_of(-2, 250_000_000),
            UNIX_EPOCH - Duration::new(1, 750_000_000)
        );
        assert_eq!(time_of(-1, 0), UNIX_EPOCH - Duration::new(1, 0));
    }

    #[test]
    fn a_name_is_readable_once_its_directory_ends() {
        let n = Name::owned(b"x");
        assert_eq!(&*n, OsStr::from_bytes(b"x"));
    }

    #[test]
    fn the_name_buffer_is_shared_per_directory() {
        let f = Fixture::new("arena");
        for i in 0..100 {
            fs::write(f.0.join(format!("f{i:03}")), b"x").unwrap();
        }
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        assert_eq!(v.len(), 100);
        let ptr = |e: &Lite| Rc::as_ptr(e.name.names.as_ref().unwrap()) as *const u8;
        assert!(v.windows(2).all(|w| ptr(&w[0]) == ptr(&w[1])));
        assert!(v.iter().all(|e| e.stat().is_ok()));
    }
}

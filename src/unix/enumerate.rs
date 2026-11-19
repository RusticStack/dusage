//! Directory enumeration for Linux, in two layers.
//!
//! [`Batch`] is the primitive: one `openat` per directory, then `getdents64` into a reused
//! 64 KiB buffer, with every `linux_dirent64` record parsed in place and its name borrowed
//! straight out of that buffer. The entry type comes with the record (`d_type`), so a walk
//! that only needs names and types never stats. Anything that needs the inode — size,
//! allocation, mtime, device, file id — is one `statx` on the open directory descriptor,
//! which is `lstat` without a path to resolve.
//!
//! [`list`] is the names-and-types layer. [`DirReader::read`] and [`read_plain`] add the
//! `statx`; [`Lite::stat`] defers it until asked. A directory that changes while it is
//! being read is read again, and a name it repeats is reported once.
//!
//! Symlinks take the place of Windows reparse points: they are reported and never
//! followed. Per-entry failures are recorded in the caller's `ErrorCounts` and the walk
//! continues; only an unreadable directory is returned as `Err`.

use super::sys;
use crate::model::ErrorCounts;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Bytes per `getdents64` call. One 64 KiB batch covers a thousand dense records, and the
/// buffer is reused by every directory the thread reads.
const DIR_BUF: usize = 64 * 1024;

/// `linux_dirent64` (dirent.h). Declared for its layout, which the parser indexes: a record
/// is `d_reclen` bytes long and `d_name` is a NUL-terminated name that runs to the end.
#[repr(C)]
#[allow(dead_code)]
struct LinuxDirent64 {
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
    d_name: [u8; 0],
}

const DIRENT_OFF: usize = std::mem::offset_of!(LinuxDirent64, d_off);
const DIRENT_RECLEN: usize = std::mem::offset_of!(LinuxDirent64, d_reclen);
const DIRENT_TYPE: usize = std::mem::offset_of!(LinuxDirent64, d_type);
const DIRENT_NAME: usize = std::mem::offset_of!(LinuxDirent64, d_name);

// `d_type` (dirent.h). Anything unlisted is a socket, fifo, device or whiteout.
const DT_UNKNOWN: u8 = 0;
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;

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
    /// The filesystem did not say: one `statx` answers it, as `read_dir` does on `DT_UNKNOWN`.
    Unknown,
}

impl Kind {
    fn of_dtype(t: u8) -> Kind {
        match t {
            DT_DIR => Kind::Dir,
            DT_REG => Kind::File,
            DT_LNK => Kind::Link,
            DT_UNKNOWN => Kind::Unknown,
            _ => Kind::Other,
        }
    }

    fn of_mode(mode: u16) -> Kind {
        match mode & sys::S_IFMT {
            sys::S_IFDIR => Kind::Dir,
            sys::S_IFLNK => Kind::Link,
            sys::S_IFREG => Kind::File,
            _ => Kind::Other,
        }
    }

    fn of_type(t: fs::FileType) -> Kind {
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
    kind: Kind,
    modified: SystemTime,
}

impl Stat {
    pub fn modified(&self) -> SystemTime {
        self.modified
    }

    fn of_statx(s: &sys::Statx) -> Stat {
        let kind = Kind::of_mode(s.mode);
        let dev = sys::make_dev(s.dev_major, s.dev_minor);
        Stat {
            size: if kind == Kind::Dir { 0 } else { s.size },
            alloc: s.blocks * 512,
            file_id: (dev as u128) << 64 | s.ino as u128,
            dev,
            kind,
            modified: time_of(s.mtime),
        }
    }

    fn of_metadata(md: &fs::Metadata) -> Stat {
        let dev = md.dev();
        Stat {
            size: if md.is_dir() { 0 } else { md.len() },
            alloc: md.blocks() * 512,
            file_id: (dev as u128) << 64 | md.ino() as u128,
            dev,
            kind: Kind::of_type(md.file_type()),
            modified: md.modified().unwrap_or(UNIX_EPOCH),
        }
    }
}

fn time_of(t: sys::StatxTimestamp) -> SystemTime {
    let span = Duration::new(t.tv_sec.unsigned_abs(), t.tv_nsec);
    if t.tv_sec < 0 {
        UNIX_EPOCH - span
    } else {
        UNIX_EPOCH + span
    }
}

fn stat_of(p: &Path) -> io::Result<Stat> {
    Ok(Stat::of_metadata(&fs::symlink_metadata(p)?))
}

fn malformed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed dirent record")
}

fn c_path(p: &OsStr) -> io::Result<CString> {
    CString::new(p.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// `lstat` one name of an open directory, without resolving a path.
fn stat_at(fd: RawFd, name: &CStr) -> io::Result<Stat> {
    let mut stx = sys::Statx::default();
    // SAFETY: `name` is NUL-terminated and lives in the directory's buffer, `fd` is open on
    // that directory, and `stx` is a live `struct statx` of exactly the size declared.
    let rc = unsafe {
        sys::statx(
            fd,
            name.as_ptr(),
            sys::AT_SYMLINK_NOFOLLOW,
            sys::STATX_BASIC_STATS,
            &mut stx,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Stat::of_statx(&stx))
}

/// `n` bytes at `off`: the callers have already checked the record is that long.
fn bytes<const N: usize>(raw: &[u8], off: usize) -> [u8; N] {
    let mut out = [0; N];
    out.copy_from_slice(&raw[off..off + N]);
    out
}

/// One record out of the batch buffer, borrowed from it in place.
struct Rec<'a> {
    name: &'a [u8],
    /// The same name NUL-terminated, which `statx` takes and `OsStr` does not.
    cname: &'a CStr,
    kind: Kind,
}

/// Where a pass over one directory stopped.
enum Pass {
    /// The directory was read to the end.
    End,
    /// The directory changed while it was read, so it has to be read again.
    Restart,
}

/// An open directory, read in `getdents64` batches.
struct Batch<'a> {
    fd: OwnedFd,
    buf: &'a mut [u8],
    /// Bytes of the current batch in `buf`, and how far the parse has got.
    len: usize,
    pos: usize,
    /// The kernel has handed back the last batch.
    done: bool,
}

impl<'a> Batch<'a> {
    fn open(dir: &Path, buf: &'a mut [u8]) -> io::Result<Batch<'a>> {
        let path = c_path(dir.as_os_str())?;
        // SAFETY: `path` is NUL-terminated and outlives the call, the flags are the ones
        // `read_dir` opens a directory with, and a failure is reported as -1 with `errno`.
        let raw = unsafe {
            sys::openat(
                sys::AT_FDCWD,
                path.as_ptr(),
                sys::O_RDONLY | sys::O_DIRECTORY | sys::O_CLOEXEC | sys::O_NONBLOCK,
                0,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a descriptor this call just opened and nothing else will close.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Batch {
            fd,
            buf,
            len: 0,
            pos: 0,
            done: false,
        })
    }

    /// Feed every record of this directory to `sink`, until the directory ends or moves
    /// out from under the read. `.` and `..` never come out of here.
    fn drain<S: Sink + ?Sized>(
        &mut self,
        sink: &mut S,
        fuse: &mut Option<HashSet<Vec<u8>>>,
        last_off: &mut i64,
        restart: bool,
    ) -> io::Result<Pass> {
        let fd = self.fd.as_raw_fd();
        loop {
            if self.pos == self.len {
                if self.done {
                    return Ok(Pass::End);
                }
                // The buffer leaves `self` for the call and comes straight back, so the
                // borrow the fill takes never overlaps the borrow the parse takes.
                let buf = std::mem::take(&mut self.buf);
                let filled = fill(fd, buf);
                self.buf = buf;
                let n = filled?;
                // A short batch is not the end: the kernel stops where its own buffer
                // ends, so only an empty batch ends the directory.
                self.len = n;
                self.pos = 0;
                if n == 0 {
                    self.done = true;
                    return Ok(Pass::End);
                }
            }
            let raw = &self.buf[self.pos..self.len];
            if raw.len() < DIRENT_NAME {
                return Err(malformed());
            }
            let reclen = u16::from_ne_bytes(bytes::<2>(raw, DIRENT_RECLEN)) as usize;
            if reclen < DIRENT_NAME || reclen > raw.len() {
                return Err(malformed());
            }
            let off = i64::from_ne_bytes(bytes::<8>(raw, DIRENT_OFF));
            let dtype = raw[DIRENT_TYPE];
            let tail = &raw[DIRENT_NAME..reclen];
            self.pos += reclen;
            let Some(nul) = tail.iter().position(|&b| b == 0) else {
                return Err(malformed());
            };
            let name = &tail[..nul];
            if name == b"." || name == b".." {
                continue;
            }
            match off {
                // Some filesystems never fill `d_off`: positions are unusable, so a
                // repeated name is dropped instead of noticed.
                0 => {
                    fuse.get_or_insert_with(HashSet::new);
                }
                // The offset went backwards, so the directory changed under the read:
                // read it again, and report each name once.
                off if off <= *last_off => {
                    if restart {
                        return Ok(Pass::Restart);
                    }
                    fuse.get_or_insert_with(HashSet::new);
                }
                off => *last_off = off,
            }
            if let Some(seen) = fuse {
                if !seen.insert(name.to_vec()) {
                    continue;
                }
            }
            let cname = CStr::from_bytes_with_nul(&tail[..=nul]).map_err(|_| malformed())?;
            sink.push(
                fd,
                &Rec {
                    name,
                    cname,
                    kind: Kind::of_dtype(dtype),
                },
            );
        }
    }

    /// Back to the first record, for a directory that changed while it was read.
    fn rewind(&mut self) -> io::Result<()> {
        // SAFETY: the descriptor is open on this directory and the result is checked.
        let off = unsafe { sys::lseek(self.fd.as_raw_fd(), 0, sys::SEEK_SET) };
        if off < 0 {
            return Err(io::Error::last_os_error());
        }
        self.len = 0;
        self.pos = 0;
        self.done = false;
        Ok(())
    }
}

/// One `getdents64` batch into `buf`: the bytes written, 0 at the end of the directory.
fn fill(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `fd` is open on a directory and `buf` is a live slice of `buf.len()` bytes
    // for the kernel to fill with records.
    let n = unsafe { sys::getdents64(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Where one directory's records go.
trait Sink {
    /// Take one record from the directory open as `fd`, which `stat_at` resolves names in.
    fn push(&mut self, fd: RawFd, rec: &Rec<'_>);
    /// Drop everything pushed so far, for the second read of a directory that changed.
    fn clear(&mut self);
}

// One reused `getdents64` buffer per thread: a walk keeps one buffer rather than
// allocating 64 KiB per directory.
thread_local! {
    static DIR_BUF_CELL: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Read one directory into `sink`, reusing this thread's batch buffer.
fn read_dir_raw<S: Sink + ?Sized>(dir: &Path, sink: &mut S) -> io::Result<()> {
    DIR_BUF_CELL.with(|cell| {
        let mut reused = cell.borrow_mut();
        if reused.len() < DIR_BUF {
            reused.resize(DIR_BUF, 0);
        }
        let mut batch = Batch::open(dir, reused.as_mut_slice())?;
        let mut last_off = 0i64;
        let mut fuse: Option<HashSet<Vec<u8>>> = None;
        let mut rewound = false;
        loop {
            match batch.drain(sink, &mut fuse, &mut last_off, !rewound)? {
                Pass::End => return Ok(()),
                Pass::Restart => {
                    rewound = true;
                    batch.rewind()?;
                    sink.clear();
                    // From here on, a name read twice is reported once.
                    fuse = Some(HashSet::new());
                    last_off = 0;
                }
            }
        }
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

struct LiteSink<'a> {
    out: &'a mut Vec<Lite>,
    errs: &'a mut ErrorCounts,
}

impl Sink for LiteSink<'_> {
    fn push(&mut self, fd: RawFd, rec: &Rec<'_>) {
        let kind = match rec.kind {
            // One `statx` answers a type the filesystem left out, exactly as `read_dir`
            // does; an entry that vanished meanwhile is counted and dropped.
            Kind::Unknown => match stat_at(fd, rec.cname) {
                Ok(s) => s.kind,
                Err(err) => {
                    self.errs.record(&err);
                    return;
                }
            },
            kind => kind,
        };
        self.out.push(Lite {
            name: OsString::from_vec(rec.name.to_vec()),
            kind,
            stat: Cell::new(None),
        });
    }

    fn clear(&mut self) {
        self.out.clear();
    }
}

/// List one directory into `out`: names and types only, no `stat`.
///
/// `out` is cleared and its allocation reused, so a walk keeps one entry vector instead of
/// allocating one per directory. An entry that cannot be typed is recorded in `errs` and
/// dropped, like an entry that vanishes between `getdents` and `lstat` always was.
pub fn list(dir: &Path, out: &mut Vec<Lite>, errs: &mut ErrorCounts) -> io::Result<()> {
    out.clear();
    read_dir_raw(dir, &mut LiteSink { out, errs })
}

/// True for entries that are counted as links and never descended into.
pub fn is_skipped(e: &Entry) -> bool {
    e.is_link
}

struct EntrySink<'a> {
    out: &'a mut Vec<Entry>,
    errs: &'a mut ErrorCounts,
    detail: bool,
}

impl Sink for EntrySink<'_> {
    fn push(&mut self, fd: RawFd, rec: &Rec<'_>) {
        // `statx` is `lstat`: a symlink is described, not followed.
        match stat_at(fd, rec.cname) {
            Ok(s) => self.out.push(Entry {
                name: OsString::from_vec(rec.name.to_vec()),
                is_dir: s.kind == Kind::Dir,
                is_link: s.kind == Kind::Link,
                size: s.size,
                alloc: if self.detail { s.alloc } else { 0 },
                file_id: if self.detail { s.file_id } else { 0 },
                dev: if self.detail { s.dev } else { 0 },
                modified: s.modified,
            }),
            Err(err) => {
                self.errs.record(&err);
            }
        }
    }

    fn clear(&mut self) {
        self.out.clear();
    }
}

/// List one directory with an `lstat` per entry. `detail` fills allocation size, device and
/// file id, which costs nothing extra here (one `lstat` per entry either way) but keeps the
/// `readdir` backend's documented "on-disk unknown, no file ids" behaviour.
fn read_with(
    dir: &Path,
    out: &mut Vec<Entry>,
    errs: &mut ErrorCounts,
    detail: bool,
) -> io::Result<()> {
    read_dir_raw(dir, &mut EntrySink { out, errs, detail })
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

    #[test]
    fn dirent_layout_matches_linux_dirent64() {
        assert_eq!(DIRENT_OFF, 8);
        assert_eq!(DIRENT_RECLEN, 16);
        assert_eq!(DIRENT_TYPE, 18);
        assert_eq!(DIRENT_NAME, 19);
        assert_eq!(std::mem::size_of::<LinuxDirent64>(), 24);
    }

    #[test]
    fn statx_fields_match_std_metadata() {
        let f = Fixture::new("statx");
        fs::create_dir(f.0.join("sub")).unwrap();
        fs::write(f.0.join("a.bin"), vec![0u8; 9000]).unwrap();
        std::os::unix::fs::symlink(f.0.join("a.bin"), f.0.join("lnk")).unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        DirReader::new().read(&f.0, &mut v, &mut errs).unwrap();
        assert_eq!(errs.total(), 0);
        assert_eq!(v.len(), 3);
        for e in &v {
            let md = fs::symlink_metadata(f.0.join(&e.name)).unwrap();
            let name = &e.name;
            assert_eq!(e.size, if md.is_dir() { 0 } else { md.len() }, "{name:?}");
            assert_eq!(e.alloc, md.blocks() * 512, "{name:?}");
            assert_eq!(e.dev, md.dev(), "{name:?}");
            assert_eq!(
                e.file_id,
                (md.dev() as u128) << 64 | md.ino() as u128,
                "{name:?}"
            );
            assert_eq!(e.is_dir, md.is_dir(), "{name:?}");
            assert_eq!(e.is_link, md.file_type().is_symlink(), "{name:?}");
            assert_eq!(e.modified(), md.modified().unwrap(), "{name:?}");
        }
    }

    #[test]
    fn an_empty_directory_lists_nothing() {
        let f = Fixture::new("empty");
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        DirReader::new().read(&f.0, &mut v, &mut errs).unwrap();
        assert!(v.is_empty() && errs.total() == 0);
        let mut lite = Vec::new();
        list(&f.0, &mut lite, &mut errs).unwrap();
        assert!(lite.is_empty() && errs.total() == 0);
    }

    #[test]
    fn names_survive_multiple_batches() {
        let f = Fixture::new("many");
        let want: Vec<String> = (0..5000).map(|i| format!("f{i:05}")).collect();
        for n in &want {
            fs::write(f.0.join(n), b"x").unwrap();
        }
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        assert_eq!(errs.total(), 0);
        let mut got = names_of(&v);
        got.sort();
        assert_eq!(got, want);
        let mut v: Vec<Entry> = Vec::new();
        DirReader::new().read(&f.0, &mut v, &mut errs).unwrap();
        assert_eq!(v.len(), want.len());
        assert!(v.iter().all(|e| !e.is_dir && e.size == 1 && e.file_id != 0));
    }

    /// Hand-built `linux_dirent64` records, as one batch the kernel could have returned.
    fn stage(batch: &mut Batch<'_>, records: &[(u8, i64, &str)]) {
        batch.len = 0;
        batch.pos = 0;
        batch.done = true;
        for &(dtype, off, name) in records {
            let start = batch.len;
            let reclen = (DIRENT_NAME + name.len() + 1 + 7) & !7;
            let rec = &mut batch.buf[start..start + reclen];
            rec.fill(0);
            rec[DIRENT_OFF..DIRENT_OFF + 8].copy_from_slice(&off.to_ne_bytes());
            rec[DIRENT_RECLEN..DIRENT_RECLEN + 2].copy_from_slice(&(reclen as u16).to_ne_bytes());
            rec[DIRENT_TYPE] = dtype;
            rec[DIRENT_NAME..DIRENT_NAME + name.len()].copy_from_slice(name.as_bytes());
            batch.len += reclen;
        }
    }

    fn names_of(v: &[Lite]) -> Vec<String> {
        v.iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect()
    }

    /// The sink the two synthetic tests read through: names only, no `statx`.
    struct Collected(Vec<String>);

    impl Sink for Collected {
        fn push(&mut self, _fd: RawFd, rec: &Rec<'_>) {
            self.0.push(String::from_utf8(rec.name.to_vec()).unwrap());
        }

        fn clear(&mut self) {
            self.0.clear();
        }
    }

    #[test]
    fn a_rewound_directory_is_read_again() {
        let f = Fixture::new("rewind");
        let mut buf = [0u8; 512];
        let mut batch = Batch::open(&f.0, &mut buf).unwrap();
        // Two records, then the offset the reader has already passed: the directory
        // gained or lost an entry between two batches.
        stage(
            &mut batch,
            &[(DT_REG, 10, "a"), (DT_REG, 20, "b"), (DT_REG, 20, "c")],
        );
        let mut got = Collected(Vec::new());
        let (mut fuse, mut last_off) = (None, 0);
        let pass = batch
            .drain(&mut got, &mut fuse, &mut last_off, true)
            .unwrap();
        assert!(matches!(pass, Pass::Restart));
        batch.rewind().unwrap();
        got.clear();
        let mut fuse = Some(HashSet::new());
        let mut last_off = 0;
        let pass = batch
            .drain(&mut got, &mut fuse, &mut last_off, false)
            .unwrap();
        assert!(matches!(pass, Pass::End));
        // The fixture itself is empty, so the second pass reads nothing new.
        assert!(got.0.is_empty());
    }

    #[test]
    fn a_repeated_name_is_reported_once() {
        let f = Fixture::new("fuse");
        let mut buf = [0u8; 512];
        let mut batch = Batch::open(&f.0, &mut buf).unwrap();
        // A `d_off` of 0 means the filesystem cannot report positions, so a record that
        // comes back is dropped instead of listed twice.
        stage(
            &mut batch,
            &[(DT_REG, 0, "a"), (DT_REG, 0, "a"), (DT_REG, 0, "b")],
        );
        let mut got = Collected(Vec::new());
        let (mut fuse, mut last_off) = (None, 0);
        let pass = batch
            .drain(&mut got, &mut fuse, &mut last_off, true)
            .unwrap();
        assert!(matches!(pass, Pass::End));
        assert_eq!(got.0, ["a", "b"]);
    }

    fn ms(d: std::time::Duration) -> f64 {
        d.as_secs_f64() * 1e3
    }
}

//! Directory enumeration for macOS, over libSystem.
//!
//! [`Dir`] is the primitive: one `open` of a directory, then `fdopendir`, then `readdir`
//! record by record, with the entry type coming free from `d_type` and the name read out
//! of the record the kernel handed back. Anything that needs the inode — size, allocation,
//! mtime, device, file id — is one `fstatat` on the open directory descriptor, which is
//! `lstat` without a path to resolve.
//!
//! [`list`] is the names-and-kinds layer. [`DirReader::read`] and [`read_plain`] add the
//! `fstatat`; [`Lite::stat`] defers it until asked. Symlinks take the place of Windows
//! reparse points: they are reported and never followed. Per-entry failures are recorded
//! in the caller's `ErrorCounts` and the walk continues; only an unreadable directory is
//! returned as `Err`.
//!
//! Darwin has no `getdents64`: `readdir` hands back one record at a time from the
//! directory stream it keeps, so the batch-restart logic the Linux enumerator needs for a
//! directory that mutates mid-read has no counterpart here. A directory that gains or
//! loses an entry during a read is reported as the kernel streamed it, and the next walk
//! sees the new state.

use super::sys;
use crate::model::{ErrorCounts, Kind, Stat};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr, CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
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

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The error one unreadable record reports.
///
/// It carries the record's own fields and the first bytes of both the record and its name,
/// because the failure this diagnostic exists for is a wrong ABI: when the fields are read
/// at the wrong offsets, the hex dump says where the name really starts and what the real
/// `d_reclen`/`d_namlen` are. A fixed string cost a whole CI round trip once already.
fn malformed_record(rec: &sys::Dirent, why: &str) -> io::Error {
    // SAFETY: `rec` is a live record from `readdir`, and the 24 bytes read are inside it.
    let head = unsafe { std::slice::from_raw_parts(rec as *const _ as *const u8, 24) };
    let name_head = &rec.d_name[..16];
    let c_len = c_name(rec).map_or(0, |n| n.len());
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "malformed dirent record ({why}): d_reclen={} d_namlen={} d_type={} \
             name_strlen={c_len} d_name={} record_head={}",
            rec.d_reclen,
            rec.d_namlen,
            rec.d_type,
            hex(name_head),
            hex(head)
        ),
    )
}

/// The name of one record, found by the NUL that terminates it inside `d_name`.
///
/// The length comes from that terminator and never from `d_namlen`. A name is
/// self-describing without the length field, and reading the field at a guessed offset is
/// exactly how the first real macOS run failed: every record came back with a length of
/// zero, which rejected the first `.` of every directory. `d_namlen` is still reported
/// when a record is rejected — see [`malformed_record`] — but nothing depends on it.
fn c_name(rec: &sys::Dirent) -> Option<&[u8]> {
    // SAFETY: `d_name` is an array of exactly `DIRENT_NAME_MAX` bytes inside `rec`.
    let raw = unsafe { std::slice::from_raw_parts(rec.d_name.as_ptr(), sys::DIRENT_NAME_MAX) };
    let nul = raw.iter().position(|&b| b == 0)?;
    Some(&raw[..nul])
}

/// The `dirent` type, which is exact and free when the kernel fills it in: `readdir`
/// reports it, so neither `list` nor a walk that only needs names pays a syscall for it.
///
/// The eight codes below are the whole set this trusts. Anything else — `DT_UNKNOWN`, the
/// whiteout code, a value from another ABI — is left to `None`, which makes the caller ask
/// `fstatat` rather than guess. A wrong guess here would decide whether a directory is
/// descended into.
fn kind_of_dtype(t: u8) -> Option<Kind> {
    Some(match t {
        sys::DT_DIR => Kind::Dir,
        sys::DT_REG => Kind::File,
        sys::DT_LNK => Kind::Symlink,
        sys::DT_FIFO => Kind::Fifo,
        sys::DT_CHR => Kind::Char,
        sys::DT_BLK => Kind::Block,
        sys::DT_SOCK => Kind::Socket,
        _ => return None,
    })
}

fn kind_of_mode(mode: u32) -> Kind {
    match mode & sys::S_IFMT {
        sys::S_IFDIR => Kind::Dir,
        sys::S_IFREG => Kind::File,
        sys::S_IFLNK => Kind::Symlink,
        sys::S_IFIFO => Kind::Fifo,
        sys::S_IFSOCK => Kind::Socket,
        sys::S_IFCHR => Kind::Char,
        sys::S_IFBLK => Kind::Block,
        _ => Kind::Other,
    }
}

fn time_of(secs: i64, nsec: i64) -> SystemTime {
    if secs < 0 && nsec > 0 && nsec < 1_000_000_000 {
        // `tv_sec` floors: -0.5 s arrives as `tv_sec = -1, tv_nsec = 500_000_000`, so
        // there is one whole second less before the epoch than `|tv_sec|` reads.
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

/// One `struct stat` as the shared [`Stat`].
fn stat_of(s: &sys::StatBuf) -> Stat {
    Stat {
        kind: kind_of_mode(u32::from(s.st_mode)),
        size: s.st_size.max(0) as u64,
        blocks: s.st_blocks.max(0) as u64,
        mtime: time_of(s.st_mtime, s.st_mtime_nsec),
        atime: time_of(s.st_atime, s.st_atime_nsec),
        ctime: time_of(s.st_ctime, s.st_ctime_nsec),
        links: u64::from(s.st_nlink),
        inode: s.st_ino,
        dev: s.st_dev as u64,
        mode: u32::from(s.st_mode),
        uid: s.st_uid,
        gid: s.st_gid,
    }
}

fn c_path(p: &OsStr) -> io::Result<CString> {
    CString::new(p.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

fn stat_path(p: &Path, follow: bool) -> io::Result<Stat> {
    let path = c_path(p.as_os_str())?;
    let mut stx = sys::StatBuf::default();
    // SAFETY: `path` is NUL-terminated and outlives the call, and `stx` is a live
    // `struct stat` of exactly the size declared above.
    let rc = unsafe {
        if follow {
            sys::stat(path.as_ptr(), &mut stx)
        } else {
            sys::lstat(path.as_ptr(), &mut stx)
        }
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat_of(&stx))
}

/// `lstat` one path: the root, a `--follow` target, or a `-newerXY` reference file. The
/// walk itself never comes through here — it stats names against its open directory.
pub fn lstat(p: &Path) -> io::Result<Stat> {
    stat_path(p, false)
}

/// `stat` one path, following a symlink (what `-follow` and `-L` turn an entry into).
pub fn stat(p: &Path) -> io::Result<Stat> {
    stat_path(p, true)
}

// One reused name buffer per thread, so naming a file to `fstatat` costs a copy, not an
// allocation.
thread_local! {
    static NAME_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// `lstat` one name of an open directory, without resolving a path.
fn fstat_at(fd: c_int, name: &[u8]) -> io::Result<Stat> {
    NAME_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        buf.extend_from_slice(name);
        buf.push(0);
        let cname = CStr::from_bytes_with_nul(&buf)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL byte"))?;
        let mut stx = sys::StatBuf::default();
        // SAFETY: `cname` is NUL-terminated for the whole call — it is borrowed from this
        // thread's buffer, which nothing else borrows while it runs — `fd` is open on the
        // directory the name is relative to, and `stx` is a live `struct stat`.
        let rc = unsafe { sys::fstatat(fd, cname.as_ptr(), &mut stx, sys::AT_SYMLINK_NOFOLLOW) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat_of(&stx))
    })
}

/// An open directory stream, closed when the last entry listed with it is dropped.
struct Dir {
    ptr: *mut sys::DIR,
}

impl Dir {
    fn open(path: &Path) -> io::Result<Dir> {
        let p = c_path(path.as_os_str())?;
        // SAFETY: `p` is NUL-terminated and outlives the call, and the flags are the ones
        // a directory is opened with: read-only, must be a directory, close-on-exec, and
        // non-blocking so a FIFO-shaped path cannot park the open.
        let fd = unsafe {
            sys::open(
                p.as_ptr(),
                sys::O_RDONLY | sys::O_DIRECTORY | sys::O_CLOEXEC | sys::O_NONBLOCK,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a descriptor this call just opened; `fdopendir` takes ownership
        // of it on success.
        let ptr = unsafe { sys::fdopendir(fd) };
        if ptr.is_null() {
            let e = io::Error::last_os_error();
            // SAFETY: `fd` is still ours, because `fdopendir` failed and did not take it.
            unsafe { sys::close(fd) };
            return Err(e);
        }
        Ok(Dir { ptr })
    }

    fn fd(&self) -> c_int {
        // SAFETY: `self.ptr` is a live `DIR *` for as long as `self` lives.
        unsafe { sys::dirfd(self.ptr) }
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` came from `fdopendir` and is closed exactly once, here.
        unsafe { sys::closedir(self.ptr) };
    }
}

/// One record out of the directory stream, borrowed from the kernel's own copy.
struct Rec<'a> {
    name: &'a [u8],
    /// `None` when `d_type` was `DT_UNKNOWN`.
    kind: Option<Kind>,
}

/// Where one directory's records go.
trait Sink {
    /// Take one record from the directory open as `dir`, which `fstat_at` resolves names in.
    fn push(&mut self, dir: &Rc<Dir>, rec: &Rec<'_>);
}

/// One `readdir` pass over `dir`, feeding every record to `sink`. `.` and `..` never come
/// out of here.
fn read_dir_raw<S: Sink + ?Sized>(dir: &Path, sink: &mut S) -> io::Result<()> {
    let d = Rc::new(Dir::open(dir)?);
    loop {
        // `readdir` answers the end of the directory and a failure the same way, so errno
        // is cleared first and only a value set again afterwards is an error.
        // SAFETY: `__error` returns this thread's errno slot.
        unsafe { *sys::__error() = 0 };
        // SAFETY: `d.ptr` is a live `DIR *`; the record it returns is owned by the stream
        // and stays valid until the next call on it, which is why it is read here and
        // nowhere later.
        let ent = unsafe { sys::readdir(d.ptr) };
        if ent.is_null() {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                None | Some(0) => Ok(()),
                Some(_) => Err(e),
            };
        }
        // SAFETY: `ent` points at a record the stream owns, valid until the next call on
        // it, which is after this one.
        let rec = unsafe { &*ent };
        let Some(name) = c_name(rec) else {
            return Err(malformed_record(rec, "d_name has no NUL terminator"));
        };
        // A record naming nothing would make `dir.join("")` name the directory itself and
        // send the walk into it.
        if name.is_empty() {
            return Err(malformed_record(rec, "empty name"));
        }
        if name == b"." || name == b".." {
            continue;
        }
        sink.push(
            &d,
            &Rec {
                name,
                kind: kind_of_dtype(rec.d_type),
            },
        );
    }
}

/// Every name of one directory, back to back: the entries listed with it index into this
/// one allocation instead of owning a name each. The bytes are the kernel's, copied once,
/// so a name that is not valid UTF-8 reaches the caller exactly as it was reported.
type Names = Rc<Vec<u8>>;

/// One entry's name: a byte range in its directory's [`Names`].
///
/// This is what a walker borrows a name from, so it derefs to `OsStr` and compares and
/// orders the way `OsStr` does. The range is set when the entry is read and the arena it
/// points into when the directory ends, which is why a name is only readable once
/// [`list`] has returned.
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

/// One name from a directory, as cheaply as the kernel gives it up: a name, a type that
/// came with the `dirent`, the directory it was read from, and an empty slot for its
/// `lstat` data.
///
/// `stat` is the only thing that costs a syscall, and it runs once: the entry remembers the
/// answer, so a size asked for in a filter and again in the output is one `fstatat`. It
/// resolves the name against the open directory, never the full path, which is what find's
/// `fstatat` does.
pub struct Lite {
    pub name: Name,
    kind: Kind,
    /// The directory this name came from, alive as long as any entry listed with it.
    dir: Rc<Dir>,
    stat: Cell<Option<Stat>>,
}

impl Lite {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn is_reparse(&self) -> bool {
        self.kind == Kind::Symlink
    }

    /// The kind the `dirent` reported: exact, and free.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// `lstat` this entry. The result is cached: a second call is free.
    pub fn stat(&self) -> io::Result<Stat> {
        if let Some(s) = self.stat.get() {
            return Ok(s);
        }
        let s = fstat_at(self.dir.fd(), self.name.slice())?;
        self.stat.set(Some(s));
        Ok(s)
    }
}

struct LiteSink<'a> {
    out: &'a mut Vec<Lite>,
    errs: &'a mut ErrorCounts,
    /// The names pushed so far, back to back, which become the arena when the directory
    /// ends. Grown once rather than once per entry.
    names: Vec<u8>,
}

impl Sink for LiteSink<'_> {
    fn push(&mut self, dir: &Rc<Dir>, rec: &Rec<'_>) {
        let kind = match rec.kind {
            // One `fstatat` answers a type the filesystem left out; an entry that vanished
            // meanwhile is counted and dropped.
            None => match fstat_at(dir.fd(), rec.name) {
                Ok(s) => s.kind,
                Err(err) => {
                    self.errs.record(&err);
                    return;
                }
            },
            Some(kind) => kind,
        };
        let at = self.names.len();
        self.names.extend_from_slice(rec.name);
        self.out.push(Lite {
            name: Name {
                names: None,
                at,
                len: rec.name.len(),
            },
            kind,
            dir: Rc::clone(dir),
            stat: Cell::new(None),
        });
    }
}

/// List one directory into `out`: names and types only, no `stat`.
///
/// `out` is cleared and its allocation reused, so a walk keeps one entry vector instead of
/// allocating one per directory. An entry that cannot be typed is recorded in `errs` and
/// dropped, like an entry that vanishes between `readdir` and `lstat` always was.
///
/// The directory's names are copied into one buffer and shared by every entry listed, which
/// is one allocation per directory instead of one per name.
pub fn list(dir: &Path, out: &mut Vec<Lite>, errs: &mut ErrorCounts) -> io::Result<()> {
    out.clear();
    let mut sink = LiteSink {
        out,
        errs,
        names: Vec::new(),
    };
    read_dir_raw(dir, &mut sink)?;
    let names: Names = Rc::new(sink.names);
    for e in sink.out.iter_mut() {
        e.name.names = Some(Rc::clone(&names));
    }
    Ok(())
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
    fn push(&mut self, dir: &Rc<Dir>, rec: &Rec<'_>) {
        // `fstatat` with `AT_SYMLINK_NOFOLLOW` is `lstat`: a symlink is described, not
        // followed.
        match fstat_at(dir.fd(), rec.name) {
            Ok(s) => self.out.push(Entry {
                name: OsString::from_vec(rec.name.to_vec()),
                is_dir: s.kind == Kind::Dir,
                is_link: s.kind == Kind::Symlink,
                // A directory's size is its own bookkeeping, not its contents.
                size: if s.kind == Kind::Dir { 0 } else { s.size },
                alloc: if self.detail { s.alloc() } else { 0 },
                file_id: if self.detail { s.file_id() } else { 0 },
                dev: if self.detail { s.dev } else { 0 },
                modified: s.mtime,
            }),
            Err(err) => {
                self.errs.record(&err);
            }
        }
    }
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
/// and file id, which costs nothing extra here (one `fstatat` per entry either way) but
/// keeps the `readdir` backend's documented "on-disk unknown, no file ids" behaviour.
fn read_with(
    dir: &Path,
    out: &mut Vec<Entry>,
    errs: &mut ErrorCounts,
    detail: bool,
) -> io::Result<()> {
    out.clear();
    read_dir_raw(dir, &mut EntrySink { out, errs, detail })
}

/// The `readdir` backend: names and sizes only, no allocation sizes or file ids.
pub fn read_plain(dir: &Path, out: &mut Vec<Entry>, errs: &mut ErrorCounts) -> io::Result<()> {
    read_with(dir, out, errs, false)
}

/// Err when `path` is itself a symlink: scanning or pruning it would silently act on its
/// target instead. The wording is Linux's, because the callers and the tests match on it.
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

/// Filesystem type names by device, for `-fstype`.
///
/// Darwin keys a filesystem by `f_fsid`, which is not a `dev_t`, so the device comes from
/// `stat`ing each mount point: the same number `Entry::dev` and `dev_of` report, which is
/// what the caller indexes the table with. A mount whose point cannot be stat-ed (a dead
/// network mount, a stale entry) simply contributes nothing, the way a device missing from
/// Linux's `/proc/self/mountinfo` does.
pub fn filesystem_types() -> HashMap<u64, String> {
    let mut out = HashMap::new();
    for (point, name) in mounts() {
        if let Ok(dev) = dev_of(&point) {
            out.insert(dev, name);
        }
    }
    out
}

/// Every mount point the kernel reports, for the guard's volume roots.
///
/// Reading `/Volumes` was the previous answer and covers only the mounts a person or
/// `hdiutil` put there: a filesystem mounted anywhere else (`mount -t nfs ... /srv/data`,
/// a volume under `/private/var/folders` mounted by a test harness) was invisible. The
/// kernel's own table has no such blind spot.
pub fn mount_roots() -> Vec<PathBuf> {
    mounts().into_iter().map(|(point, _)| point).collect()
}

/// The mount table: mount point and filesystem type name, in the order the kernel lists
/// them.
///
/// `getmntinfo` hands back a pointer to an array it keeps and overwrites on the next call,
/// so the records are copied out here and nothing below holds the pointer.
/// `MNT_NOWAIT` keeps a dead mount from blocking the query.
fn mounts() -> Vec<(PathBuf, String)> {
    let mut buf: *mut sys::StatFs = std::ptr::null_mut();
    // SAFETY: `getmntinfo` writes exactly one pointer into `buf`. A non-positive return is
    // an error, checked before `buf` is read.
    let n = unsafe { sys::getmntinfo(&mut buf, sys::MNT_NOWAIT) };
    if n <= 0 || buf.is_null() {
        return Vec::new();
    }
    // SAFETY: the kernel returned `n` consecutive records at `buf`, valid until the next
    // `getmntinfo` call, and the fresh slice does not outlive this copy.
    let records = unsafe { std::slice::from_raw_parts(buf, n as usize) };
    records
        .iter()
        .filter_map(|fs| {
            let point = c_field(&fs.f_mntonname)?;
            let kind = c_field(&fs.f_fstypename)?;
            Some((
                PathBuf::from(OsString::from_vec(point.to_vec())),
                String::from_utf8_lossy(kind).into_owned(),
            ))
        })
        .collect()
}

/// One fixed-width C string field, cut at its NUL.
///
/// Like a directory name, the length is never taken from anything but the terminator: a
/// field with no NUL in its width is not a name this layer can trust, so it yields `None`
/// rather than a truncated guess.
fn c_field<const N: usize>(field: &[c_char; N]) -> Option<&[u8]> {
    // SAFETY: `field` is a live array of exactly `N` bytes, and the slice borrows it for
    // no longer than `field` itself lives.
    let raw = unsafe { std::slice::from_raw_parts(field.as_ptr() as *const u8, N) };
    let nul = raw.iter().position(|&b| b == 0)?;
    Some(&raw[..nul])
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
    use std::fs;
    use std::path::PathBuf;
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
    fn fields_match_std_metadata() {
        use std::os::unix::fs::MetadataExt;
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
        assert!(list(&f.0.join("nope"), &mut Vec::new(), &mut errs).is_err());
    }

    #[test]
    fn a_file_is_not_a_directory() {
        let f = Fixture::new("notdir");
        fs::write(f.0.join("a"), b"x").unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        assert!(DirReader::new()
            .read(&f.0.join("a"), &mut v, &mut errs)
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
    fn lite_types_come_from_the_dirent() {
        let f = Fixture::new("lite");
        fs::create_dir(f.0.join("sub")).unwrap();
        fs::write(f.0.join("a.bin"), vec![0u8; 4096]).unwrap();
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
        let dir = |n: &str| v.iter().find(|e| e.name == n).unwrap().is_dir();
        assert!(dir("sub") && !dir("a.bin") && !dir("link"));
        assert!(v.iter().find(|e| e.name == "link").unwrap().is_reparse());
        // The kinds `find -type` filters on, taken from `d_type` with no `fstatat` behind
        // them: a wrong offset here makes every file answer "not a regular file", and a
        // `-type f` search then prints nothing at all.
        let kind = |n: &str| v.iter().find(|e| e.name == n).unwrap().kind();
        assert_eq!(kind("sub"), Kind::Dir);
        assert_eq!(kind("a.bin"), Kind::File);
        assert_eq!(kind("link"), Kind::Symlink);
        // `list` clears and reuses the caller's vector.
        list(&f.0.join("sub"), &mut v, &mut errs).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn lite_stat_is_exact_and_cached() {
        use std::os::unix::fs::MetadataExt;
        let f = Fixture::new("stat");
        fs::write(f.0.join("a"), vec![0u8; 5000]).unwrap();
        std::os::unix::fs::symlink(f.0.join("a"), f.0.join("lnk")).unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        for e in &v {
            let p = f.0.join(&e.name);
            let md = fs::symlink_metadata(&p).unwrap();
            let s = e.stat().unwrap();
            assert_eq!(
                s.size,
                if md.is_dir() { 0 } else { md.len() },
                "{:?}",
                e.name
            );
            assert_eq!(s.alloc(), md.blocks() * 512, "{:?}", e.name);
            assert_eq!(s.dev, md.dev(), "{:?}", e.name);
            assert_eq!(s.file_id(), (md.dev() as u128) << 64 | md.ino() as u128);
            assert_eq!(s.modified(), md.modified().unwrap(), "{:?}", e.name);
            assert_eq!(
                e.stat().unwrap(),
                s,
                "cached: the same answer, byte for byte"
            );
        }
        // A link stats as itself: its own length, not its target's 5000 bytes.
        let lone = v.iter().find(|e| e.name == "lnk").unwrap();
        assert_eq!(lone.stat().unwrap().kind, Kind::Symlink);
        assert_ne!(lone.stat().unwrap().size, 5000);
        assert_ne!(
            file_id_of(&f.0.join("lnk")).unwrap(),
            file_id_of(&f.0.join("a")).unwrap()
        );
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

    /// `Lite::stat` resolves the name against the directory the entry was read from, so it
    /// keeps working when the path it was reached by no longer leads there — the whole
    /// point of `fstatat(dirfd, name)` over a path-resolving `lstat`.
    #[test]
    fn lite_stat_survives_the_path_being_renamed() {
        let f = Fixture::new("moved");
        fs::create_dir(f.0.join("real")).unwrap();
        fs::write(f.0.join("real/a"), b"12345").unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0.join("real"), &mut v, &mut errs).unwrap();
        fs::rename(f.0.join("real"), f.0.join("moved")).unwrap();
        assert!(!f.0.join("real/a").exists());
        let s = v[0].stat().unwrap();
        assert_eq!(s.size, 5);
        assert_eq!(s.kind, Kind::File);
    }

    #[test]
    fn posix_fields_are_present() {
        use std::os::unix::fs::MetadataExt;
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
        if fs::create_dir(f.0.join(dir)).is_err() {
            // APFS and HFS+ answer a name that is not valid UTF-8 with EILSEQ, so there is
            // nothing to enumerate. The Linux walk still covers this case.
            eprintln!("skipped: this filesystem refuses a name that is not valid UTF-8");
            return;
        }
        fs::write(f.0.join(dir).join(OsStr::from_bytes(b"in\xff.go")), b"1").unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        assert_eq!(errs.total(), 0);
        let e = v
            .iter()
            .find(|e| e.name.as_bytes() == b"bad\xff\xfedir")
            .expect("the name reaches the caller exactly as the kernel gave it");
        assert!(e.is_dir());
        assert_eq!(e.stat().unwrap().kind, Kind::Dir);
        let mut w = Vec::new();
        list(&f.0.join(dir), &mut w, &mut errs).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].name.as_bytes(), b"in\xff.go");
        assert_eq!(w[0].stat().unwrap().size, 1);
    }

    #[test]
    fn a_name_at_the_name_limit() {
        let f = Fixture::new("namelen");
        let exact = "e".repeat(255);
        fs::write(f.0.join(&exact), vec![0u8; 11]).unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].stat().unwrap().size, 11);
    }

    #[test]
    fn names_survive_a_large_directory() {
        let f = Fixture::new("many");
        let want: Vec<String> = (0..2000).map(|i| format!("f{i:05}")).collect();
        for n in &want {
            fs::write(f.0.join(n), b"x").unwrap();
        }
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        assert_eq!(errs.total(), 0);
        let mut got: Vec<String> = v
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, want);
        let mut w: Vec<Entry> = Vec::new();
        DirReader::new().read(&f.0, &mut w, &mut errs).unwrap();
        assert_eq!(w.len(), want.len());
        assert!(w.iter().all(|e| !e.is_dir && e.size == 1 && e.file_id != 0));
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

    /// A record as the kernel would hand it back, with the given name in `d_name`.
    fn record(name: &[u8], d_type: u8, d_namlen: u16) -> sys::Dirent {
        let mut rec = sys::Dirent {
            d_ino: 1,
            d_seekoff: 0,
            d_reclen: 24,
            d_namlen,
            d_type,
            d_name: [0; sys::DIRENT_NAME_MAX],
        };
        rec.d_name[..name.len()].copy_from_slice(name);
        rec
    }

    /// The field that failed the first real macOS run. A length read at the wrong offset
    /// was zero for every record, so every listing died on its first `.`; the name itself
    /// is NUL-terminated and needs no length field at all.
    #[test]
    fn the_name_comes_from_the_terminator_not_the_length_field() {
        for bogus in [0u16, 1, 3, 999, u16::MAX] {
            let rec = record(b"a.txt", sys::DT_REG, bogus);
            assert_eq!(
                c_name(&rec),
                Some(&b"a.txt"[..]),
                "d_namlen={bogus} must not decide the name"
            );
        }
        assert_eq!(c_name(&record(b".", sys::DT_DIR, 1)), Some(&b"."[..]));
        assert_eq!(c_name(&record(b"..", sys::DT_DIR, 2)), Some(&b".."[..]));
    }

    /// The next failure of this kind has to explain itself from the CI log alone.
    #[test]
    fn a_rejected_record_reports_the_fields_an_abi_mistake_moves() {
        let mut rec = record(b"", sys::DT_REG, 7);
        rec.d_reclen = 4096;
        for b in rec.d_name.iter_mut() {
            *b = b'x';
        }
        assert_eq!(c_name(&rec), None, "no terminator at all");
        let text = malformed_record(&rec, "d_name has no NUL terminator").to_string();
        for want in [
            "d_name has no NUL terminator",
            "d_reclen=4096",
            "d_namlen=7",
            "d_type=8",
            "name_strlen=0",
            "record_head=",
        ] {
            assert!(text.contains(want), "{want:?} missing from {text}");
        }
        // The head dump is the raw first 24 bytes: d_ino=1 in little-endian, then
        // d_reclen=4096, then d_namlen=7 and d_type=DT_REG.
        assert!(text.contains("0100000000000000"), "{text}");
        assert!(text.contains("0010"), "{text}");
    }

    /// A type code outside the eight known ones is not a guess: it is `None`, and the
    /// caller asks `fstatat` instead. Getting this wrong decides whether a directory is
    /// descended into.
    #[test]
    fn an_unknown_type_code_is_left_to_fstatat() {
        for code in [0u8, 3, 5, 7, 9, 11, 13, 14, 200, 255] {
            assert_eq!(kind_of_dtype(code), None, "code {code}");
        }
        for (code, kind) in [
            (sys::DT_DIR, Kind::Dir),
            (sys::DT_REG, Kind::File),
            (sys::DT_LNK, Kind::Symlink),
            (sys::DT_FIFO, Kind::Fifo),
            (sys::DT_CHR, Kind::Char),
            (sys::DT_BLK, Kind::Block),
            (sys::DT_SOCK, Kind::Socket),
        ] {
            assert_eq!(kind_of_dtype(code), Some(kind), "code {code}");
        }
    }

    #[test]
    fn kinds_come_from_the_dirent_and_the_mode() {
        assert_eq!(kind_of_dtype(sys::DT_FIFO), Some(Kind::Fifo));
        assert_eq!(kind_of_dtype(sys::DT_CHR), Some(Kind::Char));
        assert_eq!(kind_of_dtype(sys::DT_BLK), Some(Kind::Block));
        assert_eq!(kind_of_dtype(sys::DT_SOCK), Some(Kind::Socket));
        assert_eq!(kind_of_dtype(sys::DT_DIR), Some(Kind::Dir));
        assert_eq!(kind_of_dtype(sys::DT_UNKNOWN), None);
        assert_eq!(kind_of_mode(sys::S_IFIFO | 0o644), Kind::Fifo);
        assert_eq!(kind_of_mode(sys::S_IFCHR | 0o600), Kind::Char);
        assert_eq!(kind_of_mode(sys::S_IFBLK | 0o600), Kind::Block);
        assert_eq!(kind_of_mode(sys::S_IFSOCK | 0o755), Kind::Socket);
        assert_eq!(kind_of_mode(sys::S_IFLNK | 0o777), Kind::Symlink);
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
        assert_eq!(
            time_of(-1, 1_500_000_000),
            UNIX_EPOCH - Duration::new(2, 500_000_000)
        );
    }

    #[test]
    fn the_name_buffer_is_reused_without_truncation() {
        let f = Fixture::new("namebuf");
        let long = "l".repeat(250);
        fs::write(f.0.join(&long), vec![0u8; 300]).unwrap();
        fs::write(f.0.join("s"), vec![0u8; 7]).unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        let stat = |n: &str| v.iter().find(|e| e.name == n).unwrap().stat().unwrap();
        assert_eq!(stat(&long).size, 300);
        assert_eq!(stat("s").size, 7);
        assert_eq!(stat(&long).size, 300);
        assert_eq!(stat(&long).kind, Kind::File);
    }

    #[test]
    fn a_name_with_a_nul_cannot_be_stat_ed() {
        let f = Fixture::new("nul");
        fs::write(f.0.join("a"), b"x").unwrap();
        let mut v = Vec::new();
        let mut errs = ErrorCounts::default();
        list(&f.0, &mut v, &mut errs).unwrap();
        let bad = Name::owned(b"a\0b");
        // Either kind is the right answer: this is our own conversion refusing the name,
        // not a filesystem error. What matters is that it is an error and not a stat of
        // some other file.
        let kind = fstat_at(v[0].dir.fd(), bad.slice()).unwrap_err().kind();
        assert!(
            matches!(
                kind,
                io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData
            ),
            "{kind:?}"
        );
    }

    /// The mount table is the only source for volume roots and `-fstype`, and this is the
    /// assertion that fails if `struct statfs` is bound at the wrong offsets: a field read
    /// four bytes early yields a mount point that is not a path at all.
    #[test]
    fn the_mount_records_are_read_at_the_right_offsets() {
        let mut buf: *mut sys::StatFs = std::ptr::null_mut();
        // SAFETY: `getmntinfo` writes one pointer; the count gates the read below.
        let n = unsafe { sys::getmntinfo(&mut buf, sys::MNT_NOWAIT) };
        assert!(n > 0, "getmntinfo returned {n}");
        assert!(!buf.is_null(), "getmntinfo returned a null pointer");
        // SAFETY: `n` consecutive records at `buf`, as above.
        let records = unsafe { std::slice::from_raw_parts(buf, n as usize) };
        for fs in records {
            let point = c_field(&fs.f_mntonname).expect("a mount point with no terminator");
            assert!(
                point.starts_with(b"/"),
                "mount point {:?} is not absolute",
                String::from_utf8_lossy(point)
            );
            let kind = c_field(&fs.f_fstypename).expect("a type name with no terminator");
            assert!(
                !kind.is_empty(),
                "{:?} has no type name",
                String::from_utf8_lossy(point)
            );
            assert!(
                kind.iter().all(u8::is_ascii_alphanumeric),
                "type name {:?} is not a filesystem name",
                String::from_utf8_lossy(kind)
            );
            // A block size is a power of two of at least 512 bytes on every filesystem
            // this tool could meet; the field is the first in the struct, so a wrong
            // offset shows up here rather than as a plausible-looking path.
            assert!(
                fs.f_bsize >= 512 && fs.f_bsize.is_power_of_two(),
                "block size {} for {:?}",
                fs.f_bsize,
                String::from_utf8_lossy(point)
            );
        }
    }

    #[test]
    fn the_mount_table_names_the_root_filesystem() {
        let mounts = mounts();
        assert!(!mounts.is_empty(), "the kernel reported no mounts");
        let (_, kind) = mounts
            .iter()
            .find(|(p, _)| p == Path::new("/"))
            .expect("no mount for the root filesystem");
        assert!(!kind.is_empty(), "the root filesystem has no type name");
    }

    #[test]
    fn the_type_table_is_keyed_by_the_device_stat_reports() {
        let types = filesystem_types();
        assert!(
            !types.is_empty(),
            "no filesystem types: `-fstype` would match nothing"
        );
        let dev = dev_of(Path::new("/")).expect("stat the root");
        let name = types
            .get(&dev)
            .unwrap_or_else(|| panic!("the root device {dev} has no type name"));
        assert!(
            !name.is_empty(),
            "the root device {dev} has an empty type name"
        );
    }

    #[test]
    fn every_mount_root_is_listed_as_its_own_path() {
        let roots = mount_roots();
        assert!(!roots.is_empty(), "the kernel reported no mount points");
        assert!(
            roots.iter().any(|p| p == Path::new("/")),
            "the root is missing from {roots:?}"
        );
    }
}

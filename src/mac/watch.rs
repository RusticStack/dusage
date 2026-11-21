//! Change feed for `--watch` on macOS, over kqueue.
//!
//! `EVFILT_VNODE` is registered once per directory on a single kqueue, so one descriptor
//! covers the whole watch: the event's `ident` is the descriptor of the directory that
//! changed, and `dirs` maps it back to the path the scanner walks. `poll` waits for the
//! first event, then drains until nothing is ready, and reports the affected entry paths
//! so the scanner can rescan just those top-level entries.
//!
//! kqueue names no entry, only the directory whose contents changed. A change inside a
//! subdirectory therefore reports that subdirectory, which [`scan::top_for`] attributes to
//! the top-level entry containing it; a change in the root itself reports the root's
//! children, which costs one directory read and is the only way to name what moved.
//!
//! `struct kevent` is pinned by the `const` assertions at the bottom of this file, which
//! run during a cross-check from Linux as well as on a Mac.
//!
//! # Watch budget and what it costs
//!
//! A kqueue watch is per-descriptor: every watched directory holds an open file
//! descriptor in this process. A tree of 100k directories cannot be watched that way at
//! all, and the default `RLIMIT_NOFILE` on macOS is far below it, so the feed admits at
//! most [`MAX_WATCHED_DIRS`] directories. When a tree reaches that ceiling
//! [`ChangeSource::open`] returns `None` rather than watching a prefix of it: a partial
//! watch would silently never report half the tree, while no watch at all falls back to
//! the full rescan the caller already implements. This is a behaviour change from Linux,
//! whose inotify feed covers every directory or gives up entirely; on macOS a tree larger
//! than the budget is reported only by the periodic full rescan.

use crate::mac::enumerate::{self, DirReader};
use crate::mac::sys;
use crate::model::ErrorCounts;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The most directories this feed will watch. Each one holds an open descriptor, so the
/// ceiling exists to stay under `RLIMIT_NOFILE` rather than to bound memory.
pub const MAX_WATCHED_DIRS: usize = 4096;

// kqueue (sys/event.h).
const EVFILT_VNODE: i16 = -4;
const EV_ADD: u16 = 0x0001;
const EV_ENABLE: u16 = 0x0004;
const EV_CLEAR: u16 = 0x0020;
const EV_ERROR: u16 = 0x4000;
const NOTE_WRITE: u32 = 0x0002;
const NOTE_DELETE: u32 = 0x0001;
const NOTE_EXTEND: u32 = 0x0004;
const NOTE_ATTRIB: u32 = 0x0008;
const NOTE_LINK: u32 = 0x0010;
const NOTE_RENAME: u32 = 0x0020;
const NOTE_REVOKE: u32 = 0x0040;

const VNODE_EVENTS: u32 =
    NOTE_DELETE | NOTE_WRITE | NOTE_EXTEND | NOTE_ATTRIB | NOTE_LINK | NOTE_RENAME | NOTE_REVOKE;

/// One `kevent` batch. Small: kqueue returns at most this many per call and the loop
/// drains until it comes back empty.
const BATCH: usize = 64;

/// `struct kevent` (sys/event.h) for the 64-bit Darwin ABI.
#[repr(C)]
#[derive(Clone, Copy)]
struct Kevent {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
    udata: *mut c_void,
    ext: [u64; 4],
}

/// `struct timespec` (sys/_types/_timespec.h).
#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

impl Timespec {
    fn new(d: Duration) -> Timespec {
        Timespec {
            tv_sec: d.as_secs().min(i64::MAX as u64) as i64,
            tv_nsec: i64::from(d.subsec_nanos()),
        }
    }
}

const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Kevent>() == 64, "struct kevent is 64 bytes");
    assert!(offset_of!(Kevent, ident) == 0);
    assert!(offset_of!(Kevent, filter) == 8);
    assert!(offset_of!(Kevent, flags) == 10);
    assert!(offset_of!(Kevent, fflags) == 12);
    assert!(offset_of!(Kevent, data) == 16);
    assert!(offset_of!(Kevent, udata) == 24);
    assert!(offset_of!(Kevent, ext) == 32);
    assert!(size_of::<Timespec>() == 16);
    assert!(offset_of!(Timespec, tv_sec) == 0);
    assert!(offset_of!(Timespec, tv_nsec) == 8);
};

extern "C" {
    fn kqueue() -> i32;
    fn kevent(
        kq: i32,
        changelist: *const Kevent,
        nchanges: i32,
        eventlist: *mut Kevent,
        nevents: i32,
        timeout: *const Timespec,
    ) -> i32;
}

/// The root as the kernel reports it (symlinks resolved), so watch descriptors map back
/// to the paths the scanner walks.
pub fn resolve_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

pub struct ChangeSource {
    kq: OwnedFd,
    /// Descriptor of a watched directory -> its path, and the descriptor it holds open.
    dirs: HashMap<RawFd, (OwnedFd, PathBuf)>,
    /// The same paths, for the "already watched" test.
    watched: HashSet<PathBuf>,
    root: PathBuf,
}

impl ChangeSource {
    /// `None` when kqueue is unavailable or the tree is larger than the watch budget; the
    /// caller then rescans the whole tree every interval.
    pub fn open(root: &Path) -> Option<Self> {
        Self::open_err(root).ok()
    }

    /// Like `open`, but says why the feed is unusable: no kqueue, or the watch budget ran
    /// out before the tree was covered.
    pub fn open_err(root: &Path) -> io::Result<Self> {
        // SAFETY: `kqueue` takes no arguments and returns a new descriptor or -1.
        let raw = unsafe { kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` was just returned by `kqueue` and is owned from here on.
        let kq = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut s = ChangeSource {
            kq,
            dirs: HashMap::new(),
            watched: HashSet::new(),
            root: root.to_path_buf(),
        };
        if !s.watch_tree(root) {
            return Err(io::Error::other("kqueue watch budget exhausted"));
        }
        Ok(s)
    }

    /// Register one directory. False when the budget is spent.
    fn add(&mut self, dir: &Path) -> bool {
        if self.watched.contains(dir) {
            return true;
        }
        if self.watched.len() >= MAX_WATCHED_DIRS {
            return false;
        }
        let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
            return true;
        };
        // SAFETY: `c` is NUL-terminated and outlives the call; the flags are a read-only
        // directory open, and a failure is -1 with errno set.
        let raw = unsafe {
            sys::open(
                c.as_ptr(),
                sys::O_RDONLY | sys::O_DIRECTORY | sys::O_CLOEXEC | sys::O_NONBLOCK,
                0,
            )
        };
        if raw < 0 {
            // An unreadable directory is skipped, as inotify skips one it cannot watch.
            return true;
        }
        // SAFETY: `raw` is a descriptor this call just opened.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let ev = Kevent {
            ident: fd.as_raw_fd() as usize,
            filter: EVFILT_VNODE,
            flags: EV_ADD | EV_ENABLE | EV_CLEAR,
            fflags: VNODE_EVENTS,
            data: 0,
            udata: std::ptr::null_mut(),
            ext: [0; 4],
        };
        // SAFETY: one change is passed with a count of 1 and no event list, so the kernel
        // registers it and returns 0.
        let rc = unsafe {
            kevent(
                self.kq.as_raw_fd(),
                &ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if rc < 0 {
            return true;
        }
        self.watched.insert(dir.to_path_buf());
        self.dirs.insert(fd.as_raw_fd(), (fd, dir.to_path_buf()));
        true
    }

    /// Watch `dir` and every directory below it. False when the budget ran out first.
    fn watch_tree(&mut self, dir: &Path) -> bool {
        let mut reader = DirReader::new();
        let mut errs = ErrorCounts::default();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            if !self.add(&d) {
                return false;
            }
            let mut ents = Vec::new();
            if reader.read(&d, &mut ents, &mut errs).is_ok() {
                for e in &ents {
                    if e.is_dir && !e.is_reparse() {
                        stack.push(d.join(&e.name));
                    }
                }
            }
        }
        true
    }

    /// One `kevent` call: the descriptors that changed within `timeout`.
    fn wait(&mut self, timeout: Duration) -> io::Result<Vec<RawFd>> {
        let ts = Timespec::new(timeout);
        let mut buf = [Kevent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
            ext: [0; 4],
        }; BATCH];
        // SAFETY: no changes are passed; `buf` is a live array of BATCH events for the
        // kernel to fill, and `ts` is a live `struct timespec`.
        let n = unsafe {
            kevent(
                self.kq.as_raw_fd(),
                std::ptr::null(),
                0,
                buf.as_mut_ptr(),
                BATCH as i32,
                &ts,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(Vec::new());
            }
            return Err(e);
        }
        let mut out = Vec::new();
        for ev in buf.iter().take(n as usize) {
            // A registration that failed is reported as an event with `EV_ERROR` and the
            // errno in `data`; a revoked volume has nothing left to watch. Either way the
            // events after it cannot be trusted, which is the overflow answer inotify
            // gives: hand the caller `Err` and let it reopen and rescan.
            if ev.flags & EV_ERROR != 0 || ev.fflags & NOTE_REVOKE != 0 {
                return Err(io::Error::other("kqueue watch lost"));
            }
            out.push(ev.ident as RawFd);
        }
        Ok(out)
    }

    /// The entry paths a change to `fd`'s directory affects.
    fn paths_of(&self, fd: RawFd) -> Vec<PathBuf> {
        let Some((_, dir)) = self.dirs.get(&fd) else {
            return Vec::new();
        };
        if *dir != self.root {
            return vec![dir.clone()];
        }
        // A change in the root itself names no entry: report its children, which is the
        // same set a full rescan would cover, and costs one directory read.
        let mut ents = Vec::new();
        if enumerate::list(&self.root, &mut ents, &mut ErrorCounts::default()).is_err() {
            return Vec::new();
        }
        ents.iter().map(|e| self.root.join(&e.name)).collect()
    }

    /// Watch the subdirectories a changed directory has gained.
    fn watch_children(&mut self, fd: RawFd) {
        let Some(dir) = self.dirs.get(&fd).map(|(_, p)| p.clone()) else {
            return;
        };
        let mut ents = Vec::new();
        if enumerate::list(&dir, &mut ents, &mut ErrorCounts::default()).is_err() {
            return;
        }
        for e in &ents {
            if e.is_dir() && !e.is_reparse() {
                self.add(&dir.join(&e.name));
            }
        }
    }

    /// Block up to `interval` for the first change, then collect the burst.
    pub fn poll(&mut self, interval: Duration) -> io::Result<Vec<PathBuf>> {
        let mut changed = self.wait(interval)?;
        if changed.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(changed.len());
        loop {
            for fd in &changed {
                out.extend(self.paths_of(*fd));
            }
            for fd in &changed {
                self.watch_children(*fd);
            }
            // A burst arrives as several ready events; keep reading until nothing is left
            // so one scan covers the whole of it.
            changed = self.wait(Duration::ZERO)?;
            if changed.is_empty() {
                return Ok(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "dusage_mac_kq_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Names relative to the root the feed was opened with, which is the spelling the
    /// caller gave: the feed reports paths under that root and never a resolved one.
    fn names(paths: &[PathBuf], root: &Path) -> Vec<String> {
        let mut v: Vec<String> = paths
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn reports_created_and_removed_entries() {
        let root = tmp("create");
        std::fs::create_dir(root.join("sub")).unwrap();
        let mut src = ChangeSource::open(&root).expect("kqueue available");
        assert!(src.poll(Duration::from_millis(10)).unwrap().is_empty());
        std::fs::write(root.join("sub/new.bin"), b"x").unwrap();
        let got = names(&src.poll(Duration::from_millis(2000)).unwrap(), &root);
        assert_eq!(got, ["sub"]);
        std::fs::remove_file(root.join("sub/new.bin")).unwrap();
        let got = names(&src.poll(Duration::from_millis(2000)).unwrap(), &root);
        assert_eq!(got, ["sub"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_change_in_the_root_reports_its_children() {
        let root = tmp("rootchurn");
        std::fs::write(root.join("a.txt"), b"x").unwrap();
        let mut src = ChangeSource::open(&root).expect("kqueue available");
        std::fs::write(root.join("b.txt"), b"x").unwrap();
        let got = names(&src.poll(Duration::from_millis(2000)).unwrap(), &root);
        // The parent is named because kqueue reports the directory, never the entry.
        assert!(got.contains(&"a.txt".to_string()), "{got:?}");
        assert!(got.contains(&"b.txt".to_string()), "{got:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_new_directory_is_watched() {
        let root = tmp("newdir");
        let mut src = ChangeSource::open(&root).expect("kqueue available");
        let late = root.join("late");
        std::fs::create_dir(&late).unwrap();
        let got = names(&src.poll(Duration::from_millis(2000)).unwrap(), &root);
        assert!(got.contains(&"late".to_string()), "{got:?}");
        std::fs::write(late.join("inside.txt"), b"x").unwrap();
        let got = names(&src.poll(Duration::from_millis(2000)).unwrap(), &root);
        assert!(got.contains(&"late".to_string()), "{got:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The contract the Linux feed has: paths come back under the root the caller passed,
    /// in that spelling. On macOS the system temp dir is `/var/folders/...` and resolves to
    /// `/private/var/folders/...`, so a feed that reported the resolved form would hand the
    /// scanner paths from a corner of the filesystem it never named.
    #[test]
    fn poll_reports_paths_in_the_spelling_the_caller_gave() {
        let root = tmp("spelling");
        std::fs::create_dir(root.join("sub")).unwrap();
        let mut src = ChangeSource::open(&root).expect("kqueue available");
        std::fs::write(root.join("sub/a.bin"), b"x").unwrap();
        let got = src.poll(Duration::from_millis(2000)).unwrap();
        assert!(!got.is_empty(), "the write must be seen");
        let resolved = resolve_root(&root);
        for p in &got {
            assert!(
                p.starts_with(&root),
                "{p:?} is not under the root given, {:?}",
                root
            );
            if resolved != root {
                assert!(!p.starts_with(&resolved), "{p:?} came back resolved");
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn poll_honours_its_timeout() {
        let root = tmp("timeout");
        let mut src = ChangeSource::open(&root).unwrap();
        let t = Instant::now();
        assert!(src.poll(Duration::from_millis(120)).unwrap().is_empty());
        assert!(
            t.elapsed() >= Duration::from_millis(100),
            "{:?}",
            t.elapsed()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_tree_past_the_budget_refuses_the_whole_feed() {
        let root = tmp("budget");
        let mut s = ChangeSource::open(&root).unwrap();
        // Fill the budget by hand rather than on disk: the point is what `add` answers
        // once it is spent.
        for i in 0..MAX_WATCHED_DIRS {
            s.watched.insert(root.join(format!("d{i}")));
        }
        assert!(!s.add(&root.join("one-too-many")));
        let _ = std::fs::remove_dir_all(root);
    }
}

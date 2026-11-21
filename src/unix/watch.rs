//! Change feed for `--watch` on Linux, over inotify.
//!
//! The whole tree under the root is watched (one watch descriptor per directory,
//! added for new directories as they appear). `poll` waits for the first event,
//! then drains the queue until it goes quiet, and reports the affected entry
//! paths so the scanner can rescan just those top-level entries. An overflowed
//! queue returns `Err`, which makes the caller fall back to a full rescan.

use crate::model::ErrorCounts;
use crate::unix::enumerate::DirReader;
use crate::unix::sys;
use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const WATCH_MASK: u32 = sys::IN_CREATE
    | sys::IN_DELETE
    | sys::IN_MODIFY
    | sys::IN_ATTRIB
    | sys::IN_CLOSE_WRITE
    | sys::IN_MOVED_FROM
    | sys::IN_MOVED_TO
    | sys::IN_DELETE_SELF
    | sys::IN_MOVE_SELF
    | sys::IN_ONLYDIR;

/// Coalescing window: after the first event, keep reading until this long passes
/// without a new one, so a burst of writes becomes one rescan.
const QUIET: Duration = Duration::from_millis(50);
const BUFFER: usize = 8192;
const EVENT_HEADER: usize = 16;

/// The root as the kernel reports it (symlinks resolved), so watch descriptors map
/// back to the paths the scanner walks.
pub fn resolve_root(root: &Path) -> PathBuf {
    fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

pub struct ChangeSource {
    fd: OwnedFd,
    dirs: HashMap<RawFd, PathBuf>,
}

impl ChangeSource {
    /// `None` when inotify is unavailable or the tree exceeds the watch limit; the
    /// caller then rescans the whole tree every interval.
    pub fn open(root: &Path) -> Option<Self> {
        Self::open_err(root).ok()
    }

    /// Like `open`, but says why the feed is unusable: no inotify, or the per-user
    /// watch limit was reached before the tree was covered.
    pub fn open_err(root: &Path) -> io::Result<Self> {
        // SAFETY: inotify_init1 takes only flags and returns a new file descriptor.
        let fd = unsafe { sys::inotify_init1(sys::IN_NONBLOCK | sys::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by inotify_init1 and is owned from here on.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut s = ChangeSource {
            fd,
            dirs: HashMap::new(),
        };
        s.watch_tree(root)?;
        Ok(s)
    }

    fn add(&mut self, dir: &Path) {
        let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
            return;
        };
        // SAFETY: the descriptor is open for the lifetime of `self`, and `c` is a
        // NUL-terminated path that outlives the call.
        let wd = unsafe { sys::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), WATCH_MASK) };
        if wd >= 0 {
            self.dirs.insert(wd, dir.to_path_buf());
        }
    }

    /// Watch `dir` and every directory below it.
    fn watch_tree(&mut self, dir: &Path) -> io::Result<()> {
        let mut reader = DirReader::new();
        let mut errs = ErrorCounts::default();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            self.add(&d);
            let mut ents = Vec::new();
            if reader.read(&d, &mut ents, &mut errs).is_ok() {
                for e in &ents {
                    if e.is_dir && !e.is_reparse() {
                        stack.push(d.join(&e.name));
                    }
                }
            }
        }
        if self.dirs.is_empty() {
            return Err(io::Error::other("inotify watch limit"));
        }
        Ok(())
    }

    fn wait(&self, timeout: Duration) -> io::Result<bool> {
        let mut pfd = sys::PollFd {
            fd: self.fd.as_raw_fd(),
            events: sys::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd is passed with a count of 1.
        let n = unsafe { sys::poll(&mut pfd, 1, ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e);
        }
        Ok(n > 0 && pfd.revents & sys::POLLIN != 0)
    }

    /// Drain queued events into (owning directory, entry name) pairs.
    fn drain(&mut self) -> io::Result<Vec<(PathBuf, PathBuf)>> {
        let mut out = Vec::new();
        let mut buf = [0u8; BUFFER];
        loop {
            // SAFETY: the buffer is valid for its whole length and the descriptor is open.
            let n = unsafe {
                sys::read(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len(),
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(sys::EAGAIN) {
                    return Ok(out);
                }
                return Err(e);
            }
            if n == 0 {
                return Ok(out);
            }
            let end = n as usize;
            let mut off = 0;
            while off + EVENT_HEADER <= end {
                let wd = i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
                let mask = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap());
                let len = u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
                let name_start = off + EVENT_HEADER;
                if name_start + len > end {
                    return Ok(out);
                }
                let raw = &buf[name_start..name_start + len];
                let name_end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                off = name_start + len;

                if mask & sys::IN_Q_OVERFLOW != 0 {
                    return Err(io::Error::other("inotify queue overflow"));
                }
                if mask & sys::IN_IGNORED != 0 {
                    self.dirs.remove(&wd);
                    continue;
                }
                let Some(dir) = self.dirs.get(&wd).cloned() else {
                    continue;
                };
                let name = std::ffi::OsStr::from_bytes(&raw[..name_end]);
                let path = dir.join(name);
                if mask & (sys::IN_CREATE | sys::IN_MOVED_TO) != 0 && mask & sys::IN_ISDIR != 0 {
                    let _ = self.watch_tree(&path);
                }
                // A self event (DELETE_SELF, MOVE_SELF) has an empty name, so `path`
                // is the directory itself.
                out.push((dir, path));
            }
        }
    }

    /// Block up to `interval` for the first change, then collect the burst.
    pub fn poll(&mut self, interval: Duration) -> io::Result<Vec<PathBuf>> {
        if !self.wait(interval)? {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        loop {
            let batch = self.drain()?;
            if batch.is_empty() {
                return Ok(paths);
            }
            paths.extend(batch.into_iter().map(|(_, p)| p));
            if !self.wait(QUIET)? {
                return Ok(paths);
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
            "dusage_unix_inotify_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

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
    fn reports_created_and_removed_paths() {
        let root = tmp("create");
        fs::create_dir(root.join("sub")).unwrap();
        let mut src = ChangeSource::open(&resolve_root(&root)).expect("inotify available");
        assert!(src.poll(Duration::from_millis(10)).unwrap().is_empty());
        fs::write(root.join("sub/new.bin"), b"x").unwrap();
        let got = names(&src.poll(Duration::from_millis(2000)).unwrap(), &root);
        assert_eq!(got, ["sub/new.bin"]);
        fs::remove_file(root.join("sub/new.bin")).unwrap();
        let got = names(&src.poll(Duration::from_millis(2000)).unwrap(), &root);
        assert_eq!(got, ["sub/new.bin"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn new_directory_is_watched() {
        let root = tmp("newdir");
        let mut src = ChangeSource::open(&resolve_root(&root)).expect("inotify available");
        let late = root.join("late");
        fs::create_dir(&late).unwrap();
        let got = names(&src.poll(Duration::from_millis(2000)).unwrap(), &root);
        assert_eq!(got, ["late"]);
        fs::write(late.join("inside.txt"), b"x").unwrap();
        let got = names(&src.poll(Duration::from_millis(2000)).unwrap(), &root);
        assert_eq!(got, ["late/inside.txt"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn poll_honours_its_timeout() {
        let root = tmp("timeout");
        let mut src = ChangeSource::open(&resolve_root(&root)).unwrap();
        let t = Instant::now();
        assert!(src.poll(Duration::from_millis(120)).unwrap().is_empty());
        assert!(
            t.elapsed() >= Duration::from_millis(100),
            "{:?}",
            t.elapsed()
        );
        let _ = fs::remove_dir_all(&root);
    }
}

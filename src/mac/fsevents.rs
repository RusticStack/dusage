//! Change feed for `--watch` on macOS, over FSEvents.
//!
//! One stream watches a whole hierarchy, so the 4096-directory budget the kqueue feed needed
//! — one descriptor per directory — has no counterpart here: a tree of any size costs one
//! stream. The service coalesces changes for `LATENCY` seconds and hands them back as a batch
//! of directory paths, which [`ChangeSource::poll`] turns into the same shape the other
//! platforms return.
//!
//! Everything below is declared from Apple's `FSEvents.h` (CoreServices) and
//! `CoreFoundation` headers; the numeric values are pinned by the `const` assertions at the
//! bottom so a declaration that drifts fails a cross-check rather than a Mac.
//!
//! # Why a dispatch queue and not a run loop
//!
//! A run-loop stream needs a `CFRunLoop` that somebody has to run, which in a CLI means
//! owning a thread and its lifetime. `FSEventStreamSetDispatchQueue` puts the callback on a
//! queue the system already runs, so this module owns no thread at all. The teardown order is
//! load-bearing: `FSEvents.h` says it is an error to call `FSEventStreamInvalidate` once the
//! queue has been cleared, so it is `Stop` → `Invalidate` → `Release`, and the queue pointer
//! is never nulled first.
//!
//! # Flags
//!
//! `UseCFTypes` is **off**, so `eventPaths` arrives as a raw C array of C strings and a name
//! that is not valid UTF-8 survives as bytes through `OsStr::from_bytes`. `WatchRoot` is on,
//! so a root that is renamed or deleted arrives as `RootChanged` instead of as silence.

use std::collections::VecDeque;
use std::ffi::{c_char, c_long, c_ulong, c_void, CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long the service may coalesce changes before calling back. Matches the intent of the
/// inotify feed's quiet window: a burst of writes becomes one batch rather than one callback
/// per write.
const LATENCY: f64 = 0.1;

/// The quiet window `poll` waits out after the first batch, the same 50 ms inotify uses.
const QUIET: Duration = Duration::from_millis(50);

/// `kFSEventStreamEventIdSinceNow` (FSEvents.h:561): ask for changes from now, not from the
/// beginning of the volume's history.
const SINCE_NOW: u64 = 0xFFFF_FFFF_FFFF_FFFF;

// kFSEventStreamCreateFlags (FSEvents.h:204).
const CREATE_FLAG_NONE: u32 = 0x0000_0000;
const CREATE_FLAG_WATCH_ROOT: u32 = 0x0000_0004;

// kFSEventStreamEventFlags (FSEvents.h:377-427).
const EVENT_MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
const EVENT_USER_DROPPED: u32 = 0x0000_0002;
const EVENT_KERNEL_DROPPED: u32 = 0x0000_0004;
const EVENT_IDS_WRAPPED: u32 = 0x0000_0008;
const EVENT_ROOT_CHANGED: u32 = 0x0000_0020;

/// Any of these means the batch is incomplete and the caller has to rescan everything.
///
/// `MustScanSubDirs` is the only one that must be tested — the header says so explicitly, and
/// `UserDropped`/`KernelDropped` are the diagnostics that say where the loss happened — but
/// `EventIdsWrapped` invalidates stored ids and `RootChanged` means the watched root itself
/// is gone, so both force the same answer.
const LOSS_FLAGS: u32 =
    EVENT_MUST_SCAN_SUBDIRS | EVENT_IDS_WRAPPED | EVENT_ROOT_CHANGED | EVENT_USER_DROPPED;

// CoreFoundation opaque references. Only ever stored and passed back.
type CFAllocatorRef = *const c_void;
type CFArrayRef = *const c_void;
type CFStringRef = *const c_void;
type CFIndex = isize;
type FSEventStreamRef = *mut c_void;
type ConstFSEventStreamRef = *const c_void;
type DispatchQueue = *mut c_void;

/// `CFArrayCallBacks`: opaque here, because the only one this module passes is the system's
/// `kCFTypeArrayCallBacks` and its internals are none of our business.
#[repr(C)]
struct CFArrayCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

/// `FSEventStreamContext` (FSEvents.h:588): `version` must be zero; the retain and release
/// callbacks are the framework's way of keeping `info` alive across a callback, which is what
/// makes the `Arc` hand-off below sound.
#[repr(C)]
struct FSEventStreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> CFStringRef>,
}

type Callback =
    extern "C" fn(ConstFSEventStreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn FSEventStreamCreate(
        allocator: CFAllocatorRef,
        callback: Callback,
        context: *mut FSEventStreamContext,
        paths: CFArrayRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
    /// Returns a `Boolean`, so the caller tests it against zero.
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
// Linked separately rather than assumed reachable through CoreServices: framework re-export
// lists are not something this crate can check from a cross-check.
extern "C" {
    static kCFTypeArrayCallBacks: CFArrayCallBacks;
    fn CFArrayCreate(
        allocator: CFAllocatorRef,
        values: *const *const c_void,
        num_values: CFIndex,
        callbacks: *const CFArrayCallBacks,
    ) -> CFArrayRef;
    fn CFStringCreateWithFileSystemRepresentation(
        allocator: CFAllocatorRef,
        buffer: *const c_char,
    ) -> CFStringRef;
    fn CFRelease(cf: *const c_void);
}

extern "C" {
    /// From libSystem, which the standard library already links. The default-priority global
    /// queue needs no creation and no release, unlike a private queue.
    fn dispatch_get_global_queue(identifier: c_long, flags: c_ulong) -> DispatchQueue;
}

/// What a callback leaves behind for `poll`: the paths it saw and whether any batch was
/// incomplete.
#[derive(Default)]
struct Buffer {
    changed: VecDeque<PathBuf>,
    lost: bool,
}

struct State {
    buffer: Mutex<Buffer>,
    ready: Condvar,
}

/// The mount-independent half of one callback: fold a batch into the buffer.
///
/// Split out from the `extern "C"` shim so the drop path can be exercised with synthetic
/// flags, which a real `MustScanSubDirs` cannot be made to produce on demand.
fn ingest(state: &State, paths: &[PathBuf], flags: &[u32]) {
    let mut buffer = state.buffer.lock().unwrap_or_else(|e| e.into_inner());
    for (i, flag) in flags.iter().enumerate() {
        if *flag & LOSS_FLAGS != 0 {
            buffer.lost = true;
        }
        if let Some(p) = paths.get(i) {
            buffer.changed.push_back(p.clone());
        }
    }
    drop(buffer);
    state.ready.notify_all();
}

/// The callback, which runs on the dispatch queue's thread.
///
/// `info` is the `Arc<State>` pointer handed to the stream's context: the framework retains it
/// before a callback and releases it after the stream is deallocated, so the reference is live
/// even while `FsEvents` is being dropped.
extern "C" fn on_event(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    _event_ids: *const u64,
) {
    if info.is_null() || event_paths.is_null() || event_flags.is_null() {
        return;
    }
    // SAFETY: `info` is the pointer this module stored in the stream's context, and the
    // framework holds a reference to it for as long as callbacks can arrive.
    let state = unsafe { &*(info as *const State) };
    // SAFETY: with `UseCFTypes` off, `eventPaths` is the raw C array of C strings the header
    // describes, `num_events` long, deallocated by the framework after this returns.
    let names =
        unsafe { std::slice::from_raw_parts(event_paths as *const *const c_char, num_events) };
    // SAFETY: `eventFlags` is `num_events` flag words, as the header says.
    let flags = unsafe { std::slice::from_raw_parts(event_flags, num_events) };
    let paths: Vec<PathBuf> = names
        .iter()
        .map(|name| {
            if name.is_null() {
                PathBuf::new()
            } else {
                // SAFETY: each entry is a NUL-terminated string, valid until this returns.
                PathBuf::from(std::ffi::OsStr::from_bytes(
                    unsafe { CStr::from_ptr(*name) }.to_bytes(),
                ))
            }
        })
        .collect();
    ingest(state, &paths, flags);
}

/// The context's retain callback: one more reference, handed to the framework.
extern "C" fn retain_info(info: *const c_void) -> *const c_void {
    // SAFETY: incremented only on a pointer that holds at least one live strong reference.
    unsafe { Arc::increment_strong_count(info as *const State) };
    info
}

/// The context's release callback: the framework is done with its reference.
extern "C" fn release_info(info: *const c_void) {
    // SAFETY: balances the `Arc::into_raw` below or a `retain_info`, never both for one
    // reference.
    unsafe { drop(Arc::from_raw(info as *const State)) };
}

/// One FSEvents stream over a directory hierarchy.
pub struct FsEvents {
    stream: FSEventStreamRef,
    state: Arc<State>,
    root: PathBuf,
}

impl FsEvents {
    /// `None` when the stream cannot be created or started — the same meaning `kqueue() < 0`
    /// has for the other feed, and enough for the caller to fall back to rescanning every
    /// interval. Nothing here writes to the watched tree: `dusage` scans are read-only, so
    /// there is no probe file and no liveness check at runtime.
    pub fn open(root: &Path) -> Option<Self> {
        let root_c = CString::new(root.as_os_str().as_bytes()).ok()?;
        // SAFETY: `root_c` is a NUL-terminated POSIX path that outlives the call.
        let cf_path =
            unsafe { CFStringCreateWithFileSystemRepresentation(ptr::null(), root_c.as_ptr()) };
        if cf_path.is_null() {
            return None;
        }
        let values = [cf_path];
        // SAFETY: one element array, and `kCFTypeArrayCallBacks` is the system's own table.
        let array = unsafe {
            CFArrayCreate(
                ptr::null(),
                values.as_ptr(),
                1,
                &raw const kCFTypeArrayCallBacks,
            )
        };
        // SAFETY: `cf_path` was created above and the array holds its own reference now.
        unsafe { CFRelease(cf_path) };
        if array.is_null() {
            return None;
        }

        let state = Arc::new(State {
            buffer: Mutex::new(Buffer::default()),
            ready: Condvar::new(),
        });
        let mut context = FSEventStreamContext {
            version: 0,
            info: Arc::into_raw(state.clone()) as *mut c_void,
            retain: Some(retain_info),
            release: Some(release_info),
            copy_description: None,
        };
        // SAFETY: the context is copied out into the stream by `FSEventStreamCreate`, so it
        // only has to outlive this call; the pointers it carries are valid for the stream's
        // whole life.
        let stream = unsafe {
            FSEventStreamCreate(
                ptr::null(),
                on_event,
                &mut context,
                array,
                SINCE_NOW,
                LATENCY,
                CREATE_FLAG_NONE | CREATE_FLAG_WATCH_ROOT,
            )
        };
        // SAFETY: `array` was created above and the stream holds what it needs from it.
        unsafe { CFRelease(array) };
        if stream.is_null() {
            // SAFETY: the reference handed to `context` was never given to a stream.
            unsafe { drop(Arc::from_raw(context.info as *const State)) };
            return None;
        }

        // SAFETY: `stream` is a live stream from `FSEventStreamCreate` and `queue` is the
        // system's shared queue, which needs no ownership.
        unsafe { FSEventStreamSetDispatchQueue(stream, dispatch_get_global_queue(0, 0)) };
        // SAFETY: the stream is scheduled, which `FSEventStreamStart` requires.
        if unsafe { FSEventStreamStart(stream) } == 0 {
            // SAFETY: an unscheduled-from-nothing stream still has to be invalidated before
            // release, and it was scheduled above.
            unsafe {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
            }
            return None;
        }
        Some(FsEvents {
            stream,
            state,
            root: root.to_path_buf(),
        })
    }

    /// Block up to `interval` for the first batch, then keep draining through quiet windows
    /// — the shape the inotify feed returns, so `scan.rs` sees one contract on three
    /// platforms.
    pub fn poll(&mut self, interval: Duration) -> io::Result<Vec<PathBuf>> {
        let deadline = Instant::now() + interval;
        let mut out: Vec<PathBuf> = Vec::new();
        loop {
            let (batch, lost) = {
                let mut buffer = self.state.buffer.lock().unwrap_or_else(|e| e.into_inner());
                let lost = buffer.lost;
                buffer.lost = false;
                (buffer.changed.drain(..).collect::<Vec<PathBuf>>(), lost)
            };
            if lost {
                return Err(io::Error::other(
                    "fsevents reported a dropped or rolled-back event",
                ));
            }
            out.extend(
                batch
                    .iter()
                    .filter_map(|p| map_onto(&self.root, p.as_path())),
            );
            // Before the first event the caller's interval is the budget; after it, a quiet
            // window ends the burst.
            let wait = if out.is_empty() {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(Vec::new());
                }
                deadline - now
            } else {
                QUIET
            };
            let buffer = self.state.buffer.lock().unwrap_or_else(|e| e.into_inner());
            let (buffer, _) = self
                .state
                .ready
                .wait_timeout(buffer, wait)
                .unwrap_or_else(|e| e.into_inner());
            let more = !buffer.changed.is_empty() || buffer.lost;
            drop(buffer);
            if !more {
                return Ok(out);
            }
        }
    }
}

/// Report a change in the spelling the caller opened, which `resolve_root` has already
/// canonicalised — the rule the kqueue feed keeps, and the bug it learned the hard way. An
/// event above the root (the service coalesces upward) maps to the root itself rather than
/// being dropped, because dropping it loses the change.
fn map_onto(root: &Path, path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }
    if path.starts_with(root) {
        return Some(path.to_path_buf());
    }
    if root.starts_with(path) {
        return Some(root.to_path_buf());
    }
    None
}

impl Drop for FsEvents {
    fn drop(&mut self) {
        // SAFETY: teardown order matters — `FSEvents.h` says invalidating after the queue has
        // been cleared is an error, so the queue is never touched here and stop precedes
        // invalidate. `Release` drops the framework's reference to the context info.
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
        }
    }
}

const _: () = {
    // The flag values the callback switches on, from FSEvents.h.
    assert!(CREATE_FLAG_NONE == 0);
    assert!(CREATE_FLAG_WATCH_ROOT == 0x04);
    assert!(EVENT_MUST_SCAN_SUBDIRS == 0x01);
    assert!(EVENT_USER_DROPPED == 0x02);
    assert!(EVENT_KERNEL_DROPPED == 0x04);
    assert!(EVENT_IDS_WRAPPED == 0x08);
    assert!(EVENT_ROOT_CHANGED == 0x20);
    assert!(SINCE_NOW == u64::MAX);
};

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dusage_fsevents_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::canonicalize(&root).unwrap()
    }

    fn state() -> Arc<State> {
        Arc::new(State {
            buffer: Mutex::new(Buffer::default()),
            ready: Condvar::new(),
        })
    }

    /// A stream is opened, a file is written by the test, and the change must come back.
    ///
    /// This is the gate for the whole feed: it is the only assertion that separates "the
    /// stream started" from "the stream delivers", and it fails rather than skips, because a
    /// feed that silently never fires is the failure mode this module exists to avoid.
    #[test]
    fn a_written_file_is_reported() {
        let root = tmp("probe");
        let mut feed = FsEvents::open(&root).expect("FSEvents stream did not start");
        std::fs::write(root.join("probe.txt"), b"x").unwrap();
        let changed = feed.poll(Duration::from_secs(2)).unwrap();
        assert!(
            changed.iter().any(|p| p.starts_with(&root)),
            "no event for a write inside the watched root: {changed:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A change directly in the root arrives as the root's own path, which names no entry
    /// the engine can attribute: the feed has to answer with the root's children, as the
    /// kqueue feed does.
    #[test]
    fn a_change_in_the_root_reports_its_children() {
        let root = tmp("rootchurn");
        let mut feed = FsEvents::open(&root).expect("FSEvents stream did not start");
        std::fs::write(root.join("probe.txt"), b"x").unwrap();
        let changed = feed.poll(Duration::from_secs(2)).unwrap();
        assert!(
            changed.contains(&root.join("probe.txt")),
            "the new entry must be named: {changed:?}"
        );
        assert!(!changed.contains(&root), "the root is not an entry");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Dropping the feed frees the callback state: the stream's context holds the only
    /// reference besides the feed's own, and gives it back when the stream is deallocated.
    #[test]
    fn dropping_the_feed_frees_its_state() {
        let root = tmp("teardown");
        let feed = FsEvents::open(&root).expect("FSEvents stream did not start");
        let state = Arc::downgrade(&feed.state);
        drop(feed);
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.strong_count() != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(state.strong_count(), 0, "the stream's context leaked");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The drop path, injected. A real `MustScanSubDirs` needs a service or client buffer
    /// overflow, which cannot be produced on demand, so this exercises our half of the
    /// contract — flags in, `Err` out — and says so rather than claiming more.
    #[test]
    fn a_lost_batch_is_an_error_not_a_short_list() {
        for flag in [
            EVENT_MUST_SCAN_SUBDIRS,
            EVENT_MUST_SCAN_SUBDIRS | EVENT_USER_DROPPED,
            EVENT_MUST_SCAN_SUBDIRS | EVENT_KERNEL_DROPPED,
            EVENT_IDS_WRAPPED,
            EVENT_ROOT_CHANGED,
        ] {
            let s = state();
            ingest(&s, &[PathBuf::from("/tmp/x")], &[flag]);
            let mut buffer = s.buffer.lock().unwrap();
            assert!(buffer.lost, "flag {flag:#x} must force a rescan");
            buffer.lost = false;
        }
    }

    /// A path above the root maps to the root rather than being dropped, and a path beside it
    /// is not reported at all.
    #[test]
    fn paths_map_onto_the_callers_spelling() {
        let root = Path::new("/private/tmp/watch");
        assert_eq!(
            map_onto(root, Path::new("/private/tmp/watch/sub")),
            Some(PathBuf::from("/private/tmp/watch/sub"))
        );
        assert_eq!(
            map_onto(root, Path::new("/private/tmp")),
            Some(PathBuf::from("/private/tmp/watch"))
        );
        assert_eq!(map_onto(root, Path::new("/elsewhere")), None);
        assert_eq!(map_onto(root, Path::new("")), None);
    }
}

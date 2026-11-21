//! Pruning engine behind `fastdel`: plan (filter direct children) then execute (parallel,
//! reparse-safe deletion that also parallelises *within* large subtrees).
//!
//! Filters: `prefixes` match the entry name case-sensitively (`starts_with`); the age filter
//! deletes entries whose last write time is older than `now - min_age`. Reparse points are
//! unlinked, never descended into.

use crate::guard;
use crate::model::{ErrorCounts, ErrorKind};
use crate::native::delete::delete_entry;
use crate::native::enumerate::{self, DirReader};
use crate::par_for_each;
use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const FIRST_FAILURES: usize = 20;
const LOG_FAILURE_CAP: usize = 10_000;
/// Files of one directory deleted per work item, so a huge flat directory spreads over workers.
const FILE_CHUNK: usize = 512;
const KINDS: [ErrorKind; 5] = [
    ErrorKind::AccessDenied,
    ErrorKind::SharingViolation,
    ErrorKind::NotFound,
    ErrorKind::DirNotEmpty,
    ErrorKind::Other,
];

#[derive(Debug, Clone)]
pub struct PruneOptions {
    pub root: PathBuf,
    /// `None` = no age filter.
    pub min_age: Option<Duration>,
    /// Empty = every entry matches.
    pub prefixes: Vec<String>,
    pub threads: usize,
    pub dry: bool,
    pub log: Option<PathBuf>,
    /// Rows the CLI prints for a dry run.
    pub list_limit: usize,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_reparse: bool,
    pub modified: SystemTime,
}

#[derive(Debug, Default, Clone)]
pub struct PruneReport {
    /// Files, links and directories actually removed (recursive).
    pub deleted_entries: u64,
    pub candidates_done: u64,
    pub candidates_failed: u64,
    pub errors: ErrorCounts,
    /// Up to 20 root-cause failures: (kind label, path).
    pub first_failures: Vec<(String, PathBuf)>,
    pub elapsed: Duration,
}

/// Select the direct children of `o.root` to delete; returns (candidates, kept count).
/// Refuses protected roots unconditionally, and roots that are not directories.
pub fn plan(o: &PruneOptions) -> io::Result<(Vec<Candidate>, usize)> {
    let refuse = |m: String| Err(io::Error::new(io::ErrorKind::InvalidInput, m));
    if guard::is_protected_root(&o.root) {
        return refuse(format!(
            "refusing protected root '{}' ({} are never pruned)",
            o.root.display(),
            guard::PROTECTED_SUMMARY
        ));
    }
    enumerate::refuse_reparse_root(&o.root).or_else(|e| match e.kind() {
        io::ErrorKind::InvalidInput => Err(e),
        _ => Ok(()), // missing/unreadable roots get the clearer message below
    })?;
    match std::fs::metadata(&o.root) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return refuse(format!("'{}' is not a directory", o.root.display())),
        Err(e) => return refuse(format!("cannot access '{}': {e}", o.root.display())),
    }
    let cutoff = o.min_age.map(|a| {
        SystemTime::now()
            .checked_sub(a)
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });
    let mut ents = Vec::new();
    DirReader::new().read(&o.root, &mut ents, &mut ErrorCounts::default())?;
    let (mut cands, mut kept) = (Vec::new(), 0);
    for e in ents {
        let name = e.name.to_string_lossy().into_owned();
        let modified = e.modified();
        let hit = (o.prefixes.is_empty()
            || o.prefixes.iter().any(|p| name.starts_with(p.as_str())))
            && cutoff.is_none_or(|c| modified < c);
        if hit {
            cands.push(Candidate {
                path: o.root.join(&e.name),
                name,
                is_dir: e.is_dir(),
                is_reparse: e.is_reparse(),
                modified,
            });
        } else {
            kept += 1;
        }
    }
    cands.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((cands, kept))
}

/// A directory being deleted. `pending` = unfinished child dirs + 1 for its own listing.
struct Node {
    path: PathBuf,
    cand: usize,
    pending: AtomicUsize,
    /// 0 = fine, else 1 + index into KINDS of the first root cause below this node.
    failed: AtomicU8,
    parent: Option<Arc<Node>>,
}

enum Work {
    Leaf(usize),
    Dir(Arc<Node>),
    /// A slice of the files directly inside `Node`; holds one `pending` reference.
    Files(Arc<Node>, Vec<PathBuf>),
}

#[derive(Default)]
struct State {
    errors: ErrorCounts,
    first: Vec<(String, PathBuf)>,
    /// Root-cause failures streamed to the log so far (capped).
    sub_logged: usize,
    sub_dropped: bool,
}

struct Ctx<'a> {
    cands: &'a [Candidate],
    del: fn(&Path) -> io::Result<()>,
    deleted: AtomicU64,
    done: AtomicU64,
    failed: AtomicU64,
    st: Mutex<State>,
    /// Streaming log: each line is written whole and unbuffered, so a crash keeps it.
    log: Option<&'a Mutex<File>>,
}

fn emit(log: Option<&Mutex<File>>, line: &str) {
    if let Some(f) = log {
        let _ = f
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(line.as_bytes());
    }
}

fn code_of(k: ErrorKind) -> u8 {
    KINDS.iter().position(|&x| x == k).unwrap() as u8 + 1
}

impl Ctx<'_> {
    /// Delete one entry; a vanished entry counts as success, one retry on sharing violation.
    fn remove(&self, p: &Path) -> Result<(), u8> {
        let mut r = (self.del)(p);
        if matches!(&r, Err(e) if ErrorKind::of(e) == ErrorKind::SharingViolation) {
            std::thread::sleep(Duration::from_millis(20));
            r = (self.del)(p);
        }
        match r {
            Ok(()) => {
                self.deleted.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) if ErrorKind::of(&e) == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.fail(&e, p)),
        }
    }

    fn fail(&self, e: &io::Error, p: &Path) -> u8 {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let k = st.errors.record(e);
        let item = (k.label().to_string(), p.to_path_buf());
        if st.first.len() < FIRST_FAILURES {
            st.first.push(item.clone());
        }
        if st.sub_logged < LOG_FAILURE_CAP {
            st.sub_logged += 1;
            emit(self.log, &format!("SUBFAILED {} {}\n", item.0, p.display()));
        } else {
            st.sub_dropped = true;
        }
        code_of(k)
    }

    fn finish_candidate(&self, idx: usize, code: u8) {
        let n = if code == 0 { &self.done } else { &self.failed };
        n.fetch_add(1, Ordering::Relaxed);
        let Some(c) = self.cands.get(idx).filter(|_| self.log.is_some()) else {
            return;
        };
        let path = c.path.display();
        if code == 0 {
            emit(self.log, &format!("DELETED {path}\n"));
        } else {
            let kind = KINDS[code as usize - 1].label();
            emit(self.log, &format!("FAILED {kind} {path}\n"));
        }
    }

    /// Drop one pending reference; the last one deletes the directory (unless something
    /// below it failed) and cascades to the parent.
    fn release(&self, node: &Arc<Node>) {
        let mut cur = node.clone();
        loop {
            if cur.pending.fetch_sub(1, Ordering::AcqRel) != 1 {
                return;
            }
            // Ancestors of a failed child stay undeleted: only the root cause is counted.
            if cur.failed.load(Ordering::Acquire) == 0 {
                if let Err(c) = self.remove(&cur.path) {
                    cur.failed.store(c, Ordering::Release);
                }
            }
            let code = cur.failed.load(Ordering::Acquire);
            match cur.parent.clone() {
                Some(p) => {
                    if code != 0 {
                        let _ =
                            p.failed
                                .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire);
                    }
                    cur = p;
                }
                None => return self.finish_candidate(cur.cand, code),
            }
        }
    }

    fn work(&self, w: &Work, push: &dyn Fn(Work)) {
        thread_local!(static READER: RefCell<DirReader> = RefCell::new(DirReader::new()));
        let node = match w {
            Work::Leaf(i) => {
                let code = self.remove(&self.cands[*i].path).err().unwrap_or(0);
                return self.finish_candidate(*i, code);
            }
            Work::Dir(n) => n,
            Work::Files(n, files) => {
                for p in files {
                    if let Err(c) = self.remove(p) {
                        let _ =
                            n.failed
                                .compare_exchange(0, c, Ordering::AcqRel, Ordering::Acquire);
                    }
                }
                return self.release(n);
            }
        };
        let mut ents = Vec::new();
        let mut errs = ErrorCounts::default();
        if let Err(e) = READER.with(|r| r.borrow_mut().read(&node.path, &mut ents, &mut errs)) {
            let c = self.fail(&e, &node.path);
            let _ = node
                .failed
                .compare_exchange(0, c, Ordering::AcqRel, Ordering::Acquire);
        }
        // A mount point is left in place, like a Windows junction: descending would
        // delete another filesystem's contents. Its rmdir then fails with EBUSY/ENOTEMPTY,
        // which is reported rather than hidden.
        let parent_dev = enumerate::dev_of(&node.path).unwrap_or(0);
        let mut files = Vec::new();
        for e in &ents {
            let p = node.path.join(&e.name);
            if e.is_dir() && !e.is_reparse() && enumerate::same_filesystem(e, parent_dev) {
                node.pending.fetch_add(1, Ordering::AcqRel);
                push(Work::Dir(Arc::new(Node {
                    path: p,
                    cand: node.cand,
                    pending: AtomicUsize::new(1),
                    failed: AtomicU8::new(0),
                    parent: Some(node.clone()),
                })));
            } else {
                files.push(p);
            }
        }
        if files.len() <= FILE_CHUNK {
            for p in &files {
                if let Err(c) = self.remove(p) {
                    let _ = node
                        .failed
                        .compare_exchange(0, c, Ordering::AcqRel, Ordering::Acquire);
                }
            }
        } else {
            for chunk in files.chunks(FILE_CHUNK) {
                node.pending.fetch_add(1, Ordering::AcqRel);
                push(Work::Files(node.clone(), chunk.to_vec()));
            }
        }
        self.release(node);
    }
}

fn run(
    cands: &[Candidate],
    threads: usize,
    del: fn(&Path) -> io::Result<()>,
    progress: &dyn Fn(u64, u64),
    log: Option<&Mutex<File>>,
) -> (PruneReport, State) {
    let t0 = Instant::now();
    let ctx = Ctx {
        cands,
        del,
        deleted: AtomicU64::new(0),
        done: AtomicU64::new(0),
        failed: AtomicU64::new(0),
        st: Mutex::new(State::default()),
        log,
    };
    let work = cands
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if c.is_dir && !c.is_reparse {
                Work::Dir(Arc::new(Node {
                    path: c.path.clone(),
                    cand: i,
                    pending: AtomicUsize::new(1),
                    failed: AtomicU8::new(0),
                    parent: None,
                }))
            } else {
                Work::Leaf(i)
            }
        })
        .collect();
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::scope(|s| {
        s.spawn(|| {
            par_for_each(work, threads.max(1), |w, push| ctx.work(w, push));
            drop(tx);
        });
        // Reporter: this thread ticks until the workers finish, then the scope joins them.
        while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(Duration::from_secs(2)) {
            progress(
                ctx.done.load(Ordering::Relaxed),
                ctx.failed.load(Ordering::Relaxed),
            );
        }
    });
    let st = ctx.st.into_inner().unwrap_or_else(|e| e.into_inner());
    let rep = PruneReport {
        deleted_entries: ctx.deleted.into_inner(),
        candidates_done: ctx.done.into_inner(),
        candidates_failed: ctx.failed.into_inner(),
        errors: st.errors,
        first_failures: st.first.clone(),
        elapsed: t0.elapsed(),
    };
    (rep, st)
}

/// Delete `cands` (or only log them when `o.dry`). `progress(done, failed)` is called about
/// every 2 s on the calling thread while workers run; workers are joined before returning.
/// With `o.log` the file is opened (append) first and each candidate's line is written as it
/// completes, so an interrupted run still leaves a log.
pub fn execute(
    o: &PruneOptions,
    cands: Vec<Candidate>,
    progress: &dyn Fn(u64 /*done*/, u64 /*failed*/),
) -> PruneReport {
    let log = o.log.as_ref().and_then(|path| {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(f) => Some(Mutex::new(f)),
            Err(e) => {
                eprintln!("fastdel: cannot open log '{}': {e}", path.display());
                None
            }
        }
    });
    let log = log.as_ref();
    let (rep, st) = if o.dry {
        for c in &cands {
            emit(log, &format!("DRY {}\n", c.path.display()));
        }
        (PruneReport::default(), State::default())
    } else {
        run(&cands, o.threads, delete_entry, progress, log)
    };
    if st.sub_dropped {
        emit(
            log,
            &format!("NOTE further failure lines truncated after {LOG_FAILURE_CAP}\n"),
        );
    }
    emit(
        log,
        &format!(
            "SUMMARY root={} dry={} candidates={} deleted_entries={} done={} failed={} errors={} elapsed={:.2?}\n",
            o.root.display(),
            o.dry,
            cands.len(),
            rep.deleted_entries,
            rep.candidates_done,
            rep.candidates_failed,
            rep.errors.total(),
            rep.elapsed
        ),
    );
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::sync::atomic::AtomicU32;

    static N: AtomicU32 = AtomicU32::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "dusage_prune_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn node(path: &str, pending: usize, parent: Option<&Arc<Node>>) -> Arc<Node> {
        Arc::new(Node {
            path: path.into(),
            cand: 0,
            pending: AtomicUsize::new(pending),
            failed: AtomicU8::new(0),
            parent: parent.cloned(),
        })
    }

    /// The platform's "access denied" code, so the fake deleters classify the same.
    #[cfg(windows)]
    const DENIED: i32 = 5;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    const DENIED: i32 = 13;

    static LOG: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    fn fake_ok(p: &Path) -> io::Result<()> {
        LOG.lock().unwrap().push(p.to_path_buf());
        Ok(())
    }
    fn fake_fail(p: &Path) -> io::Result<()> {
        if p.ends_with("bad") {
            Err(io::Error::from_raw_os_error(DENIED))
        } else {
            fake_ok(p)
        }
    }

    fn ctx(del: fn(&Path) -> io::Result<()>) -> Ctx<'static> {
        Ctx {
            cands: &[],
            del,
            deleted: AtomicU64::new(0),
            done: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            st: Mutex::new(State::default()),
            log: None,
        }
    }

    // The fake-deleter tests share one log, so they run inside a single test.
    #[test]
    fn cascade() {
        // root(2: listing+1 child) -> mid(2) -> leaf(1)
        LOG.lock().unwrap().clear();
        let c = ctx(fake_ok);
        let root = node("r", 2, None);
        let mid = node("r/m", 2, Some(&root));
        let leaf = node("r/m/l", 1, Some(&mid));
        c.release(&leaf);
        assert_eq!(LOG.lock().unwrap().len(), 1);
        c.release(&mid); // mid listing done: pending 1 -> 0 after leaf released earlier
        c.release(&root);
        let order: Vec<_> = LOG.lock().unwrap().clone();
        assert_eq!(
            order,
            [
                PathBuf::from("r/m/l"),
                PathBuf::from("r/m"),
                PathBuf::from("r")
            ]
        );
        assert_eq!(c.deleted.load(Ordering::Relaxed), 3);
        assert_eq!(c.done.load(Ordering::Relaxed), 1);

        // A failing child keeps ancestors undeleted and is counted once as root cause.
        LOG.lock().unwrap().clear();
        let c = ctx(fake_fail);
        let root = node("r", 2, None);
        let bad = node("r/bad", 1, Some(&root));
        c.release(&bad);
        c.release(&root);
        assert!(LOG.lock().unwrap().is_empty());
        assert_eq!(c.failed.load(Ordering::Relaxed), 1);
        let st = c.st.lock().unwrap();
        assert_eq!(st.errors.total(), 1);
        assert_eq!(st.errors.access_denied, 1);
        assert_eq!(st.first.len(), 1);
    }

    #[test]
    fn plan_refuses_protected_and_missing() {
        let mk = |root: PathBuf| PruneOptions {
            root,
            min_age: None,
            prefixes: vec![],
            threads: 2,
            dry: true,
            log: None,
            list_limit: 50,
        };
        // A protected root is refused before it is looked at; a missing one is refused too.
        let protected = if cfg!(windows) { r"C:\" } else { "/" };
        assert!(plan(&mk(PathBuf::from(protected))).is_err());
        assert!(plan(&mk(tmp("x").join("missing"))).is_err());
    }

    #[test]
    fn plan_and_execute_real_tree() {
        let base = tmp("exec");
        let root = base.join("root");
        for d in 0..4 {
            for s in 0..5 {
                let dir = root.join(format!("pre_{d}")).join(format!("s{s}"));
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join("f"), b"x").unwrap();
            }
        }
        fs::create_dir_all(root.join("keep")).unwrap();
        let o = PruneOptions {
            root: root.clone(),
            min_age: None,
            prefixes: vec!["pre_".into()],
            threads: 4,
            dry: false,
            log: None,
            list_limit: 50,
        };
        let (cands, kept) = plan(&o).unwrap();
        assert_eq!((cands.len(), kept), (4, 1));
        let rep = execute(&o, cands, &|_, _| {});
        assert_eq!(rep.candidates_failed, 0, "{:?}", rep.first_failures);
        // per candidate: dir + 5 subdirs + 5 files
        assert_eq!(rep.deleted_entries, 4 * 11);
        assert!(root.join("keep").exists());
        let left: HashSet<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1);
        let _ = fs::remove_dir_all(&base);
    }

    fn flat_dir(tag: &str, n: usize) -> (PathBuf, PathBuf) {
        let base = tmp(tag);
        let victim = base.join("victim");
        fs::create_dir_all(&victim).unwrap();
        for i in 0..n {
            fs::write(victim.join(format!("f{i}")), b"").unwrap();
        }
        (base, victim)
    }

    fn cand(path: PathBuf) -> Candidate {
        Candidate {
            name: "victim".into(),
            is_dir: true,
            is_reparse: false,
            modified: SystemTime::now(),
            path,
        }
    }

    #[test]
    fn flat_directory_is_chunked_across_workers() {
        let n = FILE_CHUNK * 3 + 7;
        let (base, victim) = flat_dir("flat", n);
        let cands = [cand(victim.clone())];
        let (rep, _) = run(&cands, 4, delete_entry, &|_, _| {}, None);
        assert_eq!(rep.candidates_failed, 0, "{:?}", rep.first_failures);
        assert_eq!(rep.candidates_done, 1);
        assert_eq!(rep.deleted_entries, n as u64 + 1);
        assert!(!victim.exists());
        let _ = fs::remove_dir_all(&base);
    }

    fn fail_f7(p: &Path) -> io::Result<()> {
        if p.ends_with("f7") {
            return Err(io::Error::from_raw_os_error(DENIED));
        }
        delete_entry(p)
    }

    #[test]
    fn chunk_failure_keeps_dir_and_counts_once() {
        let (base, victim) = flat_dir("flatfail", FILE_CHUNK * 2 + 3);
        let cands = [cand(victim.clone())];
        let (rep, _) = run(&cands, 4, fail_f7, &|_, _| {}, None);
        assert_eq!((rep.candidates_done, rep.candidates_failed), (0, 1));
        assert_eq!(rep.errors.access_denied, 1);
        assert!(victim.join("f7").exists(), "failed file stays");
        assert!(victim.exists(), "dir with a failed child stays");
        assert!(!victim.join("f8").exists());
        let _ = fs::remove_dir_all(&base);
    }

    fn deleted_line(dir: &str, name: &str) -> String {
        format!("DELETED {}", PathBuf::from(dir).join(name).display())
    }

    static WATCH_LOG: Mutex<Option<PathBuf>> = Mutex::new(None);

    /// Deleting `b` only succeeds once `a`'s DELETED line is already in the log file.
    fn wait_for_a_logged(p: &Path) -> io::Result<()> {
        if p.ends_with("b") {
            let log = WATCH_LOG.lock().unwrap().clone().unwrap();
            for _ in 0..500 {
                if fs::read_to_string(&log).is_ok_and(|t| t.contains(&deleted_line("x", "a"))) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            return Err(io::Error::from_raw_os_error(DENIED));
        }
        Ok(())
    }

    #[test]
    fn log_streams_while_running() {
        let dir = tmp("stream");
        let logp = dir.join("run.log");
        *WATCH_LOG.lock().unwrap() = Some(logp.clone());
        let f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&logp)
            .unwrap();
        let f = Mutex::new(f);
        let mk = |n: &str| Candidate {
            path: PathBuf::from("x").join(n),
            name: n.into(),
            is_dir: false,
            is_reparse: false,
            modified: SystemTime::now(),
        };
        let cands = [mk("a"), mk("b")];
        let (rep, _) = run(&cands, 2, wait_for_a_logged, &|_, _| {}, Some(&f));
        assert_eq!((rep.candidates_done, rep.candidates_failed), (2, 0));
        let t = fs::read_to_string(&logp).unwrap();
        assert!(
            t.find(&deleted_line("x", "a")).unwrap() < t.find(&deleted_line("x", "b")).unwrap(),
            "{t}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_refuses_reparse_root() {
        let base = tmp("rroot");
        let real = base.join("real");
        fs::create_dir_all(real.join("sub")).unwrap();
        let link = base.join("lnk");
        let ok = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            let o = PruneOptions {
                root: link.clone(),
                min_age: None,
                prefixes: vec![],
                threads: 2,
                dry: true,
                log: None,
                list_limit: 50,
            };
            let e = plan(&o).unwrap_err();
            assert!(e.to_string().contains("reparse point"), "{e}");
            let _ = fs::remove_dir(&link);
        }
        let _ = fs::remove_dir_all(&base);
    }
}

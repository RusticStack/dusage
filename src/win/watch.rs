//! Change feed for `--watch` on Windows, over the NTFS USN journal.
//!
//! The journal is opened before the first scan so changes made during it are not
//! lost; each `poll` sleeps for the interval and returns the entry paths written
//! since the last read. Read-only: the journal is only queried and read. A read
//! error (journal deleted or truncated) returns `Err`, which makes the caller
//! fall back to a full rescan and reopen the journal.

use crate::win::usn;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The root as the journal reports it: links, junctions and `subst` drives
/// resolved, since journal paths come from `GetFinalPathNameByHandle`.
pub fn resolve_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

pub struct ChangeSource {
    vol: usn::Volume,
    id: u64,
    next: i64,
}

impl ChangeSource {
    /// `None` when the volume has no usable journal or the caller is not elevated;
    /// the caller then rescans the whole tree every interval.
    pub fn open(root: &Path) -> Option<Self> {
        let vol = usn::Volume::for_path(root).ok()?;
        let info = vol.query().ok()?;
        Some(ChangeSource {
            vol,
            id: info.journal_id,
            next: info.next_usn,
        })
    }

    /// Sleep for `interval`, then return the entry paths recorded since the last poll.
    pub fn poll(&mut self, interval: Duration) -> io::Result<Vec<PathBuf>> {
        std::thread::sleep(interval);
        let (recs, next) = self.vol.read(self.id, self.next)?;
        self.next = next;
        let mut parents: HashMap<u128, Option<PathBuf>> = HashMap::new();
        let mut out = Vec::new();
        for r in recs {
            let p = parents
                .entry(r.parent_ref)
                .or_insert_with(|| self.vol.path_of(r.parent_ref).ok());
            if let Some(p) = p.as_ref() {
                out.push(p.join(&r.name));
            }
        }
        Ok(out)
    }
}

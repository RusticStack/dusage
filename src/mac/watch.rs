//! Change feed for `--watch` on macOS.
//!
//! Slice 1 carries the contract without a feed: [`ChangeSource::open`] returns `None`,
//! which is the same answer `scan::watch` already understands as "no change feed
//! available", so every tick is a full rescan. The kqueue feed that replaces this keeps
//! the shape below: `open` yields `None` when it cannot watch the whole tree, and
//! `poll` returns the affected entry paths, or `Err` when events were lost.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The root with symlinks resolved, so the paths a feed reports map back to the paths
/// the scanner walks.
pub fn resolve_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

pub struct ChangeSource;

impl ChangeSource {
    /// `None` until the kqueue feed lands: the caller then rescans the whole tree every
    /// interval.
    pub fn open(_root: &Path) -> Option<Self> {
        None
    }

    /// Wait up to `interval` for changes; the affected entry paths.
    pub fn poll(&mut self, _interval: Duration) -> io::Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_feed_yet_and_that_is_a_full_rescan() {
        let dir = std::env::temp_dir();
        assert!(ChangeSource::open(&resolve_root(&dir)).is_none());
    }

    #[test]
    fn resolve_root_returns_the_directory_itself() {
        let dir = std::env::temp_dir().join(format!("dusage_mac_watch_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let resolved = resolve_root(&dir);
        assert!(resolved.is_absolute());
        assert_eq!(std::fs::canonicalize(&dir).unwrap(), resolved);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Data types shared by the scanner, the reporter and the binaries.
//! Owned by the skeleton; lanes may add fields only through the merge step.

use std::io;
use std::time::Duration;

/// Aggregated counters for a subtree, an extension, or the whole scan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    /// Logical bytes (end-of-file size).
    pub bytes: u64,
    /// Allocated bytes on disk (0 when unknown, e.g. std fallback path).
    pub alloc: u64,
    pub files: u64,
    pub dirs: u64,
}

impl Totals {
    pub fn add(&mut self, o: &Totals) {
        self.bytes += o.bytes;
        self.alloc += o.alloc;
        self.files += o.files;
        self.dirs += o.dirs;
    }
}

/// One direct child of the scan root with its recursive totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopEntry {
    pub name: String,
    pub is_dir: bool,
    pub totals: Totals,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BigFile {
    pub path: String,
    pub bytes: u64,
    pub alloc: u64,
}

/// Per-category error counters; a scan or prune never aborts on these.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCounts {
    pub access_denied: u64,
    pub sharing_violation: u64,
    pub not_found: u64,
    pub dir_not_empty: u64,
    pub other: u64,
}

/// Coarse classification of an I/O error used for reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    AccessDenied,
    SharingViolation,
    NotFound,
    DirNotEmpty,
    Other,
}

impl ErrorKind {
    pub fn of(e: &io::Error) -> Self {
        match e.raw_os_error() {
            Some(5) => Self::AccessDenied,
            Some(32) => Self::SharingViolation,
            Some(2) | Some(3) => Self::NotFound,
            Some(145) => Self::DirNotEmpty,
            _ => match e.kind() {
                io::ErrorKind::PermissionDenied => Self::AccessDenied,
                io::ErrorKind::NotFound => Self::NotFound,
                _ => Self::Other,
            },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AccessDenied => "access_denied",
            Self::SharingViolation => "sharing_violation",
            Self::NotFound => "not_found",
            Self::DirNotEmpty => "dir_not_empty",
            Self::Other => "other",
        }
    }
}

impl ErrorCounts {
    pub fn record(&mut self, e: &io::Error) -> ErrorKind {
        let k = ErrorKind::of(e);
        *self.slot(k) += 1;
        k
    }

    pub fn slot(&mut self, k: ErrorKind) -> &mut u64 {
        match k {
            ErrorKind::AccessDenied => &mut self.access_denied,
            ErrorKind::SharingViolation => &mut self.sharing_violation,
            ErrorKind::NotFound => &mut self.not_found,
            ErrorKind::DirNotEmpty => &mut self.dir_not_empty,
            ErrorKind::Other => &mut self.other,
        }
    }

    pub fn add(&mut self, o: &ErrorCounts) {
        self.access_denied += o.access_denied;
        self.sharing_violation += o.sharing_violation;
        self.not_found += o.not_found;
        self.dir_not_empty += o.dir_not_empty;
        self.other += o.other;
    }

    pub fn total(&self) -> u64 {
        self.access_denied
            + self.sharing_violation
            + self.not_found
            + self.dir_not_empty
            + self.other
    }
}

/// Which enumeration backend produced a scan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `std::fs::read_dir` (portable fallback; `alloc` unknown, no file ids).
    #[default]
    ReadDir,
    /// `GetFileInformationByHandleEx` batched directory info.
    Batched,
    /// Raw MFT read (admin).
    Mft,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadDir => "readdir",
            Self::Batched => "batched",
            Self::Mft => "mft",
        }
    }
}

/// Everything a scan produces. Lists are pre-sorted descending by `bytes`.
#[derive(Debug, Default, Clone)]
pub struct ScanResult {
    pub root: String,
    pub tops: Vec<TopEntry>,
    /// Truncated to `top_n` by the scanner.
    pub big_files: Vec<BigFile>,
    /// Lowercased extension without the dot (`""` = none). Empty unless requested.
    pub by_ext: Vec<(String, Totals)>,
    pub total: Totals,
    /// Reparse-point entries (junctions, symlinks, DFS, WIM, ...) not followed.
    pub skipped_reparse: u64,
    /// Extra hardlink names not counted again (only when dedup is on).
    pub hardlink_dupes: u64,
    pub hardlink_dupe_bytes: u64,
    /// Bytes in named alternate data streams (only when requested).
    pub stream_bytes: u64,
    pub errors: ErrorCounts,
    pub elapsed: Duration,
    pub threads: usize,
    pub source: Source,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify() {
        let mut c = ErrorCounts::default();
        assert_eq!(
            c.record(&io::Error::from_raw_os_error(5)),
            ErrorKind::AccessDenied
        );
        assert_eq!(
            c.record(&io::Error::from_raw_os_error(32)),
            ErrorKind::SharingViolation
        );
        assert_eq!(
            c.record(&io::Error::from_raw_os_error(145)),
            ErrorKind::DirNotEmpty
        );
        assert_eq!(
            c.record(&io::Error::from_raw_os_error(3)),
            ErrorKind::NotFound
        );
        assert_eq!(c.record(&io::Error::other("x")), ErrorKind::Other);
        assert_eq!(c.total(), 5);
    }
}

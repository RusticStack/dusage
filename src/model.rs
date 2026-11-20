//! Data types shared by the scanner, the reporter and the binaries.
//! Owned by the skeleton; lanes may add fields only through the merge step.

use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// What kind of object an entry is, in the classes `find -type` names.
///
/// Every platform produces all of them; the ones a platform cannot report simply never
/// occur (Windows has no fifo or socket behind a directory record), which is why the enum
/// itself carries no `#[cfg]`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Fifo,
    Socket,
    Char,
    Block,
    #[default]
    Other,
}

impl Kind {
    /// The letter `find -printf %y` prints, and what `-type`/`-xtype` accept.
    pub fn letter(self) -> char {
        match self {
            Kind::File => 'f',
            Kind::Dir => 'd',
            Kind::Symlink => 'l',
            Kind::Fifo => 'p',
            Kind::Socket => 's',
            Kind::Char => 'c',
            Kind::Block => 'b',
            Kind::Other => '?',
        }
    }

    pub fn from_letter(c: char) -> Kind {
        match c {
            'f' => Kind::File,
            'd' => Kind::Dir,
            'l' => Kind::Symlink,
            'p' => Kind::Fifo,
            's' => Kind::Socket,
            'c' => Kind::Char,
            'b' => Kind::Block,
            _ => Kind::Other,
        }
    }
}

/// One entry's `lstat` data, in a shape both platforms fill: the POSIX fields where the
/// platform has them, and zeros where it does not.
///
/// `blocks` is `st_blocks` (512-byte units, what find's `%b` prints) and `0` on Windows,
/// which has no such number. `mode`, `uid`, `gid` and `links` are `0` on Windows: a
/// directory record reports none of them, and reading them would mean opening every file.
/// A caller that needs to tell "no mode bits" from "mode 0" asks
/// `crate::native::enumerate::POSIX_FIELDS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    /// End-of-file size: the bytes a file holds, or a directory's own record size (which is
    /// why a scan reports a directory's size as 0). `find -size` and `%s` compare this.
    pub size: u64,
    pub blocks: u64,
    pub mtime: SystemTime,
    pub atime: SystemTime,
    pub ctime: SystemTime,
    pub links: u64,
    pub inode: u64,
    pub dev: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Default for Stat {
    fn default() -> Stat {
        Stat {
            kind: Kind::Other,
            size: 0,
            blocks: 0,
            mtime: UNIX_EPOCH,
            atime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            links: 0,
            inode: 0,
            dev: 0,
            mode: 0,
            uid: 0,
            gid: 0,
        }
    }
}

impl Stat {
    /// Allocated bytes on disk (`st_blocks * 512`); 0 where the platform has no block count.
    pub fn alloc(&self) -> u64 {
        self.blocks * 512
    }

    /// The hardlink identity `dev << 64 | ino`, the 128-bit stand-in for a Windows file id.
    pub fn file_id(&self) -> u128 {
        (self.dev as u128) << 64 | self.inode as u128
    }

    pub fn modified(&self) -> SystemTime {
        self.mtime
    }
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
    /// Classify an I/O error. Raw OS codes are read per platform (Win32 error codes
    /// on Windows, errno on Linux and macOS); the `ErrorKind` fallback keeps the mapping
    /// sane for errors built from `io::Error::other`.
    pub fn of(e: &io::Error) -> Self {
        if let Some(raw) = e.raw_os_error() {
            if let Some(k) = Self::from_raw(raw) {
                return k;
            }
        }
        match e.kind() {
            io::ErrorKind::PermissionDenied => Self::AccessDenied,
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::DirectoryNotEmpty => Self::DirNotEmpty,
            _ => Self::Other,
        }
    }

    #[cfg(windows)]
    fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            5 => Some(Self::AccessDenied),
            32 => Some(Self::SharingViolation),
            2 | 3 => Some(Self::NotFound),
            145 => Some(Self::DirNotEmpty),
            _ => None,
        }
    }

    #[cfg(target_os = "linux")]
    fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            1 | 13 => Some(Self::AccessDenied),
            26 => Some(Self::SharingViolation),
            2 | 20 => Some(Self::NotFound),
            39 => Some(Self::DirNotEmpty),
            _ => None,
        }
    }

    /// macOS sets no per-file errno we need beyond what `std` already maps, so the
    /// classification goes through `ErrorKind` and never through a BSD number written
    /// out here. `ETXTBSY` is the one error the mapping adds: it is the "busy" a running
    /// binary answers with, the macOS analogue of a sharing violation.
    #[cfg(target_os = "macos")]
    fn from_raw(raw: i32) -> Option<Self> {
        match io::Error::from_raw_os_error(raw).kind() {
            io::ErrorKind::PermissionDenied => Some(Self::AccessDenied),
            io::ErrorKind::ExecutableFileBusy => Some(Self::SharingViolation),
            io::ErrorKind::NotFound => Some(Self::NotFound),
            io::ErrorKind::DirectoryNotEmpty => Some(Self::DirNotEmpty),
            _ => None,
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
    /// Bytes in named alternate data streams (only when requested; NTFS only).
    pub stream_bytes: u64,
    /// Directories on another filesystem skipped by `--one-filesystem`.
    pub skipped_mounts: u64,
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
            c.record(&io::Error::from_raw_os_error(WIN_ACCESS_DENIED)),
            ErrorKind::AccessDenied
        );
        assert_eq!(
            c.record(&io::Error::from_raw_os_error(WIN_SHARING_VIOLATION)),
            ErrorKind::SharingViolation
        );
        assert_eq!(
            c.record(&io::Error::from_raw_os_error(WIN_DIR_NOT_EMPTY)),
            ErrorKind::DirNotEmpty
        );
        assert_eq!(
            c.record(&io::Error::from_raw_os_error(WIN_PATH_NOT_FOUND)),
            ErrorKind::NotFound
        );
        assert_eq!(c.record(&io::Error::other("x")), ErrorKind::Other);
        assert_eq!(c.total(), 5);
    }

    #[cfg(windows)]
    const WIN_ACCESS_DENIED: i32 = 5;
    #[cfg(windows)]
    const WIN_SHARING_VIOLATION: i32 = 32;
    #[cfg(windows)]
    const WIN_DIR_NOT_EMPTY: i32 = 145;
    #[cfg(windows)]
    const WIN_PATH_NOT_FOUND: i32 = 3;

    #[cfg(target_os = "macos")]
    const WIN_ACCESS_DENIED: i32 = 13;
    #[cfg(target_os = "macos")]
    const WIN_SHARING_VIOLATION: i32 = 26;
    #[cfg(target_os = "macos")]
    const WIN_DIR_NOT_EMPTY: i32 = 66;
    #[cfg(target_os = "macos")]
    const WIN_PATH_NOT_FOUND: i32 = 2;

    #[cfg(target_os = "linux")]
    const WIN_ACCESS_DENIED: i32 = 13;
    #[cfg(target_os = "linux")]
    const WIN_SHARING_VIOLATION: i32 = 26;
    #[cfg(target_os = "linux")]
    const WIN_DIR_NOT_EMPTY: i32 = 39;
    #[cfg(target_os = "linux")]
    const WIN_PATH_NOT_FOUND: i32 = 2;

    #[test]
    fn counts_add_and_total() {
        let mut a = ErrorCounts {
            access_denied: 1,
            ..Default::default()
        };
        let b = ErrorCounts {
            other: 2,
            ..Default::default()
        };
        a.add(&b);
        assert_eq!((a.total(), a.access_denied, a.other), (3, 1, 2));
    }
}

# Architecture and module contracts

Windows and Linux, std-only (zero crates). All `unsafe`/FFI lives under `src/win/`
or `src/unix/`; every module above them is safe Rust. `src/lib.rs` aliases the
platform module as `crate::native`, so the engines name one surface on both systems.
Research behind these choices: `docs/ROADMAP.md`.

```text
src/lib.rs            module list; re-exports par_for_each
src/model.rs          shared data types: Totals, TopEntry, BigFile, ErrorKind/ErrorCounts, Source, ScanResult
src/queue.rs          par_for_each work queue
src/cli.rs            arg parser + human units
src/guard.rs          protected-root guard
src/report.rs         text / JSON rendering of ScanResult
src/scan.rs           scanner engine: readdir/batched/mft backends, watch
src/prune.rs          pruning engine: plan + execute
src/win/sys.rs        shared FFI decls + open()/wide_verbatim()/check()
src/win/enumerate.rs  batched enumeration, readdir fallback, reparse/file_id/stream helpers
src/win/delete.rs     handle-based delete, never follows reparse points
src/win/usn.rs        USN journal + file-reference resolution
src/win/mft.rs        raw MFT reader
src/win/watch.rs      change feed for --watch (USN journal)
src/unix/sys.rs       errno table + inotify/poll declarations
src/unix/enumerate.rs readdir + statx enumeration, symlink and mount helpers
src/unix/delete.rs    unlink/rmdir delete, never follows symlinks
src/unix/watch.rs     change feed for --watch (inotify)
src/bin/dusage.rs     thin CLI over scan + report
src/bin/fastdel.rs    thin CLI over prune
tests/                dusage_cli.rs, fastdel_cli.rs (run the built binaries)
```

## Rules for every lane

- Edit only the files your lane owns. Extra FFI decls/structs/constants go in
  your own `win/*.rs` or `unix/*.rs` file, not `sys.rs`. If you truly need a
  change elsewhere, report it instead of making it.
- Platform differences belong in `crate::native` (the `win`/`unix` module), never
  in `scan.rs`, `prune.rs` or `report.rs`; reaching for `#[cfg(target_os)]` there
  is a design smell to raise, not to fix locally.
- Every `unsafe` block gets a `// SAFETY:` comment. Struct layouts for FFI are
  `#[repr(C)]` and must match the Microsoft Learn definition; add a
  `size_of`/`offset_of!` test per struct against the documented layout.
- Per-entry I/O errors never abort: return/collect them, classify with
  `model::ErrorKind`.
- Tests that need admin (raw volume, USN) must detect `ERROR_ACCESS_DENIED`
  and return early with an `eprintln!("skipped: needs admin")`, not fail.
- Tests use unique dirs under `std::env::temp_dir()` and clean up after.
  Junctions in tests: `cmd /c mklink /J link target` (no admin needed).
  Symlinks need Developer Mode: skip the test if creation fails.
- `cargo clippy --all-targets -- -D warnings` and `cargo test` must pass.

## Contracts

### queue (`src/queue.rs`)

```rust
pub fn par_for_each<T, F>(initial: Vec<T>, threads: usize, f: F)
where T: Send + 'static, F: Fn(&T, &dyn Fn(T)) + Send + Sync;
```
Signature is frozen. Requirements: a single `Mutex<State{queue, active}>` plus
`Condvar` (no sleep polling). Pop and `active += 1` happen in one critical section.
Termination when queue empty and `active == 0`. A panic in `f` must not
hang the other workers. The panic propagates to the caller after all workers stop.

### cli (`src/cli.rs`)

```rust
pub struct Args { /* positionals + flags */ }
impl Args {
    pub fn parse(raw: impl IntoIterator<Item = String>, value_flags: &[&str], bool_flags: &[&str]) -> Result<Args, String>;
    pub fn positional(&self, i: usize) -> Option<&str>;
    pub fn positionals(&self) -> &[String];
    pub fn flag(&self, name: &str) -> bool;            // "--dry"
    pub fn value(&self, name: &str) -> Option<&str>;   // "--age 7d" or "--age=7d"
}
pub fn parse_size(s: &str) -> Result<u64, String>;   // "1073741824", "300M", "1.5G", "512KiB", "2T"; 1K = 1024
pub fn parse_age(s: &str) -> Result<Option<std::time::Duration>, String>;
    // "30s","15m","2h","7d","1w"; bare number = days (back-compat, "0.0834" ok);
    // any negative bare number = Ok(None) = no age filter; garbage = Err
pub fn parse_threads(s: &str) -> Result<usize, String>; // >= 1
pub fn default_threads() -> usize;                      // 4x available_parallelism, fallback 32
pub fn fmt_bytes(b: u64) -> String;                     // "1.23 GB" (decimal, matches existing output)
```
Unknown `--flags` are an error. `-h/--help` is a bool flag the caller handles.
A `--` ends flag parsing. Negative numbers like `-1` are positionals, not flags.

### guard (`src/guard.rs`)

```rust
pub fn is_protected_root(p: &Path) -> bool;
pub const PROTECTED_SUMMARY: &str;   // the refusal message's "what" clause
```

Two implementations behind one name. **Windows:** true for any volume root or share root in
every spelling (`C:`, `C:\`, `\\?\C:\`,
`\\server\share`, `\\?\UNC\server\share\`, `\\.\C:`), `%SystemRoot%`,
`%ProgramFiles%`, `%ProgramFiles(x86)%`, `%ProgramData%`, `%USERPROFILE%`, `C:\Users`,
and any ancestor of them. Normalise via `std::path::absolute` + components
(case-insensitive, trailing separators and `.`/`..` resolved) and also try
`fs::canonicalize` when the path exists, so a junction that points at a protected
dir is caught. fastdel refuses every protected root regardless of filters. Loopback UNC forms
(`\\localhost\C$`, `127.0.0.1`, `[::1]`, `<COMPUTERNAME>`, `\\?\UNC\localhost`) map to
the drive they name; other local `$` shares (`ADMIN$`, `IPC$`) are refused.

**Linux:** normalise with `std::path::absolute` + components, then `fs::canonicalize` when the
path exists, so a symlink to a system directory is caught. Protected when the candidate lies
inside an immutable system tree (`/etc`, `/usr`, `/bin`, `/sbin`, `/lib*`, `/boot`, `/proc`,
`/sys`, `/dev`, `/run`), or is a data root (`/var`, `/opt`, `/srv`, `/root`, `/home`, `$HOME`)
or an ancestor of one. `/` is always protected, and so is a path that cannot be normalised.
Their children stay prunable (`/var/tmp`, `/opt/app/cache`), the way `C:\Windows\Temp` does on
Windows. Mount points are deliberately not special-cased: a prune never descends into one, so
only a path the user typed can be a mount.

### report (`src/report.rs`)

```rust
pub struct ReportOptions { pub top_n: usize, pub min_big: u64, pub show_alloc: bool, pub by_ext: bool }
pub fn render_text(r: &ScanResult, o: &ReportOptions, out: &mut dyn std::io::Write) -> std::io::Result<()>;
pub fn render_json(r: &ScanResult, out: &mut dyn std::io::Write) -> std::io::Result<()>;
pub fn json_escape(s: &str, out: &mut String);
```
The text format keeps the existing header/sections (TOTAL line, top entries,
big files) and adds: on-disk column when `show_alloc`, a by-extension section,
and a footer with skipped reparse points, skipped mount points (`--one-filesystem`), hardlink
dupes, stream bytes, and error counts by category. Directory rows end with the platform's
separator (`std::path::MAIN_SEPARATOR`). JSON is one object containing every `ScanResult` field. Use
`elapsed_secs` as an f64 and `source` as a label. It must be valid JSON for any
path (control chars, quotes, backslashes, non-BMP).

### Platform layer (`src/win/*`, `src/unix/*`) — the `crate::native` surface

Both platforms implement exactly this, and nothing above the platform layer knows which
one is present:

```rust
pub struct Entry { /* platform fields */ }
impl Entry {
    pub fn is_dir(&self) -> bool;
    pub fn is_reparse(&self) -> bool;               // Windows: any reparse point; Linux: symlink
    pub fn modified(&self) -> std::time::SystemTime;
}
pub fn is_skipped(e: &Entry) -> bool;              // "count as a link, never follow"
pub struct DirReader;
impl DirReader {
    pub fn new() -> Self;
    /// Enumerate one directory, skipping "." and "..". Per-entry failures go into
    /// `errs`; only an unreadable directory is returned as Err.
    pub fn read(&mut self, dir: &Path, out: &mut Vec<Entry>, errs: &mut ErrorCounts)
        -> std::io::Result<()>;
}
pub fn read_plain(dir: &Path, out: &mut Vec<Entry>, errs: &mut ErrorCounts) -> io::Result<()>;
pub fn refuse_reparse_root(path: &Path) -> io::Result<()>;
pub fn file_id_of(path: &Path) -> io::Result<u128>;
pub fn named_stream_bytes(path: &Path) -> io::Result<u64>;   // Linux: always 0
pub fn dev_of(path: &Path) -> io::Result<u64>;               // 0 on Windows
pub fn other_filesystem(e: &Entry, root_dev: u64) -> bool;   // --one-filesystem boundary
pub fn same_filesystem(e: &Entry, parent_dev: u64) -> bool;  // prune must not cross mounts
pub fn is_unsupported(e: &io::Error) -> bool;                // backend fallback trigger
pub fn is_denied(e: &io::Error) -> bool;

pub mod delete { pub fn delete_entry(path: &Path) -> io::Result<()>;
                 pub fn remove_tree(path: &Path) -> (u64, ErrorCounts); }

pub mod watch {
    pub fn resolve_root(root: &Path) -> PathBuf;   // canonicalise: feeds report real paths
    pub struct ChangeSource;
    impl ChangeSource {
        pub fn open(root: &Path) -> Option<ChangeSource>;   // None => full rescans
        /// Wait up to `interval` for changes; the affected entry paths.
        pub fn poll(&mut self, interval: Duration) -> io::Result<Vec<PathBuf>>;
    }
}
```

`ChangeSource` is the NTFS USN journal on Windows (elevated; returns Err when the journal
was truncated, which forces a full rescan) and inotify on Linux (an unprivileged watch tree
with `IN_Q_OVERFLOW` mapped to Err). Both return paths, and `scan::top_for` decides which
top-level entry each one belongs to.

### win::enumerate (`src/win/enumerate.rs`)

```rust
pub struct Entry {
    pub name: std::ffi::OsString,
    pub attributes: u32,
    pub reparse_tag: u32,          // 0 unless FILE_ATTRIBUTE_REPARSE_POINT
    pub size: u64,                 // EndOfFile
    pub alloc: u64,                // AllocationSize
    pub file_id: u128,             // 0 when unknown
    pub last_write: u64,           // FILETIME as 100ns ticks since 1601
    pub change_time: u64,
}
pub struct DirReader { /* owns a reusable 64 KiB buffer (Vec<u64> for alignment) */ }
impl DirReader {
    pub fn with_buffer(bytes: usize) -> Self;
    /// Opens with FILE_LIST_DIRECTORY|SYNCHRONIZE, BACKUP_SEMANTICS|OPEN_REPARSE_POINT.
    /// Uses FileIdExtdDirectoryInfo; on ERROR_INVALID_PARAMETER/INVALID_LEVEL/NOT_SUPPORTED
    /// falls back to FileIdBothDirectoryInfo (64-bit id) for this and later calls.
}
pub fn volume_serial(path: &std::path::Path) -> std::io::Result<u64>;
/// Sum of named ($DATA other than the unnamed one) alternate stream sizes.
pub fn named_stream_bytes(path: &std::path::Path) -> std::io::Result<u64>;  // FindFirstStreamW/FindNextStreamW
pub fn cluster_size(path: &std::path::Path) -> std::io::Result<u64>;        // GetDiskFreeSpaceW on the volume root
/// Err ("root is a reparse point; pass its target") when `path` itself is a reparse point.
pub fn refuse_reparse_root(path: &std::path::Path) -> std::io::Result<()>;
pub fn file_id_of(path: &std::path::Path) -> std::io::Result<u128>;
```

### unix::enumerate (`src/unix/enumerate.rs`)

`Entry` carries `name`, `is_dir`, `is_link`, `size`, `alloc` (`st_blocks * 512`),
`file_id` (`dev << 64 | ino`) and `dev`. Enumeration is `std::fs::read_dir` plus one
`lstat` per entry (`DirEntry::metadata` never follows a symlink), which is why the Linux
`batched` backend is "one stat per entry" rather than "many entries per syscall".
`read_plain` leaves `alloc`, `file_id` and `dev` zero, matching the `readdir` backend's
"size on disk unknown" contract. `refuse_reparse_root` refuses a symlink root;
`file_id_of` is the inode identity that makes `--dedup-hardlinks` correct; `dev_of` and
`other_filesystem` implement `--one-filesystem`; `same_filesystem` stops a prune at a
mount point.
Bounds-check every `NextEntryOffset`/`FileNameLength` (bytes) against the
returned buffer. The tests must compare against `std::fs::read_dir` on a fixture
for names, sizes and dir flags, and confirm that a junction shows `is_reparse()`.

### win::delete (`src/win/delete.rs`)

```rust
/// Delete one entry *without following* it: a junction/symlink is removed as a link.
/// Opens with DELETE|FILE_READ_ATTRIBUTES|FILE_WRITE_ATTRIBUTES, BACKUP_SEMANTICS|OPEN_REPARSE_POINT.
/// Tries FileDispositionInfoEx(DELETE|POSIX_SEMANTICS|IGNORE_READONLY_ATTRIBUTE);
/// on INVALID_PARAMETER/NOT_SUPPORTED falls back to clearing READONLY and FileDispositionInfo.
/// A non-empty directory yields ERROR_DIR_NOT_EMPTY.
pub fn delete_entry(path: &std::path::Path) -> std::io::Result<()>;
/// Recursive delete of `path` that never descends into reparse points (they are unlinked),
/// using `win::enumerate::DirReader` + `delete_entry`, deepest-first. Per-entry errors are
/// counted and the walk continues. Returns (deleted_entries, errors).
pub fn remove_tree(path: &std::path::Path) -> (u64, crate::model::ErrorCounts);
```
Tests cover: a read-only file, a nested tree, a junction whose target must
survive with its contents intact, a long path (>260 chars), and an open file (sharing
violation is reported, not panicked).

### win::usn (`src/win/usn.rs`)

```rust
pub struct JournalInfo { pub journal_id: u64, pub first_usn: i64, pub next_usn: i64, pub lowest_valid_usn: i64, pub max_usn: i64 }
pub struct UsnRecord { pub file_ref: u128, pub parent_ref: u128, pub usn: i64, pub reason: u32, pub attributes: u32, pub name: std::ffi::OsString }
pub struct Volume { /* owned \\.\X: handle */ }
impl Volume {
    pub fn open(letter: char) -> std::io::Result<Volume>;             // needs admin
    pub fn for_path(p: &std::path::Path) -> std::io::Result<Volume>;  // volume of p (drive-letter paths only)
    pub fn query(&self) -> std::io::Result<JournalInfo>;              // FSCTL_QUERY_USN_JOURNAL
    /// FSCTL_READ_USN_JOURNAL from `start`; returns records and the next start USN.
    /// ERROR_JOURNAL_ENTRY_DELETED / journal id mismatch -> io::Error the caller treats as "full rescan".
    pub fn read(&self, journal_id: u64, start: i64) -> std::io::Result<(Vec<UsnRecord>, i64)>;
    /// FSCTL_ENUM_USN_DATA over the whole MFT (names + parent refs, no sizes).
    pub fn enumerate(&self, f: &mut dyn FnMut(UsnRecord)) -> std::io::Result<()>;
    /// Resolve a file reference to a full path (OpenFileById + GetFinalPathNameByHandleW).
    pub fn path_of(&self, file_ref: u128) -> std::io::Result<std::path::PathBuf>;
}
```
Handle USN_RECORD_V2 and V3 (V3 has 128-bit refs). FSCTL codes are computed by
a `const fn ctl_code` and checked against winioctl.h values in tests.

### win::mft (`src/win/mft.rs`)

```rust
pub struct MftEntry { pub frn: u64 /* record index | seq<<48 */, pub parent: u64, pub name: std::ffi::OsString,
                      pub is_dir: bool, pub size: u64, pub alloc: u64, pub links: u16, pub reparse: bool,
                      pub approx: bool /* $DATA header not found (attribute list) */,
                      pub other_names: Vec<(u64, std::ffi::OsString)> /* further hardlinks: parent, name */ }
/// Pure parsers, testable on synthetic buffers:
pub fn apply_fixup(record: &mut [u8], bytes_per_sector: usize) -> Result<(), String>;
pub fn decode_runlist(data: &[u8]) -> Result<Vec<(i64 /*lcn, -1 sparse*/, u64 /*clusters*/)>, String>;
pub fn parse_record(record: &[u8], index: u64) -> Result<Option<MftEntry>, String>; // one record alone; None = not in use / extension record
/// Read every in-use record of the volume's $MFT (FSCTL_GET_NTFS_VOLUME_DATA, $MFT runlist from record 0
/// and its attribute list). Needs admin. Callback per entry; per-record parse errors are counted, not fatal.
pub fn scan_volume(letter: char, f: &mut dyn FnMut(MftEntry)) -> std::io::Result<MftStats>;
pub struct MftStats { pub records: u64, pub in_use: u64, pub parse_errors: u64, pub approx_records: u64, pub elapsed: std::time::Duration }
```
Every non-DOS `$FILE_NAME` is a hardlink. The first WIN32 name (else POSIX) is `name`/`parent`,
and the rest go to `other_names`. `scan` walks one link per (parent, name), and
`--dedup-hardlinks` keys on the record index. Sizes come from the unnamed `$DATA` (resident:
value length, `alloc = 0`; non-resident: the VCN-0 extent's data and allocated sizes).

`scan_volume` splits the `$MFT` stream into record-aligned 4 MiB jobs (sparse runs skipped) and
runs up to 16 workers. Each worker has its own `FILE_FLAG_NO_BUFFERING` volume handle (synchronous
I/O on one handle is serialised) and a 4 KiB-aligned buffer. Base records with an
`$ATTRIBUTE_LIST` are held, and extension records (non-zero base reference) are collected by base
index. Both are merged after all workers finish, so names and `$DATA` in extension records count.
`approx` is set only when a list-bearing file still has no `$DATA` header; `scan` prints a note
with the count.

### scan / prune (wave 2)

Implemented as designed after wave 1. Signatures as built:

```rust
// scan.rs
pub enum Backend { Auto /*default*/, ReadDir, Batched, Mft }
pub struct ScanOptions { pub root: PathBuf, pub threads: usize, pub min_big: u64, pub top_n: usize,
                         pub backend: Backend, pub dedup_hardlinks: bool, pub streams: bool,
                         pub by_ext: bool, pub one_filesystem: bool }
impl ScanOptions { pub fn new(root: impl Into<PathBuf>) -> Self; }  // defaults: 4x cores, 1 GiB, top 60, Auto
pub fn scan(o: &ScanOptions) -> io::Result<ScanResult>;
pub fn watch(o: &ScanOptions, interval: Duration,
             on_update: &mut dyn FnMut(&ScanResult, &str) -> bool) -> io::Result<()>;

// prune.rs
pub struct PruneOptions { pub root: PathBuf, pub min_age: Option<Duration>, pub prefixes: Vec<String>,
                          pub threads: usize, pub dry: bool, pub log: Option<PathBuf>, pub list_limit: usize }
pub struct Candidate { pub path: PathBuf, pub name: String, pub is_dir: bool, pub is_reparse: bool, pub modified: SystemTime }
pub struct PruneReport { pub deleted_entries: u64, pub candidates_done: u64, pub candidates_failed: u64,
                         pub errors: ErrorCounts, pub first_failures: Vec<(String, PathBuf)>, pub elapsed: Duration }
pub fn plan(o: &PruneOptions) -> io::Result<(Vec<Candidate>, usize /*kept*/)>;
pub fn execute(o: &PruneOptions, cands: Vec<Candidate>, progress: &dyn Fn(u64, u64)) -> PruneReport;
```

`scan` errors only when the root itself is missing, unreadable, not a directory or a reparse
point; everything below is counted in `ScanResult::errors`. `Backend::Mft` is refused with an
explanation on Linux. With `one_filesystem`, a directory on another device is counted in
`ScanResult::skipped_mounts` instead of being scanned. `plan` refuses protected roots and
non-directory or reparse roots (`InvalidInput`); `execute` never errors (failures are in the report).
`watch` calls `on_update` with the initial scan and then after each rescan that changed something;
returning false stops it.

#### Deviations recorded by the lanes

- `MftEntry.approx` and `MftStats.approx_records` were added to the wave-1 MFT contract.
- `ScanOptions::new(root)` supplies the defaults, so callers set only what they override.
- `Source` has three labels (`ReadDir`, `Batched`, `Mft`); `auto` reports the backend actually used.
- `--dedup-hardlinks` needs file ids, so it applies to the batched backend only (readdir has none).
- **Exit codes.** dusage: 2 = bad arguments (it also prints "try 'dusage --help'"), 1 = scan failed,
  including a missing or reparse-point root. fastdel: 2 = bad arguments or `plan` refused
  (protected or reparse root, empty `--prefix`), 1 = some candidates failed to delete, 0 = success or `--dry`.
- `fastdel` keeps the positional form `<root> <min_age> [prefixes] [threads]`; a positional `""`
  prefix means all entries (as before), while an empty `--prefix` value exits 2.

#### Behaviour notes (wave 3)

- **Roots.** A scan or prune root that is itself a reparse point (junction/symlink) is
  refused with "root is a reparse point; pass its target" (`enumerate::refuse_reparse_root`),
  because `DirReader` opens with `OPEN_REPARSE_POINT` and would list nothing. fastdel also
  refuses a root whose canonical target is protected (guard canonicalizes).
- **Guard and UNC.** `\\localhost\C$\...`, `\\127.0.0.1\C$\...`, `\\<COMPUTERNAME>\C$\...` map
  to the drive they name before the protected-set comparison; other local `$` shares
  (`ADMIN$`, `IPC$`) fail closed. Unknown aliases of the machine are not detected.
- **Watch mode.** The USN journal path comparison uses the canonicalized root (junction and
  `subst` safe). Partial rescans keep untouched top-level entries as last scanned, so a
  hardlink with names in a rescanned and a kept top may be counted per name until the next
  full rescan. With `--dedup-hardlinks` (per-run dedup state) every rescan is therefore full.
  `on_update` fires only when something changed, not on quiet ticks.
- **fastdel log.** `--log` is opened in append mode before any deletion; one line per
  candidate (`DELETED`/`FAILED`/`DRY`) and per root-cause failure (`SUBFAILED`, capped) is
  written whole as it happens, then a `SUMMARY`, so a crash mid-run keeps the log.
- **fastdel throughput.** Files in a directory are deleted in `FILE_CHUNK` slices that are
  separate work items, so one huge flat directory spreads over all workers.

# Architecture and module contracts

Windows-only, std-only (zero crates). All `unsafe`/FFI lives under `src/win/`.
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
src/win/enumerate.rs  batched directory enumeration, reparse-root check, file_id_of
src/win/delete.rs     handle-based delete, never follows reparse points
src/win/usn.rs        USN journal + file-reference resolution
src/win/mft.rs        raw MFT reader
src/bin/dusage.rs     thin CLI over scan + report
src/bin/fastdel.rs    thin CLI over prune
tests/                dusage_cli.rs, fastdel_cli.rs (run the built binaries)
```

## Rules for every lane

- Edit only the files your lane owns. Extra FFI decls/structs/constants go in
  your own `win/*.rs` file, not `sys.rs`. If you truly need a change elsewhere,
  report it instead of making it.
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
```
True for any volume root or share root in every spelling (`C:`, `C:\`, `\\?\C:\`,
`\\server\share`, `\\?\UNC\server\share\`, `\\.\C:`), `%SystemRoot%`,
`%ProgramFiles%`, `%ProgramFiles(x86)%`, `%ProgramData%`, `%USERPROFILE%`, `C:\Users`,
and any ancestor of them. Normalise via `std::path::absolute` + components
(case-insensitive, trailing separators and `.`/`..` resolved) and also try
`fs::canonicalize` when the path exists, so a junction that points at a protected
dir is caught. fastdel refuses every protected root regardless of filters. Loopback UNC forms
(`\\localhost\C$`, `127.0.0.1`, `[::1]`, `<COMPUTERNAME>`, `\\?\UNC\localhost`) map to
the drive they name; other local `$` shares (`ADMIN$`, `IPC$`) are refused.

### report (`src/report.rs`)

```rust
pub struct ReportOptions { pub top_n: usize, pub min_big: u64, pub show_alloc: bool, pub by_ext: bool }
pub fn render_text(r: &ScanResult, o: &ReportOptions, out: &mut dyn std::io::Write) -> std::io::Result<()>;
pub fn render_json(r: &ScanResult, out: &mut dyn std::io::Write) -> std::io::Result<()>;
pub fn json_escape(s: &str, out: &mut String);
```
The text format keeps the existing header/sections (TOTAL line, top entries,
big files) and adds: on-disk column when `show_alloc`, a by-extension section,
and a footer with skipped reparse points, hardlink dupes, stream bytes, and error
counts by category. JSON is one object containing every `ScanResult` field. Use
`elapsed_secs` as an f64 and `source` as a label. It must be valid JSON for any
path (control chars, quotes, backslashes, non-BMP).

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
impl Entry {
    pub fn is_dir(&self) -> bool;
    pub fn is_reparse(&self) -> bool;               // any reparse point (dir or file)
    pub fn modified(&self) -> std::time::SystemTime;
}
pub struct DirReader { /* owns a reusable 64 KiB buffer (Vec<u64> for alignment) */ }
impl DirReader {
    pub fn new() -> Self;
    pub fn with_buffer(bytes: usize) -> Self;
    /// Enumerate one directory, skipping "." and "..". Opens with
    /// FILE_LIST_DIRECTORY|SYNCHRONIZE, BACKUP_SEMANTICS|OPEN_REPARSE_POINT.
    /// Uses FileIdExtdDirectoryInfo; on ERROR_INVALID_PARAMETER/INVALID_LEVEL/NOT_SUPPORTED
    /// falls back to FileIdBothDirectoryInfo (64-bit id) for this and later calls.
    pub fn read(&mut self, dir: &std::path::Path, out: &mut Vec<Entry>) -> std::io::Result<()>;
}
pub fn volume_serial(path: &std::path::Path) -> std::io::Result<u64>;
/// Sum of named ($DATA other than the unnamed one) alternate stream sizes.
pub fn named_stream_bytes(path: &std::path::Path) -> std::io::Result<u64>;  // FindFirstStreamW/FindNextStreamW
pub fn cluster_size(path: &std::path::Path) -> std::io::Result<u64>;        // GetDiskFreeSpaceW on the volume root
/// Err ("root is a reparse point; pass its target") when `path` itself is a reparse point.
pub fn refuse_reparse_root(path: &std::path::Path) -> std::io::Result<()>;
pub fn file_id_of(path: &std::path::Path) -> std::io::Result<u128>;
```
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
                         pub backend: Backend, pub dedup_hardlinks: bool, pub streams: bool, pub by_ext: bool }
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
point; everything below is counted in `ScanResult::errors`. `plan` refuses protected roots and
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

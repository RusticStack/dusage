# dusage / fastdel: status-tracked roadmap

**Origin.** This began as a research report written from `src/lib.rs`, `src/bin/dusage.rs` and `src/bin/fastdel.rs` before any change (four research lanes plus skeptic verdicts; refuted items dropped or corrected, unconfirmed items marked unverified). The "Problem" and "Fix" text below describes the original code and is kept as written; the status line under each item says what was built. Gains stay unmeasured unless stated: no benchmark has been run on the implementation.

**Status (branch `improve/roadmap`, after wave 3).** Items 1-8 are done, 9 and 10 are partially done (implemented, not validated on a real volume), 11 is open. The benchmark questions in section 4 are answered on a real Windows volume (2026-11-21);
the elevated half of that run and the Linux sweeps are named where they are still owed.

**Status 0.5.0 (search).** `dusage` is a search tool as well: `search` (contents,
a grep/rg replacement), `find` (paths, a find replacement), `prune` (the fastdel
engine, with presets), `du` (a du-compatible per-directory report), and the
0.4.0 `scan`/`big` intact behind subcommands in one binary. A benchmark suite
with a memory pass, a cold-cache pass and a regression guard now lives in
`bench/` and `docs/BENCHMARKS.md`; the section at the end lists what shipped,
the numbers and the limits that are still open.

**Status 0.6.0 (tree and df).** Two more tools replaced: `dusage tree` prints the tree with
two size columns per row (bytes on disk and bytes claimed), subtree totals on directory rows,
levels, sorting, find's ignore layers and JSON; `dusage df` reports one row per mounted
filesystem through `statvfs` and `/proc/self/mounts` on Linux and `GetDiskFreeSpaceExW` and the
volume list on Windows. Both are measured against the tools they replace in
`docs/BENCHMARKS.md`; the last section records what the numbers say and what is still open.

**Status (macOS).** The crate compiles for `x86_64-apple-darwin` and `aarch64-apple-darwin`, and has a third
platform module, `src/mac/`: enumeration over `readdir$INODE64`/`fstatat`, deletion over
`unlink`/`rmdir`, a kqueue `EVFILT_VNODE` change feed, a macOS guard, and volume roots and
`-fstype` names from the kernel's own mount table. It is cross-checked from Linux and has run
in CI on real Intel and Apple Silicon Macs. The limits that come with it are in the macOS
section at the end, and MRI-14 carries the runner schedule.

**Status 0.4.0 (Linux).** The whole tool set now runs on Linux. Enumeration, deletion, the
protected-root guard, `--one-filesystem` and an inotify change feed are implemented and
tested; the NTFS-only read paths (`--mft`, `--streams`) are refused or report zero with an
explanation. Numbers measured on this machine are in the last section.

**FFI audit (wave 2).** Every FFI declaration, struct layout and constant under `src/win/` was audited against Microsoft Learn: 34 items, none wrong, 3 unverifiable (no authoritative source reachable).

**Wave 3 safety fixes.** Found by review after wave 2 and fixed: guard bypass via loopback admin shares (`\\localhost\C$`), empty `--prefix` meaning match-all (positional `""` kept as "all" for back-compat), junction roots scanning or pruning as empty, watch journal path via junction or `subst` root, partial watch rescans double counting hardlinks, and one worker deleting a huge flat directory.

## 1. Ranked roadmap

| Rank | Improvement | Status | Gain type | Admin | Effort | Risk |
|---|---|---|---|---|---|---|
| 1 | Skip reparse-point directories on the raw attribute | Done: `win::enumerate` (`Entry::is_reparse`, `refuse_reparse_root`), used by `scan`, `prune` | Safety (invariant 3) | No | S | Low |
| 2 | Verbatim- and UNC-aware protected-root guard in fastdel | Done: `guard` | Safety (invariant 4) | No | S | None |
| 3 | Batched enumerator via `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` | Done: `win::enumerate::DirReader` | New metric (on-disk size) plus FileId; speed unmeasured | No | M | Medium |
| 4 | Hardlink-aware totals keyed by FileId seen during enumeration | Done: `scan` (`--dedup-hardlinks`) | Accuracy (AGENTS says 5-10% overcount, unverified here) | No | S (after 3) | Low |
| 5 | Condvar work queue with one lock (replace 1 ms polling and fix the inflight race) | Done: `queue` | Tail parallelism; small | No | S | Low |
| 6 | fastdel: classify and report final error categories | Done: `model::ErrorCounts`, `prune` | Diagnosability | No | S | None |
| 7 | fastdel: parallel removal inside large subtrees | Done: `prune` (chunked `Work::Files`), `win::delete`; speedup unmeasured | Speed on skewed trees; unmeasured | No | M | Medium-high |
| 8 | Opt-in named-stream (ADS) accounting | Done: `--streams`, `enumerate::named_stream_bytes` | Accuracy for ADS-heavy trees | No | S-M | Low |
| 9 | USN change journal for incremental `--watch` | Done: `win::usn` + `scan::watch`, validated on a real journal (elevated, 2026-11-21); the unelevated fallback was an unpaced rescan loop and is fixed | Incremental rescans; 4 update lines for 12 changes, totals equal to a full scan | Yes (measured) | L | Medium |
| 10 | Elevated whole-volume fast path (USN enum plus MFT parsing) | Done: `win::mft` + `--mft`, validated against the batched backend on two real volumes | Whole-drive scan 6.2x faster (10.6 s vs 65.8 s) | Yes (measured) | L | High |
| 11 | Dev Drive guidance for scratch trees (docs only) | Done: `docs/DEV_DRIVE.md`; nothing run on a Dev Drive or ReFS volume | Unmeasured | Setup only | S | None |

### 1. Skip reparse-point directories on the raw attribute (dusage and fastdel)

**Status: Done.** `win::enumerate` flags every reparse point from the raw attribute; `scan` and `prune` skip or unlink them, and a reparse-point root is refused.

- **Problem.** `dusage.rs` lines 61 and 86 skip only `is_symlink()`. `fastdel.rs` line 129 uses `p.is_dir()`, which follows links. std's `FileType::new` (sys/fs/windows.rs, around lines 1181-1188) sets `is_symlink` only when the reparse tag has the name-surrogate bit (0x20000000). Symlinks (0xA000000C) and junctions (0xA0000003) are caught. DFS (0x8000000A), WIM (0x80000008), CSV (0x80000009), dedup (0x80000013) and HSM (0xC0000004) are not, so dusage recurses into them today (lane win32-enum verdict; `windows_sys.rs` lines 2835-2836). The cloud-files tag (0x9000001A) is also not caught, but its value is unverified.
- **Fix.** On directories, test `FILE_ATTRIBUTE_REPARSE_POINT` (0x400) through the stable `MetadataExt::file_attributes` and skip. Do not skip non-directory reparse entries such as dedup'd or cloud-placeholder files. Their size comes from the find data, so no open or hydration is needed. Keep skipping symlink files.
- **Gain.** Closes the invariant 3 gap and stops crossing into network DFS targets. No speed change.
- **APIs and FFI.** None new. std only.
- **Admin.** No.

### 2. Verbatim- and UNC-aware protected-root guard (fastdel)

**Status: Done.** `guard::is_protected_root` normalises every spelling, canonicalises junctions and maps loopback UNC admin shares; fastdel applies it regardless of filters.

- **Problem.** `is_protected_root` (fastdel.rs lines 25-35) compares strings. `\\?\C:` has length 6, so `s.len() <= 2` misses it, and there is no UNC share-root rule. The guard runs only when `min_age_days < 0` and there are no prefixes (line 61). The lane 5 verdict confirmed both gaps.
- **Fix (proposal).** Use `Path::components()`: a path with no `Normal` components is a root. This covers `C:\`, `\\?\C:\`, `\\server\share\` and `\\?\UNC\...`. Compare `USERPROFILE` after the same normalization. Add `--dry` tests for each verbatim form.
- **Also.** Build deletion paths from an absolute root and use the `\\?\` prefix for paths over MAX_PATH. The maximum-path page was not fetched, so its details are unverified.
- **Effort and risk.** S, no risk.

### 3. Batched enumerator via `FileIdExtdDirectoryInfo` (replacing the FindFirstFileExW walk)

**Status: Done.** `win::enumerate::DirReader` uses `FileIdExtdDirectoryInfo` with a `FileIdBothDirectoryInfo` fallback. Speed unmeasured.

- **What it returns.** One batch per buffer gives `EndOfFile`, `AllocationSize` (on-disk size), `FileAttributes`, `ReparsePointTag` and a 128-bit `FileId` per entry, with no per-file open. Field order is verified (https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_extd_dir_info). Class values are `FileIdExtdDirectoryInfo` = 0x13 and `FileIdExtdDirectoryRestartInfo` = 0x14 (https://learn.microsoft.com/en-us/windows/win32/api/minwinbase/ne-minwinbase-file_info_by_handle_class). Minimum server is Windows Server 2012 / Windows 8, desktop only.
- **Correction on gain.** A batch call does not cut round trips compared with FindNextFileW. The real gain is the extra fields (allocated size and FileId), which otherwise need an open per file. Speed is unmeasured.
- **APIs (kernel32).** `CreateFileW(dir, FILE_LIST_DIRECTORY, FILE_SHARE_READ|WRITE|DELETE, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OPEN_REPARSE_POINT)`, `GetFileInformationByHandleEx(h, class, buf, len) -> BOOL`, `CloseHandle`, `GetLastError`. The flag constants are standard values, to be checked against winbase.h.
- **Fallback.** `FileIdBothDirectoryInfo` (Vista) gives `AllocationSize` and a 64-bit `FileId`. Its class number is by enum order and unverified here.
- **Implementation notes.** Store the buffer as `Vec<u64>` for alignment. Bounds-check `NextEntryOffset` and `FileNameLength` (bytes, not characters). Do not use `FileIndex` (undefined on NTFS). Keep all `unsafe` in one module, as AGENTS.md asks.
- **Risk.** Medium. A wrong struct layout misreads memory silently, so compare the output to the current walker on a fixture.
- **Effort.** M. **Admin.** No.

### 4. Hardlink-aware totals from FileId seen during enumeration

**Status: Done.** `scan` with `--dedup-hardlinks` (batched backend); links whose other name is outside the root are reported separately. Watch forces full rescans when it is on.

- **Idea.** Count `AllocationSize` only once per `(volume, FileId)` within the scan. Two entries with the same FileId in one scan are hardlinks, and the enumeration record shows this without extra opens. The volume serial can be read once from the root.
- **Gain.** Removes the overcount that AGENTS.md reports as 5-10%. That figure is the user's observation and is not verified here.
- **Caveats.** A hardlink whose other name lies outside the scanned root is still counted once, so report that count separately. Memory is roughly tens of bytes per file (an estimate). On volumes where FileId is zero or unsupported, fall back to no dedup. The FileId identity on ReFS and FAT is unverified.
- **Effort.** S after item 3. **Admin.** No. **Risk.** Low.

### 5. Condvar work queue with one lock (`lib.rs`)

**Status: Done.** `queue::par_for_each` is a single-mutex condvar queue and propagates panics.

- **Verified.** The lock guard is dropped at line 39, before `inf.fetch_add(1)` at line 43. Idle workers poll with `sleep(1 ms)` (line 41). In the window between pop and increment, another worker can see an empty queue with `inflight == 0` and exit. Correctness holds, but the tail runs on fewer threads.
- **Fix.** One `Mutex<State { queue, active }>` plus `Condvar`. Pop and `active += 1` happen in the same critical section. On finish, decrement under the lock and `notify_all` when the queue is empty and `active == 0`. `push` calls `notify_one`. Loop on the predicate to handle spurious wakeups. The `par_for_each` signature is unchanged.
- **Gain.** Better tail parallelism on uneven trees. Small, and to be measured. Condvar is stable since Rust 1.0 (from memory).
- **Effort.** S. **Admin.** No. **Risk.** Low.

### 6. fastdel: classify and report the final error

**Status: Done.** `model::ErrorKind/ErrorCounts` classify errors; `fastdel` prints category counts and the first failures. fastdel exits 1 when candidates failed.

- **Correction.** std's `remove_dir_all` already retries `SHARING_VIOLATION` and `DIR_NOT_EMPTY` up to 50 times with `yield_now` (sys/fs/windows/remove_dir_all.rs, lane 1 verdict). Adding blind retries would duplicate that.
- **Fix.** Replace the single `failed` counter with categories: access denied (which also covers delete-pending in forum reports, unverified), sharing violation (32), dir not empty (145), reparse, and other. Print counts and the first N paths per category. The numeric codes match commonly published `winerror.h` values but were not re-checked here.
- **Effort.** S. **Admin.** No. **Risk.** None.

### 7. fastdel: parallel removal inside large subtrees

**Status: Done.** Files are deleted in 512-entry chunks as separate work items with per-directory pending counts, using handle-based `win::delete` (POSIX disposition with classic fallback, junctions unlinked). Speedup not measured.

- **Current.** Each top-level child is one work item (fastdel.rs lines 128-142), so one very large child is deleted by one thread.
- **Design.** Walk once and push directories and files through `par_for_each`'s `push`. Give each directory an `AtomicUsize` counting outstanding children. The last child to finish removes the now-empty directory.
- **Gain.** Unknown. It depends on tree shape and must be benchmarked.
- **Risk.** Medium-high. Path-based per-entry deletes lose std's handle-relative protection against reparse-point races (the CVE-2022-21658 class, https://blog.rust-lang.org/2022/01/20/cve-2022-21658/). Items 1 and 2 must land first. A `FILE_DISPOSITION_INFO_EX` POSIX delete could avoid `DIR_NOT_EMPTY` on the parent. Its flag values are verified (DELETE = 0x1, POSIX_SEMANTICS = 0x2, IGNORE_READONLY_ATTRIBUTE = 0x10, https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntddk/ns-ntddk-_file_disposition_information_ex). Its Windows version floors are unverified and the sources conflict. Use it only with fallback to classic `FILE_DISPOSITION_INFO`.
- **Effort.** M. **Admin.** No.

### 8. Opt-in named-stream accounting (`--streams`)

**Status: Done.** `--streams` (slow, opt-in) via `FindFirstStreamW`. Whether `AllocationSize` includes named streams is still unchecked (benchmark 5).

- **Verified.** `FindFirstStreamW` returns the unnamed `::$DATA` stream first. It fails with `ERROR_HANDLE_EOF` when there are no streams and `ERROR_INVALID_PARAMETER` on file systems without stream support (https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findfirststreamw).
- **Unverified.** Whether named-stream bytes appear in `AllocationSize`. The `WIN32_FIND_STREAM_DATA` layout must be checked against the SDK headers before writing the FFI struct.
- **Effort.** S-M. **Admin.** No. **Risk.** Low. Keep it opt-in: it costs one extra call per file.

### 9. USN change journal for incremental `--watch`

**Status: Done.** `win::usn` (query/read, V2/V3 records, path by reference) drives `--watch`
rescans of changed top-level entries, with full-rescan fallback. Validated on a real journal in
an elevated shell on DOOMBRINGER, 2026-11-21 (C:, 1 TB NTFS, journal id `0x01dd3a7d013d1acb`,
first USN `0x166b800000`, 32 MB maximum), and on a throwaway 512 MB VHD formatted NTFS, whose
journal contains only the experiment's own records. Record shape and the harness that reproduces
it: `bench/windows/admin.ps1` section 3, `bench/windows/usn_probe.py`, rows in
`bench/windows/admin-results.jsonl` and `bench/windows/checks.json`.

- **What the feed reports.** A 38-file fixture (3 subdirectories), 12 changes in 4 groups two
  seconds apart: 4 creates at the root, 2 data extends plus 1 create in a subdirectory, 2
  renames, 3 deletes. The watch reported exactly **4** update lines, naming exactly the touched
  top-level entries (`new1.bin, new2.bin, new3.bin, new4.bin` / `a, b, c` / `a, f1.txt, f1r.txt` /
  `c, f2.txt, f3.txt`), and 144 stdout lines in total — five reports of a 38-entry tree.
- **The journal's own account of those 12 changes is 28 records**, verified twice: by
  `bench/windows/usn_probe.py` and by `fsutil usn readjournal` over the same USN range. The
  histogram is the accounting: 5 `FILE_CREATE` + 5 `DATA_EXTEND+FILE_CREATE` +
  5 `DATA_EXTEND+FILE_CREATE+CLOSE` (3 records per create, because NTFS journals the create, the
  data write and the close), 2 `DATA_EXTEND` + 2 `DATA_EXTEND+CLOSE` (2 per extend), 2
  `RENAME_OLD_NAME` + 2 `RENAME_NEW_NAME` + 2 `RENAME_NEW_NAME+CLOSE` (3 per rename), 3
  `FILE_DELETE+CLOSE` (1 per delete). Identical histogram on the clean volume, where no other
  process can contribute a record.
- **Incremental equals full.** After the run, the last update's totals and every top-level
  total matched a fresh full scan of the same tree exactly (`{"bytes":6704,"alloc":6848,
  "files":40,"dirs":3}`).
- **Nothing is re-read from the beginning.** `DUSAGE_WATCH_TRACE=1` prints one line per poll
  (`N records, usn A..B`); the spans are contiguous, strictly advancing, and start at the USN
  captured before the initial scan.
- **Fallback, measured.** Deleting the journal under a running watch produced
  `change journal reset (ERROR_JOURNAL_NOT_ACTIVE (1179)); full rescan`, then
  `no change feed (...); full rescan` once per interval, and the watch recovered by itself when
  the journal was recreated, reporting the next change. A non-elevated run has no feed at all:
  opening the volume device `\\.\X:` fails with error 5, which is now reported as
  `no change feed (Access is denied. (os error 5)); full rescan`.
- **The over-reporting this item was opened for was the fallback, not the journal.** Both
  fallback arms rescanned and printed **without sleeping**, so a non-elevated `--watch` ran at
  the speed of its own rescans: the same script, fixture and host measured **142,929 stdout
  lines from 5,063 full rescans in 14 s** on the previous build against **370 lines from 12
  rescans** on this one, and the twelve known changes still produce four update lines when the
  feed is available. A watch that looks like it is flooding is a watch whose feed never opened.
- **Reason mask.** `USN_REASON_CLOSE` is out of the mask, so a close-only record cannot trigger
  a rescan; a record that carries CLOSE beside a real reason still arrives through that bit. On
  this workload the mask measured identical (28 records either way) because every close came
  bundled with its change — the exclusion removes a class of no-op, it does not change these
  counts.
- **Fallback.** On any read error (purged records, deleted journal) the caller does a full
  rescan, reopens the journal, and never rescans faster than the interval.
- **Purge, measured.** When the records a slow watch has not read are trimmed out of the
  journal, the next read fails with `ERROR_JOURNAL_ENTRY_DELETED` (1181) and the watch does what
  it should: it reports `change journal reset (...); full rescan`, rescans everything, reopens
  the journal, and its totals still match a full scan. Measured on the same 512 MB VHD, whose
  journal is the 1 MB it gets at format time, with the same 20,000 files written two ways —
  `bench/windows/usn_purge.ps1`, rows in `bench/windows/purge-results.jsonl`:

  | case | written | poll | updates | `changed:` lines | reset lines | totals equal |
  | --- | --- | --- | ---: | ---: | ---: | --- |
  | burst | 20,000 files in 2026 ms | 10 s | 2 | 0 | **1** | true |
  | paced | the same 20,000 files, 2,000 per round over 12,939 ms | 1 s | 15 | 14 | **0** | true |

  The trigger is the journal's maximum size against the records written between two polls, not
  the volume's size: a 1 MB journal loses a burst of ~5.8 MB of records (20,000 creates at the
  measured three records and ~96 bytes each, read as 60,000 records in one poll when it does
  catch up), while the same burst is roughly a fifth of C:'s 32 MB journal. C:'s journal was not
  shrunk and no purge was forced there, per the owner's instruction; the 32 MB and 1 MB
  geometries are both `fsutil usn queryjournal` output.
- **Journal geometry is fixed at format time on this build.** `fsutil usn createjournal
  m=0x4000000 a=0x800000` on a freshly formatted volume returns 0 and leaves `Maximum Size` at
  `0x100000` (1 MB) with a 256 KB delta, so the small-journal-versus-large-journal comparison
  this item originally wanted cannot be set up with that command here. The purge experiment
  instead varies what the watch actually consumes between polls.
- **Unverified.** Extended record version 3 was not observed in any journal here; C: and the VHD
  both report "maximum record version 4" and emit V2 records.

### 10. Elevated whole-volume fast path (USN enumeration plus MFT parsing)

**Status: Done.** `win::mft` (fixup, runlists, record parser, parallel unbuffered volume reader, extension-record merge) backs `--backend mft`. It was validated in an elevated shell on two NTFS volumes. Its totals match the batched backend, with and without `--dedup-hardlinks`, plus the NTFS metadata files on a volume root. Benchmarked in the README: 10.6 s against 65.8 s for a whole system drive with 8.6M files. Reading the table (about 2 GB/s) dominates.

- **Verified.** `FSCTL_ENUM_USN_DATA` on `\\.\X:` is NTFS-only. You loop on `StartFileReferenceNumber` (https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_enum_usn_data). It yields names and parents, not sizes.
- **Sizes need MFT parsing.** That means the FILE-record header, update-sequence fixup and runlists. Only the struct members and the NTFS 3.0/3.1 version gate are verified (https://learn.microsoft.com/en-us/windows/win32/devnotes/file-record-segment-header). The byte offsets are community NTFS documentation and unverified.
- **Admin.** Expected, not stated by the Microsoft page. Test it.
- **Gain.** Potentially large for whole-volume scans. WizTree-style speed claims are marketing and unverified.
- **Effort.** L. **Risk.** High (large unsafe surface). Gate it behind a benchmark that shows directory I/O dominates.

### 11. Dev Drive guidance for scratch trees (documentation only)

**Status: Done (documentation).** Written as `docs/DEV_DRIVE.md`: the requirements below, what each command does on such a volume as read from the code, and what is still unknown. No command has been run on a Dev Drive or a ReFS volume; the ReFS rows of `bench/windows/admin.ps1` are still owed.

- **Requirements (verified).** Windows 11 build 10.0.22621.2338 or later, 50 GB minimum, designation only at format time, C: not eligible, `fsutil devdrv trust` needs an elevated shell, performance mode needs a trusted volume with real-time protection on (https://learn.microsoft.com/en-us/windows/dev-drive/).
- **Causal claim refuted.** The WinDev issue reports a 119,954-file tree delete at 58 s with protection on, 57 s with the folder excluded, and 45 s with protection disabled (https://github.com/microsoft/WinDev/issues/27). Defender does not explain the delete time. The restic thread is a backup scan, not a delete timing.
- **Recommendation.** Documentation only. Do not recommend disabling protection.
- **Effort.** S. **Admin.** Setup only. **Risk.** None.

## 2. Facts that correct AGENTS.md

1. **"`DirEntry::metadata()` is one syscall" is wrong.** On Windows it returns `Ok(self.data.into())`, a conversion from the cached `WIN32_FIND_DATAW` with no OS call (std `sys/fs/windows.rs`, around line 192; lane 1 and lane 3 verdicts). The per-directory cost is in enumeration batches and in any per-file open. Rewrite the note accordingly.
2. **Invariant 3 is incomplete.** `FileType::is_symlink()` is true only for name-surrogate reparse tags (symlinks and junctions). Other reparse directories (DFS, WIM, CSV, dedup, HSM, and possibly cloud) are `is_dir()` and get recursed into. The rule should test `FILE_ATTRIBUTE_REPARSE_POINT` on directories (item 1).
3. **Invariant 4's guard does not cover all forms.** The guard is right in intent, but `\\?\C:` and UNC roots pass it (item 2).
4. **"The tool is syscall-bound, so thread count matters more than CPU work"** is a hypothesis. It rests on the wrong per-file metadata premise above and is unmeasured. Reword it as a hypothesis to test.
5. **"Hardlinks inflate naive sums by 5-10%"** is the user's observation and is not verified here. The mechanism is right: each name is counted.
6. **Not checked here.** "Directory mtime = last direct-child create/delete" and "`robocopy /L /E /XJ` is a decent oracle." `/XJ` addresses junctions only, so its behaviour on other reparse directories is unverified.
7. **FFI note, not an AGENTS fact.** The crate is edition 2021, so plain `extern "system"` blocks compile. Edition 2024 requires `unsafe extern`.

## 3. Lane findings dropped or corrected

- **Dropped.** `NtQueryDirectoryFileEx` class 60 as a roadmap item (the value and signature are unverified and the gain is unclear). The Project Zero pool-leak claim. The WizTree 4x figure. The Defender causal explanation for delete speed.
- **Corrected.** The lane 4 premise that `FILE_ID_BOTH_DIR_INFO` avoids metadata calls is wrong. Its real gain is `AllocationSize` without an open. The lane 4 claim that `FILE_ID_INFO` requires Windows 8 is not supported: the client minimum is "None supported" and the server minimum is Windows Server 2012 (desktop apps only).
- **Corrected.** `FIND_FIRST_EX_LARGE_FETCH` changes buffering only. Its benefit is unmeasured, and the Raymond Chen warning about early-abort searches was not fetched.
- **Corrected.** Item 6 is reporting, not retry, because std already retries.
- **Not used as facts.** `FILE_DISPOSITION_INFO_EX` version floors (1607, 1709 and 1809 are cited in conflicting sources). The NTFS-only restriction. The claim that the name "disappears at once" (the sources disagree on when removal happens). The FSCTL_ENUM_USN_DATA admin requirement, which is an expectation. Zero allocation for resident files. The `GetCompressedFileSizeW`-follows-symlinks claim was verified from the doc and is kept only as a test oracle.

## 4. Questions measured on a real machine (answered 2026-11-21)

All twelve ran on DOOMBRINGER (Windows 11 10.0.26300, i7-13700KF, 24 threads, 48 GB, three
local NTFS volumes) with the harness in `bench/windows/`. Every number below is that harness's
output; `docs/BENCHMARKS.md` ("Windows (DOOMBRINGER)") holds the tables, the conditions
record and the caveats. Re-run the harness rather than trusting this list if the scanner moves.

1. **Enumeration cost. Answered.** The batch the tool already ships is the fastest spelling on
   the real tree — 5334 ms for `C:\Windows` against 6429 ms for the plain `FindFirstFileExW`
   walk — and level with it on a generated 1M-file tree (206.8 against 265.7 ms). Buffer size
   is not a lever: 64 KiB, 256 KiB and 1 MiB land inside one another's spread, so the 64 KiB
   default stays. `FIND_FIRST_EX_LARGE_FETCH` takes 25% off the plain walk on flat and wide
   trees (265.7 → 198.7 ms) and costs 8% on the deep real tree (6429 → 6942 ms), so it is not
   worth a second code path. The per-directory cost is what dominates: 1M entries in 1000
   directories enumerate at 3.8M/s, while `C:\Windows` pays 41k/s for 264k entries arriving
   from 68k directory opens.
2. **Thread sweep. Answered, and the default changed with it.** Both shapes plateau at eight
   workers, and past that the directory-heavy real tree is slower: 752 ms at 8 threads, 755 at
   16, 795 at 24, 850 at 32. That is a latency ceiling, not a CPU one — 24 hardware threads
   exist and the eighth worker already reaches the wall. `native::MAX_WORKERS` is now 8 on
   Windows, so the default keeps the throughput and halves the workers; Linux keeps its
   inherited 4..16 clamp, which is unswept above 2 vCPUs rather than measured. The
   "4x cores" hypothesis is dead.
3. **Protection effect. Measured in the elevated half of the harness.** `admin.ps1` runs the
   same scan and the same delete with real-time protection on and with the fixture root
   excluded, and removes the exclusion before it returns. Measurement only: nothing here
   recommends turning protection off.
4. **Delete strategy. Answered.** Parallelism is the whole win. `fastdel` deletes a 20k-file
   tree in 514 ms where serial `remove_dir_all` takes 1231 ms and the handle-based serial walk
   1225 ms (POSIX) or 1279 ms (classic); on 5k × 128 KiB files it is 189 ms against 431, 419
   and 424. The dispositions are a tie, so the POSIX-first-with-classic-fallback order stays.
   Every row ended with zero errors in every category from item 6.
5. **Allocation fidelity. Answered.** `AllocationSize` follows compression (1,048,576 →
   196,608) and sparseness (→ 0) exactly and agrees with `GetCompressedFileSizeW` on both; it
   reports 8 bytes for a one-byte resident file where `GetCompressedFileSizeW` reports 1; and
   it **does not include named streams** — a 4-byte file with a 1 MiB alternate stream reports
   8. Item 8's open question is closed, and `--streams` stays opt-in for that reason.
6. **FileId semantics. Answered for NTFS.** Hardlinks share one 128-bit `FileId`, and
   `fsutil file queryFileID <path>` (no `=`) prints the same value the handle API returns.
   FAT32 and ReFS come from the VHDX fixtures in the elevated half (`bench/windows/admin.ps1`,
   section 5). Nothing attaches or formats a Dev Drive, so Dev Drive stays unanswered.
7. **Reparse gap. Closed and confirmed closed.** `is_symlink()` is true for a junction on this
   build, and the tool no longer depends on it: it tests `FILE_ATTRIBUTE_REPARSE_POINT`, so
   the DFS and dedup tags `is_symlink()` misses fall under the same rule. A junction fixture
   scans as 8 files and 1 directory with its target counted once. `find` not listing a link
   without `--follow` is asserted by `tests/search_cli.rs` and is a documented choice.
8. **Privileges. Answered.** Opening `\\.\C:` or `\\.\D:` fails with error 5 for a
   non-elevated token and succeeds elevated, which is why `--mft` documents an administrator
   shell.
9. **Junction at top level. Answered.** `fastdel` unlinks a top-level junction and leaves its
   target alone; `tests/fastdel_cli.rs::junctions_unlinked_not_followed` pins it.
10. **POSIX delete. Answered.** `FILE_DISPOSITION_FLAG_POSIX_SEMANTICS` is accepted on this
    build. While the handle is open the name resolves but refuses to open (error 5, delete
    pending); it is gone once the last handle closes. Since it is also no faster than the
    classic path, the fallback order stays as it is.
11. **Overcount. Answered, and 5-10% is far too low.** `C:\projects`: 3.58 GB naive against
    2.95 GB with `--dedup-hardlinks`, 21.4%. `C:\forge`: 6.51 against 4.53 GB, 43.7%. The
    pnpm store itself is 0%, which is the control that explains it: its links point into
    project trees, and a link whose other name is outside the root is counted once.
12. **Long paths. Answered.** A 393-character path opens and deletes with the verbatim prefix,
    which is what the tool always uses. This build also has `LongPathsEnabled=1`, so the plain
    spelling works here too; the prefix is what makes it work where that flag is off.

## 5. Remaining work

- The elevated half of `bench/windows/admin.ps1` — real-time protection A/B, `--mft`
  against the batched backend and `robocopy /L /E /XJ`, the cold-cache pass and the FAT32/ReFS
  FileId checks — needs one administrator approval on the machine and is the only part of
  section 4 still owed. Its USN `--watch` section is done: it now records the ground-truth
  experiment from item 9 (twelve known changes, the journal's own record count as an oracle,
  totals against a full scan) instead of counting a stdout stream.
- Run the benchmark list in section 4 and record results: done 2026-11-21, and the thread
  sweep decided the default (8 workers on Windows, `native::MAX_WORKERS`).
- Item 11: Dev Drive guidance is written (`docs/DEV_DRIVE.md`). Running the tools on a real
  Dev Drive is not done; the guidance says so row by row.
- Done in 0.5.0: the tools merged under subcommands in one binary (`dusage
  scan|big|search|find|prune|du`), with the `fastdel` binary and `dusage fastdel|del`
  running `prune`; and fastdel presets (`--preset`, `--print-presets`).
- Done in 0.6.0: `dusage tree` and `dusage df` (`dusage scan|big|search|find|prune|du|tree|df`).
- Open after 0.6.0: tree buffers one record per entry (measured side by side with `tree -s`
  in `docs/BENCHMARKS.md`); the Windows `df` path is compiled but has never run on a Windows
  box, so its volume enumeration and its folder-mounted volumes are unverified.

## Sources

- Rust std source (1.100.0): `library/std/src/sys/fs/windows.rs` and `library/std/src/sys/fs/windows/remove_dir_all.rs`
- Repo: `src\lib.rs`, `src\bin\dusage.rs`, `src\bin\fastdel.rs`, `AGENTS.md`, `Cargo.toml`
- Microsoft Learn: `FindFirstFileExW` (fileapi), `FILE_ID_EXTD_DIR_INFO` and `FILE_ID_BOTH_DIR_INFO` (winbase), `FILE_INFO_BY_HANDLE_CLASS` (minwinbase), `USN_RECORD_V2` and `FSCTL_ENUM_USN_DATA` (winioctl), `FILE_RECORD_SEGMENT_HEADER` (devnotes), `RemoveDirectoryA` (fileapi), `FindFirstStreamW` (fileapi), `FILE_DISPOSITION_INFO_EX` (ntddk), Dev Drive (windows/dev-drive)
- `https://devblogs.microsoft.com/oldnewthing/20131024-00/?p=2843` (not fetched)
- `https://github.com/microsoft/WinDev/issues/27`
- `https://blog.rust-lang.org/2022/01/20/cve-2022-21658/`
## Linux port (0.4.0)

**Done.**

- `src/unix/` mirrors `src/win/`: `enumerate` (read_dir + one `lstat`/`statx` per entry,
  `st_blocks` for on-disk, `dev << 64 | ino` for hardlink identity, symlinks as the reparse
  class), `delete` (unlink/rmdir, never follows a symlink, stops at a mount point), `watch`
  (inotify, per-directory watches, `IN_Q_OVERFLOW` forces a full rescan) and an errno table.
- `--one-filesystem` (`-x`): skip a directory on another device and count it.
- Linux guard: `/`, the immutable system trees, the data roots, `/home` and `$HOME`, plus
  their ancestors, canonicalised so a symlink to `/etc` is caught.
- CI on `ubuntu-latest` including a `/usr` oracle comparison against `find -printf`, and a
  Linux tarball plus loose binaries on every tagged release.

**Measured** (Ubuntu 24.04 container, Linux 6.1, 2 vCPU, overlayfs, warm cache, release):

| Workload | dusage | baseline |
|---|---|---|
| Scan `/usr`, 180,497 files / 7.99 GB | 0.31 s | `du -sb` 0.62 s, `find -printf` + awk 0.68 s |
| Scan `/` with `-x`, 251,816 files, 7 mounts skipped | ~1 s | — |
| Delete 50,301 entries (300 dirs × 100 files) | 0.36 s | `rm -rf` 0.60 s |

Totals agree with `find` exactly (7,994,136,968 bytes); `du` differs only by counting each
inode once and adding directory sizes, which `--dedup-hardlinks` reproduces within
directory-entry noise.

**Open.**

- The raw `getdents64` enumerator and the per-entry `statx` this list used to call
  unexplored are the backend as shipped (`src/unix/enumerate.rs`, since #45): one
  `getdents64` per directory into a reused buffer and one `statx` per entry. Measured at
  48a92f6 on the 4-vCPU container: `scan /usr` costs 267,507 `statx`, 35,572 `getdents64`,
  17,774 `openat` and 35,536 `close` for 180,451 entries, i.e. exactly one stat per entry,
  and 34% of its instructions are in `malloc`/`free` from the per-entry name allocation.
  Four candidate changes were measured and rejected: a 256 KiB batch and a SWAR scan for
  each record's NUL are inside the run-to-run spread (the SWAR scan is 2.4% more
  instructions on 12-byte names), the `statx` mask cannot be trimmed because
  `--newerat`/`--newerct` and the ownership tests read every field `STATX_BASIC_STATS`
  fills, and `STATX_MNT_ID` adds nothing because the same call already returns the device.
  What is left is `io_uring`-style batching, which the zero-dependency rule and the
  read-only contract both argue against.
- Thread sweep and cold-cache numbers on a real machine (2 vCPUs here, and the machine is a
  container on overlayfs, so delete timings are not comparable to ext4 or btrfs).
- `--watch` under `max_user_watches` pressure: the fallback is implemented (full rescans)
  but has only been tested by construction, not by lowering the limit.
- macOS build numbers, arm64 and FSEvents — see the macOS section at the end.

## Search suite, find and du (0.5.0, as shipped)

**Status.** `search` (contents), `find` (paths), `du` (per-directory totals) and
`prune` share one binary with `scan`/`big`. What follows is the state of branch
`docs/truth` at `c9dd6a0`; every behaviour below was run against the built
binary while `docs/SEARCH.md` was rewritten, and the performance numbers are the
ones in `docs/BENCHMARKS.md`.

**Done.**

- `search`: the flag surface matches `dusage search --help`. Case modes
  (`-i -s -S`), word and invert, `-n --column --byte-offset --vimgrep --heading
  --trim`, the three separator flags and `--path-separator`, `-H -h`, `-l -L
  -c --count-matches --include-zero -o -q -m`, `-A -B -C --stop-on-nonmatch`,
  `-a -I --binary-files -d`, `-F -E -G -P --engine -e -f -r`, `-g --iglob -t -T
  --type-list --type-add --type-clear --include --exclude --exclude-dir`, `-u`/`-uu`,
  `--max-depth --max-filesize -M --max-columns-preview --encoding`, `-Z -0
  --null-data`, `--hidden`, the `--no-ignore-*` family, `--ignore-file` with
  `--ignore-file-case-insensitive`, `--follow`, `-x`, `--sort --sortr --sort-files`,
  `--passthru`, `--pre --pre-glob`, `-z`, `-U --multiline-dotall`, `--json
  --color --stats --no-messages`, and the ripgrep no-op flags (`--mmap` pair,
  `--binary`, `--line-buffered`, `--block-buffered`, `--crlf`).
- `find`, glob form: a glob with `* ? ** [...] [!...] {a,b}` and a trailing `/`,
  `-F` for a substring, `-i`, `--name`/`--full-path`, `-t`/`--type`, `--ext`,
  `--size`, `--older`, `--newer`, `--newermt`/`--newerat`/`--newerct`, `--empty`,
  `--mindepth`/`--max-depth` with find's own numbering, `--dirs`/`--files`,
  `--long`, `--sort`/`--sortr`, `--follow`, `-Z`, `--json`, and the same walk
  filters and ignore layers as `search`.
- `find`, expression form: GNU find's language wired to the CLI (`src/findexpr.rs`
  plus `split_find_args` and `search::walk_expr`). `-name`/`-iname`, `-path`,
  `-regex`, `-type`, `-size`, `-empty`, `-perm`, `-user`/`-group`/`-uid`/`-gid`,
  `-links`, `-inum`, `-newer*`, `-mtime`/`-atime`/`-ctime` and the `-min` forms,
  `-fstype`, `-true`/`-false`, `-a`/`-o`/`!` with parentheses, `-printf`, `-ls`,
  `-print`/`-print0`, `-prune`, `-quit`, `-depth`, `-xdev`, `-exec`/`-execdir`
  behind `--allow-exec`, and `-delete` refused in every spelling. Measured
  against findutils 4.9.0: identical selections for `-name '*.gz' -type f`,
  `-size +100k` and a `-prune` expression, the same set in a different order for
  `%p %s` `-printf`, `-maxdepth 2 -type d` and `-mtime -3 -type f`, identical
  `-printf` output for one entry's `%p %s %y %m %TY-%Tm-%Td`, and identical
  `-exec … {} +` batch contents. `docs/SEARCH.md` §1.2 has the table and the
  exact commands.
- **The regex engine of our own** (`regex.rs`): a Thompson NFA, run by a lazily
  built DFA keyed by byte classes with the Pike VM underneath it, so no pattern
  backtracks. It has two programs now — a byte program for ASCII lines and a
  character program for any line holding a non-ASCII byte or any pattern naming
  something only Unicode can answer (`\p{L}`, `[[:alpha:]]`, `\w`, `\b`, `-i`
  over non-ASCII). That closed the Unicode gap: `\w+` matches `café`, `\p{L}`
  works on the default engine, `-i 'CAF[É]'` matches `café`, and `\b` and `-w`
  now agree on non-ASCII text. A plain-literal pattern still skips the engine
  for the vectorised byte scan, and the rest is prefiltred by a mandatory byte
  and by the longest run of bytes every match must contain.
- **The `-P` engine** (`pcre.rs`): a bounded backtracking matcher reached by
  `-P`, `--engine pcre2`, or automatically when the linear engine rejects the
  pattern. It implements the constructs the other engine cannot: lookahead and
  bound-limited lookbehind, `\1`/`\k<name>`/`(?P=name)` backreferences, named
  groups, atomic groups, possessive quantifiers and `(?#…)` comments. Case
  folding there is ASCII-only, `\p{..}` is refused, capture groups are not
  recorded (so `-r '$1'` is refused under `-P`), and every call runs under a
  1,000,000-instruction budget.
- **Multi-line search**: `-U`/`--multiline` over the whole file, with
  `--multiline-dotall`; `-v` and the context flags are refused rather than
  answered wrongly, and a pattern spelling a newline without `-U` is refused.
- **Compressed input**: `-z` expands gzip, bzip2, lz4, xz, zstd and brotli
  through decoders written in std (`archive.rs`, `archive_bz2.rs`,
  `archive_lz4.rs`, `archive_xz.rs`, `archive_zstd.rs`, `archive_brotli.rs`,
  with the shared Brotli dictionary in `archive_brotli_dict.rs`).
- **Follow**: `--follow` walks links once per real directory, comparing the real
  path chain so a link back up the tree is skipped. `-L` stays
  `--files-without-match`.
- **Ignore handling** matches ripgrep where it matters and beats it where it is
  opinionated: the built-in cache list, git's global excludes file
  (`core.excludesFile`, else `$XDG_CONFIG_HOME/git/ignore`), `.gitignore` and
  `.ignore` per directory with deeper files winning, `.git/info/exclude` at a
  `.git` directory, the rule files of a root's ancestors, and `--ignore-file`
  for the caller's own. A positive `-g` switches the file layers off.
- **`du`** (`cmd/du.rs`): a du-compatible per-directory report printing
  allocated blocks by default (`st_blocks * 512`, `AllocationSize` on Windows),
  with `-a -s -c -d -b --apparent-size -k -m --block-size -h --si --inodes -S
  -t -x --exclude --exclude-from --time --time-style --threads --stats`. On
  `/usr` its totals are byte-identical to GNU du 9.4 and it is faster (§ below).
- **Benchmarks**: a measured suite (`bench/run.sh`, `bench/summary.sh`,
  `bench/cases.sh`), peak-RSS and cold-cache passes, and a regression guard
  (`scripts/perf-guard.sh`) with a machine-specific baseline. The numbers and
  the caveats live in `docs/BENCHMARKS.md`.

**Measured.** `docs/BENCHMARKS.md` is the document of record and prints only
generated tables: `bench/summary.sh` over `bench/results.json`, plus the guard
output. Read the case ids there — `search-rare`, `search-common`, `search-ci`,
`search-regex`, `search-regex-nomatch`, `search-count`, `search-only-matching`,
`search-multiline`, `search-gzip`, `search-files-with-matches`, `find-name`,
`find-path`, `find-type`, `find-expr`, `du-s`, `du`, `threads-scan-*`,
`threads-search-*` — rather than a table copied here, because a copy goes stale
at the next merge. The ratio there is dusage over the fastest reference, so **below
1.00x is dusage ahead** — an earlier revision of this paragraph read that backwards and
called 0.94x "behind". At the commit the tables were last generated at (2cdb809, a
4-vCPU container, warm cache) the metadata rows win outright (`scan` 0.27x, `du`
0.28-0.35x, `find-name` 0.59x, `find-path` 0.70x, `find-type` 0.71x, `find-expr` 0.99x),
the output-heavy content rows are well ahead (`search-files-with-matches` 0.69x,
`search-count` 0.70x, `search-multiline` 0.75x, `search-common` 0.80x,
`search-only-matching` 0.84x), and the plain content rows are level with `ripgrep`
(`search-rare` 1.04x, `search-regex` 1.05x, `search-regex-nomatch` 1.01x, `search-ci`
0.97x) — inside the ±10% that a shared container moves those rows between passes, and
behind on one row, `search-multiline-unicode` at 1.15x. The suite cannot resolve anything
smaller than that; `docs/BENCHMARKS.md` says so in its noise bullet and the
`bench/records/` pair is what a before/after claim has to cite.

**Known limits, each an open item.**

- **Peak RSS is bounded, and still above the references.** 8-17 MiB on the
  content rows against both `grep`'s 5 and `ripgrep`'s 9–10, 15 MiB on
  `find-name` against find's 5, and 19–20 MiB on `du` against 4. It no longer
  scales with the tree, because results stream with a 4 MiB printer window and a
  `threads × 1024`-file path window. The floor above the references is the
  pipeline itself — a read buffer and a queue per worker — so closing it means
  giving up the threads that make the other rows ahead.
- **Half of content search is kernel time.** Over `/usr/local/go` a run spends
  roughly 60 ms in user space against 180 ms in the kernel, so the
  instruction-level work of the last rounds has run out of room: the plain rows
  sit at 0.90–0.94x of their reference and further gains there have to remove
  per-file syscalls (an open, a read per 64 KiB block, a close, plus the walk's
  `statx` per entry) rather than instructions. The find walks are `getdents`-bound
  for the same reason.
- **`find`'s small-tree rows are level, not comfortably ahead** (`find-path`
  0.98x, `find-expr` 0.99x, `find-type` 1.03x), where the cost is keeping the
  sorted output order that GNU find never has to produce, on a job of some 40 ms
  where startup is a visible share. The big-tree row (`find-name` 0.71x) is
  comfortably ahead.
- **The expression walk is sequential**, because `-prune` and `-depth` decide the
  next step from the entry just seen: `search::walk_expr` cannot use the worker
  pool. Parallelising it needs a per-directory evaluator that can defer `-prune`
  decisions, not a thread-pool change. `find-expr` in `bench/cases.sh` is its
  row; the round that took it to 0.99x cut its instructions 38% and moved its
  clock not at all, which is the measurement that says its remaining cost is
  syscalls.
- **`--no-unicode` has no equivalent.** The engine picks the byte path per line
  and the character path for everything else; there is no switch that forces
  byte semantics on a non-ASCII line, and `--glob-case-insensitive`,
  `--hyperlink-format`, `--debug`/`--trace` and ripgrep's full `--encoding`
  label set are all absent. `docs/SEARCH.md` §1.3 lists them with the letter
  collisions (`-L`, `-E`, `-I`).
- **No mmap.** Files are read in blocks into a buffer each worker reuses; mapping
  was measured slower on `/usr` (6.4–7.5 s against 3.7–4.8 s), and the profile of
  the parity rows names no page faults, so ripgrep's advantage there is not
  mapping but asking the kernel fewer questions per file.
- **`find --help` still lists five flags the parser rejects** (`--pre`,
  `--pre-glob`, `--binary-files`, `-I`, `-d`), documented in the appendix of
  `docs/SEARCH.md`. The other four it once carried (`--include`, `--exclude`,
  `--exclude-dir`, `--iglob`) are now walk filters `find` honours.

**Open questions worth measuring later.**

- **Answered: where the time goes in `-o`.** Not in the printer. Callgrind over
  `/usr/local/go` at 48a92f6, one thread: `-o` costs 367.9M instructions against 333.5M
  for the same query printed as lines and 299.6M for `-c`; inside the `-o` run the
  per-match loop (`block_matches`, 202.6M, most of it the SIMD literal scan) is 55%, the
  allocations of the per-match records and their pooled text are ~16%, and
  `SearchStream::file` is 5.8% (21.4M). So the lever is the match loop and what it
  allocates per file, not how the records are written: the 0.6 round floors those buffers
  and counts `-o` 367.9M -> 363.3M and the plain mode 333.5M -> 321.7M, where a rewritten
  printer would chase 6%.
- Whether the literal fast path is worth a multi-byte (Two-Way or memchr-style)
  search for needles longer than a couple of bytes.
- Whether the expression walk can be parallelised per directory without losing
  `-prune`/`-depth` semantics.
- `--stats` is one line today; a per-directory breakdown would make "why is this
  slow" answerable without a profiler.

## macOS (x86_64 and arm64), as running

**Status.** `lib.rs` accepts `target_os = "macos"` and aliases `src/mac/` as `crate::native`,
the third arm beside `src/unix/` and `src/win/`. Two things prove it, in this order: a
cross-check from Linux for `x86_64-apple-darwin` and `aarch64-apple-darwin` — `cargo check`,
`cargo check --tests` and `cargo clippy --all-targets -D warnings` — and two CI legs that run
the same format, lint, test and release-build steps on real Macs: `macos-15-intel` and
`macos-15` (Apple Silicon). Both are green, so the symbols resolve and the layouts hold on
both architectures: everything the cross-check could not prove, the runners proved.

**Both architectures are verified by a run, and they are verified separately.** The Intel leg
executes the `$INODE64` bindings; the Apple Silicon leg executes the plain ones, because
`sys.rs` gates every alias to `not(target_arch = "aarch64")`. That second leg is the first
test the plain names ever had, and it is why adding it was worth more than a fifth run of the
Intel path. Cross-checking remains the fast gate — it proves the `cfg` wiring, the types and
that the FFI declarations compile — and the runner is what proves symbols and layouts.

**Done.** Enumeration is `open` + `fdopendir` + `readdir$INODE64`, with `fstatat$INODE64`
per entry against the open directory descriptor, so `alloc` is `st_blocks * 512` and
`file_id` is `dev << 64 | ino` exactly as on Linux. Deletion is `unlink`/`rmdir`, path-based
and never following a symlink. `--watch` is kqueue. `df` reads `statfs$INODE64` — not
`statvfs`, whose Darwin block counts are 32-bit and which carries no type name — and the
mount table behind both it and the guard is `getmntinfo$INODE64`. The guard refuses `/`, the
system trees, `/Users`, `$HOME`, `/opt` and every mount root the kernel reports, wherever it
is mounted; `-fstype` answers from the same table, keyed by the device `stat` reports for
each mount point.

**Learned by running.** Four facts that no amount of cross-checking produced, each with the
consequence it has for the next change here.

- **A 64-bit caller must bind `$INODE64`, and `cargo check` cannot know it.** Darwin's
  headers resolve `readdir`, `stat`, `lstat`, `fdopendir`, `fstatat`, `statfs` and
  `getmntinfo` onto suffixed symbols, and the suffixed and plain versions are not
  interchangeable: the plain `fdopendir` allocates the stream in the legacy 32-bit record
  format, so *every* listing came back empty — 52 of 629 tests on the first macOS run — and
  the plain `getmntinfo` fills the legacy `struct statfs` instead of
  `__DARWIN_STRUCT_STATFS64`, so the record count is right and every mount point and type
  name reads as zeroes. Two failures, one rule: symbols resolve at link time and nowhere
  earlier, so a new `extern "C"` name here is verified by the CI leg or not at all.
- **Refusing `/private` subtree-wide makes `$TMPDIR` unprunable.** macOS's `/var` and `/tmp`
  are `/private/var` and `/private/tmp`, and `$TMPDIR` lives at
  `/private/var/folders/<xx>/<hash>/T`, so a whole-subtree rule there produces a `prune`
  that cannot operate anywhere the system puts scratch space. The split that works is
  `/private/etc` subtree-wide, with `/private/tmp` and `/private/var` as data roots whose
  children stay prunable — the shape Linux already has with `/var` and `/var/tmp`.
- **A change feed must report paths in the spelling the caller gave.** Returning
  canonicalised absolute paths from `--watch` breaks the `ChangeSource` contract Linux keeps,
  even though every value is correct once resolved: the caller matches what it gets back
  against the root it passed in, and `/tmp/x` and `/private/tmp/x` are different strings.
- **BSD `du` and GNU `du` disagree twice over.** macOS ships BSD `du`, which counts 512-byte
  blocks where GNU counts 1024 — a factor of two in every row — so an oracle comparison
  pins `-k` explicitly instead of trusting the default. They also round `-h` differently:
  GNU rounds up to the next unit, BSD truncates, which is a one-step difference on
  boundaries only and is why the `-h` oracle compares magnitudes within one display step
  while numeric `-k` stays byte-exact.

**Open, each with what it costs.**

- **The kqueue feed has a directory budget.** A watch is per-descriptor, so the feed admits
  4096 directories; a tree past that gets no feed at all and every tick is a full rescan.
  That is a behaviour change from Linux, whose inotify feed covers every directory or gives
  up as a whole. Removing the ceiling is what MRI-13 exists to do.
- **FSEvents is deferred, and MRI-13 carries it.** kqueue is what shipped for this port.
  The reason recorded at the time was CoreFoundation against zero dependencies; the binding
  turns out to be reachable — `#[link(name = "CoreFoundation", kind = "framework")]` is what
  `core-foundation-sys` does with no build script — so the open question is the change feed's
  session behaviour and its dropped-event path, not the dependency rule.
- **`df` has no macOS oracle.** Every `dusage df` number on macOS is checked against the
  kernel's own records and our arithmetic, not against BSD `df`: the row-comparison tests are
  `cfg(target_os = "linux")` because BSD `df` has no `--version` and prints a different set
  of columns. A comparison of total size alone against `df -P -k` is the missing check.
- **The Darwin struct layouts are asserted on Linux and confirmed by the runner.**
  `struct stat` (144 bytes), `struct dirent`, `struct kevent` and `struct statfs` (2168
  bytes, `f_fstypename` at 72, `f_mntonname` at 88) are pinned by `size_of`/`offset_of`
  assertions that run during the cross-check, which proves the declaration is self-consistent
  and nothing about libSystem; the macOS leg is what confirms them, and the first run of it
  is what caught the missing `fdopendir$INODE64` alias. Those assertions stay: they are the
  guard against a layout drifting silently, and the mount-record test dumps the record when
  one drifts.
- **The coverage is a runner label, and labels retire.** Both architectures are verified:
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` and
  `cargo build --release` all pass on Intel and on Apple Silicon, and
  `cargo clippy --target aarch64-apple-darwin` runs locally against the same `cfg` path. What
  is not durable is the label: the macOS images GitHub offers change, and `macos-14` was
  already gone when the arm64 leg was added (a job asking for it is cancelled with no runner
  assigned). MRI-14 tracks the schedule.

**What the legs are for.** Four times now, a change has landed on `main` that no macOS build
could see, and three of those came *after* the port existed: `tree` gated its own
`path_alloc` to `target_os = "linux"` with only a Windows sibling, `tree_cli`'s `link_dir`
had the same two-armed shape and left the function's body empty where a `bool` was required,
`df` shipped with a native module for Windows and Linux and none for macOS so the platform
did not compile at all, and the arm64 lint surfaced dead code that had never been reachable
there. Each was written by someone who could not have known macOS was a target, because
nothing on `main` built for it. That is the argument for the macOS legs, and it is the reason
a change that cannot be built for them is not finished: the port was the first code written
against this platform, not the last.

# dusage / fastdel: status-tracked roadmap

**Origin.** This began as a research report written from `src/lib.rs`, `src/bin/dusage.rs` and `src/bin/fastdel.rs` before any change (four research lanes plus skeptic verdicts; refuted items dropped or corrected, unconfirmed items marked unverified). The "Problem" and "Fix" text below describes the original code and is kept as written; the status line under each item says what was built. Gains stay unmeasured unless stated: no benchmark has been run on the implementation.

**Status (branch `improve/roadmap`, after wave 3).** Items 1-8 are done, 9 and 10 are partially done (implemented, not validated on a real volume), 11 is open. The benchmark questions in section 4 are all still open, apart from the Linux work in the last section.

**Status 0.5.0 (search).** `dusage` is a search tool as well: `search` (contents,
a grep/rg replacement), `find` (paths, a find replacement), `prune` (the fastdel
engine, with presets), `du` (a du-compatible per-directory report), and the
0.4.0 `scan`/`big` intact behind subcommands in one binary. A benchmark suite
with a memory pass, a cold-cache pass and a regression guard now lives in
`bench/` and `docs/BENCHMARKS.md`; the section at the end lists what shipped,
the numbers and the limits that are still open.

**Status (macOS, x86_64).** The crate compiles for `x86_64-apple-darwin` and has a third
platform module, `src/mac/`: enumeration over `readdir$INODE64`/`fstatat`, deletion over
`unlink`/`rmdir`, a kqueue `EVFILT_VNODE` change feed and a macOS guard. Everything is
cross-checked from Linux; nothing has been built or run on a Mac yet, and arm64 is
untested. The limits that come with it are in the macOS section at the end.

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
| 9 | USN change journal for incremental `--watch` | Partial: `win::usn` + `scan::watch`; not validated on a real journal (admin) | Feature; faster repeat scans | Expected yes (unverified) | L | Medium |
| 10 | Elevated whole-volume fast path (USN enum plus MFT parsing) | Done: `win::mft` + `--mft`, validated against the batched backend on two real volumes | Whole-drive scan 6.2x faster (10.6 s vs 65.8 s) | Yes (measured) | L | High |
| 11 | Dev Drive guidance for scratch trees (docs only) | Open: docs only, not written | Unmeasured | Setup only | S | None |

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

**Status: Partially done.** `win::usn` (query/read, V2/V3 records, path by reference) drives `--watch` rescans of changed top-level entries, with full-rescan fallback. Admin behaviour and journal invalidation are untested on a real volume.

- **APIs.** `DeviceIoControl` on `\\.\X:` with `FSCTL_QUERY_USN_JOURNAL` and `FSCTL_READ_USN_JOURNAL`. The `USN_RECORD_V2` layout is verified: a 60-byte header, `RecordLength` for stepping, and `FileNameOffset`/`FileNameLength` in bytes (https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v2).
- **Unverified.** Journal ID invalidation rules, the `ERROR_JOURNAL_ENTRY_DELETED` behaviour, and the privilege needed for query and read. The legacy page that covers these was not fetched.
- **Fallback.** On journal truncation, do a full rescan.
- **FFI.** Kernel32 `DeviceIoControl`. FSCTL values should be computed with a `CTL_CODE` const fn and checked against winioctl.h.
- **Effort.** L. **Admin.** Expected yes, unverified. **Risk.** Medium. NTFS only.

### 10. Elevated whole-volume fast path (USN enumeration plus MFT parsing)

**Status: Done.** `win::mft` (fixup, runlists, record parser, parallel unbuffered volume reader, extension-record merge) backs `--backend mft`. It was validated in an elevated shell on two NTFS volumes. Its totals match the batched backend, with and without `--dedup-hardlinks`, plus the NTFS metadata files on a volume root. Benchmarked in the README: 10.6 s against 65.8 s for a whole system drive with 8.6M files. Reading the table (about 2 GB/s) dominates.

- **Verified.** `FSCTL_ENUM_USN_DATA` on `\\.\X:` is NTFS-only. You loop on `StartFileReferenceNumber` (https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_enum_usn_data). It yields names and parents, not sizes.
- **Sizes need MFT parsing.** That means the FILE-record header, update-sequence fixup and runlists. Only the struct members and the NTFS 3.0/3.1 version gate are verified (https://learn.microsoft.com/en-us/windows/win32/devnotes/file-record-segment-header). The byte offsets are community NTFS documentation and unverified.
- **Admin.** Expected, not stated by the Microsoft page. Test it.
- **Gain.** Potentially large for whole-volume scans. WizTree-style speed claims are marketing and unverified.
- **Effort.** L. **Risk.** High (large unsafe surface). Gate it behind a benchmark that shows directory I/O dominates.

### 11. Dev Drive guidance for scratch trees (documentation only)

**Status: Open.** Documentation only; nothing written yet.

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

## 4. Open questions for benchmarks on a real machine (all open)

1. **Enumeration cost.** Compare FindFirstFileExW (basic) against the `FileIdExtdDirectoryInfo` batch at 64 KiB, 256 KiB and 1 MiB buffers, with and without `LARGE_FETCH`. Measure cold and warm, on a generated 1M-file tree and on a real tree. dusage already prints files/s.
2. **Thread sweep.** Run 1x, 2x, 4x and 8x cores. Find out whether the work is syscall-bound or serialized by filter drivers.
3. **Protection effect.** Scan and delete the same tree with real-time protection on and with the folder excluded. Measure only. Do not recommend turning protection off.
4. **Delete strategy.** Compare serial `remove_dir_all` against the item 7 prototype, and classic against POSIX disposition. Record the error categories from item 6.
5. **Allocation fidelity.** Compare `AllocationSize`, `GetCompressedFileSizeW` and Explorer "Size on disk" on fixtures: compressed (`compact /c`), sparse, small resident files, a named stream, a hardlink pair, and a junction. Check whether `AllocationSize` includes named streams.
6. **FileId semantics.** Confirm that hardlinks share a FileId. Check the behaviour on ReFS, Dev Drive and FAT (zero or unsupported?). The `fsutil file queryFileID` syntax must be verified first.
7. **Current reparse gap.** Confirm that `is_symlink()` returns true for junctions and false for a live DFS or dedup reparse directory, and that dusage currently recurses into it. Reproduce on a fixture before fixing.
8. **Privileges.** Check whether `CreateFileW(\\.\C:, GENERIC_READ)` fails for a non-elevated token, and whether `FSCTL_QUERY_USN_JOURNAL` and `FSCTL_ENUM_USN_DATA` behave differently with and without elevation. Check the journal state on C:.
9. **Junction at top level.** Confirm whether fastdel's `remove_dir_all` removes only the link when it is handed a top-level junction. The lane 5 claim is unverified.
10. **POSIX delete.** Confirm whether `FILE_DISPOSITION_FLAG_POSIX_SEMANTICS` works on the target build, and when the name disappears.
11. **Overcount.** Measure the hardlink overcount on a real pnpm store or `.git` tree, using FileId dedup as the reference.
12. **Long paths.** Delete a path over 260 characters with and without the `\\?\` prefix.

## 5. Remaining work

- Validate `--mft` and the USN watch path in an elevated shell on a real NTFS volume; compare `--mft` totals against the batched backend and robocopy.
- Run the benchmark list in section 4 and record results; decide the default thread count from the thread sweep.
- Item 11: Dev Drive guidance in the docs.
- Done in 0.5.0: the tools merged under subcommands in one binary (`dusage
  scan|big|search|find|prune|du`), with the `fastdel` binary and `dusage fastdel|del`
  running `prune`; and fastdel presets (`--preset`, `--print-presets`).

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

- A raw `getdents64` enumerator (own syscall, no `read_dir` allocation per entry) and
  `statx` with `STATX_MNT_ID` batching are unexplored; the current backend is one syscall
  per entry, the same as `du`'s, and wins only on parallelism and on doing less per entry.
- Thread sweep and cold-cache numbers on a real machine (2 vCPUs here, and the machine is a
  container on overlayfs, so delete timings are not comparable to ext4 or btrfs).
- `--watch` under `max_user_watches` pressure: the fallback is implemented (full rescans)
  but has only been tested by construction, not by lowering the limit.
- macOS build numbers, arm64, FSEvents and `getmntinfo` — see the macOS section at the end.

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
at the next merge. At the commit the tables were last generated at (323bf88,
2 vCPUs, warm cache) every content row is ahead: the output-heavy ones at
`search-count` 0.69x, `search-files-with-matches` 0.69x,
`search-only-matching` 0.84x, `search-multiline` 0.88x and `search-common`
0.76x, the plain ones at `search-rare` 0.94x, `search-regex` 0.90x,
`search-regex-nomatch` 0.94x and `search-ci` 0.91x (they were 1.02–1.10x two
rounds ago, and callgrind counts the literal scan 25-28% cheaper and the regex
candidate scan 29% cheaper rather than that being drift alone). `find` meets its
reference on every row but one — `find-name` 0.71x, `find-path` 0.98x,
`find-expr` 0.99x, `find-type` 1.03x — and both `du` rows are ahead of GNU `du`
at 0.58–0.60x with identical totals. The content row still behind is
`search-multiline-unicode` at 1.15x.

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

- Where the time goes in `-o`: the match loop or the retained output.
- Whether the literal fast path is worth a multi-byte (Two-Way or memchr-style)
  search for needles longer than a couple of bytes.
- Whether the expression walk can be parallelised per directory without losing
  `-prune`/`-depth` semantics.
- `--stats` is one line today; a per-directory breakdown would make "why is this
  slow" answerable without a profiler.

## macOS (x86_64), as built

**Status.** `lib.rs` accepts `target_os = "macos"` and aliases `src/mac/` as `crate::native`,
the third arm beside `src/unix/` and `src/win/`. The crate cross-checks for
`x86_64-apple-darwin` from Linux — `cargo check`, `cargo check --tests` and
`cargo clippy --all-targets -D warnings` — and CI carries a `macos-15-intel` leg that runs
the format, lint, test and release-build steps the other platforms run. **Nothing here has
been built or run on a Mac.** Cross-checking proves the `cfg` wiring, the types and the FFI
declarations compile; it cannot prove a symbol resolves or that a struct layout matches
libSystem.

**Done.** Enumeration is `open` + `fdopendir` + `readdir$INODE64`, with `fstatat$INODE64`
per entry against the open directory descriptor, so `alloc` is `st_blocks * 512` and
`file_id` is `dev << 64 | ino` exactly as on Linux. Deletion is `unlink`/`rmdir`, path-based
and never following a symlink. `--watch` is kqueue. The guard refuses `/`, the system trees,
`/Users`, `$HOME`, `/opt` and every volume root under `/Volumes`.

**Open, each with what it costs.**

- **The Darwin struct layouts are asserted, not verified.** `struct stat` (144 bytes),
  `struct dirent`, `struct kevent` and the `$INODE64` symbol aliases are pinned by
  `size_of`/`offset_of` assertions that run during the cross-check, which proves the
  declaration is self-consistent and nothing about libSystem. The first `cargo build` and
  `cargo test` on a Mac is what confirms them.
- **The kqueue feed has a directory budget.** A watch is per-descriptor, so the feed admits
  4096 directories; a tree past that gets no feed at all and every tick is a full rescan.
  That is a behaviour change from Linux, whose inotify feed covers every directory or gives
  up as a whole.
- **FSEvents is deferred.** It needs CoreFoundation, which zero dependencies rules out for
  this port. kqueue is what shipped.
- **The guard's two sets were split by evidence, not by taste.** Refusing `/private` as a
  whole subtree, which the first version did, made every temp directory unprunable: macOS's
  `$TMPDIR` is `/private/var/folders/...`, so a pruner could not touch the one place macOS
  scratch actually collects. `/private/etc` is a refused subtree; `/private/tmp` and
  `/private/var` are refused as data roots whose children stay prunable, the shape Linux
  has with `/var` and `/var/tmp`. Matching runs on both the lexical and the canonical
  spelling, so `/etc/passwd` and `/private/etc/passwd` both answer.
- **A volume mounted outside `/Volumes` is not refused as a volume root.** `getmntinfo` and
  `struct statfs` are not bound, so the guard only sees the mounts `/Volumes` lists; the
  prune's device comparison is what stops a walk into any other one. `-fstype` answers
  nothing for the same reason.
- **CI is on a clock.** The `macos-15-intel` runner label is Intel-only and GitHub retires
  x86_64 macOS runners in August 2027, so an arm64 answer is needed before then.

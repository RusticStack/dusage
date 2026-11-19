# dusage / fastdel: status-tracked roadmap

**Origin.** This began as a research report written from `src/lib.rs`, `src/bin/dusage.rs` and `src/bin/fastdel.rs` before any change (four research lanes plus skeptic verdicts; refuted items dropped or corrected, unconfirmed items marked unverified). The "Problem" and "Fix" text below describes the original code and is kept as written; the status line under each item says what was built. Gains stay unmeasured unless stated: no benchmark has been run on the implementation.

**Status (branch `improve/roadmap`, after wave 3).** Items 1-8 are done, 9 and 10 are partially done (implemented, not validated on a real volume), 11 is open. The benchmark questions in section 4 are all still open, apart from the Linux work in the last section.

**Status 0.5.0 (search).** `dusage` is now a search tool as well: `search` (contents,
a grep/rg replacement), `find` (paths, a find replacement), `prune` (the fastdel engine,
with presets), and the 0.4.0 `scan`/`big` intact behind subcommands in one binary. The
section at the end lists what shipped and the limits that are still open.

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
  scan|big|search|find|prune`), with the `fastdel` binary and `dusage fastdel|del` running
  `prune`; and fastdel presets (`--preset`, `--print-presets`).

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
- macOS would need a kqueue/FSEvents change feed and its own guard list; the crate refuses
  to compile there today, on purpose.

## Search suite (0.5.0)

**Done.** The main flags are covered by `tests/search_cli.rs` (built binaries, exit codes,
output shapes) and by the unit suites in `regex`, `glob`, `ignore`, `pattern`, `encodings`
and `preset`; every invocation in the README and the skill was run against the binary while
those docs were written.

- `search`: a walk plus a streaming line scanner, with `-i -s -S -w -v -n -H -h -l -L -c
  --count-matches -o -r -q -a -F -P -E -G -e -f -g --iglob -t -T --type-list --type-add
  --type-clear -A -B -C -m -j --max-depth --max-filesize --max-columns
  --max-columns-preview --encoding --hidden --no-ignore --no-ignore-vcs/-dot/-parent/-files
  -u -x --follow -U --multiline-dotall -z -Z --null-data --passthru --sort --sortr --column
  --byte-offset --heading --line-regexp --include --exclude --exclude-dir --binary-files -I
  -d --pre --pre-glob --json --color --stats`.
- `find`: glob or `-F` substring over relative paths, with `-i --name --full-path -t --ext
  --size --older --newer --newermt --newerat --newerct --empty --mindepth --dirs --files
  --long --json --follow` and the same walk filters.
- A regex engine of our own (`regex.rs`): Thompson NFA, run by a lazily built DFA (one
  table lookup per byte) with the Pike VM underneath it, no backtracking, so a pathological
  pattern costs time, never exponential time. A plain-literal pattern skips it for a byte
  scan, and the rest is prefiltred twice: by a single mandatory byte and by the longest run of
  bytes every match must contain (`.go` for `[A-Z][a-zA-Z0-9_]*\.go`), with the run's skip plan
  built once at compile time. Together they took regex counting from 348 ms to 220 ms on
  `/usr/local/go`. A block scan now runs regexes as well as literals: the forward automaton
  alone finds where a match ends, which is enough to say which line to look at, and the exact
  span is worked out afterwards on that one line. That is only sound for a pattern with no
  line-relative assertion and no instruction that permits a newline, and only for a pattern
  that does not need the character program, which `Pattern::block_re` checks. A pattern that
  never matches is now slightly faster than ripgrep (`ZZZ[A-Z]qqq`: 80 ms against 90 ms);
  the 2.4x loss that remains is for patterns that match often, where the per-byte automaton
  walk is the cost and ripgrep's is more than twice as fast.
- Ignore handling close enough to ripgrep to be unsurprising: the built-in cache list, then
  `.gitignore` and `.ignore` per directory with deeper files winning and `!` re-including,
  then `-g`; a positive `-g` switches the ignore layers off. `--no-ignore` switches them off
  by hand.
- UTF-16LE/BE detection (BOM or NUL pattern) and transcoding, Latin-1 on request.
- Unicode case folding for literal needles and non-ASCII word bytes for `-w`; the regex
  engine folds ASCII only.
- Binary detection by a NUL in the first 8 KiB, reported as `Binary file X matches` unless
  `-a`.
- `prune`: `--preset` and `--print-presets`, with `cargo`, `node` and `agent-temp` built in
  and a `presets.conf` of the user's own.
- One binary with subcommands and the legacy `dusage <root>` form kept.

**Known limits, each an open item.**

- No `-P`/PCRE2: no lookaround and no backreferences. The engine rejects them by name and
  offset rather than silently matching something else. Adding them means a backtracking
  engine beside the Pike VM, with the whole worst-case-cost argument that comes with it.
- Regex `-i` folds ASCII letters only, so `-i 'CAF[É]'` misses `café` where ripgrep matches.
  The fix is Unicode simple case folding in the compiler: every non-ASCII atom becomes a
  class of its case variants, which costs program size but no complexity.
- No multi-line matching (`rg -U`). Matching is per line by construction; `(?s)` only widens
  `.`, it does not let a pattern cross a newline.
- Regexes still cost about 2x ripgrep's time on a general pattern, and the line-at-a-time path
  is not the reason any more: the DFA's transition table is keyed by byte classes and the
  automaton now walks only the lines that hold the mandatory literal run. What is left is the
  per-line cost of entering the automaton at all — about 60 ns a line against ripgrep's 30 —
  and the block-scanned alternative was measured three times and lost every time: 0.7-1.5 s
  against 0.18 s for `[A-Z][a-zA-Z0-9_]*\.go` over `/usr/local/go`, because a bounded reverse
  walk still ends up re-entering the DFA once per candidate line. A reverse automaton that
  answers "where does the match start" in one suffix pass over the block is the change that
  would fix it; that is a new machine, not a tweak.
- No `--follow`. The walker never follows a symlink, by the same invariant the scanner and
  the pruner keep, and a followed tree needs cycle detection before it can be offered.
- `dusage find` still collects every path before printing, because its output is the sorted
  candidate list itself: 68 MB over `/usr` against `find(1)`'s 4.8 MB. Sorting at the end from a
  stream is the change that would close it; `search` no longer has this shape.
- No mmap. Files are read in 64 KiB blocks into a buffer each worker reuses, and mapping large
  files instead was measured and rejected: on `/usr` the mapping's page faults cost more than
  `read`'s copy (6.4-7.5 s against 3.7-4.8 s for the same search), and on a page-cached tree
  the two are within noise.
- No `.git/info/exclude` and no global `core.excludesFile`, and ignore files are read from
  each root down, so a `.gitignore` above the root does not apply even though ripgrep honours
  it inside a repository.
- No `-U` (multi-line) and no `-P` lookaround, as above; those are the two flag-level gaps left.

**Open questions worth measuring later.**

- Where the time actually goes: read, match or walk, on a cold cache and on a tree that is
  already in page cache.
- Whether the literal fast path is worth a multi-byte (Two-Way or memchr-style) search for
  needles longer than a couple of bytes.
- `--stats` is one line today; a per-directory breakdown would make "why is this slow"
  answerable without a profiler.

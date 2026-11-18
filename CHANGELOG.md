# Changelog

## [0.4.0] - 2026-11-18

`dusage` and `fastdel` now run on Linux as well as Windows. Everything the Windows build
does has a Linux equivalent except the NTFS-only read paths: there is no master file table
to read, and no named alternate data streams.

### Added

- **Linux support.** A scan walks `std::fs::read_dir` with one `statx` per entry, so every
  row still carries its on-disk size (`st_blocks`) and a 128-bit file id (`dev << 64 | ino`)
  for hardlink dedup. A 180k-file `/usr` scans in 0.9 s, the same totals as
  `find -printf '%s'`.
- **`--watch` on Linux** reads an inotify watch tree instead of the USN journal and rescans
  only the top-level entries that changed. A new directory is watched as it appears, a burst
  of writes is coalesced into one rescan, and a queue overflow falls back to a full rescan.
- **`-x, --one-filesystem`** keeps a scan on the root's filesystem, counting mount points as
  skipped instead of descending into `/proc`, `/sys` or another disk.
- **Protected roots on Linux**: `/`, `/etc`, `/usr`, `/bin`, `/sbin`, `/lib*`, `/boot`,
  `/proc`, `/sys`, `/dev`, `/run`, `/var`, `/opt`, `/srv`, `/root`, `/home`, `$HOME` and their
  ancestors are refused by `fastdel`, including through a symlink. Symlinks are the Linux
  reparse point: skipped by a scan, unlinked by a prune, refused as a root.
- `fastdel` never follows a tree into another filesystem: a mount point inside a candidate is
  left in place and reported, the way a junction is on Windows.
- Linux CI job (fmt, clippy, tests, release build, binary artifact) and a Linux tarball on
  release.

### Changed

- Error categories are read per platform: errno on Linux (`EACCES`, `ENOTEMPTY`, `ETXTBSY`,
  `ENOENT`) and the Win32 codes on Windows. Previously the Win32 numbers were assumed, which
  misclassified Linux errors (`5` is `EIO`, not "access denied").
- `enumerate::DirReader::read` takes an error sink, so a per-entry failure inside a directory
  listing is counted instead of dropped.
- Directory rows in the text report use the platform separator.
- `--streams` is documented as NTFS-only and reports 0 bytes elsewhere.

### Fixed

- A failed `--backend mft` request on Linux now explains itself instead of failing to build.

## [0.3.0] - 2026-11-15

### Changed

- `dusage --mft` is no longer experimental. It reads the master file table on up to 16
  threads, each with its own unbuffered volume handle, and scans a whole system drive about
  6x faster than the default backend (11 s against 66 s in the README benchmark).

### Fixed

- `--mft` failed on volumes whose `$MFT` has a non-resident attribute list, and on reads
  that were not sector aligned.
- `--mft` undercounted files with several hardlinks, and files whose attributes spill into
  extension records (extra names, or the data size of a fragmented file). Extension records
  are now merged into their base record, and the totals match the default backend.

## [0.2.0] - 2026-11-15

The first public release. `dusage` gains native Windows backends and accurate
on-disk sizes; `fastdel` becomes safer and deletes big trees in parallel. All
earlier positional commands keep working.

### Added

- **Batched enumeration** (default backend): `GetFileInformationByHandleEx`
  returns many directory entries per call, with each file's size on disk and
  128-bit file id at no extra cost.
- `dusage --on-disk`, `--by-ext`, `--json`, `--dedup-hardlinks`, `--streams`
  (named alternate data streams), `--backend auto|batched|readdir|mft`.
- `dusage --watch SECS`: periodic rescans; in an elevated shell the NTFS USN
  journal limits each rescan to the top-level entries that changed.
- `dusage --mft` (experimental, admin): reads the raw NTFS `$MFT`.
- `fastdel --age 7d|2h|30m`, `--prefix`, `--log FILE` (streamed, survives a
  crash), `--limit`, and a per-category error summary.
- Human sizes everywhere (`300M`, `1.5G`, `512KiB`).
- CI on Windows (fmt, clippy, tests, release build) and tagged binary releases.

### Changed

- `fastdel` deletes large subtrees and huge flat directories across all
  workers, not one thread per top-level entry.
- Deletion is handle-based with POSIX semantics, ignores the read-only
  attribute, and works on paths longer than 260 characters.
- The work queue uses a condition variable instead of 1 ms polling.

### Fixed

- `fastdel` turned an unparsable age (`7x`, `7d` on 0.1) into **1 day**; it is now
  an error.
- Reparse points other than symlinks and junctions (DFS, dedup, WIM, cloud
  placeholders) were followed by `dusage`; every reparse point is now skipped.
- `fastdel` followed a top-level junction when checking `is_dir`; links are now
  unlinked, never descended.
- The protected-root guard missed `\?\C:\`, UNC and loopback admin shares
  (`\localhost\C$`), `..` spellings and junctions to protected folders, and only
  applied when no filter was given. It now refuses every spelling, always.
- A panicking worker could hang the scan; it now propagates.
- A root that is itself a junction or symlink is refused instead of scanning
  as empty.
- Access-denied and vanished entries are counted and reported instead of
  silently missing from totals.

## [0.1.0]

Initial private version: `dusage` and `fastdel` over `std::fs`.

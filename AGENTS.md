# AGENTS.md — dusage repo

Guidance for agents developing this repo. The user intends to grow this into a
proper disk-tooling suite installed on PATH — keep the codebase easy to extend.
Module contracts and wave-3 behaviour notes: `docs/ARCHITECTURE.md`. Status of
every improvement: `docs/ROADMAP.md`.

## Layout

```text
src/lib.rs            module list; re-exports queue::par_for_each; `native` = win or unix
src/queue.rs          par_for_each: condvar work queue (one mutex, panic-safe)
src/cli.rs            arg parser, human units (sizes, ages), thread default
src/guard.rs          protected-root guard, one implementation per platform
src/model.rs          shared types: Totals, TopEntry, BigFile, ErrorKind/ErrorCounts, Source, ScanResult
src/report.rs         text / JSON rendering of ScanResult, SearchReport, path listings
src/scan.rs           scan engine (readdir / batched / mft backends) and watch
src/prune.rs          prune engine: plan + execute, streaming --log, chunked deletes
src/preset.rs         prune presets (built in + presets.conf)
src/search.rs         search/find engine: tree walk, filters, streaming line scanner
src/pattern.rs        literal vs regex dispatch, case and word rules
src/regex.rs          Pike VM over a Thompson NFA (no backtracking)
src/glob.rs           gitignore-style glob matcher
src/ignore.rs         builtin caches + .gitignore/.ignore layers + -g filters
src/bytes.rs          memmem, case folding, word boundaries
src/encodings.rs      UTF-8/UTF-16/Latin-1 detection and transcoding
src/win/sys.rs        shared FFI decls and helpers
src/win/enumerate.rs  batched directory enumeration, readdir fallback, reparse/stream helpers
src/win/delete.rs     handle-based delete that never follows reparse points
src/win/usn.rs        USN journal query/read/enumerate, path by file reference
src/win/mft.rs        raw $MFT reader (admin)
src/win/watch.rs      --watch change feed over the USN journal
src/unix/sys.rs       errno table + inotify/poll FFI decls
src/unix/enumerate.rs readdir + statx enumeration, symlink/reparse-root checks, mount helpers
src/unix/delete.rs    unlink/rmdir delete that never follows symlinks
src/unix/watch.rs     --watch change feed over inotify
src/mac/sys.rs        Darwin `struct stat`/`struct dirent` layouts, kqueue decls, errno helpers
src/mac/enumerate.rs  readdir$INODE64 + fstatat enumeration, symlink/mount helpers
src/mac/delete.rs     unlink/rmdir delete that never follows symlinks
src/mac/watch.rs      --watch change feed over kqueue (EVFILT_VNODE)
src/cmd/              subcommand front ends: scan/big, search/find, prune/fastdel
src/bin/dusage.rs     thin CLI over cmd::dispatch + cmd::finish
src/bin/fastdel.rs    thin CLI over cmd::prune
tests/                dusage_cli.rs, fastdel_cli.rs, search_cli.rs (run the built binaries)
```

The two platform modules expose the same small surface, which is all `scan.rs`,
`prune.rs` and `guard.rs` see through `crate::native`:

```rust
enumerate::{Entry, DirReader::read(dir, out, errs), read_plain, is_skipped,
            refuse_reparse_root, file_id_of, named_stream_bytes, dev_of,
            other_filesystem, same_filesystem, is_unsupported, is_denied}
delete::delete_entry
watch::{resolve_root, ChangeSource::open(root), ChangeSource::poll(interval) -> Vec<PathBuf>}
```

All `unsafe` and FFI lives under `src/win/`, `src/unix/`, `src/mac/` or `src/simd.rs`; code
outside those is safe Rust. `src/simd.rs` is the only non-platform module with
intrinsics: x86 vector loads for one-byte search and counting, reached only
after a runtime feature check and always falling back to a portable loop.
`par_for_each(initial, threads, f)` runs `f(&item, push)` per item; children are
enqueued via `push`. It stops when the queue is empty and nothing is active.

## Commands

- Format: `cargo fmt --check`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Test: `cargo test` (tests needing admin skip themselves)
- Build: `cargo build --release`
- CI (`.github/workflows/ci.yml`): the same four steps on `windows-latest` and
  `ubuntu-latest`, plus a Linux smoke test that compares a `/usr` scan with
  `find -printf '%s'`. Run them locally before finishing.
- Install to PATH: `cargo install --path .` → `%CARGO_HOME%\bin` / `~/.cargo/bin`
- Cross-check the other platform without a toolchain VM:
  `rustup target add x86_64-pc-windows-msvc && cargo clippy --target x86_64-pc-windows-msvc --all-targets -- -D warnings`
- Smoke test: `dusage <small dir>` and `fastdel <dir> --age 7d --dry`
  (legacy `fastdel <dir> <age> <prefix> <n> --dry` still works)

## Invariants — do not break

1. **Zero dependencies.** std only. If a feature truly needs a crate, raise it
   explicitly — do not quietly add deps.
2. **dusage stays read-only.** No writes, ever (the USN journal is only
   queried and read).
3. **Never follow reparse points.** On Windows skip ANY entry with
   `FILE_ATTRIBUTE_REPARSE_POINT` (junctions, symlinks, DFS, dedup, WIM, ...),
   not just `is_symlink()`; on Linux a symlink is the whole class. A scan/prune
   root that is itself one is refused (`enumerate::refuse_reparse_root`); the user
   passes its target. A prune also stops at a mount point (`same_filesystem`), so
   a delete never walks into another device.
4. **fastdel is permanent** (no recycle bin). `guard::is_protected_root`
   refuses protected roots in every spelling (`C:`, `\?\C:\`, UNC, loopback
   admin shares, junction targets; on Linux `/`, the system trees, `/home`,
   `$HOME` and ancestors, symlink targets included) regardless of age/prefix filters. Never add
   an implicit yes/auto-confirm. An empty `--prefix` value is an error; a positional
   `""` prefix keeps its original meaning (all entries) for back-compat.
5. Deletion/scan callbacks must handle per-entry errors by skipping — a
   locked or vanished file must not abort the run.
6. **`unsafe` stays under `src/win/`, `src/unix/`, `src/mac/` and `src/simd.rs`**, each block with a
   `// SAFETY:` comment and FFI structs checked by size/offset tests.

7. **One platform surface.** `scan.rs`, `prune.rs` and `report.rs` must stay free
   of `#[cfg(target_os)]` where possible: put platform differences behind
   `crate::native`. The crate builds only for Windows, Linux and macOS (see `lib.rs`).

## Domain facts learned (Linux)

- `DirEntry::metadata()` is `lstat`: a symlink is described, never followed, so the
  enumerator needs no `FILE_FLAG_OPEN_REPARSE_POINT` equivalent.
- `file_type()` is free from `d_type` when the filesystem fills it; sizes still need one
  `statx` per entry (Rust's `metadata` uses `statx` on modern kernels), so the Linux
  `batched` backend is "one stat per entry", not "many entries per syscall".
- `st_blocks * 512` is the on-disk size; sparse and compressed files read 0 or small, which
  is the same number `du` reports and different from `ls -l`.
- A hardlink identity is `dev << 64 | ino`, the 128-bit stand-in for a Windows file id.
- A mount point is a directory whose `st_dev` differs from its parent's: the volume-root
  analogue. Mount points are scanned unless `--one-filesystem`, and never descended into by
  a prune (that is what makes "delete this tree" not delete another disk).
- inotify gives per-directory watches (a tree of 100k folders can exceed
  `max_user_watches`; the watcher then falls back to full rescans) and an
  `IN_Q_OVERFLOW` event when the queue drops events, which must force a full rescan.
- `rm -rf` on 50k entries took 0.60 s on 2 vCPUs; fastdel took 0.36 s. The parallel
  chunked delete wins here too, but the margin is much smaller than on Windows
  (reflinks/NTFS deletes are slower than ext4/overlayfs unlink).

## Domain facts learned (Windows)

- Directory mtime = last direct-child create/delete (not recursive).
- `DirEntry::metadata()` on Windows is served from the cached find data: no
  extra syscall. "The tool is syscall-bound, so threads matter more than CPU"
  is only a hypothesis; thread default stays 4x cores until benchmarked.
- `FILE_ID_EXTD_DIR_INFO` (`GetFileInformationByHandleEx`) returns
  AllocationSize and a 128-bit FileId per entry without opening each file.
- std `remove_dir_all` already retries sharing violations; blind retries add
  nothing. fastdel reports final errors by category instead.
- `robocopy /L /E /XJ` is a decent correctness oracle for directory sizes.
- Hardlinks (pnpm stores, .git objects) inflate naive byte sums — reported by
  the user as ~5-10%, not verified. `--dedup-hardlinks` counts each FileId once.
- Tool and agent temp dirs can regenerate at
  ~100 GB/day — that's why fastdel exists.

## Roadmap status (details in `docs/ROADMAP.md`)

Done: raw-attribute reparse skipping, verbatim/UNC guard, batched enumerator,
hardlink dedup, condvar queue, error categories, chunked parallel deletes, ADS
accounting (`--streams`), USN-assisted `--watch`, MFT backend (`--mft`),
`--json`, `--by-ext`, `--on-disk`, `fastdel --log`, human-unit age, real arg
parsing, CI, and (0.4.0) the whole Linux port: statx enumeration, inode dedup,
symlink handling, inotify `--watch`, Linux guard, `--one-filesystem`, Linux CI
and release artifacts.

Also done in 0.5.0: the search suite (`search`/`find` over one deterministic
walk plus a parallel content scanner) and the single binary with subcommands
(`dusage scan|big|search|find|prune`, `fastdel` and `del` aliases for `prune`,
presets for the pruner). Search limits are listed in `docs/ROADMAP.md`.

Also done on the macOS branch: the macOS port, `src/mac/` as the third `crate::native`
arm — `readdir$INODE64`/`fstatat` enumeration, `unlink`/`rmdir` deletion, a kqueue
`EVFILT_VNODE` change feed and a macOS guard — cross-checked for `x86_64-apple-darwin`
from Linux, with a `macos-15-intel` CI leg. See the open item below for what that does
not prove.

Open:
- Validate the USN `--watch` path on a real volume in an elevated shell (the
  MFT backend is validated and benchmarked; see README).
- Run the benchmark questions in `docs/ROADMAP.md` (no speedup is measured yet).
- Measure the Linux backends properly: the thread sweep and the cold-cache numbers in
  README were taken on a 2-vCPU container, and a `getdents64`-level enumerator (raw
  syscall, no `read_dir` allocation) is unexplored.
- Verify the macOS port on a real Mac. Everything there is cross-checked from Linux and
  nothing has been run on macOS: the Darwin `struct stat`/`dirent`/`kevent` layouts and the
  `$INODE64` symbol aliases are pinned by `const` assertions that prove the declaration is
  self-consistent and nothing about libSystem, so the first `cargo build` and `cargo test`
  on a Mac is what confirms them. arm64 is untested; FSEvents is deferred (CoreFoundation
  against zero dependencies); the kqueue feed stops at 4096 watched directories and gives
  up wholesale past that; `getmntinfo` is unbound, so only the mounts `/Volumes` lists are
  refused as volume roots and `-fstype` answers nothing. Details in `docs/ROADMAP.md`.

## Style

- Compact std code; comments only where semantics are non-obvious.
- Errors: `eprintln!` for diagnostics, keep going on per-file failures. Exit
  codes: dusage 2 = bad arguments, 1 = scan failed (including a missing or
  reparse-point root); fastdel 2 = bad arguments or plan refused (protected or
  reparse root), 1 = some candidates failed to delete.

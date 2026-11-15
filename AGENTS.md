# AGENTS.md — dusage repo

Guidance for agents developing this repo. The user intends to grow this into a
proper disk-tooling suite installed on PATH — keep the codebase easy to extend.
Module contracts and wave-3 behaviour notes: `docs/ARCHITECTURE.md`. Status of
every improvement: `docs/ROADMAP.md`.

## Layout

```text
src/lib.rs            module list; re-exports queue::par_for_each
src/queue.rs          par_for_each: condvar work queue (one mutex, panic-safe)
src/cli.rs            arg parser, human units (sizes, ages), thread default
src/guard.rs          protected-root guard (every path spelling, loopback UNC)
src/model.rs          shared types: Totals, TopEntry, BigFile, ErrorKind/ErrorCounts, Source, ScanResult
src/report.rs         text / JSON rendering of ScanResult
src/scan.rs           scan engine (readdir / batched / mft backends) and watch
src/prune.rs          prune engine: plan + execute, streaming --log, chunked deletes
src/win/sys.rs        shared FFI decls and helpers
src/win/enumerate.rs  batched directory enumeration, reparse-root check, stream/cluster helpers
src/win/delete.rs     handle-based delete that never follows reparse points
src/win/usn.rs        USN journal query/read/enumerate, path by file reference
src/win/mft.rs        raw $MFT reader (admin)
src/bin/dusage.rs     thin CLI over scan + report
src/bin/fastdel.rs    thin CLI over prune
tests/                dusage_cli.rs, fastdel_cli.rs (run the built binaries)
```

All `unsafe` and FFI lives under `src/win/`; code outside it is safe Rust.
`par_for_each(initial, threads, f)` runs `f(&item, push)` per item; children are
enqueued via `push`. It stops when the queue is empty and nothing is active.

## Commands

- Format: `cargo fmt --check`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Test: `cargo test` (tests needing admin skip themselves)
- Build: `cargo build --release`
- CI (`.github/workflows/ci.yml`, windows-latest): fmt, clippy, test, release
  build, upload both binaries. Run the first four locally before finishing.
- Install to PATH: `cargo install --path .` → `%CARGO_HOME%\bin`
- Smoke test: `dusage <small dir>` and `fastdel <dir> --age 7d --dry`
  (legacy `fastdel <dir> <age> <prefix> <n> --dry` still works)

## Invariants — do not break

1. **Zero dependencies.** std only. If a feature truly needs a crate, raise it
   explicitly — do not quietly add deps.
2. **dusage stays read-only.** No writes, ever (the USN journal is only
   queried and read).
3. **Never follow reparse points.** Skip ANY entry with
   `FILE_ATTRIBUTE_REPARSE_POINT` (junctions, symlinks, DFS, dedup, WIM, ...),
   not just `is_symlink()`. A scan/prune root that is itself a reparse point is
   refused (`enumerate::refuse_reparse_root`); the user passes its target.
4. **fastdel is permanent** (no recycle bin). `guard::is_protected_root`
   refuses protected roots in every spelling (`C:`, `\?\C:\`, UNC, loopback
   admin shares, junction targets) regardless of age/prefix filters. Never add
   an implicit yes/auto-confirm. An empty `--prefix` value is an error; a positional
   `""` prefix keeps its original meaning (all entries) for back-compat.
5. Deletion/scan callbacks must handle per-entry errors by skipping — a
   locked or vanished file must not abort the run.
6. **`unsafe` stays under `src/win/`**, each block with a `// SAFETY:` comment
   and FFI structs checked by size/offset tests.

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
parsing, CI.

Open:
- Validate MFT and USN paths on real volumes in an elevated shell (tests skip
  without admin; only synthetic buffers are covered).
- Run the benchmark questions in `docs/ROADMAP.md` (no speedup is measured yet).
- Merge to one binary with subcommands (`dusage scan|prune|big`) if more tools
  appear — keep `fastdel` as an alias for `prune`.
- Preset configs for fastdel.

## Style

- Compact std code; comments only where semantics are non-obvious.
- Errors: `eprintln!` for diagnostics, keep going on per-file failures. Exit
  codes: dusage 2 = bad arguments, 1 = scan failed (including a missing or
  reparse-point root); fastdel 2 = bad arguments or plan refused (protected or
  reparse root), 1 = some candidates failed to delete.

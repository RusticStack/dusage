<p align="center">
  <img src="assets/brand/logo.png" width="128" alt="dusage logo: a pixel-art disk-usage ring in rust, amber and teal on a dark badge, with one violet slice lifted out">
</p>

<h1 align="center">dusage</h1>

<p align="center"><b>See where your disk space went, then get it back.</b><br>
A fast disk-usage scanner and a safe parallel pruner for Windows. Two small exes, zero dependencies.</p>

<p align="center">
  <a href="https://github.com/RusticStack/dusage/releases/latest"><img src="https://img.shields.io/badge/Download_for_Windows-dusage-CC420C?style=for-the-badge&logo=windows11&logoColor=white" height="44" alt="Download dusage for Windows"></a>
</p>

<p align="center">
  <a href="https://github.com/RusticStack/dusage/releases/latest"><img src="https://img.shields.io/github/v/release/RusticStack/dusage?style=flat-square&label=latest&color=CC420C" alt="Latest release"></a>
  <img src="https://img.shields.io/badge/Windows-10%20%7C%2011-2A2A30?style=flat-square" alt="Windows 10 and 11">
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-2A2A30?style=flat-square" alt="MIT OR Apache-2.0"></a>
  <a href="https://github.com/RusticStack/dusage/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/RusticStack/dusage/ci.yml?branch=main&style=flat-square&label=CI" alt="CI status"></a>
  <img src="https://img.shields.io/badge/dependencies-0-2A2A30?style=flat-square" alt="Zero dependencies">
</p>

<p align="center">
  <img src="assets/demo.svg" width="820" alt="dusage scanning a demo folder: 2.02 GB in 11,104 files, with size on disk, the biggest files and a by-extension breakdown; the compressed logs folder holds 0.24 GB in 0.03 GB on disk">
</p>

<p align="center"><i>Built to dig a developer workstation out of a full system drive. <a href="EXAMPLE.md">The story</a>.</i></p>

## What it does

- **`dusage` finds the space.** It walks a folder or a whole drive on every core and
  reports each top-level entry's total, the biggest files, and optionally a breakdown by
  extension. It is read-only: it never writes anything.
- **Real numbers.** Size on disk next to file size (`--on-disk`), so compressed, sparse and
  tiny files are counted the way Explorer's *Size on disk* counts them. Hardlinked files
  (pnpm stores, `.git` objects) can be counted once (`--dedup-hardlinks`).
- **Fast.** Directory entries come from Windows in large batches, with size on disk and file
  ids included, so there is no per-file call: up to millions of files per second on a warm
  cache, and a whole 10-million-file system drive in under 3 minutes (see
  [Benchmarks](#benchmarks)).
- **`fastdel` gets it back.** It deletes the children of a folder that match a name prefix
  and are older than an age (`7d`, `2h`), in parallel, including inside one huge subtree.
- **Safe by design.** Volume roots, Windows, Program Files, ProgramData and your profile are
  refused in every spelling. Junctions, symlinks and other reparse points are never
  followed. `--dry` shows the plan first, and `--log` records every deletion as it happens.
- **Scriptable.** `--json` output, plain exit codes, and the old positional commands still
  work.

## Getting started

1. [Download the latest release](https://github.com/RusticStack/dusage/releases/latest):
   `dusage.exe` and `fastdel.exe`, or the zip with both. There's no installer: put them in a
   folder on your `PATH`.
2. Windows may show a SmartScreen warning because the exes aren't code-signed yet. You can
   check each download against `SHA256SUMS.txt` on the release page.
3. Find what is big, then preview the cleanup before running it:

```powershell
dusage C:\ --on-disk --by-ext                 # whole drive
dusage $env:LOCALAPPDATA --top 20             # one folder, top 20 rows
fastdel $env:TEMP --age 7d --dry              # what would go: entries idle for 7 days
fastdel $env:TEMP --age 7d --log prune.log    # do it, with a log
```

With Rust installed you can also build and install both tools with
`cargo install --git https://github.com/RusticStack/dusage`.

## dusage

```text
dusage <root> [OPTIONS]
dusage <root> [min_big] [threads] [top_n]      (positional form, still supported)
```

| Option | Meaning |
|---|---|
| `--min-big SIZE` | list files of at least SIZE (default `1G`; sizes like `300M`, `1.5G`, `512KiB`) |
| `--top N` | rows per section (default 60) |
| `--threads N` | worker threads (default 4x cores) |
| `--on-disk` | show size on disk next to file size |
| `--by-ext` | add a breakdown by file extension |
| `--dedup-hardlinks` | count a file with several hardlinks once |
| `--streams` | add named alternate data streams (slower: one probe per file) |
| `--json` | machine-readable output on stdout |
| `--watch SECS` | rescan every SECS seconds and print what changed; Ctrl+C stops |
| `--backend NAME` | `auto` (default), `batched`, `readdir` or `mft` |
| `--mft` | same as `--backend mft` |

**Backends.** `auto` uses `batched` and falls back to `readdir` on file systems that
don't support it. `batched` is the default fast path. `readdir` uses the Rust standard
library and can't report size on disk. `mft` reads the NTFS master file table directly. It
needs an elevated shell and is **experimental**: it has only been validated on synthetic
records so far, so compare it with the default backend before relying on it.

**Watch.** In an elevated shell, `--watch` reads the NTFS change journal and rescans only
the top-level entries that changed. Without admin rights every tick is a full rescan. An
update is printed only when something changed.

## fastdel

```text
fastdel <root> --age AGE [--prefix a,b] [OPTIONS]
fastdel <root> <min_age> [prefixes] [threads] [--dry]   (positional form, still supported)
```

| Option | Meaning |
|---|---|
| `--age AGE` | delete entries last modified more than AGE ago: `30s`, `15m`, `2h`, `7d`, `1w`, or a number of days. A negative number means no age filter. Required |
| `--prefix a,b` | only names starting with one of these (case-sensitive) |
| `--dry` | print the plan and delete nothing |
| `--log FILE` | append a line per candidate (`DELETED`, `FAILED`, `DRY`) and a summary, written as it happens |
| `--limit N` | rows listed by `--dry` (default 50) |
| `--threads N` | worker threads (default 4x cores) |

```powershell
# cargo target folders anywhere under D:\work, any age; preview first
fastdel D:\work --age -1 --prefix target,cargo-target --dry
# temp entries idle for more than 2 hours
fastdel C:\scratch --age 2h --prefix tmp- --log prune.log
```

Deletion is **permanent**: nothing goes to the Recycle Bin, and there is no confirmation
prompt, so run with `--dry` first. Exit codes: `0` success, `1` some entries could not be
deleted (the summary lists them by cause), `2` bad arguments or a refused root.

### What fastdel will not touch

- **Protected roots**, whatever the filters: any drive or share root, `Windows`,
  `Program Files`, `ProgramData`, `C:\Users` and your profile folder, plus every folder
  above them. Every spelling is caught: `\\?\C:\`, `C:\x\..`, short names like `PROGRA~1`,
  `\\localhost\C$`, and junctions that point at them.
- **Anything behind a link.** A junction or symlink is removed as a link and its target is
  left alone. A root that is itself a link is refused: pass its target.
- Files that are locked or vanish mid-run are skipped and counted, never fatal.

## Benchmarks

Measured on one development PC: Intel Core i7-13700KF, NVMe SSD, Windows 11, release
builds, 96 worker threads unless noted. Times are the median of three runs, with the
range in brackets.

**Scanning** (warm cache, `dusage <root>`):

| Workload | Sequential tools | dusage 0.1 | dusage 0.2 |
|---|---|---|---|
| Whole system drive, ~9.7M files / 3.7M folders | 15-20 min (`Get-ChildItem`, robocopy) | ~163 s | not re-run |
| Temp tree, 3.9M files (64 threads) | ~20 min | 71-80 s | not re-run |
| Source tree, 4.6M files | | 156k files/s (129-169k) | 155k files/s (148-165k) |
| Toolchain folder, 325k files | | 4.34M files/s (4.06-4.41M) | 4.29M files/s (3.89-4.55M) |

Version 0.2 scans as fast as 0.1, and the default backend adds size on disk and file
ids at no extra cost. Full-drive and MFT-backend timings on 0.2 are still to be measured.

**Deleting** (`fastdel <root> -1 <prefix> 96`, one candidate folder each):

| Workload | fastdel 0.1 | fastdel 0.2 |
|---|---|---|
| 600 folders × 100 files (60,601 entries) | 4.18 s (3.88-4.57) | **1.45 s** (1.39-1.71), 2.9× faster |
| One flat folder, 60,000 files | 3.84 s (3.21-4.78) | **2.86 s** (2.10-3.79), 1.3× faster |
| 142,801 temp entries (48 threads) | ~141 s | not re-run |
| 143 GB of build outputs (64 threads) | ~58 s | not re-run |

Version 0.1 deleted each top-level candidate on a single thread. Version 0.2 spreads
one big subtree, or one huge flat folder, across every worker.

## Good to know

- `dusage` counts each hardlink name separately unless you pass `--dedup-hardlinks`, so
  folders full of hardlinks (pnpm, git) read higher than their real usage.
- `fastdel`'s age is the entry's last-write time. For a folder on NTFS that is the last
  time a direct child was created or deleted, which works well for "idle since".
- Both tools need Windows 10 or 11 on NTFS for every feature. Other file systems fall back
  to the standard-library backend where they can.

---

## For developers

dusage is written in Rust with the standard library only: no crates. All Windows FFI and
`unsafe` code lives in `src/win/`, and every struct layout there is asserted in tests
against the Microsoft documentation.

```powershell
cargo build --release                      # target\release\dusage.exe and fastdel.exe
cargo test                                 # unit + CLI tests on temp fixtures
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The C runtime is linked statically (`.cargo/config.toml`), so the exes run on a clean
Windows install. The MFT and change-journal tests need an elevated shell and skip
themselves otherwise. Pushing a `v*` tag builds, tests and publishes a release
(`.github/workflows/release.yml`).

```text
src/
  queue.rs      parallel work queue shared by both tools
  scan.rs       scanner engine and watch mode          (dusage)
  prune.rs      planning and parallel deletion          (fastdel)
  guard.rs      protected-root checks
  cli.rs        argument parsing, sizes and ages
  report.rs     text and JSON output
  model.rs      shared result types
  win/          Windows layer: batched enumeration, handle-based delete,
                USN journal, raw MFT reader, shared FFI
  bin/          the two thin command-line front ends
```

More detail: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (module contracts),
[docs/ROADMAP.md](docs/ROADMAP.md) (research and open work) and [AGENTS.md](AGENTS.md)
(rules for contributors and coding agents).

## License

dusage is free software, dual-licensed under the [MIT License](LICENSE-MIT) or the
[Apache License 2.0](LICENSE-APACHE) at your option, made by
[RusticStack](https://github.com/RusticStack).

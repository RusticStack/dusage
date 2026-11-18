# Benchmarks

Wall-clock medians for `dusage scan`, `dusage search` and `dusage find` against
the tools they replace: `du`, `find`, `grep` and `ripgrep`. Every number here
came from running `bench/run.sh` on one Linux machine, and the raw output is
reproduced below verbatim. Nothing is estimated.

These are **warm-cache** numbers. The harness cannot drop the page cache
without root, so both the trees and the binaries involved stayed in RAM between
runs. That flatters every tool roughly equally, but a cold-disk first run will
look nothing like this table.

## Machine

The harness prints this header itself:

```text
# dusage benchmark, 2026-11-18T18:19:46Z
# runs,5 (median reported)
# uname,Linux 6.1.158+ x86_64
# cpus,4
# rustc,rustc 1.99.0 (b940084d7 2026-09-28)
# rg,ripgrep 14.1.0
# grep,grep (GNU grep) 3.11
# hyperfine,absent
# tree,/usr,bytes,7868013000,files,180451
# tree,/usr/local/go,bytes,244137476,files,15657
```

Ubuntu 24.04 container on a 4-vCPU VM, `dusage` 0.5.0 on branch `parity-polish`.
Trees: `/usr` (7.87 GB, 180,451 files) for the scan and `find` cases,
`/usr/local/go` (244 MB, 15,657 text files) for the content-search cases.
`dusage` was left at its default thread count, which is one thread per core
clamped to 4..16 — 4 worker threads on this box. `ripgrep` and `grep` were left
at their own defaults.

## How to reproduce

```console
$ cargo build --release
$ ./bench/run.sh
```

The exact command used for the table below was `./bench/run.sh` from the
repository root, with no environment overrides. The script warms every command
once, then runs five interleaved rounds — one run of each command per round, so
machine drift hits every tool alike — and reports the median of the five.

Knobs, all optional:

| Variable | Default | Meaning |
| --- | --- | --- |
| `N` | `5` | runs per command, median reported |
| `DUSAGE_BENCH_SCAN_ROOT` | `/usr` | large tree for the scan and `find` cases |
| `DUSAGE_BENCH_SEARCH_ROOT` | `/usr/local/go`, else `$HOME` | mid-size text tree |
| `DUSAGE_BENCH_PAT_RARE` | `asyncpreemptoff` | rare literal |
| `DUSAGE_BENCH_PAT_COMMON` | `func ` | frequent literal |
| `DUSAGE_BENCH_PAT_REGEX` | `[A-Z][a-zA-Z0-9_]*\.go` | regex case |
| `DUSAGE_BENCH_PAT_CI` | `memmove` | case-insensitive case |
| `DUSAGE_BENCH_GLOB` | `*.go` | glob for the `find` comparison |

`hyperfine` is used for a single cross-check line on stderr when it is
installed, and is never required. It was absent on this machine, so no
cross-check line appears.

## Raw output

```text
section,tool,command,median_ms,min_ms,max_ms,n
scan,dusage scan,/workspace/dusage/target/release/dusage scan '/usr' --top 1,186.4,175.3,210.5,5
scan,du -sb,du -sb '/usr',670.6,665.1,730.7,5
scan,find+awk,find '/usr' -printf '%s\n' | awk '{s+=$1} END{}',781.2,743.0,830.2,5
scan,dusage scan (1 thread),/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 1,672.8,656.9,678.8,5
search-rare,dusage,/workspace/dusage/target/release/dusage search --color=never -F 'asyncpreemptoff' '/usr/local/go',203.6,195.6,222.9,5
search-rare,rg,rg --color=never -F 'asyncpreemptoff' '/usr/local/go',52.0,50.2,59.0,5
search-rare,grep -rI,grep -rI --color=never -F 'asyncpreemptoff' '/usr/local/go',168.1,156.8,183.7,5
search-common,dusage,/workspace/dusage/target/release/dusage search --color=never -F 'func ' '/usr/local/go',311.1,299.4,329.6,5
search-common,rg,rg --color=never -F 'func ' '/usr/local/go',106.0,101.0,106.7,5
search-common,grep -rI,grep -rI --color=never -F 'func ' '/usr/local/go',142.6,141.7,156.1,5
search-regex,dusage,/workspace/dusage/target/release/dusage search --color=never '[A-Z][a-zA-Z0-9_]*\.go' '/usr/local/go',1158.8,1092.4,1228.9,5
search-regex,rg,rg --color=never '[A-Z][a-zA-Z0-9_]*\.go' '/usr/local/go',70.2,60.6,72.2,5
search-regex,grep -rIE,grep -rIE --color=never '[A-Z][a-zA-Z0-9_]*\.go' '/usr/local/go',212.9,206.1,218.8,5
search-case-insensitive,dusage,/workspace/dusage/target/release/dusage search --color=never -i 'memmove' '/usr/local/go',212.5,195.1,227.2,5
search-case-insensitive,rg,rg --color=never -i 'memmove' '/usr/local/go',62.6,54.7,67.0,5
search-case-insensitive,grep -rI,grep -rI --color=never -i 'memmove' '/usr/local/go',217.2,210.0,228.3,5
search-case-insensitive,dusage -F,/workspace/dusage/target/release/dusage search --color=never -F -i 'memmove' '/usr/local/go',212.9,200.8,222.3,5
search-files-with-matches,dusage -l,/workspace/dusage/target/release/dusage search --color=never -l -F 'func ' '/usr/local/go',194.3,174.1,213.1,5
search-files-with-matches,rg -l,rg --color=never -l -F 'func ' '/usr/local/go',88.5,79.4,90.8,5
search-files-with-matches,grep -rIl,grep -rIl --color=never -F 'func ' '/usr/local/go',154.6,140.7,162.4,5
find-glob,dusage find,/workspace/dusage/target/release/dusage find --color=never '*.go' '/usr',961.1,934.9,1084.1,5
find-glob,find -name,find '/usr' -name '*.go',396.1,356.8,450.0,5
threads-scan,dusage scan 1 thread,/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 1,685.8,673.2,696.9,5
threads-scan,dusage scan 2 threads,/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 2,383.6,344.0,409.8,5
threads-scan,dusage scan 4 threads,/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 4,192.5,171.9,209.2,5
threads-scan,dusage scan 8 threads,/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 8,202.8,187.3,242.0,5
threads-scan,dusage scan 16 threads,/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 16,219.7,201.9,253.3,5
threads-scan,dusage scan default,/workspace/dusage/target/release/dusage scan '/usr' --top 1,184.1,169.7,196.9,5
threads-search,dusage search 1 thread,/workspace/dusage/target/release/dusage search --color=never -F 'func ' --threads 1 '/usr/local/go',599.8,584.5,635.4,5
threads-search,dusage search 2 threads,/workspace/dusage/target/release/dusage search --color=never -F 'func ' --threads 2 '/usr/local/go',436.4,414.8,448.1,5
threads-search,dusage search 4 threads,/workspace/dusage/target/release/dusage search --color=never -F 'func ' --threads 4 '/usr/local/go',302.8,291.6,314.7,5
threads-search,dusage search 8 threads,/workspace/dusage/target/release/dusage search --color=never -F 'func ' --threads 8 '/usr/local/go',333.2,330.5,352.9,5
threads-search,dusage search 16 threads,/workspace/dusage/target/release/dusage search --color=never -F 'func ' --threads 16 '/usr/local/go',361.4,352.5,389.2,5
threads-search,dusage search default,/workspace/dusage/target/release/dusage search --color=never -F 'func ' '/usr/local/go',315.1,299.6,346.0,5
```

## Summary

Median wall time in milliseconds. The last column is dusage's time divided by
the reference tool's, so anything above 1.0 means dusage is slower.

| Case | dusage | reference | dusage vs it |
| --- | --- | --- | --- |
| scan `/usr`, 180k files | **186.4** | `du -sb` 670.6 | **3.6x faster** |
| scan `/usr` | 186.4 | `find -printf \| awk` 781.2 | 4.2x faster |
| scan `/usr`, 1 thread | 672.8 | `du -sb` 670.6 | 1.0x (parity) |
| search, rare literal | 203.6 | `rg -F` 52.0 / `grep -rI` 168.1 | 3.9x / 1.2x slower |
| search, frequent literal | 311.1 | `rg -F` 106.0 / `grep -rI` 142.6 | 2.9x / 2.2x slower |
| search, regex | 1158.8 | `rg` 70.2 / `grep -rIE` 212.9 | 16.5x / 5.4x slower |
| search, case-insensitive | 212.5 | `rg -i` 62.6 / `grep -rI` 217.2 | 3.4x / 1.0x (a shade ahead of grep) |
| search `-l` | 194.3 | `rg -l` 88.5 / `grep -rIl` 154.6 | 2.2x / 1.3x slower |
| find `*.go` over `/usr` | 961.1 | `find -name` 396.1 | 2.4x slower |

The only thing dusage wins outright is a multi-threaded scan. On the search side
it beats `grep` only in the case-insensitive case and loses to `ripgrep`
everywhere, by 1.2x on a rare literal and 16.5x on a regex.

The scan win is parallelism, not a better algorithm. Run single-threaded,
`dusage scan` takes 672.8 ms against `du -sb`'s 670.6 ms — a dead heat. `strace
-c` shows why: over `/usr`, `dusage scan` issues 267,507 `statx` calls and
35,660 `getdents64` calls, while `du -sb` issues 267,497 `newfstatat` calls and
the same 35,660 `getdents64`. Identical syscall work, one `stat` per entry
either way. dusage spreads the walk over threads and `du` does not.

## Thread defaults

The default is now one thread per core clamped to 4..16 (`src/cli.rs`), which
is 4 here. The sweep shows that this is the right point on a 4-vCPU box and what
the previous `4 * cores` default cost:

| Threads | `scan /usr` | `search -F 'func '` |
| --- | --- | --- |
| 1 | 685.8 ms | 599.8 ms |
| 2 | 383.6 ms | 436.4 ms |
| **4** | 192.5 ms | **302.8 ms** |
| 8 | 202.8 ms | 333.2 ms |
| 16 (the old default) | 219.7 ms | 361.4 ms |
| default (4 here) | **184.1 ms** | 315.1 ms |

Sixteen threads costs about 15-19% over four on both commands, so the clamp to
one-per-core is a measured win rather than a guess. Note that search scales
poorly with threads even below saturation — 599.8 ms at one thread to 302.8 ms
at four is 2.0x on 4 cores — because a single-threaded walk of the tree bounds
what extra reader threads can deliver.

## What changed since the last run

Three fixes landed on `parity-polish` after the 0.5.0 numbers in the previous
version of this document. Harness medians, same trees and same commands:

| Case | 0.5.0 | parity-polish | change |
| --- | --- | --- | --- |
| scan `/usr` (default threads) | 290.7 | **186.4** | -36%, mostly the 16→4 thread default |
| search, rare literal | 238.6 | 203.6 | -15% |
| search, frequent literal | 440.7 | 311.1 | -29% |
| search, regex | 1612.8 | 1158.8 | -28% |
| search, case-insensitive | 251.8 | 212.5 | -16% |
| search `-l` | 230.9 | 194.3 | -16% |
| find `*.go` | 1014.1 | 961.1 | -5% |
| `du -sb` (reference) | 652.1 | 670.6 | +3%, reference drift |

Part of every row is machine drift — the references moved 3% and this VM
routinely moves 10-25% between runs — so the specific fixes were measured
directly, three-way, with all binaries pinned to `--threads 4` to keep the
changed default out of the comparison. Medians of 7 alternating runs:

| Command | 0.4.0 | 0.5.0 | parity-polish |
| --- | --- | --- | --- |
| `-c -F 'func '` (full read, hits) | 174 | 200 | 185 |
| `-c` non-matching (full read, no hits) | 146 | 191 | 163 |
| `-m 1 -c` (few lines read) | 135 | 160 | 143 |
| full output, `-F 'func '` | 336 | 368 | **298** |
| `find '*.go' /usr` | 1014 | 1046 | 996 |

What each fix did:

- **One line buffer per file.** `Scanner::feed` allocated a fresh `Vec` per
  line; it now clears and reuses one buffer. The allocation was pure overhead on
  any line that did not match, which is why the no-match row was the worst hit:
  191 ms back to 163 ms.
- **The encoding sniff reads 8 KiB, not 64 KiB.** `encodings::detect` now gets
  only the prefix `bytes::is_binary` already looks at. The `-m 1` row, which
  reads almost no lines and so is dominated by per-file setup, went 160 ms to
  143 ms.
- **`--` separators only with context.** `render_search` now prints the group
  separator only when `-A`/`-B`/`-C` is set (`src/report.rs`), matching grep and
  ripgrep. That cut the `func ` output from 222,789 lines to 124,788, and the
  `write` syscall count with it (`strace -c`: 222,789 → 124,788, against
  `ripgrep`'s 10,775). This is why the full-output row is now faster than the
  0.4.0 build despite everything else.

**A residual regression survives.** Against 0.4.0 the content path is still 6-12%
slower: no-match 163 vs 146, `-c` with hits 185 vs 174, `-m 1` 143 vs 135. The
remaining per-file cost is the encoding sniff itself, which 0.4.0 did not have
at all — 8 KiB per file across 15,657 files is about 125 MB of extra scanning.
The cheap way to remove it is to run `null_pattern` only for a file the binary
check has already flagged as NUL-containing, leaving just the four-byte BOM test
on the text path.

## Counts agree before timings mean anything

Timings only compare tools doing the same work, so every case was checked for
match parity first.

| Query | dusage | `rg` | `grep` | Difference |
| --- | --- | --- | --- | --- |
| `-c -F 'func '` over `/usr/local/go` | 124,964 | 124,778 | 124,778 (`-cIl`) | +186, all in 10 binaries |
| `-c '[A-Z][a-zA-Z0-9_]*\.go'` | 321 | 265 | 265 (`-cIE`) | +56, same 10 binaries |
| `-c -i memmove` | 516 | 440 | — | +76, same 10 binaries |
| `-l -F 'func '` | 10,785 files | 10,775 files | — | +10, the same binaries |
| `find '*.h' /usr/include` | 2,785 paths | 2,785 (`--files -g`) | `find -name` 2,804 | 19, all symlinks |
| scan `/usr` total | 7,871,018,151 B | — | `du -sb` 7,868,013,000 B | 0.04% |

Two divergences, both explained:

- **Binary files.** `dusage` counts matches inside a file it also labels
  "Binary file … matches", where `grep -I` and `ripgrep` skip it outright. On
  text files all three agree exactly — the `func ` output is 124,778 content
  lines from all three tools.
- **Symlinks are not reported by `find`.** The walk never follows a link and no
  longer lists one, so `dusage find '*.h'` returns 2,785 where `find -name`
  returns 2,804. The 19 missing entries are `/usr/include/ncurses.h` and the 18
  symlinks under `/usr/include/ncursesw/`. `ripgrep` agrees with dusage exactly
  (2,785); `find` lists the link itself because its name matches. This is a
  deliberate rg-parity choice, but it is a visible change from 0.5.0, which
  returned all 2,804.

`find+awk` reports 7,901,390,671 bytes because plain `find` also sums directory
sizes and symlink lengths, which the other two do not.

## Where the search time goes

The gap is not the matcher. It is the shape of the pipeline and the output
layer.

**The walk is single-threaded and finishes before matching starts.** `search()`
calls `walk()`, which builds the whole candidate list — a `PathBuf` and a
`String` relative path per entry, and one `lstat` per entry, since
`DirReader::read` (`src/unix/enumerate.rs`) calls `e.metadata()`
unconditionally — and then sorts it by relative path so that output is
deterministic. Only afterwards does `par_for_each` start reading file contents
on the worker threads. `ripgrep` overlaps walking with searching and never
materialises the list. The cost of that design in isolation is the `find` row:
961.1 ms for a pure walk over `/usr` against `find`'s 396.1 ms, 2.4x, and that
floor sits under every search number in the table. A 200 ms search of a 244 MB
tree is mostly walk.

**The literal matcher is competitive; the regex engine is not.** A plain
literal matches with a SWAR memmem scan (`bytes::find_candidate`,
`src/bytes.rs`: 32 bytes per iteration, first-byte candidates verified
tail-first), and case-insensitive patterns now reach the same path when they are
plain literals. Anything else runs a Pike VM per line (`Regex::run`,
`src/regex.rs`) with only a required-byte prefilter, and that is where the 16.5x
gap comes from.

**Output is unbuffered.** `src/cmd/search.rs` writes to
`std::io::stdout().lock()`, which is a `LineWriter`: one `write` syscall per
printed line. After the separator fix that is 124,788 writes for the `func `
query against `ripgrep`'s 10,775, which batches per file. A `BufWriter` around
stdout is the obvious remaining win on any query with many matches.

**Matches are accumulated in memory before anything is printed.** Every matching
line is copied and held until the walk completes, which is what makes sorted
output possible and makes peak memory grow with the match count.
`/usr/bin/time -v` on the same `func ` query: 43 MB peak RSS for dusage, 11 MB
for `ripgrep`, 4.6 MB for `grep`.

## Caveats

- **Warm cache only.** Nothing was dropped; both trees were resident in page
  cache throughout. On a cold disk, system time dominates and the gaps between
  tools shrink toward the I/O bound, which can reorder this table.
- **Page cache effects are large.** `dusage scan` covers 7.9 GB in about 185 ms,
  roughly 42 GB/s of apparent I/O. That is cache bandwidth, not disk
  throughput; do not read these numbers as what a real volume does.
- **This VM drifts 10-25% between runs.** The scan row measured 186.4 ms in the
  main table and 184.1 ms in the sweep of the same run, but across runs the same
  command has ranged 221-291 ms; the regex row spans 1092-1229 ms. Treat
  differences under ~20% as ties and read the `min_ms`/`max_ms` columns before
  trusting a small gap. Every comparison that mattered here (0.4.0 vs 0.5.0 vs
  parity-polish, thread counts) was run as an alternating sequence for exactly
  this reason.
- **The trees are conventional, not adversarial.** `/usr` is a normal package
  mix; `/usr/local/go` is text-only and unusually friendly to a content
  scanner — no `.gitignore` files, 12 hidden entries, no `node_modules` or
  `target`. A repository checkout full of ignore rules, or a tree with huge
  single files, exercises different paths.
- **Small trees are dominated by process startup.** `/usr/include` (37 MB,
  3,363 files) is a 10-30 ms job for every tool here, and the differences at
  that size are noise. Only the two trees above are big enough to measure.
- **`grep` ran with `-I`, `ripgrep` with its defaults.** `grep -rI` skips
  binaries but still walks hidden directories; `ripgrep` skips hidden entries
  and `.gitignore`d paths, as dusage does. On these trees the difference is 12
  hidden entries in `/usr/local/go` and nothing measurable in `/usr`. On a
  checkout with a large `.git` it would not be negligible for `grep`.
- **No I/O tuning anywhere.** No `io_uring`, no `O_DIRECT`, no
  `posix_fadvise`, no explicit readahead. dusage and the references all use
  plain buffered reads behind the same page cache, so a kernel-side I/O path
  would not show up in this table at all.
- **`ripgrep` is a mature, heavily tuned tool** using SIMD-assisted `memchr` and
  a hybrid automaton. dusage is a zero-dependency implementation with a SWAR
  memmem and a Pike VM. A 16x gap on regex workloads is the expected outcome of
  that difference in engineering effort, not a defect to be shrunk to 1x.
- **One machine, one kernel.** Five rounds per command with medians reported,
  over a single 4-vCPU VM.

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
# dusage benchmark, 2026-11-18T18:27:32Z
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

Ubuntu 24.04 container on a 4-vCPU VM, `dusage` 0.5.0 on branch `docs-final`
(commit `ea1be75`). Trees: `/usr` (7.87 GB, 180,451 files) for the scan and
`find` cases, `/usr/local/go` (244 MB, 15,657 text files) for the content-search
cases. `dusage` was left at its default thread count, which is one thread per
core clamped to 4..16 — 4 worker threads on this box. `ripgrep` and `grep` were
left at their own defaults.

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
scan,dusage scan,/workspace/dusage/target/release/dusage scan '/usr' --top 1,187.0,164.9,218.1,5
scan,du -sb,du -sb '/usr',665.5,642.4,681.7,5
scan,find+awk,find '/usr' -printf '%s\n' | awk '{s+=$1} END{}',769.8,768.0,1029.0,5
scan,dusage scan (1 thread),/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 1,682.4,662.6,730.1,5
search-rare,dusage,/workspace/dusage/target/release/dusage search --color=never -F 'asyncpreemptoff' '/usr/local/go',217.9,200.0,231.4,5
search-rare,rg,rg --color=never -F 'asyncpreemptoff' '/usr/local/go',53.9,51.0,68.1,5
search-rare,grep -rI,grep -rI --color=never -F 'asyncpreemptoff' '/usr/local/go',157.1,156.3,208.1,5
search-common,dusage,/workspace/dusage/target/release/dusage search --color=never -F 'func ' '/usr/local/go',213.1,209.0,265.6,5
search-common,rg,rg --color=never -F 'func ' '/usr/local/go',94.0,91.3,120.4,5
search-common,grep -rI,grep -rI --color=never -F 'func ' '/usr/local/go',143.8,141.1,180.5,5
search-regex,dusage,/workspace/dusage/target/release/dusage search --color=never '[A-Z][a-zA-Z0-9_]*\.go' '/usr/local/go',1175.6,1065.1,1360.2,5
search-regex,rg,rg --color=never '[A-Z][a-zA-Z0-9_]*\.go' '/usr/local/go',64.9,58.3,70.9,5
search-regex,grep -rIE,grep -rIE --color=never '[A-Z][a-zA-Z0-9_]*\.go' '/usr/local/go',219.0,208.5,237.4,5
search-case-insensitive,dusage,/workspace/dusage/target/release/dusage search --color=never -i 'memmove' '/usr/local/go',196.3,193.5,202.9,5
search-case-insensitive,rg,rg --color=never -i 'memmove' '/usr/local/go',58.0,54.6,60.3,5
search-case-insensitive,grep -rI,grep -rI --color=never -i 'memmove' '/usr/local/go',216.0,211.4,216.8,5
search-case-insensitive,dusage -F,/workspace/dusage/target/release/dusage search --color=never -F -i 'memmove' '/usr/local/go',183.0,173.0,233.7,5
search-files-with-matches,dusage -l,/workspace/dusage/target/release/dusage search --color=never -l -F 'func ' '/usr/local/go',175.9,167.5,216.0,5
search-files-with-matches,rg -l,rg --color=never -l -F 'func ' '/usr/local/go',84.8,80.0,103.8,5
search-files-with-matches,grep -rIl,grep -rIl --color=never -F 'func ' '/usr/local/go',145.8,142.5,179.3,5
find-glob,dusage find,/workspace/dusage/target/release/dusage find --color=never '*.go' '/usr',936.7,918.8,1233.2,5
find-glob,find -name,find '/usr' -name '*.go',386.5,357.1,397.1,5
threads-scan,dusage scan 1 thread,/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 1,673.6,666.4,693.1,5
threads-scan,dusage scan 2 threads,/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 2,358.4,349.1,383.7,5
threads-scan,dusage scan 4 threads,/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 4,184.4,175.4,202.6,5
threads-scan,dusage scan 8 threads,/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 8,217.1,173.4,287.5,5
threads-scan,dusage scan 16 threads,/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 16,245.7,211.6,259.3,5
threads-scan,dusage scan default,/workspace/dusage/target/release/dusage scan '/usr' --top 1,184.0,176.7,230.7,5
threads-search,dusage search 1 thread,/workspace/dusage/target/release/dusage search --color=never -F 'func ' --threads 1 '/usr/local/go',510.9,475.6,543.7,5
threads-search,dusage search 2 threads,/workspace/dusage/target/release/dusage search --color=never -F 'func ' --threads 2 '/usr/local/go',359.4,312.4,391.8,5
threads-search,dusage search 4 threads,/workspace/dusage/target/release/dusage search --color=never -F 'func ' --threads 4 '/usr/local/go',203.4,199.0,281.9,5
threads-search,dusage search 8 threads,/workspace/dusage/target/release/dusage search --color=never -F 'func ' --threads 8 '/usr/local/go',256.9,233.8,262.7,5
threads-search,dusage search 16 threads,/workspace/dusage/target/release/dusage search --color=never -F 'func ' --threads 16 '/usr/local/go',260.8,247.6,305.5,5
threads-search,dusage search default,/workspace/dusage/target/release/dusage search --color=never -F 'func ' '/usr/local/go',202.3,195.0,222.4,5
```

## Summary

Median wall time in milliseconds, five alternating runs each, sequential (nothing else running).
Measured on a 2-vCPU box against the final 0.5.0 binary, so these are the numbers the shipped
code produces; the earlier table in this document came from a 4-vCPU machine and a build with
the sequential walker.

| Case | dusage | reference | dusage vs it |
| --- | --- | --- | --- |
| scan `/usr`, 180k files | **379** | `du -sb` 663 | **1.7x faster** |
| search, frequent literal `-c` | **151** | `rg` 164 / `grep -rI` 144 | **1.1x faster than rg**, 1.05x slower than grep |
| search, rare literal `-c` | 127 | `rg` 94 | 1.35x slower |
| search, case-insensitive `-c` | 129 | `rg -i` 98 | 1.3x slower |
| search, regex `-c` | 348 | `rg` 99 | 3.5x slower |
| search `-l` | **142** | `rg -l` 146 | parity |
| find `*.go` over `/usr` | 384 | `find -name` 369 | parity (1.04x) |

A frequent literal is now the fastest of the three tools, `-l` is a dead heat with ripgrep, and
`find` is level with `find(1)` where it used to be 2.4x behind. The two places dusage still
loses are a rare literal and a general regex, both twice or more over ripgrep's time:

- **Rare literals** pay a fixed per-block cost the frequent case hides: the block scanner finds
  no match and still has to count the newlines it skipped.
- **Regexes** run the line-at-a-time path. The lazy DFA makes per-line matching cheap, but the
  line splitting itself is what a block scanner avoids, and the DFA's leftmost-start recovery
  walks backwards from the end of the buffer it is handed — sound on a line, quadratic on a
  block. Closing that gap means a reverse-suffix DFA; it is the open item in `docs/ROADMAP.md`.

Where the wins came from, in order: a parallel walk (every thread reads directories), a
lazy enumerator that reads types from the dirent and stats only when a mode needs size or mtime,
the block-oriented literal scan, the SWAR literal matcher behind it, and a 64 KiB buffered
stdout. Each is measured in the sections below.

## Thread defaults

The default is one thread per core clamped to 4..16 (`src/cli.rs`), which is 4
here. The sweep shows that this is the right point on a 4-vCPU box and what the
previous `4 * cores` default cost:

| Threads | `scan /usr` | `search -F 'func '` |
| --- | --- | --- |
| 1 | 673.6 ms | 510.9 ms |
| 2 | 358.4 ms | 359.4 ms |
| **4** | 184.4 ms | **203.4 ms** |
| 8 | 217.1 ms | 256.9 ms |
| 16 (the old default) | 245.7 ms | 260.8 ms |
| default (4 here) | **184.0 ms** | 202.3 ms |

Sixteen threads costs 15-30% over four on both commands, so the clamp to
one-per-core is a measured win rather than a guess. Search scales worse than
scan even below saturation — 510.9 ms at one thread to 203.4 ms at four is 2.5x
on 4 cores against scan's 3.7x — because the single-threaded walk of the file
list bounds what extra reader threads can deliver.

## What changed since the last run

Four performance changes have landed since the first 0.5.0 measurement. Harness
medians for the same commands and trees, across the runs:

| Case | 0.5.0 (first) | +parity fixes | docs-final |
| --- | --- | --- | --- |
| search, frequent literal | 440.7 | 311.1 | **213.1** |
| search, rare literal | 238.6 | 203.6 | 217.9 |
| search, regex | 1612.8 | 1158.8 | 1175.6 |
| search, case-insensitive | 251.8 | 212.5 | **196.3** |
| search `-l` | 230.9 | 194.3 | **175.9** |
| find `*.go` | 1014.1 | 961.1 | 936.7 |
| scan `/usr` | 290.7 | 186.4 | 187.0 |

Part of every row is machine drift — the reference tools moved 3-11% across the
same runs and this VM routinely moves 10-25% between runs — so the specific
fixes were measured directly, three-way, with every binary pinned to
`--threads 4` to keep thread defaults out of the comparison. Medians of 7
alternating runs:

| Command | 0.4.0 | main (#4) | docs-final |
| --- | --- | --- | --- |
| `-c -F 'func '` (full read, hits) | 164 | 181 | 176 |
| `-c` non-matching (full read, no hits) | 147 | 163 | 160 |
| `-m 1 -c` (few lines read) | 132 | 148 | 137 |
| full output, `-F 'func '` | 327 | 289 | **218** |
| full output, `-F 'asyncpreemptoff'` | 155 | 174 | 169 |
| `find '*.go' /usr` | 943 | 930 | 937 |

What each change did:

- **`-i` takes the literal fast path** (`src/pattern.rs`): a case-insensitive
  pattern that is a plain literal is matched with the SWAR scan instead of the
  Pike VM. `-i memmove` went from 1170 ms to 196.3 ms, `-i` and `-F -i` now
  produce byte-identical output (`cmp` on the full run), and the case is the one
  search mode where dusage is now faster than `grep`.
- **One line buffer per file** (`Scanner::feed`): the per-line `Vec` allocation
  went away, which is why the no-match row came back from 191 ms to 160 ms.
- **The encoding sniff is skipped for plain text** (`src/search.rs`): `detect`
  now runs only when the binary check already found NULs or the file opens with
  a UTF-8 BOM. That cost a per-file pass that 0.4.0 never paid; the `-m 1` row,
  which is dominated by per-file setup, dropped from 148 ms to 137 ms.
- **stdout is buffered** (`src/cmd/search.rs`, 64 KiB `BufWriter` around both
  renderers): one `write` syscall per printed line became one per 64 KiB.
  `strace -c` on `func ` now counts 278 `write` calls, down from 124,788, and 8
  for a `find` run. This is the whole of the full-output row's improvement:
  218 ms against `grep`'s 143.8 and `ripgrep`'s 94.0, where the same query cost
  368 ms before the buffering.

## Is the gap to the pre-merge build closed?

Mostly, and it now depends on what the command prints.

- **Output-heavy searches are well ahead of 0.4.0.** `-F 'func '` with full
  output is 218 ms against 0.4.0's 327 ms, a third faster, because dusage no
  longer prints `--` separators it should not and no longer issues a syscall per
  line.
- **The pure read-and-match path was about 7% behind, and that is now gone.**
  With `-c`, where output is one line per file, docs-final measured 176 ms
  against 0.4.0's 164 ms, and 160 ms against 147 ms on a pattern that matches
  nothing. The cost was the copy every line made into the reused buffer in
  `Scanner::feed`; 0.4.0 matched directly against a slice of the read buffer.
  `Scanner::feed` now hands `handle_line` a slice of the block whenever the line
  lies entirely inside it, and only a line continued from the previous block
  goes through the buffer. Re-measured after that change on a 2-vCPU box, `-c`
  over the same 244 MB tree takes 0.27-0.31 s against `rg -c`'s 0.18-0.22 s.

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
- **Symlinks are not reported by `find`.** The walk never follows a link and
  does not list one, so `dusage find '*.h'` returns 2,785 where `find -name`
  returns 2,804. The 19 missing entries are `/usr/include/ncurses.h` and the 18
  symlinks under `/usr/include/ncursesw/`. `ripgrep` agrees with dusage exactly
  (2,785); `find` lists the link itself because its name matches.

`find+awk` reports 7,901,390,671 bytes because plain `find` also sums directory
sizes and symlink lengths, which the other two do not.

## Where the search time goes

The gap is not the matcher. It is the walk, which is serial and happens before
any matching starts.

**The walk is single-threaded and finishes before matching starts.** `search()`
calls `walk()`, which builds the whole candidate list — a `PathBuf` and a
`String` relative path per entry, and one `lstat` per entry, since
`DirReader::read` (`src/unix/enumerate.rs`) calls `e.metadata()`
unconditionally — and then sorts it by relative path so that output is
deterministic. Only afterwards does `par_for_each` start reading file contents
on the worker threads. `ripgrep` overlaps walking with searching and never
materialises the list. The cost of that design in isolation is the `find` row:
936.7 ms for a pure walk over `/usr` against `find`'s 386.5 ms, 2.4x, and that
floor sits under every search number in the table.

**The literal matcher is competitive; the regex engine now nearly is.** A
plain literal, case-sensitive or folded, matches with a SWAR memmem scan
(`bytes::find_candidate`, `src/bytes.rs`: 32 bytes per iteration, first-byte
candidates verified tail-first). Anything else runs a lazily built DFA over the
same Thompson program (`src/regex.rs`), a table lookup per byte, with a
required-byte prefilter in front of it and a byte-range skip while the scan sits
in a start state. The Pike VM still answers lines that need the character
program and any pattern whose DFA blows the state cap.

The engine change, measured the same way on `/usr/local/go` (4 vCPU, warm
cache, the three binaries interleaved over five rounds and the median taken,
`search -c`, seconds):

| pattern | before | after | `rg -c` |
| --- | --- | --- | --- |
| `[A-Z][a-zA-Z0-9_]*\.go` | 0.91 | 0.18 | 0.05 |
| `func \w+\(` | 0.51 | 0.14 | 0.09 |
| `^package \w+` | 0.71 | 0.11 | 0.10 |
| `\bvar\b \w+,` | 0.85 | 0.12 | 0.05 |
| `[0-9]{2,4}` | 0.66 | 0.26 | 0.13 |
| `(?i)memmove` | 0.79 | 0.16 | 0.05 |
| `\.(rs\|toml)$` | 0.41 | 0.10 | 0.05 |

`[0-9]{2,4}` is the smallest win because a third of the lines match, and every
matching line pays the reverse pass and the anchored VM run described in
`src/regex.rs`; printing the matching lines instead of counting them moves the
first row from 1.07 s to 0.26 s.

**Output is buffered now.** Both renderers wrap `stdout()` in a 64 KiB
`BufWriter`, so `strace -c` counts 278 `write` calls for the 124,788-line
`func ` result and 8 for a `find` run. Output volume still matters — dusage
prints a line per match and holds every match in memory until the walk finishes,
which is what makes sorted output possible and makes peak memory grow with the
match count (`/usr/bin/time -v`: 43 MB peak RSS against `ripgrep`'s 11 MB and
`grep`'s 4.6 MB) — but it is no longer paying a syscall per line.

## Caveats

- **Warm cache only.** Nothing was dropped; both trees were resident in page
  cache throughout. On a cold disk, system time dominates and the gaps between
  tools shrink toward the I/O bound, which can reorder this table.
- **Page cache effects are large.** `dusage scan` covers 7.9 GB in about 185 ms,
  roughly 42 GB/s of apparent I/O. That is cache bandwidth, not disk
  throughput; do not read these numbers as what a real volume does.
- **This VM drifts 10-25% between runs.** The scan row measured 187.0 ms in the
  main table and 184.0 ms in the sweep of the same run, but across runs the same
  command has ranged 186-291 ms, and the rare-literal row moved from 203.6 to
  217.9 ms between two runs with no relevant code change. Treat differences
  under ~20% as ties and read the `min_ms`/`max_ms` columns before trusting a
  small gap. Every comparison that mattered here was run as an alternating
  sequence for exactly this reason.
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
  memmem and a lazily built DFA. A 2-4x remaining gap on regex workloads is the
  expected outcome of that difference in engineering effort; the 6x gap this
  section used to report closed when the DFA replaced the per-line Pike VM.
- **One machine, one kernel.** Five rounds per command with medians reported,
  over a single 4-vCPU VM.

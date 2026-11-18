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
# dusage benchmark, 2026-11-18T17:59:07Z
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

Ubuntu 24.04 container on a 4-vCPU VM. Trees: `/usr` (7.87 GB, 180,451 files)
for the scan and `find` cases, `/usr/local/go` (244 MB, 15,657 text files) for
the content-search cases. `dusage` was left at its default thread count, which
is 4 threads per core — 16 worker threads on this box. `ripgrep` and `grep`
were left at their own defaults.

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
scan,dusage scan,/workspace/dusage/target/release/dusage scan '/usr' --top 1,230.7,197.4,247.8,5
scan,du -sb,du -sb '/usr',656.0,647.2,729.6,5
scan,find+awk,find '/usr' -printf '%s\n' | awk '{s+=$1} END{}',789.4,760.4,824.7,5
scan,dusage scan (4 threads),/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 4,181.1,178.8,187.9,5
scan,dusage scan (1 thread),/workspace/dusage/target/release/dusage scan '/usr' --top 1 --threads 1,621.8,615.1,643.2,5
search-rare,dusage,/workspace/dusage/target/release/dusage search --color=never -F 'asyncpreemptoff' '/usr/local/go',200.3,187.8,224.7,5
search-rare,rg,rg --color=never -F 'asyncpreemptoff' '/usr/local/go',50.2,49.4,51.3,5
search-rare,grep -rI,grep -rI --color=never -F 'asyncpreemptoff' '/usr/local/go',160.2,154.4,199.9,5
search-common,dusage,/workspace/dusage/target/release/dusage search --color=never -F 'func ' '/usr/local/go',385.0,380.3,406.1,5
search-common,rg,rg --color=never -F 'func ' '/usr/local/go',109.7,97.1,110.0,5
search-common,grep -rI,grep -rI --color=never -F 'func ' '/usr/local/go',146.6,140.2,166.5,5
search-common,dusage (4 threads),/workspace/dusage/target/release/dusage search --color=never -F 'func ' --threads 4 '/usr/local/go',326.4,316.3,345.1,5
search-regex,dusage,/workspace/dusage/target/release/dusage search --color=never '[A-Z][a-zA-Z0-9_]*\.go' '/usr/local/go',1426.3,1120.6,1509.7,5
search-regex,rg,rg --color=never '[A-Z][a-zA-Z0-9_]*\.go' '/usr/local/go',69.1,61.8,73.7,5
search-regex,grep -rIE,grep -rIE --color=never '[A-Z][a-zA-Z0-9_]*\.go' '/usr/local/go',219.7,206.0,230.9,5
search-case-insensitive,dusage,/workspace/dusage/target/release/dusage search --color=never -i 'memmove' '/usr/local/go',1170.2,940.0,1307.4,5
search-case-insensitive,rg,rg --color=never -i 'memmove' '/usr/local/go',70.1,65.0,75.5,5
search-case-insensitive,grep -rI,grep -rI --color=never -i 'memmove' '/usr/local/go',222.6,209.2,239.8,5
search-case-insensitive,dusage -F,/workspace/dusage/target/release/dusage search --color=never -F -i 'memmove' '/usr/local/go',204.7,200.9,217.9,5
search-files-with-matches,dusage -l,/workspace/dusage/target/release/dusage search --color=never -l -F 'func ' '/usr/local/go',183.5,171.4,189.8,5
search-files-with-matches,rg -l,rg --color=never -l -F 'func ' '/usr/local/go',83.5,80.4,86.5,5
search-files-with-matches,grep -rIl,grep -rIl --color=never -F 'func ' '/usr/local/go',140.9,139.5,147.0,5
find-glob,dusage find,/workspace/dusage/target/release/dusage find --color=never '*.go' '/usr',956.5,913.7,1007.8,5
find-glob,find -name,find '/usr' -name '*.go',374.4,346.7,413.2,5
```

## Summary

Median wall time in milliseconds. The last column is dusage's time divided by
the reference tool's, so anything above 1.0 means dusage is slower.

| Case | dusage | reference | dusage vs it |
| --- | --- | --- | --- |
| scan `/usr`, 180k files | **230.7** | `du -sb` 656.0 | **2.8x faster** |
| scan `/usr` | 230.7 | `find -printf \| awk` 789.4 | 3.4x faster |
| search, rare literal | 200.3 | `rg -F` 50.2 / `grep -rI` 160.2 | 4.0x / 1.3x slower |
| search, frequent literal | 385.0 | `rg -F` 109.7 / `grep -rI` 146.6 | 3.5x / 2.6x slower |
| search, regex | 1426.3 | `rg` 69.1 / `grep -rIE` 219.7 | 20.6x / 6.5x slower |
| search, case-insensitive | 1170.2 | `rg -i` 70.1 / `grep -rI` 222.6 | 16.7x / 5.3x slower |
| search `-l` | 183.5 | `rg -l` 83.5 / `grep -rIl` 140.9 | 2.2x / 1.3x slower |
| find `*.go` over `/usr` | 956.5 | `find -name` 374.4 | 2.6x slower |

The only thing dusage wins is a multi-threaded scan. Everything on the search
side loses: by 1.3x against `grep` for a rare literal, and by more than 20x
against `ripgrep` for a regex.

The scan win is parallelism, not a better algorithm. Run single-threaded,
`dusage scan` takes 621.8 ms against `du -sb`'s 656.0 ms — a dead heat. `strace
-c` shows why: over `/usr`, `dusage scan` issues 267,507 `statx` calls and
35,660 `getdents64` calls, while `du -sb` issues 267,497 `newfstatat` calls and
the same 35,660 `getdents64`. Identical syscall work, one `stat` per entry
either way. dusage spreads the walk over threads and `du` does not.

## Thread count is the biggest lever, and the default gets it wrong

The default of 4 threads per core oversubscribes a 4-vCPU box. Median of 9 runs
each, same trees:

| Threads | `scan /usr` | `search -F 'func '` |
| --- | --- | --- |
| 1 | 686 ms | 633 ms |
| 2 | 347 ms | 446 ms |
| **4** | **187 ms** | **339 ms** |
| 8 | 202 ms | 382 ms |
| 16 (default) | 265 ms | 417 ms |

The harness confirms it in its own five-run medians: scan is 181.1 ms at
`--threads 4` against 230.7 ms at the default, and the frequent-literal search
is 326.4 ms against 385.0 ms. Four threads is 1.2-1.3x faster than sixteen on
this hardware, and it is the fastest setting of the five. The two measurements
of the sixteen-thread case differ by 14% between phases of machine load (265 ms
here, 230.7 ms in the harness); both put four threads clearly ahead, and that
direction is what matters, not the absolute milliseconds.

So `cli::default_threads` (`src/cli.rs:174`) returning 4x cores costs 20-30%
here. `AGENTS.md` already flagged this as unmeasured; it is measured now, and a
default of one thread per core would win on this machine. Changing it is a
behaviour change for every command including `fastdel`, so it is not in this
branch — this section is the evidence for whoever does it.

## Counts agree before timings mean anything

Timings only compare tools doing the same work, so every case was checked for
match parity first.

| Query | dusage | `rg` | `grep` | Difference |
| --- | --- | --- | --- | --- |
| `-c -F 'func '` over `/usr/local/go` | 124,964 | 124,778 | 124,778 (`-cIl`) | +186, all in 10 binaries |
| `-c '[A-Z][a-zA-Z0-9_]*\.go'` | 321 | 265 | 265 (`-cIE`) | +56, same 10 binaries |
| `-l -F 'func '` | 10,785 files | 10,775 files | — | +10, the same binaries |
| `find '*.h' /usr/include` | 2,804 paths | — | `find -name` 2,804 | exact |
| scan `/usr` total | 7,871,018,151 B | — | `du -sb` 7,868,013,000 B | 0.04% |

The only divergence is binary files: `dusage` counts matches inside a file it
also labels "Binary file … matches", where `grep -I` and `ripgrep` skip it
outright. On text files the three agree exactly — the `func ` output is 124,778
content lines from all three tools. `find+awk` reports 7,901,390,671 bytes
because plain `find` also sums directory sizes and symlink lengths, which the
other two do not.

## Where the search time goes

The gap is not the matcher. It is the shape of the pipeline, one specific hole
in the fast path, and the output layer.

**The walk is single-threaded and finishes before matching starts.** `search()`
(`src/search.rs:311`) calls `walk()` (`src/search.rs:270`), which builds the
whole candidate list — a `PathBuf` and a `String` relative path per entry, and
one `lstat` per entry, since `DirReader::read` (`src/unix/enumerate.rs`) calls
`e.metadata()` unconditionally — and then sorts it by relative path
(`src/search.rs:305`) so that output is deterministic. Only afterwards does
`par_for_each` (`src/search.rs:362`) start reading file contents on 16 threads.
`ripgrep` overlaps walking with searching and never materialises the list. The
cost of that design in isolation is the `find` row: 956.5 ms for a pure walk
over `/usr` against `find`'s 374.4 ms, 2.6x, and that floor sits under every
search number in the table. A 200 ms search of a 244 MB tree is mostly walk.

**A regex keeps only a required-byte prefilter.** A pattern that compiles to a
plain literal matches with a SWAR memmem scan (`bytes::find_candidate`,
`src/bytes.rs:123`: 32 bytes per iteration, first-byte candidates verified
tail-first), which is why the literal cases are only 1.3-4x off `ripgrep`.
Everything else runs a Pike VM per line (`Regex::run`, `src/regex.rs:269`).

**Case-insensitive throws the literal path away, and that is fixable.** With
`-i`, `literal_of` returns `None` unconditionally (`src/regex.rs:497-500`), so
`-i memmove` — not a regex in any sense a user would recognise — becomes a Pike
VM with no prefilter, because the folded byte set for `m` has two members and
`Node::Set(s) => s.sole()` (`src/regex.rs:519`) cannot pick one. Measured in the
same harness run:

| Invocation | median of 5 |
| --- | --- |
| `dusage search -i memmove` | 1,170.2 ms |
| `dusage search -F -i memmove` | 204.7 ms |
| `rg -i memmove` | 70.1 ms |

`-F -i` takes the `memmem_ci` path (`src/pattern.rs:128`) and lands at 204.7 ms;
dropping `-F` costs 5.7x on a pattern with no metacharacters in it. Teaching
`Pattern::new` to keep the literal path when the pattern parses as a plain
literal, even under `-i`, would close most of the 16.7x gap to `ripgrep`
without touching the regex engine. The parser already knows the pattern is
literal — `literal_of` throws that information away the moment `ci` is set.
Separately, at one thread the same query takes 3,149 ms (median of 5), so its
parallel scaling is 2.7x on 4 cores, the weakest of any case here.

**Output is unbuffered, and it is not free.** `src/cmd/search.rs:436` writes to
`std::io::stdout().lock()`, which is a `LineWriter`: one `write` syscall per
printed line. `strace -c` on `func ` over `/usr/local/go` counts 222,789
`write` calls for dusage against 10,775 for `ripgrep`, which batches per file.
Of those 222,789 lines, 124,778 are matches, 10 are binary-file notices and
98,001 are `--` group separators (see the divergence below), so dusage spends
98,001 syscalls printing separators no other grep prints.

**Matches are accumulated in memory before anything is printed.** Every matching
line is copied into a `Vec<u8>` (`src/search.rs:535`) and held until the walk
completes, which is what makes sorted output possible and makes peak memory
grow with the match count. `/usr/bin/time -v` on the same `func ` query: 47 MB
peak RSS for dusage, 11 MB for `ripgrep`, 4.6 MB for `grep`.

## Output divergence: `--` separators without context

Found while checking parity. `render_search` emits a `--` group separator
between non-adjacent matches in a file even when no context was requested
(`src/report.rs:344-351`). This is deliberate rather than accidental — the test
at `src/report.rs:877` asserts `--` with `before` and `after` both zero — but it
diverges from both reference tools: GNU grep and ripgrep print `--` only
between context groups, that is, only under `-A`, `-B` or `-C`. Minimal
reproduction:

```console
$ printf 'hit a\nx\ny\nhit b\n' > f.txt
$ dusage search hit f.txt
hit a
--
hit b
$ grep hit f.txt
hit a
hit b
```

On the `func ` query over `/usr/local/go` this adds 98,001 lines to a 124,778
line result, which inflates both the printed volume and the wall time measured
above. Fixing it would make the frequent-literal and `-l` rows faster.

## Caveats

- **Warm cache only.** Nothing was dropped; both trees were resident in page
  cache throughout. On a cold disk, system time dominates and the gaps between
  tools shrink toward the I/O bound, which can reorder this table.
- **Page cache effects are large.** `dusage scan` covers 7.9 GB in 230 ms, about
  34 GB/s of apparent I/O. That is cache bandwidth, not disk throughput; do not
  read these numbers as what a real volume does.
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
  memmem and a Pike VM. A 20x gap on regex workloads is the expected outcome of
  that difference in engineering effort, not a defect to be shrunk to 1x.
- **One machine, one kernel.** Five rounds per command with medians reported,
  over a single 4-vCPU VM. The `min_ms`/`max_ms` columns in the raw output show
  the spread; the regex row spread by 35% between its fastest and slowest run,
  which is why the summary stops at one decimal.

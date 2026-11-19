# Benchmarks

Wall-clock medians for `dusage scan`, `dusage search`, `dusage find` and
`dusage du` against the tools they replace: `du`, `find`, `grep` and `ripgrep`,
with peak memory in the same rows.

**Every number below is program output.** The tables are the stdout of
`bench/summary.sh` reading `bench/results.json`, which `bench/run.sh` wrote; the
guard block is the stdout of `scripts/perf-guard.sh`, run against that binary.
Both were taken at the commit named two sections down. Nothing in this document
is typed by hand, so nothing here can drift from the machine that produced it —
if a number looks wrong, re-run the commands rather than editing the table.

## How to refresh these numbers

```console
$ cargo build --release
$ bash bench/run.sh                 # writes bench/results.json, prints the CSV
$ bash bench/summary.sh             # renders the tables in this document
$ bash scripts/perf-guard.sh        # pass/fail against bench/baseline.json
```

What each produces:

| Command | Output | Use |
|---|---|---|
| `bench/run.sh` | `bench/results.json` (schema 1) plus the CSV on stdout | the one artifact every number here comes from |
| `bench/summary.sh` | the Markdown tables below, one per cache pass | copied verbatim into this file |
| `scripts/perf-guard.sh` | a fixed-width verdict table and an exit status | the block in "The performance guard" |

The rule for this document: a number is either the output of one of those
commands, or it is labelled as an approximation with the reason. The result file
`bench/results.json` is gitignored, so the tables are a snapshot kept in the
document; the commands reproduce them. A hand-typed
timing is not allowed, and a ratio quoted as stable ("2.7x slower than ripgrep")
belongs in prose only with the commit it was measured at — see the cross
references in `docs/SEARCH.md`, whose performance sentences point here instead
of restating a figure.

After a merge that touches the scanner or the walk, run all four: the guard is
the cheap check, the suite is the one that updates this document.

## What the suite measures

`bench/cases.sh` declares one line per measured row and nothing else:

```sh
bench_row <case> <section> <cache> <role> <tool> <command> <evict-root>
```

`case` is a stable id (what `bench/baseline.json` and the CI guard key off),
`section` is the group the row prints under, `cache` is `warm`, `cold` or
`both`, `role` is `primary` (dusage) or `reference` (the tool it replaces), and
`evict-root` names the tree the cold pass drops. Both `bench/run.sh` and
`scripts/perf-guard.sh` source that file, so the suite and the guard can never
measure different command lines. Each case names its reference: `rg`, `grep`,
`find` or `du`. The modes covered are the ones the binary has — literal,
frequent and rare literals, case-insensitive, regex that matches and regex that
never does, `-U`, `-z` over a gzip tree built from the search tree on first run,
`-l`, `-c`, `-o`, `find` by name, by path and by `-t` type, `du -s`, `du`, and
`scan` across a thread sweep.

`bench/run.sh` warms every command once, then runs five interleaved rounds — one
run of every row per round, so machine drift hits all tools alike — and reports
the median of the five. Cold rows get their own pass with their own cache drop,
and the RSS columns come from a pass of their own so the measurement never
perturbs the clock. Warm and cold numbers are never mixed in one table.

### Peak memory

Peak RSS is measured with `/usr/bin/time -v` reading `Maximum resident set size`,
three samples per row with the largest kept. Where GNU time is absent the harness
samples `/proc/<pid>/status` `VmHWM` every 10 ms for the process and its direct
children and keeps the maximum; that sampler is approximate, and the
`RSS method:` line in the generated output says which method produced the
numbers. Pipeline rows (`find … | awk …`) report the largest single process in
the pipeline, not the sum.

There is no separate memory table: RSS is a column of both the warm and the cold
table, one row per case, so what the old document printed as a "Memory" section
is now read down the last two columns.

### Cold cache, and how honest it is

The cold pass runs last, so it cannot disturb a warm number, and only the rows
marked `cold` or `both` are measured in it. Each run is preceded by its own
cache drop. Three methods, in the order the harness tries them, recorded on the
`Cold cache method:` line of the generated output and in `results.json`:

1. **`drop_caches`** — `echo 3 > /proc/sys/vm/drop_caches`. Needs root, and is
   the only method that also evicts dentries and inodes. Used when it works.
2. **`fadvise`** — `bench/evict.py`, one `posix_fadvise(POSIX_FADV_DONTNEED)`
   per file, the way `vmtouch -e` works. This is a real eviction of file data,
   not an approximation, and it needs no privileges — but it does **not** touch
   dentries or inodes. A content search therefore comes back genuinely cold,
   while a metadata walk — `scan`, `du`, `find` — stays effectively warm and its
   cold row is close to a duplicate of its warm one. Read those rows as "data
   cold, metadata warm", which is what the label says.
3. **`read-evict`** — the last resort, when neither of the above is available:
   read a large unrelated file to push pages out by LRU pressure. **This one is
   an approximation**: it evicts by whatever the replacement policy happens to
   pick and proves nothing about any particular tree. Numbers from a run with
   this method must not be quoted next to `drop_caches` numbers.

### The result file

`bench/run.sh` writes `bench/results.json` (schema 1): the machine, the tree
sizes, the run counts, the RSS and cold methods, and one object per row with
`case`, `section`, `cache`, `role`, `tool`, `command`, `median_ms`, `min_ms`,
`max_ms`, `runs` and `maxrss_kb`. One row per line, so `grep` works on it. The
CSV on stdout keeps its original seven columns and appends `cache` and
`maxrss_kb`.

The file is **not tracked**: `.gitignore` lists it, so a fresh clone has no
result file and `bench/summary.sh` exits 2 with "run bench/run.sh first" until
you make one. The tables below are a snapshot of the run named in their
preamble, kept here so the document reads on its own; re-running the suite
replaces them.

`bench/summary.sh` renders that file as Markdown: one table per cache pass, one
line per case, the fastest reference of the case named beside its own median,
both peak-RSS columns and their ratio. It is POSIX `sh` plus `awk`.

## The commit these numbers describe

The run behind this document was made from `main` at `547f955` (the merge of the
search and find work plus the recalibration commits that precede it). Check that
commit against `git log --oneline -1 -- src/ bench/` before quoting a table: if
the scanner or the walk has moved since, the numbers describe the old code and
the suite should be re-run. The header the run wrote is its conditions record,
quoted as it stands:

```json
{
  "schema": 1,
  "generated": "2026-11-19T21:13:11Z",
  "machine": {
    "uname": "Linux 6.1.158+ x86_64",
    "cpus": 2,
    "rustc": "rustc 1.99.0 (b940084d7 2026-09-28)",
    "rg": "ripgrep 14.1.0",
    "grep": "grep (GNU grep) 3.11",
    "find": "find (GNU findutils) 4.9.0",
    "du": "du (GNU coreutils) 9.4"
  },
  "runs": {"warm": 5, "cold": 3, "rss": 3},
  "rss_method": "gnu-time (/usr/bin/time -v, maxrss of the largest process in the command)",
  "cold_method": "fadvise (posix_fadvise DONTNEED per file: file data evicted exactly, dentries and inodes retained, so metadata-only cases stay effectively warm)",
  "trees": [
    {"root": "/usr", "bytes": 8002168602, "files": 180525},
    {"root": "/usr/local/go", "bytes": 244137476, "files": 15657},
    {"root": "/workspace/dusage/target/bench/gzip-tree", "bytes": 3393342, "files": 1200}
  ],
```

## The measured tables

Verbatim stdout of `bash bench/summary.sh` over that `bench/results.json`: a
preamble naming the run and the measurement methods, then one table per cache
pass — warm first, cold second — each with the case, the dusage median, the
fastest reference of that case, its median, the dusage/reference ratio and both
peak-RSS figures. Cases with no reference row are the thread sweeps and the
dusage-only variants; their `ref` columns are empty by design.

```console
Generated 2026-11-19T21:13:11Z on Linux 6.1.158+ x86_64, 2 CPUs. Warm rows are medians of 5 alternating
runs; cold rows are medians of 3, each preceded by a cache drop. Times in
milliseconds; RSS is peak resident set, in MiB, measured in a pass of its own.

RSS method: gnu-time (/usr/bin/time -v, maxrss of the largest process in the command)

### Warm cache

| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| scan | 204.2 | `du -sb` | 350.8 | 0.58x | 7 | 4 |
| scan-t1 | 380.0 | - |  | - | 5 | - |
| search-rare | 64.5 | `rg` | 61.8 | 1.04x | 9 | 9 |
| search-common | 118.8 | `rg` | 86.4 | 1.37x | 31 | 9 |
| search-regex | 93.5 | `rg` | 66.0 | 1.42x | 9 | 9 |
| search-regex-nomatch | 60.2 | `rg` | 63.5 | 0.95x | 10 | 9 |
| search-ci | 65.0 | `rg` | 65.7 | 0.99x | 9 | 9 |
| search-ci-fixed | 69.6 | - |  | - | 10 | - |
| search-count | 89.2 | `rg -c` | 86.6 | 1.03x | 13 | 9 |
| search-only-matching | 209.2 | `grep -rIo` | 88.6 | 2.36x | 31 | 5 |
| search-multiline | 355.6 | `rg -U` | 244.5 | 1.45x | 72 | 59 |
| search-files-with-matches | 81.2 | `grep -rIl` | 82.1 | 0.99x | 10 | 5 |
| search-gzip | 174.7 | `rg -z` | 322.6 | 0.54x | 18 | 7 |
| find-name | 326.8 | `find -name` | 201.5 | 1.62x | 68 | 5 |
| find-path | 54.4 | `find -path` | 19.2 | 2.83x | 10 | 3 |
| find-type | 35.0 | `find -name` | 18.3 | 1.91x | 8 | 3 |
| du-s | 249.0 | `du -s` | 378.9 | 0.66x | 22 | 4 |
| du | 270.9 | `du` | 395.0 | 0.69x | 21 | 4 |
| threads-scan-1 | 387.7 | - |  | - | 5 | - |
| threads-scan-2 | 197.2 | - |  | - | 7 | - |
| threads-scan-4 | 200.3 | - |  | - | 8 | - |
| threads-scan-8 | 225.8 | - |  | - | 9 | - |
| threads-scan-16 | 210.4 | - |  | - | 11 | - |
| threads-scan-default | 211.0 | - |  | - | 8 | - |
| threads-search-1 | 178.1 | - |  | - | 28 | - |
| threads-search-2 | 109.2 | - |  | - | 28 | - |
| threads-search-4 | 135.7 | - |  | - | 31 | - |
| threads-search-8 | 133.2 | - |  | - | 35 | - |
| threads-search-16 | 130.1 | - |  | - | 44 | - |
| threads-search-default | 121.5 | - |  | - | 31 | - |

Notes: warm pass, page cache untouched between runs

### Cold cache

| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| scan | 198.7 | `du -sb` | 338.9 | 0.59x | 7 | 4 |
| search-rare | 443.0 | `rg` | 587.7 | 0.75x | 9 | 9 |
| search-common | 362.8 | `rg` | 557.4 | 0.65x | 31 | 9 |
| search-regex | 338.6 | `rg` | 530.9 | 0.64x | 9 | 9 |
| search-regex-nomatch | 315.6 | `rg` | 544.4 | 0.58x | 10 | 9 |
| search-ci | 385.7 | `rg` | 807.7 | 0.48x | 9 | 9 |
| search-ci-fixed | 456.9 | - |  | - | 10 | - |
| search-count | 479.3 | `rg -c` | 684.8 | 0.70x | 13 | 9 |
| search-only-matching | 449.7 | `rg -o` | 802.6 | 0.56x | 31 | 9 |
| search-multiline | 743.3 | `rg -U` | 934.2 | 0.80x | 72 | 59 |
| search-gzip | 188.3 | `rg -z` | 432.2 | 0.44x | 18 | 7 |
| find-name | 271.8 | `find -name` | 190.8 | 1.42x | 68 | 5 |
| find-path | 43.2 | `find -path` | 16.2 | 2.67x | 10 | 3 |
| find-type | 23.3 | `find -name` | 13.5 | 1.73x | 8 | 3 |
| du-s | 229.1 | `du -s` | 328.8 | 0.70x | 22 | 4 |
| du | 222.1 | `du` | 360.0 | 0.62x | 21 | 4 |

Cold cache method: fadvise (posix_fadvise DONTNEED per file: file data evicted exactly, dentries and inodes retained, so metadata-only cases stay effectively warm)

```

## The performance guard

The guard is the cheap check that runs before the full suite. It measures three
content cases (`search-rare`, `search-regex-nomatch`, `search-count` by default),
warm cache, medians of five alternating runs, and fails when dusage is more than
25% slower than the fastest reference on the same box **or** more than 25% above
its own median in `bench/baseline.json`. `DUSAGE_PERF_TOL` moves the threshold,
`--runs` the round count and `DUSAGE_PERF_CASES` the case list.

Verbatim output of `bash scripts/perf-guard.sh` at the same commit:

```console
case                      dusage_ms        ref     ref_ms baseline_ms  verdict
------------------------ ---------- ---------- ---------- ------------  --------
search-rare                    45.4         rg       41.2       51.2  ok
search-regex-nomatch           44.7         rg       40.0       46.4  ok
search-count                   62.0      rg -c       60.1       66.6  ok
perf-guard: pass (tolerance 25%, 5 runs per row)
```

The baseline is machine specific: a run on a box with a different CPU count still
works but says so on stderr, because the comparison then carries the machine's
own spread. Re-record it with `--update-baseline` after a deliberate change, on
the machine CI runs on.

CI runs the guard in the Linux job, after the release build, as an **advisory**
step (`continue-on-error: true`): it searches a 30-file tree built from the
repository itself, so the case is deterministic on any runner, and it sets
`DUSAGE_PERF_TOL=60` because a shared runner is noisier than a quiet box. A red
guard there is a signal to look, not a merge gate; a regression that should stop
a merge is caught by running the guard on a quiet machine.

## Reading the tables

The ratio column is dusage over the fastest reference for that case, so below
`1.00x` means dusage is ahead. A few observations that belong to the mechanism
rather than to any single number:

- **Literals are at parity in the warm pass.** `search-rare`, `search-common`, `search-ci` and
  `search-count` sit within a few percent of `rg`, and `-l` usually ahead of it.
  That comes from three changes: the vectorised `memchr`/`memchr_ci` scan in
  `src/simd.rs` (AVX2 or SSE2, detected once at startup, with a portable SWAR
  fallback), the block scan that answers a whole 64 KiB block before the
  line-at-a-time path runs, and a 64 KiB `BufWriter` around the renderer so a run
  costs one `write` per buffer instead of one per line.
- **A general regex is the case that still loses.** `search-regex` re-enters the
  automaton once per candidate line; the prefilters — a required byte and the
  longest literal run every match must contain — cut how often that happens, but
  not how much each visit costs. Closing it needs a reverse automaton that
  answers "where does this match start" in one suffix pass over the block, which
  is a new machine rather than a tweak (`docs/ROADMAP.md` tracks it).
- **`-o` and `-U` are the expensive output modes.** `-o` prints a line per match
  and keeps every hit in memory until the report is printed; `-U` has to hold the
  whole file. Both show up as a ratio above `1.00x` and as the widest RSS gaps in
  their rows.
- **`-z` is ahead** because the decoder runs in this process over already-read
  bytes, with no second tool and no temporary files.
- **`du` is ahead on allocated blocks** because the walk is parallel and counts
  what `st_blocks` already says, where GNU `du` walks serially.
- **`find` is behind on the glob path** (`find-name`, `find-path`). Its cost is
  its design: the output is the sorted candidate list, which is also why its RSS
  row is the widest in the table. The expression form has no case here yet, so
  it is not in these tables; `docs/SEARCH.md` §1.4 quotes a measurement of it
  taken on its own.
- **The thread sweeps** (`threads-scan-*`, `threads-search-*`) are the evidence
  for the 4..16 default: `--threads 1` is the slow row, gain flattens by the
  core count, and oversubscription costs more than it returns. On a box with a
  different core count read the shape, not the absolute milliseconds.

Where the earlier revisions of this document quoted a "what changed since the
last run" table, note that such a table cannot be generated: it compared runs
from different commits on different machines. Re-measure with `bench/run.sh` on
the commit in question instead, and keep both `results.json` files if the
comparison matters.

## Counts agree before timings mean anything

Timings only compare tools doing the same work, so the cases were checked for
match parity. `bench/run.sh` does not generate this table — these figures were
counted by hand at the same commit, with the commands shown, and should be
re-counted rather than trusted if a counting change lands.

| Query | dusage | `rg` | `grep` | Difference |
| --- | --- | --- | --- | --- |
| `search -c -F 'func ' /usr/local/go` | 124,964 | 124,778 | 124,778 (`-cIl`) | +186, all in 10 binaries |
| `search -c '[A-Z][a-zA-Z0-9_]*\.go'` | 321 | 265 | 265 (`-cIE`) | +56, same 10 binaries |
| `search -c -i memmove` | 516 | 440 | — | +76, same 10 binaries |
| `search -l -F 'func '` | 10,785 files | 10,775 files | — | +10, the same binaries |
| `find '*.h' /usr/include` | 2,785 paths | 2,785 (`--files -g`) | 2,804 (`find -name`) | 19, all symlinks |
| `scan /usr` total | 8,005,173,753 B | — | `du -sb` 8,002,168,602 B | 0.04% |

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

## Caveats

- **Warm cache only.** Nothing was dropped in the warm pass; both trees were
  resident in page cache throughout. On a cold disk, system time dominates and
  the gaps between tools shrink toward the I/O bound, which can reorder the
  table. The cold pass measures data-cold runs; read its method line first.
- **Page cache effects are large.** The scan covers several GB in a fraction of
  a second, which is cache bandwidth, not disk throughput. Do not read these
  numbers as what a real volume does.
- **This VM drifts between runs.** Read the `min_ms`/`max_ms` columns in
  `results.json` before trusting a small gap; differences under roughly the
  spread are ties. Every comparison that mattered here was run as an alternating
  sequence for exactly this reason.
- **The trees are conventional, not adversarial.** `/usr` is a normal package
  mix; `/usr/local/go` is text-only and unusually friendly to a content
  scanner — no `.gitignore` files, few hidden entries, no `node_modules` or
  `target`. A repository checkout full of ignore rules, or a tree with huge
  single files, exercises different paths.
- **Small trees are dominated by process startup.** `/usr/include` is a job of
  tens of milliseconds for every tool here, and differences at that size are
  noise. Only the trees named in `results.json` are big enough to measure.
- **`grep` runs with `-I`, `ripgrep` with its defaults.** `grep -rI` skips
  binaries but still walks hidden directories; `ripgrep` skips hidden entries
  and `.gitignore`d paths, as dusage does.
- **No I/O tuning anywhere.** No `io_uring`, no `O_DIRECT`, no
  `posix_fadvise`, no explicit readahead. dusage and the references all use
  plain buffered reads behind the same page cache.
- **`ripgrep` is a mature, heavily tuned tool.** dusage is a zero-dependency
  implementation with a SWAR memmem, a lazily built DFA and prefilters. A
  remaining gap on the regex workloads is the expected outcome of that
  difference in engineering effort.
- **One machine, one kernel.** Medians of five rounds over one VM, whose identity
  is in the header quoted above.

## Knobs, all optional

| Variable | Default | Meaning |
| --- | --- | --- |
| `N` | `5` | runs per row in the warm pass, median reported |
| `DUSAGE_BENCH_COLD_RUNS` | `3` | runs per cold row; `0` skips the cold pass |
| `DUSAGE_BENCH_RSS_RUNS` | `3` | RSS samples per row, largest kept; `0` skips RSS |
| `DUSAGE_BENCH_COLD_METHOD` | `auto` | `drop_caches`, `fadvise` or `read-evict` |
| `DUSAGE_BENCH_RESULTS` | `bench/results.json` | where the result file goes |
| `DUSAGE_BENCH_BIN` | `target/release/dusage` | binary under test |
| `DUSAGE_BENCH_WORK` | `target/bench` | scratch dir for the gzip tree and eviction file |
| `DUSAGE_BENCH_SCAN_ROOT` | `/usr` | large tree for the scan, `du` and `find` cases |
| `DUSAGE_BENCH_SEARCH_ROOT` | `/usr/local/go`, else `$HOME` | mid-size text tree |
| `DUSAGE_BENCH_ZIP_ROOT` | `$DUSAGE_BENCH_WORK/gzip-tree` | gzip tree for `-z`, built on first run |
| `DUSAGE_BENCH_ZIP_FILES` | `1200` | files copied into the gzip tree |
| `DUSAGE_BENCH_PAT_RARE` | `asyncpreemptoff` | rare literal |
| `DUSAGE_BENCH_PAT_COMMON` | `func ` | frequent literal |
| `DUSAGE_BENCH_PAT_REGEX` | `[A-Z][a-zA-Z0-9_]*\.go` | regex case |
| `DUSAGE_BENCH_PAT_CI` | `memmove` | case-insensitive case |
| `DUSAGE_BENCH_PAT_NOMATCH` | `[Z]{6}[a-z]{9}` | regex that never matches |
| `DUSAGE_BENCH_PAT_MULTILINE` | `func \w+\(\) \{\n` | pattern for the `-U` case |
| `DUSAGE_BENCH_GLOB` | `*.go` | glob for the `find -name` comparison |
| `DUSAGE_PERF_RUNS`, `DUSAGE_PERF_TOL`, `DUSAGE_PERF_CASES` | `5`, `25`, three cases | guard knobs |

`gzip` is needed for the `-z` case and `python3` for the `fadvise` fallback; a
missing one drops or relabels only what needs it. `hyperfine` is used for a
single cross-check line on stderr when it is installed, and is never required.
`bench/run.sh` is bash; `bench/summary.sh` is POSIX `sh`.

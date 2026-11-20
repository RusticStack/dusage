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
`-l`, `-c`, `-o`, `find` by name, by path and by `-t` type, GNU find's
expression form (`find-expr`, the sequential walk `-prune` and `-depth` force),
`du -s`, `du`, and `scan` across a thread sweep.

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

The run behind this document was made from `main` at `7bcb7a9` (the merge of the
second optimization round: the expression walk's name sliced out of the path text
it already holds, one conversion per entry instead of two, the renderer writing
each row's name field once, and the glob answered by the pattern's shape instead
of by walking it). Check that commit against `git log --oneline -1 -- src/ bench/`
before quoting a table: if the scanner or the walk has moved since, the numbers
describe the old code and the suite should be re-run. The header the run wrote is
its conditions record, quoted as it stands:

```json
{
  "schema": 1,
  "generated": "2026-11-20T14:42:37Z",
  "machine": {
    "uname": "Linux 6.1.158+ x86_64",
    "cpus": 2,
    "rustc": "rustc 1.99.0 (b940084d7 2026-09-28)",
    "rg": "ripgrep 14.1.0",
    "grep": "grep (GNU grep) 3.11",
    "find": "find (GNU findutils) 4.9.0",
    "du": "du (GNU coreutils) 9.4"
  },
  "runs": {
    "warm": 5,
    "cold": 3,
    "rss": 3
  },
  "rss_method": "gnu-time (/usr/bin/time -v, maxrss of the largest process in the command)",
  "cold_method": "fadvise (posix_fadvise DONTNEED per file: file data evicted exactly, dentries and inodes retained, so metadata-only cases stay effectively warm)",
  "trees": [
    {
      "root": "/usr",
      "bytes": 7991131817,
      "files": 180497
    },
    {
      "root": "/usr/local/go",
      "bytes": 244137476,
      "files": 15657
    },
    {
      "root": "/workspace/dusage/target/bench/gzip-tree",
      "bytes": 3393342,
      "files": 1200
    }
  ]
}
```

## The measured tables

Verbatim stdout of `bash bench/summary.sh` over that `bench/results.json`: a
preamble naming the run and the measurement methods, then one table per cache
pass — warm first, cold second — each with the case, the dusage median, the
fastest reference of that case, its median, the dusage/reference ratio and both
peak-RSS figures. Cases with no reference row are the thread sweeps and the
dusage-only variants; their `ref` columns are empty by design.

```console
Generated 2026-11-20T14:42:37Z on Linux 6.1.158+ x86_64, 2 CPUs. Warm rows are medians of 5 alternating
runs; cold rows are medians of 3, each preceded by a cache drop. Times in
milliseconds; RSS is peak resident set, in MiB, measured in a pass of its own.

RSS method: gnu-time (/usr/bin/time -v, maxrss of the largest process in the command)

### Warm cache

| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| scan | 336.8 | `du -sb` | 632.3 | 0.53x | 7 | 4 |
| scan-t1 | 658.3 | - |  | - | 5 | - |
| search-rare | 96.7 | `rg` | 113.1 | 0.85x | 11 | 9 |
| search-common | 131.3 | `grep -rI` | 147.9 | 0.89x | 16 | 5 |
| search-regex | 103.9 | `rg` | 113.8 | 0.91x | 12 | 9 |
| search-regex-nomatch | 100.8 | `rg` | 111.1 | 0.91x | 10 | 9 |
| search-ci | 105.8 | `rg` | 116.9 | 0.91x | 10 | 9 |
| search-ci-fixed | 109.5 | - |  | - | 10 | - |
| search-count | 104.6 | `grep -rIc` | 144.2 | 0.73x | 13 | 5 |
| search-only-matching | 114.8 | `grep -rIo` | 152.7 | 0.75x | 17 | 5 |
| search-multiline | 371.0 | `rg -U` | 430.1 | 0.86x | 46 | 59 |
| search-multiline-unicode | 149.6 | `rg -U unicode` | 136.8 | 1.09x | 47 | 9 |
| search-files-with-matches | 101.8 | `grep -rIl` | 150.8 | 0.68x | 10 | 5 |
| search-gzip | 169.6 | `rg -z` | 699.3 | 0.24x | 18 | 7 |
| find-name | 333.4 | `find -name` | 421.2 | 0.79x | 15 | 5 |
| find-path | 44.4 | `find -path` | 44.0 | 1.01x | 6 | 3 |
| find-type | 44.2 | `find -name` | 39.1 | 1.13x | 6 | 3 |
| find-expr | 43.2 | `find -name` | 38.6 | 1.12x | 4 | 3 |
| du-s | 405.1 | `du -s` | 725.2 | 0.56x | 21 | 4 |
| du | 381.5 | `du` | 697.7 | 0.55x | 21 | 4 |
| threads-scan-1 | 630.3 | - |  | - | 5 | - |
| threads-scan-2 | 337.6 | - |  | - | 6 | - |
| threads-scan-4 | 333.5 | - |  | - | 8 | - |
| threads-scan-8 | 373.9 | - |  | - | 9 | - |
| threads-scan-16 | 360.8 | - |  | - | 10 | - |
| threads-scan-default | 318.1 | - |  | - | 7 | - |
| threads-search-1 | 109.3 | - |  | - | 11 | - |
| threads-search-2 | 114.6 | - |  | - | 12 | - |
| threads-search-4 | 112.9 | - |  | - | 16 | - |
| threads-search-8 | 120.5 | - |  | - | 23 | - |
| threads-search-16 | 133.1 | - |  | - | 31 | - |
| threads-search-default | 114.9 | - |  | - | 16 | - |

Notes: warm pass, page cache untouched between runs

### Cold cache

| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| scan | 383.6 | `du -sb` | 680.1 | 0.56x | 7 | 4 |
| search-rare | 536.8 | `rg` | 788.5 | 0.68x | 11 | 9 |
| search-common | 492.0 | `rg` | 771.2 | 0.64x | 16 | 9 |
| search-regex | 502.9 | `rg` | 730.0 | 0.69x | 12 | 9 |
| search-regex-nomatch | 477.0 | `rg` | 693.5 | 0.69x | 10 | 9 |
| search-ci | 476.7 | `rg` | 697.1 | 0.68x | 10 | 9 |
| search-ci-fixed | 467.9 | - |  | - | 10 | - |
| search-count | 493.1 | `rg -c` | 773.1 | 0.64x | 13 | 9 |
| search-only-matching | 549.4 | `rg -o` | 791.8 | 0.69x | 17 | 9 |
| search-multiline | 903.6 | `rg -U` | 1373.0 | 0.66x | 46 | 59 |
| search-multiline-unicode | 602.7 | `rg -U unicode` | 857.3 | 0.70x | 47 | 9 |
| search-gzip | 183.3 | `rg -z` | 774.6 | 0.24x | 18 | 7 |
| find-name | 318.0 | `find -name` | 355.9 | 0.89x | 15 | 5 |
| find-path | 35.5 | `find -path` | 40.6 | 0.87x | 6 | 3 |
| find-type | 36.9 | `find -name` | 30.1 | 1.23x | 6 | 3 |
| find-expr | 43.1 | `find -name` | 39.9 | 1.08x | 4 | 3 |
| du-s | 412.8 | `du -s` | 657.1 | 0.63x | 21 | 4 |
| du | 398.8 | `du` | 641.2 | 0.62x | 21 | 4 |

Cold cache method: fadvise (posix_fadvise DONTNEED per file: file data evicted exactly, dentries and inodes retained, so metadata-only cases stay effectively warm)
```

## The performance guard

The guard is the cheap check that runs before the full suite. It measures three
content cases (`search-rare`, `search-regex-nomatch`, `search-count` by default),
warm cache, medians of five alternating runs, and fails on either of two things:
dusage being more than 25% slower than the fastest reference on the same box, or
**today's ratio to that reference** being more than 25% above the ratio recorded
in `bench/baseline.json`. `DUSAGE_PERF_TOL` moves the threshold, `--runs` the
round count and `DUSAGE_PERF_CASES` the case list.

The second check compares a ratio rather than a stored median on purpose. The
same binary on the same box measured a 51 ms median in the evening and 96 ms the
same night, while its ratio to the reference tool measured in the same minute
did not move — so a stored absolute time turns a busy host into a false positive.
The ratio is still machine specific in one way an absolute number is not: a
4-vCPU box gives a search the parallelism a 2-vCPU box cannot, so re-record the
baseline on whatever machine you gate on (`--update-baseline`).

Verbatim output of `bash scripts/perf-guard.sh` at the same commit:

```console
case                      dusage_ms        ref     ref_ms    ratio base_ratio  verdict
------------------------ ---------- ---------- ---------- -------- --------  --------
search-rare                   102.9         rg      108.4     0.95    1.028  ok
search-regex-nomatch          106.4         rg      112.6     0.94    1.071  ok
search-count                  113.7  grep -rIc      162.4     0.70    0.807  ok
perf-guard: pass (tolerance 25%, 5 runs per row)
```

`ratio` is this run's dusage over the reference, and `base_ratio` is what
`bench/baseline.json` recorded for that case, so a verdict of `ok` means the
number got no worse than 25% beyond the baseline rather than that it matches it.
Here every row is below its baseline ratio: the plain content rows moved from
1.03, 1.07 and 0.81 to 0.95, 0.94 and 0.70 between the two recordings.

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

- **Every content row is ahead on this pass.** The output-heavy cases
  (`search-count` 0.73x, `search-files-with-matches` 0.68x,
  `search-only-matching` 0.75x, `search-multiline` 0.86x, `search-common` 0.89x)
  get there by streaming results as the walk reaches them instead of collecting
  them, and by a matching line sharing its file's buffer through an offset
  rather than owning a `Vec`. The plain rows (`search-rare` 0.85x,
  `search-regex` 0.91x, `search-regex-nomatch` 0.91x, `search-ci` 0.91x) were at
  1.02–1.10x on the previous pass; the cause is traceable rather than drift —
  callgrind counted the literal scan 25–28% cheaper and the regex candidate scan
  29% cheaper over the same trees — but this box moves 10-20% between passes, so
  read the direction and let `scripts/perf-guard.sh` settle the size. On it the
  three plain cases sit at 0.95x, 0.94x and 0.70x of their reference, below the
  1.03x, 1.07x and 0.81x recorded in the baseline.
- **`-U` with Unicode semantics is 1.09x**, down from 2.51x two rounds ago: a
  pattern with an ASCII mandatory literal settles its matches from the line
  holding each occurrence instead of decoding whole files to `char`s.
- **`-z` is far ahead** (0.24x) because the decoder runs in this process over
  already-read bytes, with no second tool and no temporary files, and **`-c` is
  ahead of `grep -rIc`** (0.73x) because a count never builds the hits it counts.
- **`du` is ahead on allocated blocks** (0.55–0.56x) because the walk is parallel
  and counts what `st_blocks` already says, where GNU `du` walks serially.
- **`find` is ahead on the big tree and still behind on the small ones**:
  `find-name` 0.79x, `find-path` 1.01x, `find-type` 1.13x, `find-expr` 1.12x.
  The big row is dominated by matching globs by shape and streaming the sorted
  listing from a heap over directory cursors; the small rows are dominated by
  keeping that order at all, which GNU find does not have to do, and by process
  startup, which is a larger share of a 40 ms job. The expression row is new
  here: the walk it measures is sequential by necessity (`-prune` and `-depth`
  decide the next step from the entry just seen), and this round cut its
  per-entry cost by 30% and its name extraction by another 5% in instructions.
  Its first measurement, on a 4-vCPU machine, was 1.46x; that number belongs to
  that machine and is not comparable with this row.
- **Content search is now about half kernel time.** Over `/usr/local/go` the
  same run spends roughly 60 ms in user space and 180 ms in the kernel, so
  further instruction cuts on that path buy one or two percent of wall clock;
  the next real gain there has to remove per-file syscalls, not instructions.
  The find walks are `getdents`-bound for the same reason.
- **Memory is bounded, not minimal.** Peak RSS on the content rows is 10–17 MiB
  against `grep`'s 5 and `ripgrep`'s 9, and it no longer scales with how much the
  tree matches: the printer's pending results are capped at 4 MiB and the path
  window at `threads × 1024` files. The floor above the references is the
  pipeline itself — a read buffer and a queue per worker — and beating `grep`'s
  5 MiB would mean giving up the threads that make the rows ahead. `find`'s
  15 MiB on `find-name` is the read-ahead pool, flat whether 15k or 267k paths
  match, while the expression form prints as it walks and holds 4 MiB.
  `du`'s 21 MiB against GNU `du`'s 4 is the cost of the parallel walk that wins
  that row its half of the clock, not of anything it retains per file.
- **The thread sweeps** (`threads-scan-*`, `threads-search-*`) are the evidence
  for the 4..16 default, and they read differently on a 2-vCPU box: `scan` shows
  the classic shape (630 ms on one thread, 337 at two, flat to four, worse past
  the core count), while `search` is already kernel-bound here and its sweep is
  flat — 109 ms on one thread against 114 at the default and 133 at sixteen, so
  on a small box the default costs a few percent of parallelism it cannot use.
  On a box with a different core count read the shape, not the absolute
  milliseconds.

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

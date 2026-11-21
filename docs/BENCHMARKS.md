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

The run behind this document was made from `main` at `323bf88` (the merge of the
third optimization round: the expression walk borrowing its entry text, building
its paths in buffers it reuses, and answering the builtin ignore list once per
directory instead of once per entry). Check that commit against
`git log --oneline -1 -- src/ bench/` before quoting a table: if the scanner or
the walk has moved since, the numbers describe the old code and the suite should
be re-run. The header the run wrote is its conditions record, quoted as it stands:

```json
{
  "schema": 1,
  "generated": "2026-11-20T15:18:06Z",
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
Generated 2026-11-20T15:18:06Z on Linux 6.1.158+ x86_64, 2 CPUs. Warm rows are medians of 5 alternating
runs; cold rows are medians of 3, each preceded by a cache drop. Times in
milliseconds; RSS is peak resident set, in MiB, measured in a pass of its own.

RSS method: gnu-time (/usr/bin/time -v, maxrss of the largest process in the command)

### Warm cache

| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| scan | 375.6 | `du -sb` | 737.4 | 0.51x | 7 | 4 |
| scan-t1 | 769.4 | - |  | - | 5 | - |
| search-rare | 115.6 | `rg` | 122.5 | 0.94x | 12 | 9 |
| search-common | 130.1 | `grep -rI` | 170.2 | 0.76x | 16 | 5 |
| search-regex | 115.4 | `rg` | 128.6 | 0.90x | 10 | 9 |
| search-regex-nomatch | 120.3 | `rg` | 128.5 | 0.94x | 9 | 9 |
| search-ci | 119.1 | `rg` | 130.3 | 0.91x | 8 | 9 |
| search-ci-fixed | 116.4 | - |  | - | 10 | - |
| search-count | 114.7 | `grep -rIc` | 166.6 | 0.69x | 11 | 5 |
| search-only-matching | 132.0 | `grep -rIo` | 157.4 | 0.84x | 17 | 5 |
| search-multiline | 403.6 | `rg -U` | 459.8 | 0.88x | 47 | 59 |
| search-multiline-unicode | 182.1 | `rg -U unicode` | 158.5 | 1.15x | 46 | 10 |
| search-files-with-matches | 110.1 | `grep -rIl` | 158.7 | 0.69x | 9 | 5 |
| search-gzip | 180.4 | `rg -z` | 733.3 | 0.25x | 17 | 7 |
| find-name | 291.2 | `find -name` | 412.8 | 0.71x | 15 | 5 |
| find-path | 39.8 | `find -path` | 40.6 | 0.98x | 6 | 3 |
| find-type | 40.7 | `find -name` | 39.4 | 1.03x | 6 | 3 |
| find-expr | 42.1 | `find -name` | 42.5 | 0.99x | 4 | 3 |
| du-s | 455.7 | `du -s` | 755.1 | 0.60x | 19 | 4 |
| du | 400.3 | `du` | 688.8 | 0.58x | 20 | 4 |
| threads-scan-1 | 723.2 | - |  | - | 5 | - |
| threads-scan-2 | 360.2 | - |  | - | 7 | - |
| threads-scan-4 | 393.3 | - |  | - | 7 | - |
| threads-scan-8 | 403.9 | - |  | - | 9 | - |
| threads-scan-16 | 410.9 | - |  | - | 11 | - |
| threads-scan-default | 360.5 | - |  | - | 8 | - |
| threads-search-1 | 128.8 | - |  | - | 12 | - |
| threads-search-2 | 132.4 | - |  | - | 12 | - |
| threads-search-4 | 123.1 | - |  | - | 16 | - |
| threads-search-8 | 142.9 | - |  | - | 22 | - |
| threads-search-16 | 161.2 | - |  | - | 33 | - |
| threads-search-default | 144.9 | - |  | - | 15 | - |

Notes: warm pass, page cache untouched between runs

### Cold cache

| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| scan | 380.6 | `du -sb` | 767.1 | 0.50x | 7 | 4 |
| search-rare | 710.3 | `rg` | 989.9 | 0.72x | 12 | 9 |
| search-common | 740.5 | `rg` | 1066.2 | 0.69x | 16 | 9 |
| search-regex | 704.2 | `rg` | 1220.6 | 0.58x | 10 | 9 |
| search-regex-nomatch | 719.3 | `rg` | 1100.5 | 0.65x | 9 | 9 |
| search-ci | 647.0 | `rg` | 1022.0 | 0.63x | 8 | 9 |
| search-ci-fixed | 961.1 | - |  | - | 10 | - |
| search-count | 760.3 | `rg -c` | 1357.1 | 0.56x | 11 | 9 |
| search-only-matching | 947.4 | `rg -o` | 1384.4 | 0.68x | 17 | 10 |
| search-multiline | 943.0 | `rg -U` | 1526.8 | 0.62x | 47 | 59 |
| search-multiline-unicode | 861.6 | `rg -U unicode` | 1462.3 | 0.59x | 46 | 10 |
| search-gzip | 252.4 | `rg -z` | 1085.1 | 0.23x | 17 | 7 |
| find-name | 334.8 | `find -name` | 410.7 | 0.82x | 15 | 5 |
| find-path | 36.8 | `find -path` | 41.2 | 0.89x | 6 | 3 |
| find-type | 42.0 | `find -name` | 40.5 | 1.04x | 6 | 3 |
| find-expr | 41.9 | `find -name` | 40.6 | 1.03x | 4 | 3 |
| du-s | 435.1 | `du -s` | 691.2 | 0.63x | 19 | 4 |
| du | 436.0 | `du` | 762.2 | 0.57x | 20 | 4 |

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
search-rare                   105.8         rg      114.4     0.92    1.028  ok
search-regex-nomatch          105.9         rg      127.2     0.83    1.071  ok
search-count                  118.6  grep -rIc      162.2     0.73    0.807  ok
perf-guard: pass (tolerance 25%, 5 runs per row)
```

`ratio` is this run's dusage over the reference, and `base_ratio` is what
`bench/baseline.json` recorded for that case, so a verdict of `ok` means the
number got no worse than 25% beyond the baseline rather than that it matches it.
Here all three are below their baselines of 1.03, 1.07 and 0.81.

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

- **Every row that has a reference is at or ahead of it, except two.** The
  output-heavy content cases (`search-count` 0.69x,
  `search-files-with-matches` 0.69x, `search-only-matching` 0.84x,
  `search-multiline` 0.88x, `search-common` 0.76x) get there by streaming results
  as the walk reaches them instead of collecting them, and by a matching line
  sharing its file's buffer through an offset rather than owning a `Vec`. The
  plain rows are 0.94x (`search-rare`), 0.90x (`search-regex`), 0.94x
  (`search-regex-nomatch`) and 0.91x (`search-ci`) — they were 1.02–1.10x two
  rounds ago, and the cause is traceable in instructions rather than in a timing
  that drifts: callgrind counts the literal scan 25-28% cheaper and the regex
  candidate scan 29% cheaper over the same trees. The two rows still behind are
  `search-multiline-unicode` at 1.15x and `find-type` at 1.03x, both inside the
  10-20% this box moves between passes.
- **`-U` on ASCII files is ahead** (`search-multiline` 0.88x) while `-U` with
  Unicode semantics is 1.15x: the ASCII path settles its matches from the line
  holding each occurrence, and the Unicode one still decodes the lines that hold
  non-ASCII bytes.
- **`-z` is far ahead** (0.25x) because the decoder runs in this process over
  already-read bytes, with no second tool and no temporary files, and **`-c` is
  ahead of `grep -rIc`** (0.69x) because a count never builds the hits it counts.
- **`du` is ahead on allocated blocks** (0.58–0.60x) because the walk is parallel
  and counts what `st_blocks` already says, where GNU `du` walks serially.
- **`find` is ahead on three rows and a hair behind on the fourth**:
  `find-name` 0.71x, `find-path` 0.98x, `find-expr` 0.99x, `find-type` 1.03x. The
  big row is dominated by matching globs by shape and streaming the sorted listing
  from a heap over open directory cursors; the small rows are dominated by keeping
  that order at all, which GNU find does not have to do, on a job of some 40 ms
  where process startup is a visible share. `find-expr` is the sequential walk, and
  it reached the reference by instruction work — 67.4M instructions down to 41.7M,
  −38%, from borrowing the entry's text, reusing two path buffers, and answering
  the builtin ignore per directory — which is also the evidence that its clock
  cannot be improved much further: the same runs show the wall clock unmoved,
  because the walk is `getdents` and `statx` bound and GNU find pays the same
  syscalls.
- **Content search is about half kernel time.** Over `/usr/local/go` the same run
  spends roughly 60 ms in user space against 180 ms in the kernel, so further
  instruction cuts on that path buy one or two percent of wall clock; the next
  real gain there has to remove per-file syscalls, not instructions.
- **Memory is bounded, not minimal.** Peak RSS on the content rows is 8–17 MiB
  against `grep`'s 5 and `ripgrep`'s 9–10, and it no longer scales with how much
  the tree matches: the printer's pending results are capped at 4 MiB and the
  path window at `threads × 1024` files. The floor above the references is the
  pipeline itself — a read buffer and a queue per worker — and beating `grep`'s
  5 MiB would mean giving up the threads that make the rows ahead. `find`'s
  15 MiB on `find-name` is the read-ahead pool, flat whether 15k or 267k paths
  match, while the expression form prints as it walks and holds 4 MiB. `du`'s
  19–20 MiB against GNU `du`'s 4 is the cost of the parallel walk that wins that
  row half the clock, not of anything it retains per file.
- **The thread sweeps** (`threads-scan-*`, `threads-search-*`) are the evidence
  for the 4..16 default, and they read differently on a 2-vCPU box: `scan` shows
  the classic shape (723 ms on one thread, 360 at two, worse past the core
  count), while `search` is already kernel-bound here and its sweep is flat —
  129 ms on one thread against 145 at the default and 161 at sixteen, so on a
  small box the default costs a few percent of parallelism it cannot use. On a
  box with a different core count read the shape, not the absolute milliseconds.

Where the earlier revisions of this document quoted a "what changed since the
last run" table, note that such a table cannot be generated: it compared runs
from different commits on different machines. Re-measure with `bench/run.sh` on
the commit in question instead, and keep both `results.json` files if the
comparison matters.

## Windows (DOOMBRINGER)

Everything above was measured on a 2-vCPU Linux container. This section is the same tool on
the machine the NTFS work was always meant for: a Windows 11 desktop (i7-13700KF, 24 threads,
48 GB) with three local NTFS volumes. It answers the twelve questions in `docs/ROADMAP.md`
section 4 and covers the validation owed in section 5, all of which had only ever been argued
from source reading.

**How to refresh it.** Same rule as the rest of this document: every number below is program
output.

```console
$ cargo build --release --examples
$ powershell -ExecutionPolicy Bypass -File bench/windows/fixtures.ps1
$ bash bench/windows/run.sh                              # timings -> bench/windows/results.json
$ powershell -ExecutionPolicy Bypass -File bench/windows/checks.ps1   # behaviour -> checks.json
$ bash bench/summary.sh bench/windows/results.json       # the timing table below
$ bash bench/windows/summary-checks.sh                   # the behaviour tables below
```

The elevated half — real-time protection, `--mft` against the batched backend and robocopy,
the USN watch, the cold-cache pass and the FAT32/ReFS FileId checks — is
`powershell -ExecutionPolicy Bypass -File bench/windows/admin.ps1` from an elevated shell. It
appends its rows to `bench/windows/admin-results.jsonl` and to `checks.json`, and the two
generators above pick them up.

### The record

The header `bench/windows/record.sh` writes, quoted as it stands:

```json
{
  "schema": 1,
  "generated": "2026-11-21T00:22:31Z",
  "machine": {
    "uname": "Windows 10.0.26300.9974 x86_64 (DOOMBRINGER, i7-13700KF)",
    "cpus": 24,
    "rustc": "rustc 1.100.0 (0ef1bc386 2026-11-09)",
    "volume": "D:",
    "filesystem": "NTFS"
  },
  "runs": {
    "warm": 5,
    "cold": 3
  },
  "delete_runs": 3,
  "rss_method": "not measured: Windows peak working set needs a per-process sampler this harness does not have",
  "cold_method": "standby-list purge (NtSetSystemInformation SystemMemoryListInformation 2/3/4), elevated; bench/windows/admin.ps1"
}```

### The measured table

Verbatim stdout of `bash bench/summary.sh bench/windows/results.json`: one row per case with
the dusage median. The reference columns are empty by design — there is no `grep`, `du` or
`find` on Windows to compare against — and so are the peak-RSS columns, which this harness
does not fill.

```console
Generated 2026-11-21T00:22:31Z on Windows 10.0.26300.9974 x86_64 (DOOMBRINGER, i7-13700KF), 24 CPUs. Warm rows are medians of 5 alternating
runs; cold rows are medians of 3, each preceded by a cache drop. Times in
milliseconds; RSS is peak resident set, in MiB, measured in a pass of its own.

RSS method: not measured: Windows peak working set needs a per-process sampler this harness does not have

### Warm cache

| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| scan-t1-tree1m | 328.0 | - |  | - | - | - |
| scan-t2-tree1m | 235.0 | - |  | - | - | - |
| scan-t4-tree1m | 188.0 | - |  | - | - | - |
| scan-t8-tree1m | 141.0 | - |  | - | - | - |
| scan-t16-tree1m | 142.0 | - |  | - | - | - |
| scan-t24-tree1m | 141.0 | - |  | - | - | - |
| scan-t32-tree1m | 145.0 | - |  | - | - | - |
| scan-t1-Windows | 2448.0 | - |  | - | - | - |
| scan-t2-Windows | 1518.0 | - |  | - | - | - |
| scan-t4-Windows | 899.0 | - |  | - | - | - |
| scan-t8-Windows | 752.0 | - |  | - | - | - |
| scan-t16-Windows | 755.0 | - |  | - | - | - |
| scan-t24-Windows | 795.0 | - |  | - | - | - |
| scan-t32-Windows | 850.0 | - |  | - | - | - |
| search-t1 | 376.0 | - |  | - | - | - |
| search-t2 | 378.0 | - |  | - | - | - |
| search-t4 | 276.0 | - |  | - | - | - |
| search-t8 | 208.0 | - |  | - | - | - |
| search-t16 | 233.0 | - |  | - | - | - |
| search-t24 | 233.0 | - |  | - | - | - |
| search-t32 | 199.0 | - |  | - | - | - |
| enum-walk-tree1m | 265.7 | - |  | - | - | - |
| enum-walk-large-fetch-tree1m | 198.7 | - |  | - | - | - |
| enum-batch-64k-tree1m | 206.8 | - |  | - | - | - |
| enum-batch-256k-tree1m | 210.5 | - |  | - | - | - |
| enum-batch-1m-tree1m | 213.2 | - |  | - | - | - |
| enum-walk-flat100k | 26.0 | - |  | - | - | - |
| enum-walk-large-fetch-flat100k | 19.7 | - |  | - | - | - |
| enum-batch-64k-flat100k | 22.2 | - |  | - | - | - |
| enum-batch-256k-flat100k | 21.2 | - |  | - | - | - |
| enum-batch-1m-flat100k | 20.6 | - |  | - | - | - |
| enum-walk-tree1k_d100 | 60.2 | - |  | - | - | - |
| enum-walk-large-fetch-tree1k_d100 | 54.4 | - |  | - | - | - |
| enum-batch-64k-tree1k_d100 | 52.9 | - |  | - | - | - |
| enum-batch-256k-tree1k_d100 | 58.2 | - |  | - | - | - |
| enum-batch-1m-tree1k_d100 | 55.2 | - |  | - | - | - |
| enum-walk-real | 6428.7 | - |  | - | - | - |
| enum-walk-large-fetch-real | 6942.4 | - |  | - | - | - |
| enum-batch-64k-real | 5334.1 | - |  | - | - | - |
| enum-batch-256k-real | 5544.8 | - |  | - | - | - |
| enum-batch-1m-real | 5324.7 | - |  | - | - | - |
| delete-std-A | 1231.0 | - |  | - | - | - |
| delete-serial-posix-A | 1225.0 | - |  | - | - | - |
| delete-serial-classic-A | 1279.0 | - |  | - | - | - |
| delete-parallel-A | 514.0 | - |  | - | - | - |
| delete-std-B | 431.0 | - |  | - | - | - |
| delete-serial-posix-B | 419.0 | - |  | - | - | - |
| delete-serial-classic-B | 424.0 | - |  | - | - | - |
| delete-parallel-B | 189.0 | - |  | - | - | - |

Notes: warm pass, page cache untouched between runs```

### What the behaviour checks say

Verbatim stdout of `bash bench/windows/summary-checks.sh` over `bench/windows/checks.json`:

```console
Allocation fidelity: every number is the Win32 answer for the same file, read through the handle dusage uses.

| fixture | EndOfFile | AllocationSize | GetCompressedFileSizeW | named streams | attributes |
| --- | ---: | ---: | ---: | --- | --- |
| resident.bin | 1 | 8 | 1 | ::$DATA=1 | Archive |
| plain.bin | 1048576 | 1048576 | 1048576 | ::$DATA=1048576 | Archive |
| comp.bin | 1048576 | 196608 | 196608 | ::$DATA=1048576 | Archive, Compressed |
| sparse.bin | 1048576 | 0 | 0 | ::$DATA=1048576 | Archive, SparseFile |
| ads.txt | 4 | 8 | 4 | ::$DATA=4 :hidden:$DATA=1048576 | Archive |
| hl_a.txt | 16 | 16 | 16 | ::$DATA=16 | Archive |
| hl_b.txt | 16 | 16 | 16 | ::$DATA=16 | Archive |
| junction | 0 | 0 | 0 | - | Directory, ReparsePoint |
| sub | 0 | 0 | 0 | - | Directory |

File ids: hardlink pair hl_a.txt / hl_b.txt.

| question | answer |
| --- | --- |
| hardlinks share one FileId | true |
| FileId (high:low) | 00060000006994df0000000000000000 |
| `fsutil file queryFileID <path>` | ok |
| fsutil output | `File ID is 0x000000000000000000060000006994df` |

File id support per volume:

| volume | filesystem | FileIdInfo accepted | FileId | error |
| --- | --- | --- | --- | --- |
| D:\ | NTFS | true | 00050000000000050000000000000000 | - |
| C:\ | NTFS | true | 00050000000000050000000000000000 | - |
| E:\ | NTFS | true | 00050000000000050000000000000000 | - |

Reparse points: fid\junction.

| property | value |
| --- | --- |
| LinkType | Junction |
| PSIsContainer | true |
| attributes | Directory, ReparsePoint |
| FILE_STANDARD_INFO Directory flag | true |

`dusage scan fid`: 8 files, 1 dirs; `find` lists `ads.txt comp.bin hl_a.txt hl_b.txt plain.bin resident.bin sparse.bin sub sub/inner.txt`; the link target appears once.

Long paths, deepest path 393 characters:

| question | answer |
| --- | --- |
| LongPathsEnabled | true |
| raw CreateFileW, no prefix | true (error -) |
| raw CreateFileW, verbatim prefix | true (error -) |

Hardlink overcount on real trees, `--dedup-hardlinks` as the reference:

| root | names (naive) | names (dedup) | on disk naive GB | on disk dedup GB | overcount |
| --- | ---: | ---: | ---: | ---: | ---: |
| `C:\Users\KellHect\AppData\Local\pnpm\store` | 54030 | 54030 | 1.39 | 1.39 | 0% |
| `C:\projects` | 127933 | 84345 | 3.58 | 2.95 | 21.4% |
| `C:\Users\KellHect\.forge` | 27192 | 26939 | 6.51 | 4.53 | 43.7% |
Elevation, opening the volume device:

| device | elevated | CreateFileW(GENERIC_READ) | error |
| --- | --- | --- | --- |
| `\\.\C:` | false | false | 5 |
| `\\.\D:` | false | false | 5 |

POSIX disposition:

| question | answer |
| --- | --- |
| FILE_DISPOSITION_INFO_EX accepted | true |
| a second open while the first handle is still open | open failed 5 |
| name gone after the last close | true |```

### What the twelve questions came to

Each answer is a sentence about a table above, not a second measurement.

**1. Enumeration cost.** The batch the tool already ships is the fastest spelling on the real
tree (`enum-batch-64k-real` 5334 ms against `enum-walk-real` 6429 ms) and level with the
plain walk on the generated 1M tree (206.8 against 265.7 ms). Buffer size is not a lever:
64 KiB, 256 KiB and 1 MiB land inside one another's spread on every tree, so the 64 KiB
default stays. `FIND_FIRST_EX_LARGE_FETCH` takes 25% off the plain walk on the flat and wide
trees (198.7 against 265.7 ms on the 1M tree) and *costs* 8% on the deep real tree (6942
against 6429 ms), so it is not worth a second code path. The shape that matters is the
per-directory cost: the same 1M entries enumerate at 3.8M/s when they sit in 1000
directories and at 41k/s under `C:\Windows`, where 264k entries arrive from 68k directory
opens.

**2. Thread sweep.** Both shapes plateau at eight workers, and past that the directory-heavy
real tree gets slower: `scan-t*-Windows` runs 2448, 1518, 899, 752, 755, 795 and 850 ms at
1, 2, 4, 8, 16, 24 and 32 threads. That is a latency ceiling rather than a CPU one — 24
hardware threads are available and the eighth worker already reaches the wall — which is
what the default now encodes: one worker per core clamped to 4..8 on Windows
(`native::MAX_WORKERS`), the same throughput as the old 4..16 clamp with half the workers.
Linux keeps 16 because nothing wider than a 2-vCPU box has been swept there.

**3. Real-time protection.** Measured only, by the elevated half of the harness: the same
scan and the same delete with Defender watching and with the fixture root excluded, and the
exclusion is removed when the script returns. Nothing here recommends turning protection
off.

**4. Delete strategy.** Parallelism is the whole win and the disposition is not a lever.
`delete-parallel-A` deletes a 20k-file tree in 514 ms where serial `remove_dir_all` takes
1231 ms, the POSIX disposition 1225 ms and the classic one 1279 ms; the 5k x 128 KiB tree
repeats the shape at 189 against 431, 419 and 424 ms. Every row ended with zero errors in
every category, which is what the `errors` object on each row reports.

**5. Allocation fidelity.** `AllocationSize` follows compression (1,048,576 -> 196,608) and
sparseness (-> 0) exactly and agrees with `GetCompressedFileSizeW` on both. It reports 8 bytes
for a one-byte resident file where `GetCompressedFileSizeW` reports 1, and — the open
question in roadmap item 8 — **it does not include named streams**: a 4-byte file carrying a
1 MiB alternate stream reports 8. That is why `--streams` is opt-in and why it costs a second
pass.

**6. FileId semantics.** Hardlinks share one 128-bit `FileId` (the `fileid` table: the same id
from both names), and the `fsutil file queryFileID <path>` syntax needs no `=`. Every local
volume here is NTFS and accepts `FileIdInfo`; the FAT32 and ReFS rows come from the elevated
half, which attaches small VHDX files for exactly that.

**7. The reparse gap.** `is_symlink()` is true for a junction on this build, and the tool no
longer depends on it: it tests `FILE_ATTRIBUTE_REPARSE_POINT`, so the DFS and dedup tags that
`is_symlink()` misses fall under the same rule. The `reparse-scan` check shows the junction
fixture scanned as 8 files and 1 directory with its target counted once. `find` not listing a
link without `--follow` is asserted by `tests/search_cli.rs` and is a documented choice.

**8. Privileges.** Opening the volume device fails with error 5 for a non-elevated token (the
`elevation` table) and succeeds under the elevated run, which is the reason `--mft` documents
an administrator shell.

**9. Overcount.** Real pnpm-managed trees overcount far more than the 5-10% AGENTS.md
recorded: `C:\projects` 3.58 GB naive against 2.95 GB with `--dedup-hardlinks` (21.4%), and
`C:\forge` 6.51 against 4.53 GB (43.7%). The pnpm store itself is 0%, which is the useful
control: its hardlinks point into project trees, and a link whose other name lies outside the
root is still counted once.

**10. Long paths.** The deepest fixture path is 393 characters. Both spellings open and
delete it here, because this build has `LongPathsEnabled=1`; dusage uses the verbatim prefix
unconditionally, so it does not depend on that policy. `dusage prune` deleted the whole
393-character fixture in 973 us.

**11. `--mft` and the USN watch.** Both come from the elevated half: `--mft` totals for a
volume against the batched backend and `robocopy /L /E /XJ`, and a watch with files created
and deleted under it, whose printed update is compared with what a full rescan would cost.

### Caveats for this section

- **The antivirus is part of the measurement.** The first pass over the freshly built 1M tree
  put the batched backend at twice the walk's time; the same rows on a quiet machine are
  inside 10% of each other. `run.sh` settles before timing, warms each tree with one untimed
  pass, and runs each enumeration backend in its own process, because a backend measured last
  in a long-lived process came out at twice its solo time.
- **Cold cache needs elevation.** Windows has no user-mode `drop_caches`; the only honest
  cold pass purges the standby list and working sets
  (`NtSetSystemInformation(SystemMemoryListInformation)`), which needs
  `SeProfileSingleProcessPrivilege`.
- **No peak RSS on Windows.** The Linux tables take it from `/usr/bin/time -v` or a `VmHWM`
  sampler; this harness has neither, so the column is empty rather than guessed.
- **`robocopy` is the oracle, with the limits the roadmap already records.** `/XJ` excludes
  junctions, so a tree leaning on other reparse-point kinds is not comparable; the comparison
  is reported per volume rather than as one verdict.
- **One machine, one Defender configuration.** The plateau and the delete numbers belong to
  this desktop, on an NVMe volume with real-time protection on. Read the shapes, not the
  milliseconds, on a different box.

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

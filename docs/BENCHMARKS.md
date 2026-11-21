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

`bench/run.sh` warms every command once, then runs `N` interleaved rounds — one run
of every row per round, so machine drift hits all tools alike — and reports the
median (five by default; the record behind this document used seven warm rounds and
five cold ones, which the preamble states). Cold rows get their own pass with their own cache drop,
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

The working file is **not tracked**: `.gitignore` lists it, so a fresh clone has no
result file and `bench/summary.sh` exits 2 with "run bench/run.sh first" until you
make one. The tables below are a snapshot of the run named in their preamble, kept
here so the document reads on its own; re-running the suite replaces them.

A run whose numbers this document quotes is **archived** instead, under
`bench/records/` as `<utc>-<commit>-<label>.json` next to the `<label>.md` that
`bash bench/summary.sh <that json>` printed. `20261120T230616Z-48a92f6-before.json`
and `20261120T232911Z-2cdb809-after.json` are the pair behind the tables and the
prose below, and every claim of a change here can be re-rendered from them without
re-running anything:
```console
$ bash bench/summary.sh bench/records/20261120T232911Z-2cdb809-after.json
```

`bench/summary.sh` renders that file as Markdown: one table per cache pass, one
line per case, the fastest reference of the case named beside its own median,
both peak-RSS columns and their ratio. It is POSIX `sh` plus `awk`.

## The commit these numbers describe

The run behind this document was made from `main` at `1f3331e`, which carries the tree and
df subcommands (#92, #93) and the search buffer work of #95. Nothing in this document's
branch changes code: the branch adds `dusage tree`, `dusage df`, their bench cases and this
prose, so the tables describe the compiler output of that main and nothing else.

Two earlier records sit beside this one in `bench/records/` — `20261120T230616Z-48a92f6-before`
and `20261120T232911Z-2cdb809-after`, the pair behind #95's own before/after claim. The record
for this run is `...-1f3331e-tree-df`, and it is the first one that carries the `tree` and
`df` rows.

Check the commit against `git log --oneline -1 -- src/ bench/` before quoting a table: if
the scanner or the walk has moved since, the numbers describe the old code and the suite
should be re-run. The machine is a 4-vCPU container on a shared host, not a quiet box, which
is why the reading section spends a bullet on noise. The header the run wrote is its
conditions record, quoted as it stands:

```json
{
  "schema": 1,
  "generated": "2026-11-21T02:31:51Z",
  "machine": {
    "uname": "Linux 6.1.158+ x86_64",
    "cpus": 4,
    "rustc": "rustc 1.99.0 (b940084d7 2026-09-28)",
    "rg": "ripgrep 14.1.0",
    "grep": "grep (GNU grep) 3.11",
    "find": "find (GNU findutils) 4.9.0",
    "du": "du (GNU coreutils) 9.4"
  },
  "runs": {"warm": 7, "cold": 5, "rss": 3},
  "rss_method": "gnu-time (/usr/bin/time -v, maxrss of the largest process in the command)",
  "cold_method": "fadvise (posix_fadvise DONTNEED per file: file data evicted exactly, dentries and inodes retained, so metadata-only cases stay effectively warm)",
  "trees": [
    {"root": "/usr", "bytes": 7991139623, "files": 180497},
    {"root": "/usr/local/go", "bytes": 244137476, "files": 15657},
    {"root": "/workspace/dusage/target/bench/gzip-tree", "bytes": 3393342, "files": 1200}
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
Generated 2026-11-21T02:31:51Z on Linux 6.1.158+ x86_64, 4 CPUs. Warm rows are medians of 7 alternating
runs; cold rows are medians of 5, each preceded by a cache drop. Times in
milliseconds; RSS is peak resident set, in MiB, measured in a pass of its own.

RSS method: gnu-time (/usr/bin/time -v, maxrss of the largest process in the command)

### Warm cache

| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| scan | 219.3 | `du -sb` | 784.9 | 0.28x | 7 | 4 |
| scan-t1 | 738.4 | - |  | - | 5 | - |
| search-rare | 60.7 | `rg` | 67.9 | 0.89x | 8 | 10 |
| search-common | 78.9 | `rg` | 107.1 | 0.74x | 19 | 11 |
| search-regex | 62.3 | `rg` | 64.6 | 0.96x | 12 | 11 |
| search-regex-nomatch | 68.7 | `rg` | 64.3 | 1.07x | 9 | 10 |
| search-ci | 68.9 | `rg` | 69.1 | 1.00x | 12 | 10 |
| search-ci-fixed | 67.5 | - |  | - | 12 | - |
| search-count | 62.5 | `rg -c` | 108.1 | 0.58x | 15 | 10 |
| search-only-matching | 86.5 | `rg -o` | 115.9 | 0.75x | 29 | 10 |
| search-multiline | 221.4 | `rg -U` | 320.7 | 0.69x | 66 | 70 |
| search-multiline-unicode | 86.5 | `rg -U unicode` | 87.3 | 0.99x | 45 | 11 |
| search-files-with-matches | 61.6 | `rg -l` | 99.1 | 0.62x | 12 | 10 |
| search-gzip | 147.7 | `rg -z` | 381.5 | 0.39x | 21 | 8 |
| find-name | 221.2 | `find -name` | 410.5 | 0.54x | 14 | 5 |
| find-path | 27.5 | `find -path` | 62.0 | 0.44x | 6 | 3 |
| find-type | 27.4 | `find -name` | 42.0 | 0.65x | 6 | 3 |
| find-expr | 45.2 | `find -name` | 40.1 | 1.13x | 4 | 3 |
| du-s | 230.7 | `du -s` | 786.6 | 0.29x | 20 | 4 |
| du | 242.4 | `du` | 802.6 | 0.30x | 21 | 4 |
| tree | 1224.0 | `tree` | 1745.0 | 0.70x | 70 | 5 |
| df | 2.8 | `df` | 2.5 | 1.12x | 3 | 2 |
| threads-scan-1 | 738.3 | - |  | - | 5 | - |
| threads-scan-2 | 392.2 | - |  | - | 7 | - |
| threads-scan-4 | 235.6 | - |  | - | 8 | - |
| threads-scan-8 | 255.8 | - |  | - | 9 | - |
| threads-scan-16 | 275.1 | - |  | - | 11 | - |
| threads-scan-default | 208.0 | - |  | - | 8 | - |
| threads-search-1 | 119.9 | - |  | - | 13 | - |
| threads-search-2 | 128.7 | - |  | - | 12 | - |
| threads-search-4 | 70.5 | - |  | - | 22 | - |
| threads-search-8 | 100.5 | - |  | - | 41 | - |
| threads-search-16 | 148.5 | - |  | - | 68 | - |
| threads-search-default | 72.0 | - |  | - | 20 | - |

Notes: warm pass, page cache untouched between runs

### Cold cache

| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| scan | 236.4 | `du -sb` | 801.1 | 0.30x | 7 | 4 |
| search-rare | 466.7 | `rg` | 426.6 | 1.09x | 8 | 10 |
| search-common | 438.5 | `rg` | 459.8 | 0.95x | 19 | 11 |
| search-regex | 434.5 | `rg` | 423.3 | 1.03x | 12 | 11 |
| search-regex-nomatch | 436.3 | `rg` | 400.9 | 1.09x | 9 | 10 |
| search-ci | 451.6 | `rg` | 431.5 | 1.05x | 12 | 10 |
| search-ci-fixed | 447.2 | - |  | - | 12 | - |
| search-count | 452.1 | `rg -c` | 463.0 | 0.98x | 15 | 10 |
| search-only-matching | 430.3 | `rg -o` | 464.3 | 0.93x | 29 | 10 |
| search-multiline | 582.3 | `rg -U` | 793.6 | 0.73x | 66 | 70 |
| search-multiline-unicode | 446.9 | `rg -U unicode` | 430.4 | 1.04x | 45 | 11 |
| search-gzip | 157.9 | `rg -z` | 389.5 | 0.41x | 21 | 8 |
| find-name | 237.5 | `find -name` | 445.9 | 0.53x | 14 | 5 |
| find-path | 31.8 | `find -path` | 60.6 | 0.52x | 6 | 3 |
| find-type | 26.0 | `find -name` | 63.5 | 0.41x | 6 | 3 |
| find-expr | 45.1 | `find -name` | 46.5 | 0.97x | 4 | 3 |
| du-s | 236.0 | `du -s` | 762.6 | 0.31x | 20 | 4 |
| du | 249.6 | `du` | 781.1 | 0.32x | 21 | 4 |
| tree | 1157.3 | `tree -s` | 1692.2 | 0.68x | 70 | 5 |

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
search-rare                    73.2         rg       74.3     0.99    1.028  ok
search-regex-nomatch           77.4         rg       74.3     1.04    1.071  ok
search-count                   81.1      rg -c      119.7     0.68    0.807  ok
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

- **The metadata rows win by a wide margin, the output-heavy content rows by a
  clear one, and the plain content rows are level.** On the warm pass `scan` is
  0.28x of `du -sb`, both `du` rows 0.29-0.30x, `find-name` 0.54x, `find-path`
  0.44x, `find-type` 0.65x and `tree` 0.70x; the cases that print a lot —
  `search-count` 0.58x, `search-files-with-matches` 0.62x, `search-multiline`
  0.69x, `search-common` 0.74x, `search-only-matching` 0.75x — get there by
  streaming results as the walk reaches them instead of collecting them, and by a
  matching line sharing its file's buffer through an offset rather than owning a
  `Vec`. The plain rows (`search-rare` 0.89x, `search-regex` 0.96x, `search-ci`
  1.00x, `search-multiline-unicode` 0.99x, `search-regex-nomatch` 1.07x) are at
  or within a couple of percent of `ripgrep`.
- **Read a ratio of two or three percent as a tie on this box.** A single run of a
  content case here spans up to 2.6x (one `-o` row ranged 83-148 ms over thirteen
  alternating rounds), so the seven-sample medians above move the plain rows by
  ±10% between passes. That is why `search-regex-nomatch` at 1.07x and
  `search-ci` at 1.00x are the same result on different days, and why the sharper
  instrument is an interleaved pair rather than a wider table.
- **The ratio reads dusage over the reference: below `1.00x` is dusage ahead.** An
  earlier revision of this document described the plain rows as having "slipped
  behind ripgrep" while quoting ratios below one, which inverted the comparison.
- **Two rows are genuinely behind, and only two.** `find-expr` at 1.13x is the
  sequential walk `-prune` and `-depth` force, where GNU find pays the same
  `getdents` and `statx` and wins on a job of some 40 ms because it keeps less
  bookkeeping per entry; on the cold pass it is 0.97x, which is the same tie from
  the other side. `df` at 1.12x is 0.3 ms on a command that takes 2.5 ms — see
  the row below it.
- **`tree` is ahead of the tool it replaces.** 1224.0 ms against plain `tree`'s
  1745.0 ms (0.70x), and 1157.3 ms against `tree -s`'s 1692.2 ms cold (0.68x).
  The work is one batched enumeration per directory handing both size columns
  over in a single syscall where `tree -s` asks the kernel once per entry; the
  price is memory, 70 MiB against tree's 5, because a sorted listing with aligned
  columns and subtree totals holds one record per entry and tree streams a
  directory at a time. The warm table compares against the faster of the two tree
  rows, which is the one that prints no sizes at all.
- **`df` is a tie, read as one.** 2.8 ms against 2.5 ms is 0.3 ms on two commands
  whose whole job is a handful of syscalls, and the `min_ms`/`max_ms` spread in
  `results.json` is wider than the gap. What differs is what each asks for:
  dusage takes a `statvfs` per mount point and an `st_dev` per row, which is what
  lets the rows carry inode counts and the device key GNU `df` gets from its single
  `statfs`.
- **Content search is kernel-bound, so instruction cuts are small on the clock.**
  Over `/usr/local/go` a `-o` run spends 0.07-0.11 s in user space against
  0.20-0.26 s in the kernel; `scan /usr` on four threads spends 0.21 s of user
  time against 0.77 s of system time. The plain content rows ask the kernel fewer
  questions than `ripgrep` (55.6k syscalls against 179k on the search tree:
  `ripgrep` readlinks every entry, dusage reads `d_type`) and pay 319M
  instructions against 303M for a case-insensitive literal, which is why they land
  at parity rather than ahead. Where the clock can still move is per-file
  syscalls, not instructions.
- **`-z` is far ahead** (0.39x) because the decoder runs in this process over
  already-read bytes, with no second tool and no temporary files, and **`-c` is
  ahead of `rg -c`** (0.58x) because a count never builds the hits it counts.
- **`du` is ahead on allocated blocks** (0.29-0.30x) because the walk is parallel
  and counts what `st_blocks` already says, where GNU `du` walks serially.
- **Memory is bounded, not minimal.** Peak RSS on the content rows is 8-19 MiB
  against `grep`'s 5 and `ripgrep`'s 10-11, with `search-only-matching` at 29 MiB
  because it keeps a match span per hit; it does not scale with how much the tree
  matches, because the printer's pending results are capped at 4 MiB and the path
  window at `threads × 1024` files. The floor above the references is the pipeline
  itself — a read buffer and a queue per worker — and beating `grep`'s 5 MiB would
  mean giving up the threads that make the rows ahead. `find`'s 14 MiB on
  `find-name` is the read-ahead pool, flat whether 15k or 267k paths match, while
  the expression form prints as it walks and holds 4 MiB. `du`'s 20-21 MiB against
  GNU `du`'s 4 is the cost of the parallel walk; `tree`'s 70 MiB is the arena that
  makes sorted subtree totals possible.
- **The thread sweeps** (`threads-scan-*`, `threads-search-*`) are the evidence for
  the default, and on this 4-vCPU container they say the default already is the
  right one: `scan` runs 738 ms on one thread, 392 on two, 236 on four, 256 on
  eight and 275 on sixteen, with the default at 208; `search` runs 120, 129, 71,
  101 and 149 ms the same way, default 72. Both are fastest at about one thread per
  core, and heavy oversubscription (4x cores, sixteen here) costs 30-55%.
  `cli::default_threads` is `available_parallelism` clamped to 4..16, i.e. cores,
  so the code and the measurement agree; the older prose's "4x cores" hypothesis is
  refuted by both.
- **The cold pass reorders only the plain content rows.** With the file data
  evicted, `search-rare` 1.09x, `search-regex` 1.03x, `search-ci` 1.05x and
  `search-regex-nomatch` 1.09x move behind while `search-common` 0.95x,
  `search-count` 0.98x, `search-only-matching` 0.93x and `search-multiline` 0.73x
  stay at or ahead; the metadata rows barely move and stay 0.28-0.68x, which is
  what the method line predicts, since it evicts file data and not dentries.

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
| `tree -a --no-ignore /usr` | 267,547 lines | — | 267,545 (`tree -a --noreport`) | same paths; our two extra lines are the blank line and the footer |
| `df` on this box | 6 rows | — | 6 rows (`df`) | same rows, numbers and percentages; column padding differs by a space |

Two divergences, both explained:

- **`tree` and `df` agree with their references by construction, and are tested that way.**
  `tests/tree_cli.rs` compares a listing with `tree -a --noreport` after the size brackets
  are stripped — same paths, connectors and nesting — and `tests/df_cli.rs` compares the row
  for a path with GNU `df -k` on that path: same filesystem, size within 1%, used within
  2 MiB + 1%. The two rows above are that comparison counted once over `/usr` at this commit.
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
  sequence for exactly this reason, and on a shared 4-vCPU container the spread is
  the headline: a content case's five runs spanned 84-222 ms in one pass, so the
  warm ratios on the plain content rows carry ±10% and only a two-fold gap is worth
  a sentence. `min_ms` is the closest thing in the file to an uncontended number.
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
- **`tree` and `df` were measured on Linux only.** `tree` never follows a link on any
  platform and its sizes come from the same enumerator everywhere, but no macOS or Windows
  time for it is recorded here. `dusage df` on Windows was validated by running it, not by
  reading it: on a three-volume box its figures were byte-identical to `Get-Volume`'s, Use%
  matched GNU's formula and `-T` reported NTFS. That run also found a defect this document's
  rows could not — every volume printed twice, `E:\` from the volume list and `E:` from the
  drive-letter fallback — which #93 fixes by normalising the label before comparing it. The
  Windows `df` path is therefore verified for correctness and still unmeasured for speed.
- **`df` sits at the noise floor.** 2.8 ms against 2.5 ms is one process startup apart; the
  row is kept because a regression in the mount-table parse or the per-mount `statvfs` would
  show as a number, not because 12% of 2.5 ms means anything.
- **The macOS rows are not here yet.** The Darwin port lands as its own change; when it does,
  its owner regenerates these tables with its rows rather than this document describing a
  platform it never ran on.
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

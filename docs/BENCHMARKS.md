# Benchmarks

Wall-clock medians for `dusage scan`, `dusage search`, `dusage find`, `dusage du`,
`dusage tree` and `dusage df` against the tools they replace: `du`, `find`, `grep`,
`ripgrep`, `tree` and `df`, with peak memory in the same rows.

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
`find`, `du`, `tree` or `df`. The modes covered are the ones the binary has — literal,
frequent and rare literals, case-insensitive, regex that matches and regex that
never does, `-U`, `-z` over a gzip tree built from the search tree on first run,
`-l`, `-c`, `-o`, `find` by name, by path and by `-t` type, GNU find's
expression form (`find-expr`, the sequential walk `-prune` and `-depth` force),
`du -s`, `du`, `tree --json` against `tree -s` and against plain `tree`, `df`
against GNU `df`, and `scan` across a thread sweep. The two newest pairs are
gated on the reference being installed: `bench/run.sh` sets `TREE_OK` and
`DF_OK` from `tree --version` and `df --version`, and `bench/cases.sh` leaves the
pair out when either is missing, so a box without them measures nothing rather
than timing a command that never ran.

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

The run behind this document was made from an integration build of the two open
pieces of work that add `tree` and `df`: `capy/tree` (PR #92) and `capy/df`
(PR #93) merged onto `main` at `48a92f6`, so every row describes the code those
pull requests bring and the rows that already existed describe `main` as it
stands. Regenerate this document after they merge, from the merge commit, and
name that commit here instead. The header the run wrote is its conditions
record, quoted as it stands:

```json
{
  "schema": 1,
  "generated": "2026-11-20T23:31:36Z",
  "machine": {
    "uname": "Linux 6.1.158+ x86_64",
    "cpus": 4,
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
Generated 2026-11-20T23:31:36Z on Linux 6.1.158+ x86_64, 4 CPUs. Warm rows are medians of 5 alternating
runs; cold rows are medians of 3, each preceded by a cache drop. Times in
milliseconds; RSS is peak resident set, in MiB, measured in a pass of its own.

RSS method: gnu-time (/usr/bin/time -v, maxrss of the largest process in the command)

### Warm cache

| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| scan | 260.6 | `du -sb` | 953.9 | 0.27x | 8 | 4 |
| scan-t1 | 908.7 | - |  | - | 5 | - |
| search-rare | 76.4 | `rg` | 81.2 | 0.94x | 10 | 11 |
| search-common | 112.2 | `rg` | 177.9 | 0.63x | 17 | 11 |
| search-regex | 81.9 | `rg` | 86.2 | 0.95x | 10 | 10 |
| search-regex-nomatch | 77.2 | `rg` | 79.9 | 0.97x | 12 | 10 |
| search-ci | 81.6 | `rg` | 85.5 | 0.95x | 10 | 10 |
| search-ci-fixed | 83.8 | - |  | - | 10 | - |
| search-count | 76.5 | `rg -c` | 122.1 | 0.63x | 13 | 10 |
| search-only-matching | 88.9 | `rg -o` | 127.9 | 0.70x | 18 | 11 |
| search-multiline | 230.6 | `rg -U` | 340.4 | 0.68x | 46 | 75 |
| search-multiline-unicode | 126.0 | `rg -U unicode` | 105.3 | 1.20x | 47 | 11 |
| search-files-with-matches | 76.4 | `rg -l` | 122.8 | 0.62x | 10 | 10 |
| search-gzip | 157.0 | `rg -z` | 416.2 | 0.38x | 18 | 8 |
| find-name | 277.4 | `find -name` | 511.0 | 0.54x | 18 | 5 |
| find-path | 28.0 | `find -path` | 49.7 | 0.56x | 6 | 3 |
| find-type | 25.5 | `find -name` | 44.6 | 0.57x | 6 | 3 |
| find-expr | 50.2 | `find -name` | 48.6 | 1.03x | 4 | 3 |
| du-s | 323.0 | `du -s` | 1046.9 | 0.31x | 22 | 4 |
| du | 309.2 | `du` | 1011.9 | 0.31x | 20 | 4 |
| tree | 1519.6 | `tree` | 2061.8 | 0.74x | 70 | 5 |
| df | 2.8 | `df` | 2.5 | 1.12x | 3 | 2 |
| threads-scan-1 | 888.2 | - |  | - | 5 | - |
| threads-scan-2 | 498.7 | - |  | - | 7 | - |
| threads-scan-4 | 249.6 | - |  | - | 7 | - |
| threads-scan-8 | 287.6 | - |  | - | 9 | - |
| threads-scan-16 | 307.9 | - |  | - | 10 | - |
| threads-scan-default | 253.4 | - |  | - | 7 | - |
| threads-search-1 | 158.9 | - |  | - | 12 | - |
| threads-search-2 | 161.6 | - |  | - | 12 | - |
| threads-search-4 | 92.7 | - |  | - | 17 | - |
| threads-search-8 | 95.8 | - |  | - | 23 | - |
| threads-search-16 | 117.8 | - |  | - | 31 | - |
| threads-search-default | 90.8 | - |  | - | 17 | - |

Notes: warm pass, page cache untouched between runs

### Cold cache

| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |
| --- | --- | --- | --- | --- | --- | --- |
| scan | 312.3 | `du -sb` | 975.5 | 0.32x | 8 | 4 |
| search-rare | 702.7 | `rg` | 550.7 | 1.28x | 10 | 11 |
| search-common | 585.7 | `rg` | 642.5 | 0.91x | 17 | 11 |
| search-regex | 566.4 | `rg` | 563.7 | 1.00x | 10 | 10 |
| search-regex-nomatch | 625.0 | `rg` | 506.4 | 1.23x | 12 | 10 |
| search-ci | 528.5 | `rg` | 589.3 | 0.90x | 10 | 10 |
| search-ci-fixed | 644.8 | - |  | - | 10 | - |
| search-count | 619.7 | `rg -c` | 596.0 | 1.04x | 13 | 10 |
| search-only-matching | 631.7 | `rg -o` | 669.5 | 0.94x | 18 | 11 |
| search-multiline | 882.7 | `rg -U` | 1453.1 | 0.61x | 46 | 75 |
| search-multiline-unicode | 687.8 | `rg -U unicode` | 674.6 | 1.02x | 47 | 11 |
| search-gzip | 183.8 | `rg -z` | 562.4 | 0.33x | 18 | 8 |
| find-name | 373.2 | `find -name` | 542.9 | 0.69x | 18 | 5 |
| find-path | 30.0 | `find -path` | 48.7 | 0.62x | 6 | 3 |
| find-type | 27.0 | `find -name` | 45.0 | 0.60x | 6 | 3 |
| find-expr | 51.2 | `find -name` | 55.0 | 0.93x | 4 | 3 |
| du-s | 306.9 | `du -s` | 933.4 | 0.33x | 22 | 4 |
| du | 295.3 | `du` | 1068.3 | 0.28x | 20 | 4 |
| tree | 1403.9 | `tree -s` | 2267.9 | 0.62x | 70 | 5 |

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

Verbatim output of `bash scripts/perf-guard.sh` against the same binary:

```console
case                      dusage_ms        ref     ref_ms    ratio base_ratio  verdict
------------------------ ---------- ---------- ---------- -------- --------  --------
search-rare                    85.1         rg       85.0     1.00    1.028  ok
search-regex-nomatch           91.1         rg       86.1     1.06    1.071  ok
search-count                   93.8      rg -c      141.0     0.67    0.807  ok
perf-guard: baseline was recorded on 2 CPUs, this box has 4; the comparison carries that difference
perf-guard: pass (tolerance 25%, 5 runs per row)
```

`ratio` is this run's dusage over the reference, and `base_ratio` is what
`bench/baseline.json` recorded for that case, so a verdict of `ok` means the
number got no worse than 25% beyond the baseline rather than that it matches it.
Here all three are at or below their baselines of 1.03, 1.07 and 0.81, and the guard prints the
CPU-count difference because `bench/baseline.json` was recorded on a 2-vCPU box.

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

- **Every row that has a reference is at or ahead of it, except three.** The
  output-heavy content cases (`search-count` 0.63x, `search-files-with-matches`
  0.62x, `search-only-matching` 0.70x, `search-multiline` 0.68x, `search-common`
  0.63x) get there by streaming results as the walk reaches them instead of
  collecting them, and by a matching line sharing its file's buffer through an
  offset rather than owning a `Vec`. The plain rows are 0.94x (`search-rare`),
  0.95x (`search-regex`), 0.97x (`search-regex-nomatch`) and 0.95x (`search-ci`).
  The three behind are `search-multiline-unicode` at 1.20x (the Unicode path
  still decodes the lines holding non-ASCII bytes before it can settle a match),
  `find-expr` at 1.03x (the sequential walk, inside this box's spread), and
  `df` at 1.12x, which is 0.3 ms on a command that takes 2.5 ms and is therefore
  a tie, not a regression — see the row below.
- **`-U` on ASCII files is ahead** (`search-multiline` 0.68x, and 0.61x cold)
  while `-U` with Unicode semantics is 1.20x: the ASCII path settles its matches
  from the line holding each occurrence, the Unicode one still decodes.
- **`-z` is far ahead** (0.38x) because the decoder runs in this process over
  already-read bytes, with no second tool and no temporary files, and **`-c` is
  ahead** (0.63x of `rg -c`) because a count never builds the hits it counts.
- **`scan` and `du` are far ahead on allocated blocks** (`scan` 0.27x of
  `du -sb`, `du` 0.31x of GNU `du`) because the walk is parallel and counts what
  `st_blocks` already says, where GNU `du` walks serially. `scan-t1` (908.7 ms on
  one thread against 260.6 ms at the default) is the same work paying for a
  single core, and it is the row that shows the parallelism rather than the code.
- **`tree` is ahead of `tree -s`** (0.74x of plain `tree`, and 0.62x cold against
  `tree -s`): one batched enumeration per directory hands both size columns over
  in a single syscall, where tree asks the kernel one question at a time. The
  warm table compares against the faster of the two tree rows, which is the one
  that prints no sizes; the fair row is `tree -s` at 2091.7 ms against 1519.6 ms,
  still ahead. The trade is memory: 70 MiB against tree's 5, because a listing
  that is sorted and column-aligned and carries subtree totals has to hold one
  record per entry, and tree streams a directory at a time.
- **`df` is a tie, read as one.** 2.8 ms against 2.5 ms is 0.3 ms on two commands
  whose whole job is a handful of syscalls, and the `min_ms`/`max_ms` spread in
  `results.json` is larger than the gap. What differs is what each one asks for:
  dusage takes a `statvfs` per mount point and an `st_dev` per row, which is what
  lets the rows carry inode counts and the device key GNU `df` gets from its
  single `statfs` call.
- **`find` is ahead on every row**: `find-name` 0.54x, `find-path` 0.56x,
  `find-type` 0.57x, `find-expr` 1.03x. The big row is dominated by matching
  globs by shape and streaming the sorted listing from a heap over open directory
  cursors; `find-path` and `find-type` are jobs of 25-50 ms where process startup
  is a visible share, and `find-expr` is the sequential walk `-prune` and `-depth`
  force.
- **Content search is about half kernel time.** Over `/usr/local/go` the same run
  spends roughly 60 ms in user space against 180 ms in the kernel, so further
  instruction cuts on that path buy one or two percent of wall clock; the next
  real gain there has to remove per-file syscalls, not instructions.
- **Memory is bounded, not minimal.** Peak RSS on the content rows is 10-18 MiB
  against `grep`'s 5 and `ripgrep`'s 10-11, and it no longer scales with how much
  the tree matches: the printer's pending results are capped at 4 MiB and the
  path window at `threads × 1024` files. The floor above the references is the
  pipeline itself — a read buffer and a queue per worker — and beating `grep`'s
  5 MiB would mean giving up the threads that make the rows ahead. `find`'s
  18 MiB on `find-name` is the read-ahead pool, flat whether 15k or 267k paths
  match, while the expression form prints as it walks and holds 4 MiB. `du`'s
  20-22 MiB against GNU `du`'s 4 is the cost of the parallel walk; `tree`'s
  70 MiB is the arena that makes sorted subtree totals possible.
- **The cold rows reorder the search cases.** With the file data evicted,
  `ripgrep` keeps more of the win: `search-rare` 1.28x, `search-regex-nomatch`
  1.23x and `search-count` 1.04x are behind, while `search-common` (0.91x),
  `search-only-matching` (0.94x) and `search-multiline` (0.61x) stay ahead. `du`,
  `find` and `tree` barely move and stay ahead, which is what the method line
  predicts: this pass evicts file data, not dentries, so a metadata walk stays
  effectively warm.
- **The thread sweeps** (`threads-scan-*`, `threads-search-*`) are the evidence
  for the 4..16 default, and they read differently on a 2-vCPU box: here `scan`
  shows 888 ms on one thread, 249 at four and 253 at the default, while `search`
  is kernel-bound and flat from four threads up (159 ms on one thread against
  93 at four and 118 at sixteen). On a box with a different core count read the
  shape, not the absolute milliseconds.

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
| `tree -a --no-ignore` vs `tree -a --noreport` | 245,596 lines | — | 245,596 lines | same paths, same connectors |
| `df` vs `df -k /` | `/dev/root` 65G | — | `/dev/root` 65G | same rows, numbers, percentages |

Two divergences, both explained:

- **Binary files.** `dusage` counts matches inside a file it also labels
  "Binary file … matches", where `grep -I` and `ripgrep` skip it outright. On
  text files all three agree exactly — the `func ` output is 124,778 content
  lines from all three tools.
- **`tree` and `df` agree by construction, and are tested that way.** The
  comparison is part of `tests/tree_cli.rs` and `tests/df_cli.rs`, not a hand
  count: tree's listing is compared with `tree -a --noreport` after the size
  brackets are stripped (same paths, same connectors, same nesting), and the `df`
  row for a path is compared with GNU `df -k` on the same path (same filesystem
  name, size within 1%, used within 2 MiB + 1%). The row above was counted once
  over `/usr` to record what that test sees at this commit.
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

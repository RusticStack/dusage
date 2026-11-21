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

Two changes have landed on `main` since that measurement and are not in its code: the
Windows benchmark records (#99, #101), whose section below is their own run on a real NTFS
volume, and the macOS port (#94), which adds a Darwin platform module and does not touch the
Linux paths these tables time. The macOS rows are therefore absent rather than guessed: when
the port's owner regenerates the tables with a Mac in the loop, they land beside these.

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

Verbatim output of `bash scripts/perf-guard.sh` against the same binary:

```console
case                      dusage_ms        ref     ref_ms    ratio base_ratio  verdict
------------------------ ---------- ---------- ---------- -------- --------  --------
search-rare                    69.0         rg       62.7     1.10    1.028  ok
search-regex-nomatch           65.1         rg       65.8     0.99    1.071  ok
search-count                   66.7      rg -c       99.4     0.67    0.807  ok
perf-guard: baseline was recorded on 2 CPUs, this box has 4; the comparison carries that difference
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
- **`-U` with Unicode semantics is a tie that costs 66% more instructions.** On the
  same tree the row pays 517.3M instructions where `ripgrep` pays 311.8M: 107.5M in
  the mandatory-literal prefilter, 165.0M in the byte VM run from `floor` to
  recover each match's leftmost-first start (85.5M of that building the initial
  thread list), 37.5M in the forward DFA. Two interleaved pairs measured it at
  1.01x and 1.11x while the table has read 1.15x and 0.99x on different passes, so
  the ratio is unresolved here and the instruction count is the finding. Answering
  the candidate lines instead was tried twice and cost more both times - 833M with
  the character program per line, 705M with the byte program, because the price is
  per VM restart rather than per byte. `bench/records/20261121-uni-investigation.md`
  has all three attempts with their numbers, and the reverse automaton that would
  recover the start without the VM as the next cut.
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
generators above pick them up. Those rows are **not** in the tables below: the script needs one
administrator approval on the machine and had not run when this record was taken, so every
question that depends on it (`3`, `8`, the cold pass and the FAT32/ReFS `FileId` rows) is
marked owed below rather than answered from an unprivileged measurement dressed up as one.

**The run behind this section is archived**, the same way the Linux record is:
`bench/records/20261121T002231Z-4db47d2-windows.json` and the `.md` in its name printed by
`bash bench/summary.sh`. It can be re-rendered without re-measuring anything:

```console
$ bash bench/summary.sh bench/records/20261121T002231Z-4db47d2-windows.json
$ bash bench/windows/summary-checks.sh
```

It describes the code at `4db47d2` — the commit that caps the default Windows worker count at
the measured plateau. `git diff 4db47d2 HEAD -- src/win/ src/cli.rs` is empty, so the scan and
delete paths these rows measure have not moved since; later commits on that branch changed only
this harness and the placement of imports inside the example binaries.

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

Verbatim stdout of `bash bench/summary.sh
bench/records/20261121T002231Z-4db47d2-windows.json`: one row per case with the dusage median. The reference columns are empty by design — there is no `grep`, `du` or
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
| name gone after the last close | true |

USN change feed (elevated): scan --watch 1 over a 38-file tree; 12 changes in 4 groups while it ran.

| changes made | update lines | journal records | fallback rescans | totals equal a full scan | truthful |
| ---: | ---: | ---: | ---: | --- | --- |
| 12 in 4 groups | 4 | 28 | 0 | true | true |

The journal account of those changes, from bench/windows/usn_probe.py:

| records | reasons |
| ---: | --- |
| 5 | FILE_CREATE |
| 5 | DATA_EXTEND+FILE_CREATE |
| 5 | DATA_EXTEND+FILE_CREATE+CLOSE |
| 3 | FILE_DELETE+CLOSE |
| 2 | DATA_EXTEND |
| 2 | DATA_EXTEND+CLOSE |
| 2 | RENAME_OLD_NAME |
| 2 | RENAME_NEW_NAME |
| 2 | RENAME_NEW_NAME+CLOSE |

### What a purged journal does

The one case the watch row above cannot reach on a system volume: a journal whose unread
records are trimmed away before the watch gets to them. `bench/windows/usn_purge.ps1` creates
its own 512 MB NTFS VHD (whose journal is the 1 MB it gets at format time) and writes the same
20,000 files two ways. Verbatim `MEAS:`/stderr lines from `%TEMP%\dusage_usn_purge\purge.log`,
console output, 2026-11-21, DOOMBRINGER, elevated:

```console
MEAS: scratch volume W: size=535752704 format=NTFS
MEAS: createjournal m=0x4000000 a=0x800000 exit=0 out= max_before='' max_after='0x0000000000100000 (  1,0 MB)'
STEP: burst wrote 20000 files in 2026 ms
MEAS: burst journal_max=0x0000000000100000 (  1,0 MB) flood=20000 ms=2026 updates=2 changed=0 reset=1 totals_equal=True
dusage: change journal reset (A entrada de diário foi eliminada do diário. (os error 1181)); full rescan
STEP: paced wrote 20000 files in 12939 ms across 10 paced rounds
MEAS: paced journal_max=0x0000000000100000 (  1,0 MB) flood=20000 ms=12939 updates=15 changed=14 reset=0 totals_equal=True
MEAS: verdict burst_resets=1 paced_resets=0 burst_totals_equal=True paced_totals_equal=True paced_updates=15 burst_updates=2
```

The reset line is the host's Windows display language; the number that matters is
`os error 1181`, `ERROR_JOURNAL_ENTRY_DELETED`. `updates` counts printed reports, so the burst
case's 2 is the initial scan plus the reset rescan, and its `changed=0` is the point: with the
records gone there is nothing to name, and correctness comes from the full rescan (both cases
end with totals equal to a full scan). `fsutil usn createjournal` returning 0 while leaving the
geometry at 1 MB is what stopped this from being a small-journal-versus-large-journal
comparison; the variable that does decide the outcome is unread records per poll.

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
Linux keeps its old 4..16 clamp because nothing wider than a 2-vCPU box has ever been
swept there — that 16 is inherited, **not** measured, and the only Linux sweep in this
document (2 vCPUs, `threads-scan-*`) says a small box is already past its own plateau.

**3. Real-time protection. Owed, not measured.** The elevated half of the harness measures the
same scan and the same delete with Defender watching and with the fixture root excluded, and
removes the exclusion when it returns. It has not run on this machine — it needs one
administrator approval — so there is no protection number in this document, and nothing here
recommends turning protection off.

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
volume here is NTFS and accepts `FileIdInfo`. The FAT32 and ReFS rows are **owed**: they come
from small VHDX files the elevated half attaches for exactly that, and that run has not
happened.

**7. The reparse gap.** `is_symlink()` is true for a junction on this build, and the tool no
longer depends on it: it tests `FILE_ATTRIBUTE_REPARSE_POINT`, so the DFS and dedup tags that
`is_symlink()` misses fall under the same rule. The `reparse-scan` check shows the junction
fixture scanned as 8 files and 1 directory with its target counted once. `find` not listing a
link without `--follow` is asserted by `tests/search_cli.rs` and is a documented choice.

**8. Privileges.** Opening the volume device fails with error 5 for a non-elevated token — the
`elevation` table, measured here. The success side is **owed** with the rest of the elevated
half, and it is the reason `--mft` documents an administrator shell.

**9. Overcount.** Real pnpm-managed trees overcount far more than the 5-10% AGENTS.md
recorded: `C:\projects` 3.58 GB naive against 2.95 GB with `--dedup-hardlinks` (21.4%), and
`C:\forge` 6.51 against 4.53 GB (43.7%). The pnpm store itself is 0%, which is the useful
control: its hardlinks point into project trees, and a link whose other name lies outside the
root is still counted once.

**10. Long paths.** The deepest fixture path is 393 characters. Both spellings open and
delete it here, because this build has `LongPathsEnabled=1`; dusage uses the verbatim prefix
unconditionally, so it does not depend on that policy. `dusage prune` deleted the whole
393-character fixture in 973 us.

**11. `--mft` and the USN watch.** `--mft` comes from the elevated half: totals for a
volume against the batched backend and `robocopy /L /E /XJ`. The watch row above is the
ground-truth version of that check: twelve known changes in four groups produce **four** update
lines and **28** journal records that the probe accounts for reason by reason (three per
create, two per extend, three per rename, one per delete), and the last update's totals equal a
full scan of the same tree exactly. The purge section below covers the case a system volume
cannot reach: a journal that drops unread records fails the read with 1181, and the fallback
that follows still lands on the right totals. A watch that re-emitted records would show more update
lines than groups; one that lost a change would show fewer, or totals that differ. Both
counters are in the row so either failure is visible without reading the output.

The dry run that emitted ~33,800 lines from a twelve-second watch over a 50-file directory was
that same binary *without* an administrator token: no journal handle, so both fallback arms
rescanned without sleeping. With the feed open the same twelve changes print four update lines,
and the fallback now sleeps one interval between rescans. Nothing from the flood is in these
tables, and the pre-fix count is kept only in `docs/ROADMAP.md` item 9 as the measurement that
found the bug.

### Caveats for this section

- **The antivirus is part of the measurement.** The first pass over the freshly built 1M tree
  put the batched backend at twice the walk's time; the same rows on a quiet machine are
  inside 10% of each other. `run.sh` settles before timing, warms the real system tree with one
  untimed pass, and runs each enumeration backend in its own process, because a backend
  measured last in a long-lived process came out at twice its solo time. Both traps are
  written up in `bench/windows/NOTES.md`.
- **Cold cache needs elevation, so the cold rows are owed.** Windows has no user-mode
  `drop_caches`; the only honest cold pass purges the standby list and working sets
  (`NtSetSystemInformation(SystemMemoryListInformation)`), which needs
  `SeProfileSingleProcessPrivilege`. The harness now refuses to write a cold row when that
  purge is refused, rather than labelling a warm number cold.
- **The Linux clamp is not a measurement.** 4..16 on Linux is the number this repository
  already shipped; the sweep below it was taken on a 2-vCPU container. Windows is the only
  platform where the plateau has been measured, so 8 there is a result and 16 on Linux is a
  default nobody has checked on hardware wide enough to test it.
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

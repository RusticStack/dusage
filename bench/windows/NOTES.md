# Measuring on Windows without flattering the numbers

Two traps cost real measurements on the first pass of this harness on a Windows 11 box with
Defender's real-time protection on. Both are silent: nothing fails, the numbers are just wrong
in the direction that makes dusage look better or worse than it is.

## 1. The antivirus is still walking the tree you just built

A freshly generated fixture is still being inspected when the benchmark starts. On the 1M-file
tree the batched backend came out about twice its settled time when it was measured immediately
after generation, and the same tree measured minutes later — or after an exclusion — settles at
its normal value. The tell is a first run that is far slower than every later one, in every
backend at once.

The harness waits before it measures anything (`DUSAGE_BENCH_SETTLE`, default 20 s, skipped by
`--quick`) and gives the real system tree one untimed warm walk before its five backends run.
If you regenerate the fixtures, raise the settle (`DUSAGE_BENCH_SETTLE=120 bash
bench/windows/run.sh`) rather than trusting a default that was chosen for a machine already
quiet. Do not "fix" a slow first row by hand: rerun the pass once the tree is quiet and record
that, and note in the record's header if you measured with an exclusion in place.

## 2. The last backend measured in a long-lived process is the slowest

Measuring three enumeration backends one after another inside one process inflates the last
one: on the 1M-file tree the 1 MiB batch buffer measured about twice its solo time when it ran
last in a sequence, which is the same size as the difference between backends. The walk leaves
the file cache and the standby list in a state the next backend pays for, so any conclusion
drawn from in-process ordering is an artifact of that order rather than a property of the
backend.

`run.sh` therefore invokes each backend in its own process, and `examples/win_enum_perf.rs`
takes a backend name as its third argument for exactly this reason: `cargo run --release --example win_enum_perf -- D:\dusage-bench\flat100k 5 batch-64k`
measures one backend in a fresh process. Repeat with the arguments in a different order and
check that the answers do not depend on it.

## 3. A watch with no feed looks like a watch that is flooding

`--watch` falls back to a full rescan per tick when the change feed cannot be opened, and the
only thing that separates the two modes is whether the process holds an administrator token:
`CreateFileW` on `\\.\X:` fails with error 5 without one. `fsutil usn queryjournal` works
either way, so it does not tell you whether the watch itself has a feed. Before the fallback
was paced, an unelevated `--watch 1` over a 38-file directory printed 142,929 lines in 14 s;
with the feed open and the same twelve changes it prints four update lines. Read the stderr
label on each update — `changed: ...` means the journal, `no change feed (...)` means every
tick is a full rescan — and `DUSAGE_WATCH_TRACE=1` prints the record count and USN span of
each poll.

## The rest of the rules this harness follows

- Warm and cold are different questions and only the elevated half can measure cold properly
  (standby-list purge via `NtSetSystemInformation`). An unprepped run is warm, whatever the
  ambient memory pressure looks like; do not label it cold.
- Deletes are compared against themselves on a freshly created copy of the same source, never
  against a leftover directory, and the copy is outside the clock.
- `fastdel` is permanent. Only ever point it at a tree created under the fixture root
  (`-Root`, default `D:\dusage-bench`).
- Every number in `docs/BENCHMARKS.md` came from `bench/windows/results.json`, which
  `record.sh` regenerates from the rows plus the machine header. If a table is not in that
  file, it is not measured.

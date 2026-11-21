# search-multiline-unicode: three structural attempts, all measured, none kept

A record of the `-U` Unicode row: what it costs, what was tried, and why the row stays
where it is. No code changed. Reproduce any line below with the command beside it.

Machine: the 4-vCPU container that took the `bench/records/` pair
(`Linux 6.1.158+ x86_64`, 4 CPUs, `rustc 1.99.0`, ripgrep 14.1.0). Tree: `/usr/local/go`
(244 MB, 15,657 files). Query: `search --color=never -c -U 'type \w+ struct'`. Instruction
counts are `valgrind --tool=callgrind` at `--threads 1`; wall clock is an interleaved pair,
one run of dusage and one of `rg` per round.

## What it costs

| build | instructions | note |
| --- | --- | --- |
| ripgrep 14.1.0 | 311.8M | one DFA pass over the file, match starts recovered by a reverse walk |
| dusage at `2cdb809` (main) | 517.3M | prefilter + forward DFA + byte VM per candidate |

Inclusive split of the 517.3M:

| component | instructions | share |
| --- | --- | --- |
| `simd::find_pair_avx2` (mandatory-literal prefilter) | 107.5M | 21% |
| `run_vm` from `floor`, of which `add_thread::<[u8], ByteSet>` is 85.5M | 165.0M | 32% |
| `DfaCache::forward` | 37.5M | 7% |
| `read_job` (the file reads themselves) | 37.7M | 7% |
| everything else (walk, decode of the non-ASCII files, printing) | 169.6M | 33% |

## Attempt 1 — answer each candidate line with the character program

Drop the whole-file ASCII scan and UTF-8 validation, decode only the candidate lines, run
the character program per line:

    833.2M instructions, 9% slower in wall clock. Reverted.

The per-line character VM (`add_thread::<LineHay, CharClass>` 205M) costs more than the
whole-file byte program it replaces, because the cost is per restart, not per byte.

## Attempt 2 — answer each candidate line with the byte program

The same line windows, but a line with no non-ASCII byte goes to the byte program, which is
exact for it (`ByteLine` haystack, `byte_assert` shared with `[u8]`):

    704.7M instructions. Reverted.

`add_thread::<ByteLine, ByteSet>` alone is 201.6M for 29,251 matches — about 7,000
instructions per window, spent building the initial thread list. Window size does not matter;
restarting the VM is what costs.

Attempt 2 was checked for output parity against the pre-change binary on the whole tree
(`-c` output byte-identical, 29,251 lines) before it was rejected on cost, so the numbers are
a cost comparison and not a broken build's.

## Attempt 3 — let the DFA answer without the VM

Not attempted, and the reason is structural rather than budgetary: `DfaCache::find` walks
forward to the match end and then calls `run_vm(prog, hay, floor, false)` to recover the
leftmost-first start. Dropping that call needs the start from an automaton, which means a
reverse DFA built from the reversed program - a new automaton with its own parity surface,
not a change to this one. It is the same design ripgrep uses, and the 200M-instruction gap in
the table above is what it would be buying.

## What the wall clock says

Interleaved pairs on `main`, one dusage run and one `rg -c -U` run per round:

| rounds | dusage median | rg median | ratio |
| --- | --- | --- | --- |
| 15 | 78.4 ms | 77.6 ms | 1.01x |
| 12 | 129.0 ms | 115.7 ms | 1.11x (earlier session, busier box) |

The suite's own row read 1.15x in the record before `2cdb809` and reads 0.99x in the record
`main` carries now (`2026-11-21T02:31:51Z`, same box). So the ratio on this row moves between
0.99x and 1.15x across passes while the instruction deficit stays at 66%: read the ratio as
unresolved by this instrument and the instructions as the fact.

## Where this leaves the row

The row is not reliably behind, and it is not ahead either: it is a tie that costs 66% more
instructions than the reference, and the only structural fix found so far is the reverse
automaton. Two cheaper-looking structures were built and both cost more, which is the useful
part of this record - the next attempt starts from that rather than from a guess.

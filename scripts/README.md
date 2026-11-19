# Differential correctness suite

dusage reimplements external tools with a hand-written regex engine, walker, ignore
matcher and decompressors, so its output is checked against those tools on fixture trees
built from scratch by the suite itself: `search` against `grep` and `ripgrep`, `find`
against GNU `find`, `du` against GNU `du`, and `-z` against `rg -z` over six compression
formats.

```sh
scripts/differential.sh                 # build, run every case, fail on a surprise
scripts/differential.sh --verbose       # one line per case
scripts/differential.sh --only ctx      # only cases whose name starts with `ctx`
scripts/differential.sh --keep          # keep the fixture tree and print its path
DUSAGE_SKIP_BUILD=1 scripts/differential.sh   # reuse target/release/dusage as built
```

`rg` and `grep` are optional: the comparisons that need a missing tool are skipped and the
run still passes. `find` is optional for the `find` group, `git` is optional (without it the
suite asks ripgrep to read ignore files outside a repository).

`du` and `find` are used only when their `--version` names GNU: BSD's `du` prints different
units and BSD's `find` accepts a different expression list, so a row compared with them would
fail for a reason the harness cannot explain. `gzip` is optional and only builds the one
compression fixture that is not checked in. A case whose reference is missing is counted as
skipped and named in a `note:` line before the summary, never as agreed.

The content corpus also runs from `cargo test` as `tests/differential.rs`, so CI covers it.
The `du`, `find`-expression and `-z` groups are shell-only for now — `tests/differential.rs`
carries `content` and `FindCase` rows, not these:

```sh
cargo test --test differential                         # skips cleanly without rg/grep
cargo test --test differential -- --nocapture          # shows the skip notice
```

Exit status of `scripts/differential.sh`: `0` when every `eq` case agrees and every
`known:*` case still differs, `1` on an unexpected mismatch, `2` on a build or setup error.

## How a case is compared

A content case runs

```sh
dusage search <dusage flags> PATTERN TARGET     # from inside the fixture directory
rg            <rg flags>     PATTERN TARGET
grep          <grep flags>   PATTERN TARGET
```

and compares the sorted stdout lines with a leading `./` removed, plus the exit code.
`--` group separators are compared like any other line: two dedicated cases cover a plain
match list (no separator) and context groups across files (one separator between blocks). A `find` case compares `dusage find` against `find(1)`
stdout only, because `find` exits 0 whether or not it found anything.

A `du` case runs both tools on the **same start point**, from the same directory, so the
printed paths already agree, and compares the sorted lines. The sort is the only
normalisation: dusage prints post-order and GNU `du` follows its own traversal, and neither
order is part of the contract. Sizes, units, the tab separator and the `total` label are
compared exactly as printed — nothing is rounded, rescaled or dropped. The GNU side is asked
for `--apparent-size` (except under `-b`, which implies it, and `--inodes`, which ignores
size) because that is the unit dusage documents; plain GNU `du` reports allocated blocks and
is the catalogue entry `du-allocation` instead of a rewrite of these numbers.

A `find` expression case hands the **same argv** to both tools
(`dusage find . -name '*.txt'` against `find . -name '*.txt'`) and compares sorted stdout, so
`-print0` is split into its NUL-terminated records before sorting and stdout alone decides the
verdict. Under `--only --verbose` every `du`, `find` expression and `-z` row reports how many
lines it compared: two empty outputs compare equal, and the count is what tells a real
agreement from a vacuous one.

A `-z` case runs `dusage search -z` and `rg -z` on one compressed file and compares sorted
stdout plus the exit code, so a format that decodes differently, loses lines or fails to fail
shows up. The bzip2 fixture's plaintext contains CRLF lines, and dusage drops the trailing
`\r` of a CRLF line (catalogue entry `crlf-strip`), so the trailing CR is stripped from both
sides before sorting; that byte is the only thing the comparison rewrites.

Cases come in four kinds:

| kind | meaning |
| --- | --- |
| `eq` | the outputs must agree; a difference fails the run |
| `known:<id>` | a catalogue entry below; the run expects a difference and prints `FIXED` once the outputs agree, so the entry gets retired |
| `doc:<id>` | a dusage contract, not a comparison: the exit code and silence its own help documents |
| `expr` | a `find` expression case: `eq` once the CLI takes an expression, the catalogue entry `findexpr-unwired` while it does not |

`expr` exists because a capability can arrive after the case: written as a plain `known:` row,
a half-wired evaluator would print `FIXED` and never fail the run. The runner picks the
expectation from a one-line probe (`dusage find . -name '*'` exits 2 until the CLI accepts
expressions), so the rows become real `eq` comparisons the moment the wiring lands.

## The fixture

One directory per behaviour, so a case can only fail for the reason it tests:

| tree | contents |
| --- | --- |
| `plain/` | LF text: ASCII, UTF-8 (`café`, `naïve`), an empty file, a file without a trailing newline, a tab, and a 1 MiB single line whose only match is at the end |
| `crlf/` | CRLF line endings |
| `enc/` | Latin-1 (invalid UTF-8), a binary file with NUL bytes, and UTF-16LE with a BOM |
| `tree/` | `.gitignore` (root and nested), `.ignore`, ignored/builtin-cache directories (`build/`, `node_modules/`), hidden files and directories |
| `strict/` | root-level ignore rules with no subdirectories, so no directory walk can change them |
| `dot/`, `links/`, `one/`, `wipe/` | a hidden `.rs` file, symlinks to a file and to a directory, a single-file tree, and a rule-less directory beside ignored siblings |
| `size/` | the tree `du` and the `find` expressions run against: 1–4 KiB files, a 100 KiB and a 3 MiB file, a 1 MiB holey file, an empty file, an empty directory, a hardlink pair, an `excl/` with a `.log` to exclude, and four files in `aged/` — fresh, 10 days old, 100 days old, and mode 640 |
| `rounding/` | four sub-KiB files (3, 5, 7 and 9 bytes), the tree that shows dusage rounding each file up while GNU `du` rounds the subtree total once |
| `many/` | one directory holding 400 files of 4096 bytes; allocation already equals apparent size per file, so the only number left to differ is the directory's own |
| `zip/` | the checked-in `tests/fixtures/` streams of every `-z` format with their plaintexts, copied here because the originals live in this repository where ignore rules apply, plus `gz.txt.gz` built with `gzip` |

Every directory under `tree/` that is not itself ignored carries an ignore file that matches
nothing. That was not decoration: the walker used to drop its parents' ignore rules whenever
it left a directory that had no ignore file, so a natural tree failed every ignore case for
that one reason. `wipe/` is the minimal tree that demonstrated it, and both it and `strict/`
now pass as ordinary `eq` cases.

## Catalogue: known differences

Every entry is a mismatch the suite expects to see. `verdict` column: **bug** is something
the tool gets wrong relative to its own documentation or to the invariant it claims,
**documented** is a deliberate semantic choice that the help text or module documentation
states, **gap** is missing compatibility with a flag or syntax the other tools accept.

| id | what differs | verdict | where |
| --- | --- | --- | --- |
| `binary-report` | A binary file that matches prints `Binary file <path> matches` on stdout; rg prints nothing and grep notes it on stderr. dusage follows grep's wording on the wrong stream. | documented | `src/report.rs:341` |
| `utf16-autodetect` | `-a` on a tree holding a UTF-16LE file: dusage detects the encoding and prints the text, grep and rg see NUL bytes and match nothing (or a binary line). Detection is the point of the feature, so the difference is expected. | documented | `src/search.rs` (`scan_file` sniff), `src/encodings.rs` |
| `crlf-strip` | Matching and echo strip the trailing `\r` of a CRLF line, so dusage prints `crlf needle` where grep and rg print `crlf needle\r`. | documented | `src/search.rs` (`strip_cr`), documented on `LineHit` |
| `crlf-anchor` | The same rule makes `$` match before the `\r`: `second line$` matches a CRLF line in dusage, not in rg or grep. | documented | `src/search.rs` (`strip_cr`) |
| `byte-dot` | The regex engine is byte-oriented, so `.` consumes one byte of a multi-byte character: `^caf.$` never matches a line that is exactly `café`, which rg matches as one character. | documented | `src/regex.rs` (documented semantics; a literal character compiles to its UTF-8 bytes) |
| `max-columns` | `--max-columns 20` truncates the line and prints it; rg omits the line and prints `[Omitted long matching line]`. | documented | `src/search.rs` (truncate) and the help text |
| `type-plus` | `--type-add 'rust:+*.md'` extends an existing type, as ripgrep's documentation describes; ripgrep 14.1 stores `+*.md` as a glob that matches nothing. | documented | `src/cmd/search.rs` (`Types::add`) |
| `du-allocation` | Plain GNU `du` reports allocated blocks (`st_blocks`); dusage reports apparent size (`st_size`), so every file smaller than its allocation unit, and every hole in a sparse file, reads differently. The `size/` fixture carries a 1 MiB holey file so the gap is structural, not a rounding coincidence. Measured on the `size/` fixture: GNU `du -k size` 3236, `du --apparent-size -k size` and `dusage du -k size` both 4228 — the hole alone accounts for the inversion. | documented | `src/cmd/du.rs` (module doc, SIZES) |
| `du-dir-inode` | Directory entries contribute zero to a dusage total; GNU `du` counts the blocks a directory inode allocates. `many/` holds 400 files of 4096 bytes, so allocation already equals apparent size per file and the remaining 8 KiB is the directory alone: GNU `du -k many` 1608, `dusage du -k many` 1600. | documented | `src/cmd/du.rs` (module doc, SIZES) |
| `du-block-rounding` | dusage rounds each *file* up into its block, so a file's own line and the directory above it agree; GNU `du --apparent-size -k` rounds a subtree's *total* up once. `rounding/` is four files of 3, 5, 7 and 9 bytes: GNU prints 1 KiB, dusage 4. `-b` (exact bytes) and `-m` (total converted once, as GNU does) are unaffected and are `eq` rows. | documented | `src/cmd/du.rs` (`Options::units`, `size_text`) |
| `findexpr-unwired` | GNU find's expression language (`-name`, `-type`, `-size`, `-mtime`, `-perm`, `-prune`, `-printf`, `-o`, `!`, parentheses, ...) exists as `src/findexpr.rs`, a library with no CLI caller: `dusage find` still takes a glob positional and answers the first expression token with "unknown option", exit 2. The 24 `find_expr_*` rows hand the same argv to both tools and flip to `eq` automatically once the CLI accepts expressions. | gap | `src/findexpr.rs`, `src/cmd/search.rs` (`FIND_HELP`) |

Everything the first pass of this suite catalogued as a **bug** is fixed and is now an `eq`
case: the ignore-layer wipe on a rule-less directory (`ancestor-wipe`), symlinked files being
followed, a dropped file name when one file matches, `--` separators without context flags and
between files, `-m 0`, ignore re-inclusion through `-g`, hidden files named by a glob, ignore
files above the scan root, UTF-16 transcoding, `--max-depth` counting, `-C1` and other attached
short values, and POSIX bracket expressions. They stay in the harness as `eq` rows, so they
would fail the run if they broke again.

`doc` cases pin dusage's own contracts rather than a comparison: `search ""` exits 2, an
unknown POSIX class exits 2, and a successful `-C1` search prints output.

## Adding a case

1. Add a row to the relevant table in `scripts/differential.sh`. The runner comes first:
   `content_case <name> <expect> <tools> <tree> <target> <pattern> <dusage> <rg> <grep>`,
   `du_case <name> <expect> <tree> <target> <dusage-flags> <du-flags>`,
   `find_expr_case <name> <expect> <tree> <expr> [nul]`,
   `zip_case <name> <expect> <fmt> <packed> <needle> <dusage-flags> <rg-flags>`, or
   `find_case <name> <expect> <tree> <target> <dusage-find-args> <find-args>`.
   `tools` is `3`, `rg` or `grep`; `expect` is `eq`, `known:<id>`, `doc:<id>` or `expr`.
   Flags, patterns and expressions are shell words: split on spaces, never globbed, so quote
   anything with a metacharacter. A `-z` case also needs a `zip_probe <fmt> <packed> <plain>
   <needle>` line in its section, or it is skipped as an unknown format.
2. Mirror a `content_case` or `find_case` row in `tests/differential.rs` (`case(...)` in
   `corpus()`, `FindCase` for `find`). The `du`, `find`-expression and `-z` groups have no
   Rust mirror yet, so a row there runs from this script only.
3. Run `scripts/differential.sh` and `cargo test --test differential`; both must pass.
4. When a `known:` case starts agreeing, the suite prints `FIXED [name]`: delete the entry
   from the table here and the row from both harnesses.

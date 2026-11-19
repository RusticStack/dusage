# Differential correctness suite

dusage reimplements three external tools (`grep`, `ripgrep`, `find`) with a hand-written
regex engine, walker and ignore matcher, so its output is checked against those tools on a
fixture tree built from scratch by the suite itself.

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

The same corpus runs from `cargo test`, as `tests/differential.rs`, so CI covers it:

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

Cases come in three kinds:

| kind | meaning |
| --- | --- |
| `eq` | the outputs must agree; a difference fails the run |
| `known:<id>` | a catalogue entry below; the run expects a difference and prints `FIXED` once the outputs agree, so the entry gets retired |
| `doc:<id>` | a dusage contract, not a comparison: the exit code and silence its own help documents |

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

1. Add a row to the relevant table in `scripts/differential.sh`:
   `content_case <name> <expect> <tools> <tree> <target> <pattern> <dusage> <rg> <grep>`.
   `tools` is `3`, `rg` or `grep`; `expect` is `eq`, `known:<id>` or `doc:<id>`. Flags and
   patterns are shell words: split on spaces, never globbed, so quote anything with a
   metacharacter.
2. Mirror it in `tests/differential.rs` (`case(...)` in `corpus()`, `FindCase` for `find`).
3. Run `scripts/differential.sh` and `cargo test --test differential`; both must pass.
4. When a `known:` case starts agreeing, the suite prints `FIXED [name]`: delete the entry
   from the table here and the row from both harnesses.

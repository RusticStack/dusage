# Search parity: `dusage search` and `dusage find` against grep, ripgrep and find

`dusage search` (aliases `grep`, `rg`) matches file contents, and `dusage find`
(alias `ls`) matches paths, both behind the same ignore-aware walker. This
document is a map of what they do relative to GNU grep, ripgrep and GNU find:
which flag means what, which syntax works, where the semantics silently
differ, and what would bite someone swapping one for the other.

**Evidence base.** Everything stated about this tool is read from the source of
branch `docs/truth` at commit `c9dd6a0` (0.5.0) and, wherever the claim is about
behaviour, was reproduced by running `./target/release/dusage` built from that
commit on Linux x86_64 (a 2-vCPU Ubuntu 24.04 container, warm page cache).
Source claims name the module and the function, not a line number, because
lines move between commits. Claims about the *other* tools come from their own
documentation (cited inline) and from probing the local binaries: ripgrep
14.1.0, GNU grep 3.11, GNU findutils 4.9.0 and GNU coreutils 9.4 (`du`).
Performance numbers are the ones in `docs/BENCHMARKS.md` and are marked as such
there; every other number here was measured on the machine above.

The parser itself is shared with the rest of the tool: `Args::parse`
(`src/cli.rs`) accepts `--name value` and `--name=value`, a bare `--`
terminator, an attached short value (`-A2`, `-m5`, `-g*.rs`, `-tjs`), and it
treats a lone `-` or a negative number as a positional. Short flags cluster, as
in ripgrep: `-in` is `-i -n`, `-nm1` is `-n -m 1`, `-inA2` is `-i -n -A 2`, and a
value flag at the end of a bundle takes the next argument (`-g *.rs` is `-g*.rs`).
An unknown character anywhere in a bundle is an error naming the whole argument.
A repeated *boolean* flag is idempotent.
Repeated *value* flags keep their whole history (`Args::values`,
`src/cli.rs`), and what each caller does with it is its own business: `-e` and
`-f` are joined into one alternation, `-g`, `--iglob`, `-t`, `-T`, `--include`,
`--exclude`, `--exclude-dir`, `--pre-glob` and `--ignore-file` combine, and
`--size`, `--older`, `--newer`, `--max-depth`, `--max-filesize`,
`--max-columns`, `--encoding` and `--sort` take the last. The option lists are
`SEARCH_VALUE_FLAGS` / `SEARCH_BOOL_FLAGS` (`src/cmd/search.rs`) for `search`
and an inline pair in `run_query` for `find`; an unrecognised flag is `exit 2`,
not a warning, and so is a flag that `find` does not take.

## 1. Flag map

### 1.1 `dusage search`

| Our flag | grep / rg / find equivalent | Notes |
|---|---|---|
| `<pattern>` positional, `-e`, `--regexp`, `-f`, `--file` | `grep PATTERN`, `rg PATTERN`, `rg -e`, `grep -f` / `rg -f` | `-e` takes the next argument, so a pattern starting with `-` can be passed, and it repeats: every pattern is an alternative (`alternation`, `src/cmd/search.rs`). `-f PATH` adds the patterns in a file, one per line, blank lines skipped and CRLF accepted, and repeats; `-f -` reads them from standard input (measured: a file holding `foo`, a blank line and `bar` makes `-c` report 2 on a two-line haystack, one per pattern). |
| `-F`, `--fixed-strings` | `grep -F` / `rg -F` | Forces literal matching. A regex the engine can prove is a literal already takes the byte fast path without `-F` (`Pattern::fast_literal`, `src/pattern.rs`). |
| `-i`, `--ignore-case`, `-s`, `--case-sensitive`, `-S`, `--smart-case` | `rg -i`, `rg -S`; grep's `-s` means something else | The last case mode wins, as in ripgrep. Smart case folds only when the pattern holds no uppercase letter. Folding is Unicode simple folding on any line or pattern that needs the character program, and per-byte ASCII folding otherwise — see §2.3. |
| `-P`, `--pcre2`, `--engine auto\|default\|pcre2` | `rg -P`, `rg --engine` | `-P` forces the backtracking engine, which is the only path to lookaround and backreferences; `auto` (the default) falls back to it for a pattern the linear engine rejects, and `default` refuses that fallback with a message naming `-P` (measured: `--engine default '(a)\1'` is exit 2, the same pattern without it matches). `pcre2` is `-P`; any other value is `unrecognized regex engine`, matching `rg --engine bogo`. The engine's subset and its step budget are in §2.2. |
| `--column`, `--byte-offset`, `--heading`, `--no-heading` | `rg --column`, `rg -b`, `rg --heading`, `rg --no-heading` | Prefix order is ripgrep's: line, column, byte offset, then the text. There is no `-b` short form; `-b` is an unknown option here (measured) while `--byte-offset` prints the line's offset in the file (`--byte-offset 'second'` on a two-line file prints `18`). `--heading` prints the file name once and a blank line between files; `--no-heading` is its opposite and the last of the two wins. `--column` with `-o` prints the match's own column (`--column -o foo` on `alpha foo bar foo` gives `7` and `15`). |
| `--vimgrep` | `rg --vimgrep` | `-n --column` with one row per match: `path:line:column:text`, the whole line repeated per match (measured: `a.txt:1:7:alpha foo bar foo` and `a.txt:1:15:…`). It turns the file name on by itself, as rg does. |
| `--trim` | `rg --trim` | Drops the leading whitespace of each printed line; `--max-columns` then measures what is left. The column still counts from the start of the untrimmed line, which is rg's answer. |
| `-M N`, `--max-columns N`, `--max-columns-preview` | `rg -M`, `--max-columns`, `--max-columns-preview` | Truncates the printed line to N **characters** — the cut takes `char_indices().nth(N)` (`src/search.rs`), so 20 `é` at `-M 5` keep five characters, ten bytes (measured) — although the help text says bytes. `--max-columns-preview` appends `[... omitted end of long line]` (measured). There is no "N matches hidden" note; rg drops an over-long line instead of truncating it, this tool truncates. |
| `--type-add`, `--type-clear`, `--type-list` | `rg --type-add`, `--type-clear`, `--type-list` | A type is a set of globs in a table the flags can edit: `--type-add 'web:*.html'` defines one (measured: `-t web` then lists the `.html` file), naming an existing type adds to it, and `--type-clear rust` forgets one so that `-t rust` is then an unknown type (measured, exit 2). `--type-list` prints the table as edited. |
| `-t NAME`, `--type`, `-T NAME`, `--type-not` | `rg -t`, `rg -T` | A fixed list of types mapped to extension globs (`ext_globs`, `src/cmd/search.rs`): rust, python, js, ts, c, cpp, go, java, ruby, markdown, json, toml, yaml, sh, html, css, xml, txt, log, lock. **Different from `-g`**: a type filter narrows results and leaves the ignore rules in force (measured: `-t js` finds nothing where the only `.js` files sit under `node_modules` or a gitignored `build/`). `-T` excludes a type. |
| `-g PATTERN`, `--glob`, `--iglob` | `rg -g`, `rg --iglob` | Repeatable, and the repeats combine: `-g '!x' -g '*.rs'` means "not x, and a `.rs` file". `!` negates. A **positive** glob switches the ignore layers off entirely, exactly as in rg, so it can re-include a path a `.gitignore` hid and it descends into `node_modules` (measured: `-g '*.js'` lists both the ignored `build/x.js` and `node_modules/p/i.js`). A run of only negative globs keeps them on. `--iglob` is `-g` without case. |
| `--include`, `--exclude`, `--exclude-dir` | `grep --include` and friends | `--include` is a narrowing glob (what `-t` does), `--exclude` is `-g '!…'`, and `--exclude-dir` adds the trailing slash that makes the pattern match a directory only. All three repeat. Measured: `--exclude-dir d1` leaves `d2` and the root files. |
| `-I`, `--binary-files TYPE` | `grep -I`, `grep --binary-files` | `binary` (the default: the file is named, not printed), `text` (`-a`), or `without-match`. `-I` is `without-match`, so a NUL-carrying file is not even a match, while the default reports `Binary file bin.dat matches` and exits 0 (both measured). **Not ripgrep's `-I`**, which is `--no-filename` ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-I, --no-filename`). |
| `-d`, `--directories ACTION` | `grep -d` | `recurse` (the default) or `skip`, which stops the walk descending. `read` is refused: dusage reads files, never directories. |
| `-w`, `--word-regexp` | `grep -w` / `rg -w` | Boundaries are `[0-9A-Za-z_]` plus every byte `>= 0x80` (`is_word_byte`, `src/bytes.rs`), so a non-ASCII letter counts as word text: `-w caf` does not match inside `café`, and neither does `-w caf.` (the span ends after `0xC3`), while `-w 'caf..'` does because two dots consume the whole character (measured). |
| `-v`, `--invert-match` | `grep -v` / `rg -v` | Selects lines with no match. `-o` and colour spans are suppressed when inverted. |
| `-n`, `--line-number` | `grep -n` / `rg -n` | Off by default, including on a TTY. ripgrep differs: on a TTY it enables line numbers, colours and headings by default ([rg(1)](https://man.archlinux.org/man/rg.1.en), DESCRIPTION). |
| `-H`, `--with-filename` / `-h`, `--no-filename` | `grep -H` / `grep -h` | Without either, the file name is printed when the run has more than one file (`-H` forces it, `-h` suppresses it; both measured). `-h` is *never* help here; `--help` is. GNU grep makes the same choice; ripgrep does not — its `-h` prints help ([rg(1)](https://man.archlinux.org/man/rg.1.en)). |
| `-l`, `--files-with-matches` / `-L`, `--files-without-match` | `grep -l` / `grep -L` | `-l` prints paths only; `-L` prints the paths with no match, exit 0 when at least one is printed. **Not ripgrep's `-L`**: in rg, `-L` is `--follow` ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-L, --follow`). The collision stands; the long spelling `--follow` is the follow option here. |
| `-c`, `--count` / `--count-matches` / `--include-zero` | `grep -c` / `rg --count-matches` / `rg --include-zero` | `-c` counts matching *lines* per file; `--count-matches` counts individual matches, so a line holding `needle needle` counts twice (measured: `--count-matches foo` on `alpha foo bar foo` prints `2`). `--include-zero` also prints the files that matched nothing (measured: `--include-zero` prints `./big.bin:0`). |
| `-o`, `--only-matching` | `grep -o` / `rg -o` | One output line per match, capped at 1024 matches per input line (`MAX_PER_LINE`, `src/search.rs`). Measured: a 3000-byte line of `x` with pattern `x` prints 1024 lines. |
| `-q`, `--quiet` | `grep -q` / `rg -q` | Prints nothing and stops at the first matching file. That path is sequential — it does not use the thread pool. |
| `-m N`, `--max-count N` | `grep -m` / `rg -m` | Per file, in matching lines; the file stops being read when the limit is reached. |
| `-A N`, `-B N`, `-C N`, `--after-context`, `--before-context`, `--context`, `--stop-on-nonmatch` | `grep`/`rg` same letters; `rg --stop-on-nonmatch` | `-C` sets both and each falls back in the order `-A`/`--after-context`, then `-C`. Non-adjacent groups are separated by a context separator, and only when a context window was asked for. `--stop-on-nonmatch` ends a file at the first line after a match that does not match, which bounds an `-A` window. |
| `--context-separator SEP`, `--group-separator SEP`, `--no-context-separator` | `rg --context-separator`, `rg --no-context-separator`; `grep --group-separator` | The line between non-adjacent context groups, `--` by default. `--group-separator` is grep's spelling, accepted for the same switch (rg 14.1 rejects it). `--no-context-separator` prints nothing between groups while an empty separator prints a blank line, which is rg's answer as well. |
| `--field-match-separator SEP`, `--field-context-separator SEP`, `--path-separator SEP` | the same flags in rg | Between the fields of a matching line (`:` by default) and of a context line (`-`); `--path-separator` is one byte, printed wherever a path holds `/` or `\`, and anything longer is exit 2, which is rg's rule. All three apply to match rows, `--vimgrep`, `--heading`, `-l`/`-L`, `-c` and the binary notice. |
| `-a`, `--text` | `grep -a` / `rg -a` | Binary files are always *searched*; the flag only replaces the one-line `Binary file … matches` summary with normal output. ripgrep's default is to stop at the NUL ([GUIDE](https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md#automatic-filtering)). |
| `-j N`, `--threads N` | `rg -j` | Default is one worker per core, clamped to 4..16, with 8 on a machine that will not report a core count (`src/cli.rs`). Output order does not depend on it — see §5.3. |
| `--max-depth N`, `--max-filesize SIZE` | `find -maxdepth`, `rg --max-filesize` | `--max-depth N` is `find -maxdepth N` exactly: measured on a three-level tree, `--max-depth 1` lists the root's children and `--max-depth 2` the grandchildren, the same set `find -maxdepth 1` and `-maxdepth 2` print. `--max-filesize` is applied during the walk from the size the enumerator already returned, so an oversized file is never opened; a file named explicitly on the command line bypasses it. Sizes accept `K/M/G/T`, binary multiples. |
| `--encoding NAME` | `rg -E`, `--encoding` | `auto` (default), `utf-8`, `utf-16le`, `utf-16be`, `latin1`; an unknown name is exit 2 with the list (`dusage: unknown encoding "klingon" (auto, utf-8, utf-16le, utf-16be, latin1)`). Search always runs on UTF-8 bytes, so a forced encoding or a detected UTF-16 file is transcoded first. Auto-detection answers only UTF-8, UTF-16LE or UTF-16BE — Latin-1 is never guessed, it must be asked for. **Not ripgrep's `-E`**: rg's `-E` is `--encoding` ([rg(1)](https://man.archlinux.org/man/rg.1.en)), while `-E` here is grep's `--extended-regexp`, so `rg -E utf-16le` reaches this parser as a regex dialect flag and a pattern of `utf-16le`. |
| `-Z`, `--null`, `-0` | `rg --null` / `grep -Z` | A NUL instead of `:` after the file name, for `xargs -0`. |
| `--null-data` | `rg --null-data` | Records are separated by NUL instead of a newline, and the output ends each record the same way, so `dusage search --null-data … \| xargs -0` is safe (measured: the `-o` output ends in `\0`). A record is cut in memory rather than streamed, since the line scanner is built around a newline; `-c`, `-l`, `-o` and `-n` count and print by record. |
| `-z`, `--search-zip` | `rg -z` | Expands a compressed file and searches the contents. gzip, bzip2, lz4, xz and zstd are recognised by their magic and brotli by its `.br` extension, since it has no signature; every member of a multi-member file is expanded in order. Measured with one file per format: `search -z -l needle` lists all six and the uncompressed original. Each format has its own decoder written in std — `src/archive.rs` (gzip/DEFLATE and the CRC-32 it checks with), `src/archive_bz2.rs`, `src/archive_lz4.rs`, `src/archive_xz.rs`, `src/archive_zstd.rs` and `src/archive_brotli.rs`. A member that starts with the right magic and then ends early is reported as an unreadable path and exits 2 (measured on a truncated gzip); a file without the magic is simply searched as text, so a bogus `.gz` is a normal non-match. |
| `-U`, `--multiline`, `--multiline-dotall` | `rg -U`, `rg --multiline-dotall` | Searches the whole file at once, so a match can contain a newline (`-U -n 'fn \w+\(\) \{\n'` prints `1:fn a() {` on a three-line Rust file); `--multiline-dotall` widens `.` to a newline (`-o 'fn.*\}'` prints the whole function). The bytes have to be in memory, which is the price of the mode; every other mode streams. `-v` and `-A`/`-B`/`-C` are refused rather than answered wrongly (`-U cannot be combined with -v/--invert-match`), and a pattern spelling a newline is refused without `-U`, as ripgrep refuses it. |
| `--follow` | `rg --follow` (`rg -L`) | Follows symbolic links, walking each real directory once: the chain of real paths above a directory is compared before descending, so a link pointing back up the tree is skipped instead of recursing (measured: with `real/up -> ..` in the tree the run still lists each file once). `-L` is not the short form here, because `-L` already means `--files-without-match`; the long spelling is the only one. A link whose target is gone is reported as an unreadable path rather than skipped silently, and a cycle is skipped without ripgrep's "File system loop found" line. |
| `--passthru` | `rg --passthru` | Prints every line, with matches marked as hits and the rest as context, so a file with no match still prints in full while the exit status stays 1. |
| `--sort KEY`, `--sortr KEY`, `--sort-files` | `rg --sort`, `rg --sortr`, `rg --sort-files` | `path` (the default, and what `--sort-files` means), `none` for the order the walk produced, or `modified`, `accessed`, `created` for oldest-first by that time. Any other key is refused with exit 2 and the list (`unsupported --sort key "bogus"`). A time sort costs one `stat` per result. |
| `--files` | `rg --files` | With no pattern, lists the files a search would read: the same walk and the same ignore rules, printed root-relative with no `./` prefix (`a.txt`, not `./a.txt`; measured). `-t`/`-g`/`--hidden`/`--no-ignore` all apply. |
| `-r`, `--replace TEXT` | `rg -r` / `grep -r` | Prints each line with its matches replaced. `$0` is the whole match, `$1`..`$99` (or `${1}`) a capture group, `$$` a dollar; grep's `\1` spelling works too (measured: both `$1: $2` and `\1 -> \2` rewrite `name=value`). The replacement is what gets highlighted and what `-o` prints. A reference the pattern cannot satisfy is refused with exit 2. Group references need the default engine: `-P` does not record groups, so `-P -r '$1'` is refused (`-r cannot use capture groups with -P/--pcre2`) while replacing with `$0` works. |
| `--pre COMMAND`, `--pre-glob` | `rg --pre`, `rg --pre-glob` | Runs `COMMAND PATH` and searches what it prints, which is how a PDF or an office document becomes text without this tool knowing the format. The command is run directly with the path as its only argument, not through a shell, so a pipeline belongs in a script of your own. `--pre-glob` limits which files it runs on; the rest are searched as they are. A command that fails with no output is an error, reported like an unreadable path. Measured: a `.bin` holding `needle` is listed only through `--pre`. |
| `--crlf` | `rg --crlf` | Makes the `\r\n` terminator explicit: `$` matches before the `\r`, and the `\r` is never printed. This scanner already reads it that way everywhere, so the flag changes nothing here — where ripgrep does the opposite, treating a `\r` as content until the flag is given. |
| `--mmap`, `--no-mmap`, `--binary`, `--line-buffered`, `--block-buffered` | the same flags in rg | Accepted and ignored, because each already describes what happens here: files are read in blocks rather than mapped, a binary file is reported by name rather than printed, and the output buffer is the caller's. They exist so a script written for ripgrep runs unchanged. |
| `--hidden` | `rg --hidden` / `rg -.` | Only governs names starting with `.`; it does not disable ignore rules. |
| `--no-ignore`, `-u`/`--unrestricted` | `rg --no-ignore`, `rg -u` | `--no-ignore` disables both the built-in cache list and `.gitignore`/`.ignore`/`.git/info/exclude`/the global excludes file. `-u` does the same and `-uu` also takes hidden paths (measured). A third `-u` changes nothing observable here — binary files are always searched, and what `-a` replaces is the `Binary file … matches` notice, which `-uuu` does not (`-uuu needle b.dat` on a NUL-carrying file still prints that notice). Hidden files are still skipped under `--no-ignore`, exactly as in rg, where `--hidden` is a separate flag. |
| `--no-ignore-vcs`, `--no-ignore-dot`, `--no-ignore-parent`, `--no-ignore-files`, `--no-ignore-global` | `rg --no-ignore-vcs` and friends | Each removes one source of ignore rules: `.gitignore` and `.git/info/exclude`, `.ignore`, the rule files stacked from a root's ancestors, all of them at once, or git's global excludes file. See §3. |
| `--ignore-file PATH`, `--ignore-file-case-insensitive` | `rg --ignore-file` | Stacks a file of the caller's own on top of the ignore layers; repeatable, read once per walk, and the case-insensitive switch folds it. Measured: `--ignore-file myignore` with `a.txt` in it hides `a.txt` from the walk. |
| `-x`, `--one-filesystem` | `rg --one-file-system`, `find -xdev` | **Not grep's `-x`** (`--line-regexp`) and not rg's `-x`, which rg also spells line-regexp ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-x, --line-regexp`). The whole-line mode is `--line-regexp` here, with no short form. `-x` stays on the root's device. |
| `--line-regexp` | `grep -x` / `rg -x` | Wraps the pattern in `^(?:...)$`. There is no short form because `-x` here means `--one-filesystem`. |
| `--json` | `rg --json` | Different shape: one JSON document for the whole run, not JSON Lines. See §4.2. |
| `--color[=WHEN]` | `grep --color`, `rg --color` | `always`/`yes`/`true`, `never`/`no`/`false`, `auto`. Only the `--color=WHEN` spelling is parsed; `auto` — and an unrecognised WHEN, which is not an error — consults `is_terminal()` on stdout, so a pipe stays plain. |
| `--stats` | `rg --stats` | One summary line on **stderr**, not stdout: `dusage: 1 files scanned (1 binary), 1 matched, 1 matches, 0 errors, 712.99µs` (measured). |
| `--no-messages` | `rg --no-messages` | Drops the "path(s) could not be read" line. |
| `--help`, `-V`, `--version` | — | `--help` prints the help text (`-h` does not; it is `--no-filename`). `-V` prints `dusage <version>`. |

Not present on `search`, and rejected as unknown options: `--ext`, `--size`,
`--older`, `--newer`, `--dirs`, `--long`, `--empty`, `--mindepth` (find-side
only, per the `SEARCH_VALUE_FLAGS`/`SEARCH_BOOL_FLAGS` lists).

### 1.2 `dusage find`

`find` has two forms. The glob form is the table below: a glob or a substring plus
a set of walk filters. The expression form is GNU find's own language, entered when
an argument is expression syntax (see "The expression form" after the table).

| Our flag | grep / rg / find equivalent | Notes |
|---|---|---|
| `<glob>` positional | `find -name`, `find -path` | A glob with no `/` is matched against the entry's file name, gitignore-style; one with `/` is anchored at the root (`src/glob.rs`). Supports `*`, `?`, `**`, `[...]`, `[!...]`, `{a,b}` expansion, `\` escapes and a trailing `/` for directories. Prefix `!` inverts. Measured: `'*.rs'` prints `src/main.rs`, `'**/*.txt'` prints `src/sub/a.txt` and `zero.txt`. |
| `[paths...]` | find starting points | Defaults to `.`. |
| `-F`, `--fixed` | `grep -F`, `find -name 'x'` | Substring match on the relative path instead of a glob (measured: `-F sub` prints `src/sub` and `src/sub/a.txt`). |
| `-t NAME`, `--type` | `rg -t`, `find -name '*.rs'` | Same fixed list and table as `search`, repeatable. A type filter also implies files-only: `find -t js` never lists directories. |
| `--ext rs,md` | `find -name '*.rs' -o -name '*.md'` | Comma-separated, translated to `*.ext` globs and combined with any `-t`. |
| `--size +10M` / `-10M` / `10M` | `find -size +10M` | Ours is exact: `+10M` is `size >= 10 MiB`. GNU find rounds *up* to the unit, so its `+10M` excludes a file of exactly 10 MiB while ours includes it — measured again on this machine: of a 10 MiB file, `find -size +10M` printed nothing and `find --size +10M` printed it. A bare `10M` here means within ±5%, which find has no equivalent for; an 11 MiB file falls outside that window (measured). Only the last `--size` is honoured despite the help text calling it repeatable (`--size +10M --size -1K` behaves as `-1K` alone). |
| `--older 7d`, `--newer 7d` | `find -mtime +7`, `find -mtime -7` | Ours compares the exact elapsed duration with `>` and `<`; `find -mtime +7` works in whole 24-hour periods. Units: `s m h d w`, or a bare number meaning days. |
| `--newermt`, `--newerat`, `--newerct` | `find -newermt`, `-newerat`, `-newerct` | Keep what was modified, accessed or changed after an absolute date: `YYYY-MM-DD`, `YYYY-MM-DD HH:MM[:SS]`, `YYYY-MM-DDTHH:MM:SS`, or `@SECONDS`. Dates are read as UTC, since a time-zone library is a dependency this tool does not have; `-d '2 days ago'`-style date arithmetic is `--newer 2d` instead. |
| `--empty` | `find -empty` | A file of no bytes, or a directory with no entries (measured: prints `empty` and `zero.txt` and nothing else). A directory costs one `read_dir` to answer, which is why the flag is opt-in. |
| `--dirs`, `--files` | `find -type d`, `find -type f` | Directories only, or files only. |
| `--long` | `find -ls`, `find -printf '%s %TY… %p\n'` | Prints size, modification time in UTC, and the relative path (`         2 B  2026-11-19 20:35:05  src/main.rs`, measured). Directory rows show size `0 B` and a `-` timestamp, because the enumerator reports 0 for directories and the walk passes `None` for a directory's mtime. |
| `--max-depth N`, `--mindepth N` | `find -maxdepth`, `find -mindepth` | Both are find's own numbering: `--max-depth 1` lists a root's children, `--mindepth 2` starts at the grandchildren (both measured against findutils 4.9.0 on the same tree). |
| `-g PATTERN`, `--glob` | `find -path`, `find -prune` | Same include/`!`-exclude filter as `search`, and the same override rule: a positive glob switches ignore rules off. `find` has no `--include`/`--exclude`/`--exclude-dir`, despite what its help text lists. |
| `-i`, `--ignore-case` | `find -iname` | Folds the glob, or the `--fixed` substring, without changing what it is matched against. |
| `--name`, `--full-path` | `find -name` / `find -path` | `--name` matches the file name alone, `--full-path` the whole path below the root. Without either, the pattern decides: no `/` means name, a `/` means path — `find -name` and `find -path` in one flag. |
| `--sort KEY`, `--sortr KEY` | `find` has no order flag | `path` (the default), `none`, `modified`, `accessed`, `created`, oldest first; `--sortr` reverses. The same keys and the same exit-2 refusal as `search`. Ignored by an expression walk, which prints in its own order. |
| `--follow` | `find -L` | Follows links and lists what they point at, walking each real directory once. Without it a link is not listed at all — neither the link nor its target — where GNU find lists the link itself. |
| `--hidden` | no find equivalent | GNU find lists dotfiles like any other name; we skip them unless this flag is given. |
| `--no-ignore`, `--no-ignore-*`, `--ignore-file`, `--ignore-file-case-insensitive` | no find equivalent | The ignore layers apply to `find` exactly as they do to `search` (§3). |
| `-x`, `--one-filesystem` | `find -xdev` | Device comparison per directory. |
| `-Z`, `--null` | `find -print0` | NUL after each path instead of a newline (measured with `od -c`). |
| `--allow-exec` | no find equivalent (find always runs `-exec`) | Permits `-exec`/`-execdir` in an expression. Without it both are refused before any output, with exit 2. |
| `--json` | no find equivalent | `{"paths":[…],"count":N}` — see §4.3. Refused with exit 2 when an expression is given, because the actions decide what is printed. |
| `--color[=WHEN]` | `ls --color`-ish | Colours the printed path only, in both forms. |
| `-u`, `--unrestricted` | `rg -u` | `-u` drops the ignore rules, `-uu` also takes hidden paths (both measured on a tree holding `node_modules` and a `.gitignore`). `find` never reads contents, so the third `-u` has nothing to add. |
| `--no-messages` | `rg --no-messages` | Drops the "path(s) could not be read" line. |
| `--help`, `-V`, `--version` | — | `find -h` is rejected as an unknown option here, unlike `search`, where `-h` is `--no-filename`. |

**Advertised but rejected.** `find --help` still lists nine flags that the
parser does not accept: `--include`, `--exclude`, `--exclude-dir`, `--pre`,
`--pre-glob`, `--binary-files`, `-I`, `-d`/`--directories` and `--iglob`. Each
is exit 2 (measured, one flag per run): eight with `unknown predicate`, and `-d`
by starting an expression, which makes the `*` after it a misplaced path
(`paths must precede expression`). Only `-g`/`--glob` and `-t`/`--type` narrow a
globbing `find` walk; the rest of that block is a copy of the `search` help.

#### The expression form

A run is an expression when one of its arguments is expression syntax: a token
that starts with `-` and is not one of our own flags, or a bare `!`, `(` or `)` —
exactly what `dusage find --help` says. The positionals before that token are the
starting points and everything from it on is the expression, which is find's own
order: `dusage find . -name "*.rs" -o -type d` is the find command line. With no
expression token in sight the glob form runs exactly as it did. Our own flags are
recognised wherever they appear, except inside a `-exec` command, where every
token belongs to the command; using one as a predicate is refused with a message
naming the expression spelling (`--size does not combine with a find expression;
use -size`, measured), so a mistyped flag cannot silently change what is printed.
The split is `split_find_args` in `src/cmd/search.rs`; the language, evaluator and
`printf` renderer are `src/findexpr.rs`; the walk is `search::walk_expr` in
`src/search.rs`, sequential by nature because `-prune` and `-depth` decide the next
step from the entry just seen.

| Expression | Equivalent / notes |
|---|---|
| `-name GLOB`, `-iname`, `-path`, `-ipath`, `-wholename`, `-iwholename` | find's globs, against the name or the whole path as printed (the starting point included). `-iname` folds ASCII only. |
| `-regex RE`, `-iregex` | Compiled by this crate's engine (`src/regex.rs`), wrapped as `\A(?:…)\z`. The dialect is not Emacs': bare `( ) \| + ? {n,m}` are operators and `\(` is a literal paren, so a pattern written for find may need its punctuation rewritten. `-regextype` is validated against findutils' names but does not change the dialect. |
| `-type f\|d\|l`, `-xtype` | `-type b`, `-c`, `-p` and `-s` are refused at parse time (measured: `-type b: the walk never opens special files, so it cannot tell a block device from any other node`). `%y` and `-ls` still report `p`, `s`, `c`, `b` where `lstat` can tell. |
| `-size n[cwbkMG]`, `-empty` | find's rounding: the size is divided by the unit, rounded up, and `+n`/`-n` compare that count. |
| `-mtime`, `-atime`, `-ctime`, `-mmin`, `-amin`, `-cmin` | find's two roundings: day tests count whole days down, minute tests round up. |
| `-newer`, `-anewer`, `-cnewer`, `-newerXY`, `-newermt` | A reference file or an absolute date, read as UTC. |
| `-perm`, `-user`, `-group`, `-uid`, `-gid`, `-nouser`, `-nogroup`, `-links`, `-inum`, `-samefile`, `-fstype`, `-used` | Octal `-perm` only; symbolic modes are refused (measured: `invalid mode`, then the reason that symbolic modes like `u+w` are not supported). Names resolve through `/etc/passwd` and `/etc/group`, read once per run. `-fstype` answers from `/proc/self/mountinfo`. |
| `-a`/`-and`, `-o`/`-or`, `!`/`-not`, `(`, `)` | find's precedence: `!` tightest, then implicit-and, then `-o`. |
| `-maxdepth N`, `-mindepth N`, `-depth`, `-xdev`/`-mount`, `-follow`, `-noleaf` | Global options of the expression, applied after our flags of the same meaning, so the expression wins. |
| `-print`, `-print0` | The same path printer as the glob form, so `--color` reaches it and the terminator is the one asked for. |
| `-printf FORMAT` | `%p %f %h %P %s %b %k %y %m %M %i %n %d %l %u %g %t %a %c` and `%TY %Tm %Td %TH %TM %TS`, `\n`-style escapes and `%%`; times are UTC. Measured identical to findutils 4.9.0 for `'%p %s\n'` and for `'%p %s %y %m %TY-%Tm-%Td\n'` on one entry; the full-run `%p %s` output differs only in order. |
| `-ls` | find's column layout (inode, 1K blocks, mode, links, user, group, size, time, path), except the date column, which is this tool's `YYYY-MM-DD HH:MM:SS` where find prints `Nov 19 20:51` (measured). |
| `-exec CMD … ;`, `-exec CMD {} +`, `-execdir` | **The one write path in this tool.** Refused as a whole, before any output, with exit 2 unless `--allow-exec` is given (measured); `-delete` is refused whatever the flag says. Without a shell, every `{}` is replaced by the entry's path, and a trailing `{} +` collects one batch at the end (measured: same batch contents as find, different order). `-execdir` runs in the entry's directory with `./name`. The command's exit status is the predicate's value. |
| `-prune`, `-quit` | `-prune` stops the descent into the directory that matched it and is a no-op under `-depth`; `-quit` ends the run at once. Exit status is 0 when something was printed or the expression was true, 1 when neither, 2 on an error (all measured). |
| `-true`, `-false` | Constant predicates. |
| `-delete`, `-ok`, `-okdir`, `-daystart`, `-fprintf`, `-fls`, the comma operator, `-newerXY` with `Y = B` | Refused at parse time with a message naming the token. `-delete` says so in its own words: it never deletes from a find expression and points at `dusage prune`. |

Parity measured against findutils 4.9.0 on a fixture with four directories, files
of 0 bytes to 200 KB, dates from one to five days old and a gzip file, each
command run once through each tool:

```
$ dusage find . -name '*.gz' -type f        # identical output
$ dusage find . -size +100k                 # identical output
$ dusage find . -name skipme -prune -o -type f -print   # identical once sorted
$ dusage find . -printf '%p %s\n'            # same set, different order
$ dusage find . -maxdepth 2 -type d         # same set, different order
$ dusage find . -mtime -3 -type f           # same set, different order
```

The order difference is deliberate: the walk sorts per directory so two runs of
one walk print the same thing, where find prints in readdir order. `-ls` and the
`-exec` batching differ the same way, in the date column and the argument order,
not in what they select.

### 1.3 ripgrep flags we do not have

Each of these is genuinely absent — none is spelled differently, none has a
partial stand-in in the parser.

| rg flag | What it does in ripgrep | What its absence means here |
|---|---|---|
| `-l` vs `--files-without-match` | rg spells the negative form `--files-without-match` and reserves `-L` for `--follow` ([rg(1)](https://man.archlinux.org/man/rg.1.en)). | We spell it `-L`. That is a direct letter collision: a script written for rg that passes `-L` to follow symlinks gets a list of files with no matches instead. |
| `-E` vs `--encoding` | rg's `-E` is `--encoding` ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-E, --encoding`). | `-E` here is grep's `--extended-regexp`, so `rg -E utf-16le` becomes a dialect flag plus a pattern of `utf-16le`. Use `--encoding utf-16le`. |
| `-I` vs `--no-filename` | rg's `-I` is `--no-filename` ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-I, --no-filename`). | `-I` here is grep's `--binary-files=without-match`. Use `-h` for no file name. |
| `--no-unicode` | Turns off Unicode-aware matching, so `\w`, `\d`, `\b` and case folding are ASCII ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--no-unicode`). | Supported. The pattern gets no character program, so `.`, `\w`, `\d`, `\s`, `\b`, negated classes, `-w` and `-i` keep their ASCII/byte meaning on every line, non-ASCII ones included; a non-ASCII character in the pattern is its UTF-8 bytes. `\p{..}` and a non-ASCII class member are refused with exit 2 instead of being narrowed. It does not reach `-P`: the backtracking engine has no Unicode switch. There is no `--unicode` to take it back. |
| `--glob-case-insensitive` | Folds every `-g` glows without case ([rg(1)](https://man.archlinux.org/man/rg.1.en)). | Absent; the per-file `--ignore-file-case-insensitive` switch exists, `--iglob` folds a single glob, and there is no global switch. |
| `--encoding` label set | rg accepts a few dozen labels and maps each to a codec ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--encoding`). | Five names only: `auto`, `utf-8`, `utf-16le`, `utf-16be`, `latin1`. A UTF-32 or Shift-JIS tree cannot be read. |
| `--hyperlink-format`, `--debug`, `--trace`, `--pcre2-version`, `--no-config` | Terminal hyperlinks, diagnostics, the PCRE2 build version, and refusing configuration files ([rg(1)](https://man.archlinux.org/man/rg.1.en)). | Absent and rejected as unknown options (each measured exit 2). There is no configuration file to refuse, no terminal hyperlink output, and no diagnostic tracing; `--stats` and the timing line are the whole reporting surface. |

Two things that *are* supported and easy to miss. `-e` may repeat, and every
pattern is an alternative; with two or more patterns and `-F` each one is
escaped before they are joined, so the set stays literal. One pattern is the
exception — see the appendix: `-F` with a single `-e` or `-f` pattern that holds
a regex metacharacter matches nothing (measured: `-F -e 'a.b'` does not match a
line `a.b`, while `-F 'a.b'` and `-F -e 'axb'` do). And **stdin** is searched
when it is piped with no paths, or when a `-` path names it, exactly as in grep
and rg: a piped stdin replaces the default `.`, an explicit path still wins over
the pipe, and `-H` labels the output `(standard input)` (measured).

### 1.4 Limits of the expression form

Three deviations from GNU find survive, and one cost.

- **Order.** Results come out sorted per directory, not in readdir order, because
  the rest of the tool promises that two runs of one walk print the same thing.
  The fixture above shows the same set in a different sequence, and so do the
  `-exec` batches.
- **`-ls`'s date column** is `YYYY-MM-DD HH:MM:SS` where find prints
  `Nov 19 20:51`; the other columns match.
- **An expression holding both `-exec` and `-execdir`** runs both the `-exec`
  way: `findexpr::ExecRequest` carries the command and the terminator, not which
  predicate asked for it. `-execdir` alone behaves as find's does (measured: the
  same per-directory batches, different order).
- **The expression walk is sequential; the glob form is not.** `walk_expr`
  cannot use the worker pool, because `-prune` and `-depth` decide the next step
  from the entry just seen, so an expression pays find's own shape: one
  `DirReader` at a time. `find-expr` in `docs/BENCHMARKS.md` is its row — 0.99x
  against `find -name` on the 15k-file tree, reached by cutting its instructions
  38% and not by its clock, which is syscall bound. The glob form is parallel and streams its listing in sorted order
  from a heap over open directory cursors, with a bounded read-ahead pool;
  `find-name`, `find-path` and `find-type` are its rows, and its peak memory no
  longer depends on how many paths match. Re-measure before quoting a ratio.

`-regextype` is accepted and validated against findutils' names but does not
change the dialect (`-regex` always uses this crate's syntax), symbolic `-perm`
modes and `-ok`/`-okdir` are refused, and `-delete` is refused in every
spelling, with or without `--allow-exec`.

## 2. Regex dialect

### 2.1 The default engine

The help text is the shipped contract for the dialect (`src/cmd/search.rs`):

> literals, `.` `*` `+` `?` `{n,m}` `[...]` `[^...]` `(` `)` `(?:` `)` `|` `^` `$`
> `\A` `\z` `\b` `\B` `\d` `\w` `\s` and their negations; inline flags `(?i)`
> `(?s)` `(?m)`. No lookaround or backreferences.

The engine is `src/regex.rs` — a Thompson NFA, run by a lazily built DFA that
does one table lookup per byte and by the Pike VM underneath it, so there is no
backtracking and no pathological pattern. Its module documentation is the
fuller contract and it is *wider* than the help text: POSIX bracket
expressions, `\p{...}` general categories, lazy quantifiers, `{n,}`, the
`\n \r \t \f \v \0 \xHH` escapes and the `(?-i)` / `(?ism)` group flags all
work. Every row below was run against the binary.

| Construct | Status | Evidence |
|---|---|---|
| literals, `.`, `*`, `+`, `?`, `{n}`, `{n,}`, `{n,m}` | works | `a{2,3}` on `aaaa` prints `aaa` |
| lazy `*?` `+?` `??` `{n,m}?` | works | `a+?` on `aaaa` prints `a` four times, `a+` prints `aaaa` once |
| alternation (vertical bar), `(...)`, `(?:...)` | works | `foo` or `bar` alternates and prints both matches; captures are never reported by this engine |
| `^`, `$`, `\A`, `\z`, `\b`, `\B` | works | `\Ax` matches at the start of the line |
| `[abc]`, `[^abc]`, `[a-z]`, multi-byte members | works | `[é]` matches the two-byte `0xC3 0xA9` as **one** character |
| `\d \D \w \W \s \S` | works, Unicode-aware on a non-ASCII line | `\w+` on `naïve café` prints `naïve` and `café` (measured); the character path is chosen per line (§2.3) |
| `\p{L}`, `\p{Nd}`, `\p{Lu}` and the categories `src/unicode.rs` holds | works on the default engine | `-o '\p{L}'` on `naïve café` prints the letters; `\p{..}` is refused by the `-P` engine, which is the reverse of the old behaviour |
| `[[:alpha:]]` and 13 friends | works | see the table below |
| lookaround `(?=…)` `(?!…)` `(?<=…)` `(?<!…)` | works through the backtracking engine | `'foo(?= bar)'` matches without any flag: the linear engine rejects the construct and `auto` falls back; `--engine default` refuses it instead (exit 2) |
| backreferences `\1`, `\k<name>`, `(?P=name)` | works through the backtracking engine | `'(\w+) \1'` prints `foo foo` from a file holding `foo foo` and `foo bar` |
| atomic groups, possessive quantifiers, comments | works through the backtracking engine | `'(?>foo) bar'`, `'fo*+ '` and `'foo(?#hi) bar'` each match (measured) |
| unknown POSIX class name | **rejected at compile time, exit 2** | `dusage: unknown POSIX class "bogus" at offset 1 in pattern "[[:bogus:]]"` |
| unterminated `[` or `[[:alpha:]` | **rejected at compile time, exit 2** | `dusage: unterminated character class at offset 0 in pattern "[[:alpha:]"` |
| repetition above 1000 | **rejected at compile time, exit 2** | `a{1001}` → `repetition bound above 1000 at offset 1` |
| unknown alphabetic escape | **rejected at compile time, exit 2** | `\q` → `unknown escape at offset 0 in pattern "\\q"` |
| `(?i)` `(?s)` `(?m)` and negations, at the pattern or a group start | works | `(?m)^x` matches; `(?i)` folds non-ASCII too on the character path — `'(?i)café'` matches `CAFÉ` (measured), which the old engine did not |
| `\n \r \t \f \v \0 \xHH` | works | `\xc3\xa9` matches `é` |

The fourteen POSIX bracket expressions, all measured on the line `a1 _Z `:

| Class | Matches | Class | Matches |
|---|---|---|---|
| `[[:alpha:]]` | `a`, `Z` | `[[:digit:]]` | `1` |
| `[[:alnum:]]` | `a`, `1`, `Z` | `[[:word:]]` | `a`, `1`, `_`, `Z` |
| `[[:space:]]` | the space | `[[:blank:]]` | the space |
| `[[:punct:]]` | `_` | `[[:xdigit:]]` | `a`, `1` |
| `[[:cntrl:]]` | (nothing on that line) | `[[:graph:]]` | `a`, `1`, `_`, `Z` |
| `[[:print:]]` | `a`, `1`, space, `_`, `Z` | `[[:upper:]]` | `Z` |
| `[[:lower:]]` | `a` | `[[:ascii:]]` | `a`, `1`, space, `_`, `Z` |

On the byte path they are ASCII classes; on the character path a pattern that
names one is Unicode, so `[[:alpha:]]` matches `é` as a single character
(measured: `-o '[[:alpha:]]'` on `café` prints four matches, the last being the
two bytes `0xC3 0xA9`). A class name outside the list is the compile error
above, not a silently empty set — which is the important half of parity,
because GNU grep accepts unknown names in a UTF-8 locale and matches nothing.

Structurally verified: the matcher receives raw bytes and returns byte offsets.
`Pattern::find_at(&self, hay: &[u8], from: usize) -> Option<Match>`
(`src/pattern.rs`), and `Match` is `{start, end}` in bytes, sliced directly out
of the line buffer for `-o` and colouring. A pattern that fails to compile
returns `Err(String)`, which propagates to exit 2.

Two things the engine *must* expose are visible from its callers, because
`src/pattern.rs` depends on them:

- `Regex::literal()` — if the whole pattern is a literal, the pattern is
  downgraded to the byte fast path and the engine is never consulted again
  (`Pattern::fast_literal`).
- `Regex::required_byte()` — at most **one** byte that every match must
  contain, used as the per-line prefilter.

### 2.2 The `-P` engine

`src/pcre.rs` is a bounded backtracking matcher for exactly the constructs the
linear engine cannot express. It is reached by `-P`/`--pcre2`, by
`--engine pcre2`, or automatically when the linear compiler rejects the pattern
and `--engine` is `auto` (the default).

| Construct | Status |
|---|---|
| lookahead `(?=…)` `(?!…)`, lookbehind `(?<=…)` `(?<!…)` | works; a lookbehind body must be bounded and at most 1024 bytes (`MAX_LOOKBEHIND`), and `(?<=a*)` is a compile error |
| backreferences `\1`..`\9`, `\k<name>`, `(?P=name)` | works; a reference to a group not yet set fails the match, as in PCRE |
| named groups `(?<name>…)` `(?P<name>…)` | works |
| atomic groups `(?>…)`, possessive `a*+` `a++` `a?+` `a{n,m}+` | works |
| comments `(?#…)` | works |
| `[abc]`, `[[:alpha:]]`, `\d \w \s`, `\b`, `\A \z`, `^ $`, `\xHH`, inline and scoped flags | works |
| `\p{…}` and `\P{…}` | **rejected**: `-P '\p{L}'` is exit 2, `Unicode property classes (\p{..}) are not supported` |
| `\Z \G \K \R \X`, conditionals, subroutine calls, branch reset | **rejected** at compile time, naming the offset |
| case folding | **ASCII only**, including inside backreference comparison; `-P -i` does not fold `É` to `é` |
| capture groups for `-r` | **not recorded**: `-P -r '$1'` is refused with exit 2, as it was before `-P` could match anything |
| step budget | 1,000,000 VM instructions per call (`DEFAULT_STEP_BUDGET`), counted across every start position and every lookaround body; `Pcre::with_budget` changes it, the CLI does not |

The budget is the price of allowing a backtracking engine at all, and exhausting
it is an error, not a non-match: `Pcre::try_find_at` returns `BudgetExceeded`
rather than `Ok(None)`, `Pattern::try_find_at` passes it on, and the pattern
keeps the first overrun it saw (`Overrun` in `src/pattern.rs`) so `dusage
search` can report it after the walk joins. The command then exits **2** with
`pattern exceeded its step budget (N steps, limit 1000000)` on stderr and
nothing on stdout — the answer is unknown, so printing "no match" would be a
wrong answer with grep's exit 1. `rg -P` does the same: `PCRE2: error matching:
match limit exceeded`, exit 2, no output.

Measured on `40` `a`s followed by `b`: `dusage search -P '(a+)+$'` exits 2 after
1,000,001 steps, and the same pattern without `-P` (the linear engine) exits 1,
which is the real no-match. The trailing byte is what makes it burn: a pure run
of `a`s matches at the first position and answers in microseconds, which is why
the billion-laughs shape needs the mismatching tail. `tests/search_cli.rs`
(`a_pattern_that_burns_the_step_budget_exits_2`) and the unit test
`a_budget_overrun_is_reported_instead_of_read_as_no_match` lock both halves,
alongside `budget_error_is_distinguishable` in `src/pcre.rs`, which sets a
20,000-step budget directly.

The pattern is never silently reinterpreted: every unsupported construct is a
compile error naming the offset, with exit 2, and the error is on stderr.

### 2.3 Byte and character paths

`src/regex.rs` runs two programs over the same VM. The **byte** program matches
the haystack bytes: `.` consumes one byte, `\d`/`\w`/`\s` and `\b` are ASCII.
The **character** program runs over the characters of the line: `.` consumes one
code point, `\d` is `\p{Nd}`, `\w` and `\b` are Unicode, and `[[:alpha:]]`
matches `é`. A pattern needs the character path when it names something only
Unicode can answer — a non-ASCII character, a `\p{..}` class, a POSIX class, a
`\d`-style shorthand, `\b`, or `-i` folding over a non-ASCII character — and the
choice is made per line: an ASCII line is answered by the byte program, and only
a line holding a non-ASCII byte is decoded and given to the character program.

Measured, with a file containing the three characters `a`, `é` (`0xC3 0xA9`) and
`b`:

```
$ printf 'a\xc3\xa9b\n' > f.txt
$ dusage search -o --no-filename '.' f.txt | wc -l       # 4  (a, 0xC3, 0xA9, b)
$ dusage search -o --no-filename '[^x]' f.txt | wc -l    # 4
$ rg -o --no-filename '.' f.txt | wc -l                  # 3  (a, é, b)
$ LC_ALL=C grep -o '[^x]' f.txt | wc -l                  # 4  (each byte separately)
$ LC_ALL=C.UTF-8 grep -o '[^x]' f.txt | wc -l            # 3
```

So `.` and `[^x]` still consume one **byte**, and `-o` with `.` on `é` still
prints two lines — one holding `0xC3`, one holding `0xA9` (`od -c` shows
`a \n 303 \n 251 \n b \n`). That is deliberate: the module documents it ("every
atom consumes bytes, so `.` matches one byte of a multi-byte character" is the
byte path's half of the contract) and it is what keeps a binary file from
turning into millions of replacement characters. The repair the old engine
lacked is that a class written with a non-ASCII member matches the whole
character (`[é]` is one unit), and that everything else — `\w`, `\d`, `\b`,
`[[:alpha:]]`, `\p{..}` — is now Unicode-correct on a line that needs it.

`-i` follows the same split:

- A **literal** needle under `-i` is folded per character with
  `char::to_lowercase` on both sides when the needle holds a non-ASCII byte, and
  with the byte-wise `memmem_ci` scan otherwise (`src/pattern.rs`). Measured:
  `-i 'café'` matches a line holding `CAFÉ`, and `-i -F 'CAFÉ'` matches `café`.
- A **regex** under `-i` folds with the simple case folding of `src/unicode.rs`
  on the character path, and per-byte ASCII otherwise (`ByteSet::fold_ascii`,
  `src/regex.rs`). Measured: `-i 'CAF[É]'` now matches `café`, where the old
  engine did not; on an ASCII line the two paths agree, so this only changes
  non-ASCII text.
- The inline `(?i)` flag now reaches the same Unicode folding as the CLI flag:
  `'(?i)café'` matches `CAFÉ` (measured). The two spellings agree, which they
  did not before.

`-w` is byte-based and has not changed: `is_word_byte` is
`is_ascii_alphanumeric() || '_' || b >= 0x80`, so `-w caf` does not match
inside `café` and neither does `rg -w caf` nor `LC_ALL=C.UTF-8 grep -w caf`
(measured). `-w 'caf.'` does not match either — the span ends after `0xC3` and
`0xA9` is still a word byte — while `-w 'caf..'` does, because two dots consume
the whole character.

`\b` inside a regular expression now agrees with `-w` on this input: on the
character path `\b` is Unicode, so `\bcaf\b` does **not** match inside `café`
(measured), as rg also refuses it. The old engine, which evaluated `\b` on the
byte program, did match; that disagreement is gone.

## 3. Ignore semantics

The walker applies four layers (`src/ignore.rs`): built-in cache names, git's
global excludes file, ignore files discovered while walking, and explicit `-g`
globs, of which a positive one switches the first three off.

1. **Built-in names.** Thirteen exact names are skipped wherever they appear as a
   path component: `.git`, `.hg`, `.svn`, `node_modules`, `target`, `__pycache__`,
   `.venv`, `venv`, `.tox`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`,
   `.gradle` (`src/ignore.rs`). Matching is per component, not on the whole
   relative path (`builtin_hit`), so `foo/target/x.rs` is skipped while
   `targets/` and `dist/` are not. The rule applies to files as well as
   directories. `--no-ignore` and `-u` turn the list off.
2. **git's global excludes file.** Read once per walk and applied as the
      lowest-priority layer (`IgnoreRules::push_global`). The path is
   `core.excludesFile` from `~/.gitconfig` and then `$XDG_CONFIG_HOME/git/config`
   (only the `[core]` section's key, `~/` expanded); with no setting it is
   `$XDG_CONFIG_HOME/git/ignore`, else `~/.config/git/ignore`. Measured: with
   `$XDG_CONFIG_HOME/git/ignore` holding `*.log`, `search -l needle tree` lists
   `tree/a.txt` only, and `--no-ignore-global` lists `tree/b.log` as well; the
   same holds with the file named by `core.excludesFile` in `~/.gitconfig`.
3. **Ignore files.** `.gitignore` and `.ignore`, read in that order from every
   directory the walk enters (`RULE_FILES`, `push_dir`), so `.ignore` overrides
   `.gitignore` within a directory. Each layer applies to paths below its own
   directory, deeper layers override shallower ones (`is_ignored`), and `!`
   re-includes. A `.git/info/exclude` is read too, but **only at a directory
   that actually holds a `.git` directory**; it is appended after both rule
   files, so it wins over both. Measured: with `.gitignore` naming `build/` and
   `.ignore` saying `!build/`, `build/x.js` is listed; with
   `.git/info/exclude` naming `keep.txt`, nothing in `.ignore` rescues it. The
   rule files of a root's **ancestors** are read as well (`push_ancestors`,
   which `ancestors` used to be the unused half of): a `.gitignore` naming
   `a.txt` one level above the root hides `p2/inner/a.txt` from
   `search -l needle p2/inner`, and `--no-ignore-parent` lists it again
   (measured).
4. **`-g` globs.** Positive globs restrict, `!` globs exclude, and directories
   are never pruned by an include glob because their children may still match
   (`GlobFilter::walk_allows`). A positive glob is an **override**, not an
   addition: `IgnoreRules::whitelisted()` returns true and the walker skips
   `is_ignored` altogether for that run. So `-g '*.log'` finds a file a
   `.gitignore` hides, and `-g '*.js'` descends into `node_modules` — both
   measured. Negative globs alone do not do this: `-g '!node_modules'` on its
   own leaves the built-in list in force. `-t`/`--ext` never switch ignores
   off; they are pushed as a separate `GlobFilter::filter` set that only
   narrows.

**No repository is required.** Nothing in `src/ignore.rs` tests for a
repository, so a directory with a `.gitignore` is filtered identically inside
and outside one. ripgrep does the opposite by default — "ripgrep will only
respect filter rules from source control ignore files when ripgrep detects that
the search is executed inside a source control repository"
([rg(1)](https://man.archlinux.org/man/rg.1.en), `--no-require-git`) — which is
exactly `--no-require-git` behaviour, on by default.

**Hidden means a leading dot.** `--hidden` toggles the check
`name.starts_with('.')`. This also means an ignore *file* is never itself a
result, and `.git` is skipped twice over (hidden and built-in) — until a
positive `-g` switches the built-in layer off, at which point
`--hidden -g '*.txt'` does list `.git` and `.git/info`.

### 3.1 Behaviour combinations

| Invocation | `.gitignore` / `.ignore` / `.git/info/exclude` / global | Built-in caches | Dotfiles | Notes |
|---|---|---|---|---|
| `dusage search pat` | applied | applied | skipped | Default. Ripgrep additionally requires a git repo for the VCS rules ([GUIDE](https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md#automatic-filtering)); ours does not, and it reads `.rgignore` not at all. |
| `dusage search --hidden pat` | applied | applied | **searched** | Same relationship as `rg --hidden`. |
| `dusage search --no-ignore pat` | **ignored** | **ignored** | skipped | Equivalent to rg's `-u` only; rg needs `-uu` to also take hidden files. |
| `dusage search -u pat` | ignored | ignored | skipped | Same as `--no-ignore`. |
| `dusage search -uu pat` | ignored | ignored | searched | Ours: `rg -uu`. |
| `dusage search -uuu pat` | ignored | ignored | searched, binaries printed | Ours: `rg -uuu`. |
| `dusage search -g '*.rs' pat` | **ignored** | **ignored** | skipped | A positive glob switches every ignore layer off (rg's override rule), so it re-includes what they hid and descends into cache directories. |
| `dusage search -g '!x' -g '*.rs' pat` | ignored | ignored | skipped | The globs **combine** — exclusion `x`, then the `*.rs` include — because `Args::values` keeps every occurrence. rg accepts repeats the same way. |
| `dusage search -g '!node_modules' pat` | applied | applied | skipped | Only negative globs: ignore rules stay on, so this particular exclusion is redundant. It matters after `--no-ignore` or beside a positive glob. |
| `dusage search -t rust pat` | applied | applied | skipped | A type filter narrows without touching ignore rules, so `-t js` finds nothing in a tree where the only `.js` files sit under `node_modules` or a gitignored `build/` (measured). |
| `dusage search -t rs -t js pat` | applied | applied | skipped | Repeated types combine into one alternative set. |
| `dusage find '*.rs'` | applied | applied | skipped | `find` (ours) inherits every ignore rule; GNU find has none of this and lists dotfiles too. |
| `dusage find --size +10M '*'` | applied | applied | skipped | `--size`, `--older`, `--newer`, `--dirs` and `--files` are applied to the candidates the walk kept, so they never widen it. |

Measured counterpart, for the built-in list: with no ignore file present at all,
ripgrep listed `./node_modules/p/i.js`; ours skips `node_modules` by name. There
is no rg option that reproduces the cache list — the nearest is a hand-written
`--glob '!node_modules'` per directory name. A repository full of
`node_modules` therefore behaves differently under the two tools, and
`--no-ignore` (or any positive `-g`) is the escape hatch.

## 4. Exit codes and output contract

### 4.1 Exit codes

`0` = something was selected, `1` = nothing was, `2` = misuse, an unusable
invocation, or **nothing was selected and some path could not be read**. That is
grep's contract, including its quiet-mode rule: "if the `-q` or `--quiet` or
`--silent` option is used and a line is selected, the exit status is 0 even if
an error occurred" ([grep manual, Exit
Status](https://www.gnu.org/software/grep/manual/html_node/Exit-Status.html));
our `-q` returns as soon as the first file matches.

| Code | `search` | `find` |
|---|---|---|
| 0 | At least one line selected (or, with `-L`, at least one path printed), **including when read errors occurred**. | At least one path survived the filters, again even with read errors. |
| 1 | No match and no read error. Returned with an empty message so nothing extra is printed. | No path matched and no read error. Note GNU find exits 0 here — confirmed: `find . -name zzz` returned 0. A script that treats non-zero as failure will behave differently. |
| 2 | Unknown flag, missing or empty pattern, pattern that does not compile, malformed number or size, unknown `--type` or `--encoding`, a root path that does not exist, **or no match after at least one path could not be read**. | Same, including a glob that fails to validate (unterminated `[...]`, trailing `\`, bare `!`) and the read-error rule. |

The read-error rule cuts both ways. A failing path is reported on stderr as
`dusage: <N> path(s) could not be read` and, when something else matched, is
otherwise harmless: `exit 0`. When nothing matched, the failure decides the
code, so a run that selected nothing because half the tree was unreadable
reports 2 rather than 1 — you cannot mistake it for "clean, no matches".
Measured on a `chmod 000` file: `dusage search foo ro` → 2 with `1 path(s)
could not be read` on stderr; `dusage search zzz .` → 2 as well. GNU grep is
blunter: any error is exit 2 even when a line matched. With `-q` the count is
neither printed nor fatal, exactly as in grep.

A nonexistent *root* is caught up front and is exit 2 with its own message,
which covers the common scripting case separately from the permission case.

### 4.2 `--json` for `dusage search`

One JSON document, no pretty-printing, emitted by `render_search_json`
(`src/report.rs`). Measured output for `search --json foo b.txt` where `b.txt`
holds one `foo` line (the output is a single line; the breaks here are for
width):

```json
{"files":[{"path":"b.txt","matches":1,"binary":false,"lines":[{"line_number":1,
"context":false,"text":"foo","submatches":[{"start":0,"end":3,"text":"foo"}]}]}],
"stats":{"files_scanned":1,"files_matched":1,"files_binary":0,"bytes_scanned":4,
"matches":1,"errors":0,"elapsed_secs":0.00022122}}
```

Field notes, all from the source: `matches` counts matching **lines** per file;
`submatches` are byte offsets into `text` — into the *transcoded* text when an
encoding was applied, so they do not index the bytes on disk — and are empty
for `-v` and for context lines; `context` is `true` for a line pulled in by
`-A`/`-B`/`-C`; `lines` is `[]` when `-c`, `-l` or `-L` is in effect, because
those modes throw the line data away; `binary` is a per-file boolean that stays
`false` when no block was read, and also `false` for a transcoded UTF-16 or
Latin-1 file, which is never examined for NULs; `stats.errors` is a single
count, not the per-category object the scan report uses; `elapsed_secs` is a
float. `text` goes through `String::from_utf8_lossy`, so invalid sequences
become U+FFFD and a repaired line is indistinguishable in the JSON from one that
really contained the replacement character. Control characters and quotes are
escaped by `json_escape` (`src/report.rs`), so a NUL in a `-a` line appears as
`\u0000`.

### 4.3 `--json` for `dusage find`

From `render_paths_json` (`src/report.rs`), measured with `find --json '*.rs'`:

```json
{"paths":[{"rel":"src/main.rs","path":"./src/main.rs","size":2,"is_dir":false,
"modified":"2026-11-19 20:35:05"}],"count":1}
```

`rel` is the path relative to the scan root with `/` separators, even on
Windows (`join_rel`); `path` is the root exactly as typed on the command line
joined with the entry name, so with the default root `.` it keeps a `./` prefix
while the text listing does not; `modified` is a UTC `YYYY-MM-DD HH:MM:SS`
string (`fmt_time`), not an ISO-8601 timestamp with an offset, and it is the
literal `"-"` for directories and for anything whose time could not be read.
Directory rows always carry `"size": 0`.

### 4.4 Text output

`search`: `path:line:text` per match, `path:line-text` for context lines, the
context separator between non-adjacent groups **only when a context window was
requested**, and `Binary file <path> matches` for a binary file with a match
unless `-a` (`render_search`, `src/report.rs`). With `-Z` the separator after
the path becomes a NUL and the line number keeps its own colon. `--vimgrep`
prints one row per match with the match's column; `--field-match-separator`,
`--field-context-separator`, `--context-separator` and `--no-context-separator`
replace the three punctuation characters above; `--trim` drops the leading
whitespace of the text; `--path-separator` rewrites the separators inside the
path. `find`: one relative path per line with no `./` prefix, or
`size  time  path` with a trailing `/` for directories in `--long` mode;
`-Z` swaps the newline for a NUL.

## 5. Performance model

### 5.1 The walk

`walk` (`src/search.rs`) is deterministic and runs on the worker pool: jobs are
directories, `par_for_each` lists each one, and a worker scans the files of the
directory it listed before taking the next job, so a directory's candidates live
only while it is being read. The result is then sorted by relative path (or by
the `--sort` key), which is what makes the output independent of the thread
count.

On Windows the size and mtime in that list come free with the name, because
`src/win/enumerate.rs` is `GetFileInformationByHandleEx` with
`FILE_ID_EXTD_DIR_INFO` — "one syscall returns many entries (name, sizes,
attributes, times, file id), so no per-file open or `stat` is needed". **On
Linux it is one `stat` per entry**: `std::fs::read_dir` plus one `lstat`, and
`AGENTS.md` records the same fact — "sizes still need one `statx` per entry …
so the Linux `batched` backend is 'one stat per entry', not 'many entries per
syscall'". `--max-filesize` is applied from that already-fetched size, and `-x`
costs one extra device lookup per directory.

### 5.2 Matching

Three paths, chosen at compile time of the pattern:

- **Literal.** `-F`, or any regex the engine reduces to a literal, goes to the
  vectorised `memmem` scan: `simd::memchr` / `simd::memchr_ci` broadcast the
  first needle byte across a vector register (AVX2 or SSE2, detected once at
  startup) with a portable SWAR loop as the fallback (`src/bytes.rs`,
  `src/simd.rs`). Verification is tail-first for needles of four bytes or more,
  so a stray first-byte hit is usually rejected without reading the interior.
- **Regex, byte path.** A plain-literal run every match must contain is pulled
  out of the pattern at compile time and scanned for before the automaton is
  touched; then a lazily built DFA walks one table lookup per byte, its
  transition table keyed by byte classes, with a byte-range skip while the scan
  sits in a start state. A single required byte gates each line as well.
- **Regex, character path.** Only a line holding non-ASCII bytes, or a pattern
  that names something Unicode-only, is decoded and given to the character
  program (§2.3). An ASCII line never pays for it.
- **Encoding.** A file whose detected encoding needs transcoding is read whole
  and converted to UTF-8 before scanning; UTF-8 streams through the 64 KiB block
  loop. That is the one input class that costs an extra full-file copy.

A file that is still unfinished after one 64 KiB block switches to 1 MiB reads,
and a file marked binary is scanned for one match instead of line by line rather
than held as lines, which is why a large binary costs a few megabytes resident
rather than its own size.

### 5.3 Parallelism and output order

`par_for_each` (`src/queue.rs`) runs the work across `--threads` workers. The
default is `available_parallelism()` clamped to **4..16**, with 8 as the
fallback when the platform will not say (`src/cli.rs`). `docs/BENCHMARKS.md`
records the sweep in the `threads-scan-*` and `threads-search-*` rows: one
thread is the slow row, the gain flattens at the core count, and past it the
clock goes up on both `scan /usr` and `search -F 'func '`. Read the shape of
those rows on your own box; the absolute milliseconds move with the machine.

Each result is stored in a pre-allocated slot indexed by its position in the
sorted list, and stats are aggregated under the same mutex. Output is therefore
byte-identical regardless of thread count and completion order. Measured: the
same `search -n 'fn ' src` report from the default thread count, `--threads 1`
and `--threads 8` has the same MD5 (`8aaef0bf…`). `-q` bypasses the pool and
scans sequentially so it can stop early.

### 5.4 What is not optimised

- **No mmap.** Every file is read with `File::open` plus `read` into a block
  buffer each worker reuses; there is no `MAP_PRIVATE` path. `--mmap` and
  `--no-mmap` are accepted for ripgrep compatibility, never error, and change
  nothing. `docs/BENCHMARKS.md` records why: on `/usr` the mapping's page faults
  cost more than `read`'s copy.
- **No incremental line reuse.** Each line is matched from offset 0, and the
  carry buffer holds only the partial line that spans a read boundary. A line
  longer than 8 MiB (`CARRY_MAX`) is matched in pieces, so a `-w` boundary at
  such a split is not a real boundary.
- **Nothing keeps the whole answer.** A content search prints each file's result
  as the walk reaches it in sorted order, with the printer's pending output
  capped at 4 MiB and the path window at `threads × 1024` files, and whole-line
  hits share their file's buffer through an offset. Peak RSS is therefore
  bounded and flat in the size of the tree, but bounded is not minimal: the
  floor above grep's 5 MiB is the pipeline itself, one read buffer and one queue
  per worker, which is what buys the parallel speed. `--sortr`, a time sort key,
  `--json` and `-q` take the collected path, because a stream cannot reverse, a
  time key must be read per result, and JSON closes with a count.
- **`find`'s glob form streams too**, in the order `--sort path` produces, from
  a heap over open directory cursors; its 15 MiB on `find-name` is the bounded
  read-ahead pool rather than the result count. The expression form streams
  directly in walk order and holds 4 MiB.
- **A repeated `-e` becomes one alternation.** The patterns are joined into a
  single regex, escaped first under `-F`, so five terms cost one walk instead of
  five — but a repeated `-e` also loses the literal fast path that a single
  plain pattern gets. The 1024-match cap bounds `-o` and colouring output on
  minified files.
- **`-U` and `-z` hold the input.** Multi-line matching needs the whole file in
  memory, and a compressed member is expanded before it is scanned; every other
  mode streams in blocks.

### 5.5 Where the numbers live

`docs/BENCHMARKS.md` is the document of record for performance. Every figure it
prints is the output of `bench/summary.sh` over `bench/results.json`, regenerated
by `bench/run.sh`, so a ratio there is only as current as the commit named in its
header. Nothing in this document restates one as if it were stable. What is worth
carrying here is the direction of each gap and the case that measures it:

- **The content rows are ahead**: `search-count` 0.69x,
  `search-files-with-matches` 0.69x, `search-only-matching` 0.84x,
  `search-multiline` 0.88x and `search-common` 0.76x against `grep` and `rg -U`,
  because results stream as the walk reaches them and a hit no longer owns a copy
  of its line.
- **The plain content rows are ahead too** — `search-rare` 0.94x,
  `search-regex` 0.90x, `search-regex-nomatch` 0.94x, `search-ci` 0.91x — where
  the previous pass had them at 1.02–1.10x. Two measurable changes did it: the
  literal scan got 25-28% cheaper in instructions and the regex candidate scan
  (`Regex::candidate_find`, which walks the line a required run sits on rather
  than the whole haystack) 29%, both counted by callgrind rather than inferred
  from a timing that drifts. Half the wall clock is now the kernel: about 60 ms
  user against 180 ms system over `/usr/local/go`, so the next gain there has to
  remove syscalls.
- **`-U`'s ASCII path is what makes `search-multiline` 0.88x**; the Unicode side
  of the same flag (`search-multiline-unicode`) is the one content row still
  behind, at 1.15x, because it decodes the lines that hold non-ASCII bytes. It was
  2.51x two rounds ago.
- **`find` meets its reference on every row**: `find-name` 0.71x,
  `find-path` 0.98x, `find-expr` 0.99x, with `find-type` 1.03x a hair behind. The
  big row is the parallel glob walk; the small ones are the same walk on a 40 ms
  job, where keeping the sorted order GNU find never produces is most of the gap.
  The expression row is the sequential walk of §1.4, which reached the reference
  by instruction work (−38%, 67.4M to 41.7M) and shows no wall-clock change for
  it, since that walk is `getdents` and `statx` bound.
- **`du` is ahead of GNU du** on the same tree (`du-s` 0.60x, `du` 0.58x), and the
  totals are byte-identical, which is what §6's du paragraph rests on.
- **Peak RSS is bounded and flat in the tree size, and still above the
  references** on the content rows (8-17 MiB against `grep`'s 5 and `ripgrep`'s
  9–10), on `find` (15 against 5) and on `du` (19–20 against 4, where GNU du gets
  its footprint by walking serially). That is the pipeline's floor, not the
  result count; see §5.

To re-measure any of the above on your own machine:

```console
$ cargo build --release
$ bash bench/run.sh && bash bench/summary.sh
$ bash scripts/perf-guard.sh
```

**No Windows number in this repository is a measurement of the shipped search
code.** The Windows CI job runs `cargo fmt --check`, clippy, `cargo test` and a
release build — it checks that the code compiles and its tests pass, and nothing
about speed. The Windows figures in the README come from one development PC and
are not reproducible from CI; treat them as provenance, not as a benchmark.

## 6. Can it replace X

**`grep -rn` in scripts.** Much closer than it was. The exit triad matches
(§4.1), `-q` still stops at the first match, `-r` is implicit, output is
deterministic, and the grep-specific spellings now exist: `-f patterns.txt`,
`--include`, `--exclude`, `--exclude-dir`, `-I`, `--binary-files`,
`-d skip`, `--line-regexp`, `-G` for basic regexps, and `-P`. What still breaks
a script: `-L` lists files without a match rather than following links (fine for
grep, wrong for rg), a pattern that begins with `-` needs `-e` or a `--`
terminator (`-e '-foo'` and `-- '-foo'` both work, measured), and `-x` does not
mean whole-line matching. Two exit-code differences remain — a partially unreadable
tree exits 0 when something matched and 2 when nothing did, where grep exits 2
either way — and reading a *pipe* is supported, so `grep` in the middle of a
pipeline can be replaced.

**`rg` interactively.** For code reading with literal or simple regex terms it
gets the same results at parity or better for literals, and its built-in cache
list (`node_modules`, `target`, `.venv`) matches what most people reach for
`--glob '!node_modules'` to do. Almost the whole flag surface is here now: `-P`
with lookaround, backreferences, atomic groups and comments, `-U` with
`--multiline-dotall`, `-z` over five compressed formats plus brotli, `--pre`
with `--pre-glob`, `-r` with capture groups, `--passthru`, `--iglob`, `--sort`
with time keys, `--type-add`/`--type-clear`/`--type-list`, `--vimgrep`,
`--trim`, the separator flags and `--follow` under its long name. The gaps that
remain are listed in §1.3 and are narrow but real: `--no-unicode`,
`--glob-case-insensitive`, the five-name `--encoding` set, terminal hyperlinks
and diagnostics, and the `-L`/`-E`/`-I` letter collisions — plus speed, though not much of it
now: a general regex measured 0.90x ripgrep's time in the last recorded run
(warm `search-regex`, commit 323bf88), and the one content row still behind is
`-U` with Unicode semantics at 1.15x. Keep rg for UTF-32 or legacy-encoding
trees, for patterns that need ripgrep's exact Unicode matching, and for the
flags in §1.3.

**`find -name`, and now `find`'s expressions.** A drop-in for most invocations:
`dusage find . -name "*.rs" -o -type d`, `-printf`, `-prune`, `-size`, `-mtime`,
`-perm`, `-newer`, `-maxdepth` and parentheses all run, and the comparisons above
agree with findutils 4.9.0 on what they select. What differs is order (sorted per
directory instead of readdir order), `-ls`'s date column, the sequential walk
(0.99x `find -name`'s clock on the 15k-file tree at commit 323bf88, §1.4), and
the refusals: `-delete` in every spelling, `-exec`/`-execdir` without
`--allow-exec`, symbolic `-perm`, `-ok`, and a `-regextype` that changes the
dialect. The glob form is faster than the
expression walk and its globs cover `**`, anchored paths and `{a,b}` — reach for
it when the query fits. Ours prints paths relative to the root, so
`dusage find '*.rs' .` prints `src/main.rs` where `find . -name '*.rs'` prints
`./src/main.rs`, and it exits 1 when nothing matches while find exits 0, which
inverts the meaning of `if find …; then`. A symlink is not listed at all without
`--follow`, where find lists the link itself; `--older 7d` selects a file aged 7
days 1 hour that `find -mtime +7` politely leaves out; and the glob form's
`--size +10M` includes a file of exactly 10 MiB that `find -size +10M` drops,
where the expression form's `-size` follows find's rounding (all measured).

**`du`.** `dusage du` is a direct replacement for the default report: same
totals, same post-order listing, `-a`, `-s`, `-c`, `-d`, `-b`, `-h`, `--si`,
`-m`, `--block-size`, `--inodes`, `-S`, `-t`, `-x`, `--exclude`,
`--exclude-from`, `--time`, `--time-style` and `--stats` all present, and on
`/usr` it is faster than GNU du (0.22 s against 0.32 s, identical totals,
§5.5). It refuses `-L/--dereference` on purpose — the walk never follows links —
and `-P` is accepted as the no-op it already is. Scripts that parse du's output
by column, or that use `--dereference`, need a look.

**Windows PowerShell `Select-String`.** This is the case the tool was built for,
and it wins on speed and on code-tree hygiene: `Select-String` has no recursion
of its own (the idiom is `Get-ChildItem -Recurse | Select-String`,
[Select-String
docs](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/select-string)),
it does no ignore handling at all, and its output is `MatchInfo` objects that
carry `Path`, `LineNumber` and `Line` as properties rather than text lines. Two
gaps survive. First, `.NET` regex constructs the backtracking engine does not
implement — `\p{…}` under `-P`, conditionals, `\K` — have to be rewritten;
lookaround and ordinary backreferences now work. Second, `Select-String` decodes
text — it detects a BOM and otherwise assumes UTF-8 — and we do the same: a
UTF-16LE file is detected from its BOM or its alternating-NUL pattern and
transcoded, so `dusage search needle u16le.txt` finds the line and prints it as
UTF-8, where rg needs `--encoding utf-16le`. Latin-1 is the remaining
difference, and it is opt-in: auto-detection never guesses it, so pass
`--encoding latin1` for files written by legacy Windows tools. Where PowerShell
wrote the files itself, keep `Select-String`; for source trees, configs and
logs, `dusage search` is the faster and cleaner answer.

---

## Appendix: quirks found while writing this

These are behaviours a contributor will trip over, all verified against the
binary and the source, and worth a follow-up issue rather than a silent fix
here:

1. The `find` help text calls `--size` repeatable, but `run_query` reads it with
   `Args::value` and only the last occurrence counts. Measured:
   `find '*' . --size +10M --size -1K` lists exactly what `--size -1K` alone
   lists, a 12 MB file included among the exclusions.
2. The `search` help text says `--max-columns` truncates at N **bytes**, while
   the implementation cuts after N characters (§1.1). Measured on 20 `é` at
   `-M 5`: five characters, ten bytes.
3. An unrecognised `--color=WHEN` value is not an error: it falls through to the
   terminal test in `color_enabled` (`src/cmd/search.rs`), so a typo like
   `--color=allways` silently means `auto` (measured, exit 0).
4. A symlink is never listed without `--follow` — not the link, not its target
   — while GNU find lists the link itself. This is deliberate (the walker never
   follows links outside `--follow`), but it is a visible difference from find
   for anything that pipes `-print0` into `xargs`.
5. `find`'s text listing and its `--json` `path` field disagree about the root
   prefix: the listing prints `big.txt` while `path` carries `./big.txt`. Both
   are measured above and both are intentional, but they are easy to confuse
   when a script switches between them.
6. `find --help` still lists five flags it does not accept: `--pre`,
   `--pre-glob`, `--binary-files`, `-I` and `-d` each exit 2 (four as `unknown
   predicate`, `-d` as a misplaced expression). The block is a copy of the
   `search` option list; the other four it used to carry (`--include`,
   `--exclude`, `--exclude-dir`, `--iglob`) are now walk filters `find` honours.

Entries that used to sit here — bundled short flags, `-exec` with `-execdir` in
one expression, `-F` with a single `-e`/`-f` pattern holding a regex
metacharacter, and a step-budget overrun reported as a silent non-match — have
since been fixed, and each one's behaviour is now asserted by a test rather than
described here.

## Sources

- ripgrep man page (text as fetched, ripgrep 14.x): <https://man.archlinux.org/man/rg.1.en>; verified against local `rg --help`, ripgrep 14.1.0.
- ripgrep user guide, "Automatic filtering": <https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md#automatic-filtering>.
- GNU grep manual: <https://www.gnu.org/software/grep/manual/grep.html>; exit-status section at <https://www.gnu.org/software/grep/manual/html_node/Exit-Status.html>; flag text verified against local `grep --help`, 3.11.
- GNU find(1): <https://man7.org/linux/man-pages/man1/find.1.html>; verified against local `find --help`, findutils 4.9.0.
- PowerShell `Select-String`: <https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/select-string>.
- This repository, branch `docs/truth` at `c9dd6a0` (plus the document's own edits): `src/cmd/search.rs`, `src/search.rs`, `src/pattern.rs`, `src/regex.rs`, `src/pcre.rs`, `src/findexpr.rs`, `src/glob.rs`, `src/ignore.rs`, `src/bytes.rs`, `src/encodings.rs`, `src/report.rs`, `src/queue.rs`, `src/cli.rs`, `src/archive*.rs`, `src/unix/enumerate.rs`, `src/win/enumerate.rs`, `AGENTS.md`, `docs/BENCHMARKS.md`. Every command quoted in this document was run against `./target/release/dusage` built from that commit.

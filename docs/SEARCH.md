# Search parity: `dusage search` and `dusage find` against grep, ripgrep and find

`dusage search` (aliases `grep`, `rg`) matches file contents, and `dusage find`
(alias `ls`) matches paths, both behind the same ignore-aware walker. This
document is a map of what they do relative to GNU grep, ripgrep and GNU find:
which flag means what, which syntax works, where the semantics silently
differ, and what would bite someone swapping one for the other.

**Evidence base.** Everything stated about this tool is read from the source of
`feat/search-suite` at commit `a42f96e`; file and line references are given for
each claim. At that commit the branch does not compile — `src/lib.rs:16` declares
`pub mod regex;` but `src/regex.rs` is not in the tree — so *no* claim here comes
from running our code, and the test suite has not been executed. Claims about the
*other* three tools come from their own documentation (cited inline) and from
probing the local binaries: ripgrep 14.1.0, GNU grep 3.11, GNU findutils 4.9.0
on Linux. Where a fact could not be established from either source it is marked
**not verified**.

The parser itself is shared with the rest of the tool: `Args::parse`
(`src/cli.rs:33`) accepts `--name value` and `--name=value`, a bare `--`
terminator, and treats a lone `-` or a negative number as a positional. Repeated
*value* flags keep only the last occurrence (`src/cli.rs:90-92`), so `-g a -g b`
means `-g b`. The option list is `SEARCH_VALUE_FLAGS` / `SEARCH_BOOL_FLAGS`
(`src/cmd/search.rs:151-207`) for `search` and an inline pair for `find`
(`src/cmd/search.rs:221-243`); an unrecognised flag is `exit 2`, not a warning.

## 1. Flag map

### 1.1 `dusage search`

| Our flag | grep / rg / find equivalent | Notes |
|---|---|---|
| `<pattern>` positional, `-e`, `--regexp` | `grep PATTERN`, `rg PATTERN`, `rg -e` | One pattern only. `-e` exists so a pattern starting with `-` can be passed, and it takes the next argument; a second `-e` replaces the first. There is no `grep -f PATTERN_FILE` / `rg -f`. Source: `src/cmd/search.rs:259-267`. |
| `-F`, `--fixed-strings` | `grep -F` / `rg -F` | Forces literal matching. A regex that the engine can prove is a literal already takes the byte fast path without `-F` (`src/pattern.rs:54`). |
| `-i`, `--ignore-case` | `grep -i` / `rg -i` | Folds ASCII `A-Z` only, via a 256-entry table that deliberately leaves `0xC1`/`0xE1` distinct (`src/bytes.rs:26-30`). See §2.3. |
| `-w`, `--word-regexp` | `grep -w` / `rg -w` | Boundaries are ASCII `[A-Za-z0-9_]` (`src/bytes.rs:244-257`). A match that starts mid-word is retried one byte later (`src/pattern.rs:118-121`). |
| `-v`, `--invert-match` | `grep -v` / `rg -v` | Selects lines with no match. `-o` and colour spans are suppressed when inverted (`src/search.rs:586-591`). |
| `-n`, `--line-number` | `grep -n` / `rg -n` | Off by default, including on a TTY. ripgrep differs: on a TTY it enables line numbers, colours and headings by default ([rg(1)](https://man.archlinux.org/man/rg.1.en), DESCRIPTION). |
| `-H`, `--with-filename` | `grep -H` / `rg -H` | Without either flag, the file name is printed when more than one file is in the report or any root is a directory (`src/report.rs:324`). |
| `-h`, `--no-filename` | `grep -h` / `rg -h` | `-h` is *never* help here; `--help` is (`src/cmd/search.rs:58`, and the help text says so). GNU grep makes the same choice; ripgrep does not — its `-h` prints help and its `--no-filename` short form is `-I` ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-I, --no-filename`, verified against `rg 14.1.0`). |
| `-l`, `--files-with-matches` | `grep -l` / `rg -l` | Prints paths only; matching lines are dropped before rendering (`src/search.rs:399-404`). |
| `-L`, `--files-without-match` | `grep -L` / `rg --files-without-match` | **Not ripgrep's `-L`**: in rg, `-L` is `--follow`, "follow symbolic links while traversing directories" ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-L, --follow`). We have no follow-symlink option at all. `exit 0` when at least one path is printed (`src/search.rs:406-408`). |
| `-c`, `--count` | `grep -c` / `rg -c` | Counts *matching lines* per file. `rg --count-matches` (occurrences) has no equivalent. Source: `src/search.rs:397-404`. |
| `-o`, `--only-matching` | `grep -o` / `rg -o` | One output line per match, capped at 1024 matches per input line (`src/search.rs:27`, `src/search.rs:595-604`). |
| `-q`, `--quiet` | `grep -q` / `rg -q` | Prints nothing and stops at the first matching file. This path is sequential — it walks the candidate list in order (`src/search.rs:331-347`) — so it does not use the thread pool. |
| `-m N`, `--max-count N` | `grep -m` / `rg -m` | Per file, in matching lines (`src/search.rs:427`). |
| `-A N`, `-B N`, `-C N`, `--after-context`, `--before-context`, `--context` | `grep`/`rg` same letters | `-C` sets both, and an explicit `-A`/`-B` overrides it (`src/cmd/search.rs:344-346`). Non-adjacent groups are separated by `--` (`src/report.rs:345-352`). |
| `-a`, `--text` | `grep -a` / `rg -a` | Binary files are always *searched*; the flag only replaces the one-line `Binary file … matches` summary with normal output (`src/report.rs:338`). grep behaves the same way; ripgrep's default is to stop at the NUL ([GUIDE](https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md#automatic-filtering)). |
| `-j N`, `--threads N` | `rg -j` | Default is 4× CPU cores (`src/cli.rs:174-179`). Output order does not depend on it — see §5.3. |
| `-g PATTERN`, `--glob` | `rg -g` | One glob only. `!` negates. A positive glob restricts results but cannot re-include a path hidden by an ignore rule — the walker ANDs the two decisions (`src/search.rs:190`), which contradicts the module doc at `src/ignore.rs:11-12`. |
| `-t NAME`, `--type` | `rg -t` | A fixed list of file types, mapped to extension globs in `ext_globs` (`src/cmd/search.rs:120-149`): rust, python, js, ts, c, cpp, go, java, ruby, md, json, toml, yaml, sh, html, css, xml, txt, log, lock, each with one or two aliases. One type per run; no `--type-list`, no `--type-add`, no `--type-not`. |
| `--max-depth N` | `find -maxdepth` | Entries directly under a root are depth 0, and a directory at depth *d* is descended while `d < N` (`src/search.rs:206`). So our `N` equals `find -maxdepth N+1`. |
| `--max-filesize SIZE` | `rg --max-filesize` | Applied during the walk from the size the enumerator already returned (`src/search.rs:210-214`), so an oversized file is never opened. Sizes accept `K/M/G/T`, binary multiples (`src/cli.rs:97-128`). |
| `--max-columns N` | `rg -M`, `--max-columns` | Truncates printed lines at N *bytes*, after matching (`src/search.rs:511-529`). No "N matches hidden" note like rg's. |
| `--hidden` | `rg --hidden` / `rg -.` | Only governs names starting with `.` (`src/search.rs:186`); it does not disable ignore rules. |
| `--no-ignore` | `rg --no-ignore` | Disables both the built-in cache list and `.gitignore`/`.ignore` (`src/search.rs:251`). Hidden files are still skipped, exactly as in rg, where `--hidden` is a separate flag ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-u/--unrestricted`). |
| `-x`, `--one-filesystem` | `rg --one-file-system`, `find -xdev` | **Not grep's `-x`** (`--line-regexp`, "match only whole lines") and not rg's `-x` either — ripgrep also spells line-regexp `-x` ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-x, --line-regexp`). We have no line-regexp mode. Stays on the root's device (`src/search.rs:195-203`). |
| `--json` | `rg --json` | Different shape: one JSON document for the whole run, not JSON Lines. See §4.2. |
| `--color[=WHEN]` | `grep --color`, `rg --color` | `always`/`yes`/`true`, `never`/`no`/`false`, `auto`. Only the `--color=WHEN` spelling is parsed (`src/cmd/search.rs:96-108`). Quirk: an explicit `--color=auto` enables colour even when stdout is a pipe (`src/cmd/search.rs:110-118`). |
| `--stats` | `rg --stats` | One summary line on **stderr**, not stdout (`src/report.rs:475`): files scanned, binary count, matched, matches, errors, elapsed. |
| `--help` | — | Prints the help text. `-h` does not. |
| `-V`, `--version` | — | Prints `dusage <version>`. |

Not present on `search`, and rejected as unknown options: `--ext`, `--size`,
`--older`, `--newer`, `--dirs`, `--files`, `--long` (find-side only, per the
`run_query` flag lists at `src/cmd/search.rs:214-243`).

### 1.2 `dusage find`

| Our flag | grep / rg / find equivalent | Notes |
|---|---|---|
| `<glob>` positional | `find -name`, `find -path` | A glob with no `/` is matched against the entry's file name, gitignore-style; one with `/` is anchored at the root (`src/glob.rs:73-92`). Supports `*`, `?`, `**`, `[...]`, `[!...]`, `\` escapes and a trailing `/` for directories (`src/glob.rs:1-9`). Prefix `!` inverts. |
| `[paths...]` | find starting points | Defaults to `.` (`src/cmd/search.rs:269-273`). |
| `-F`, `--fixed` | `grep -F`, `find -name 'x'` | Substring match on the relative path instead of a glob (`src/cmd/search.rs:363-369`). |
| `-t NAME`, `--type` | `rg -t`, `find -name '*.rs'` | Same fixed list as `search`. |
| `--ext rs,md` | `find -name '*.rs' -o -name '*.md'` | Comma-separated, translated to `*.ext` globs (`src/cmd/search.rs:290-295`). |
| `--size +10M` / `-10M` / `10M` | `find -size +10M` | Ours is exact: `+10M` is `size >= 10 MiB` (`src/cmd/search.rs:447-470`). GNU find rounds *up* to the unit, so its `+10M` excludes a file of exactly 10 MiB while ours includes it — confirmed against findutils 4.9.0: a 10 MiB file matched `-size 10M` and not `-size +10M`, while 10.5 MiB and 11 MiB matched `+10M`. A bare `10M` here means within ±5%, which find has no equivalent for. |
| `--older 7d` | `find -mtime +7` | Ours compares the exact elapsed duration with `>` (`src/cmd/search.rs:375-376`); `find -mtime +7` works in whole 24-hour periods. Confirmed against findutils 4.9.0: a file aged 7 days 1 hour went to `-mtime 7`, not `-mtime +7`; ours would select it. Units: `s m h d w`, or a bare number meaning days (`src/cli.rs:132-158`). |
| `--newer 7d` | `find -mtime -7`, `find -newermt` | Exact duration `<` instead of find's integer periods. |
| `--dirs` | `find -type d` | Directories only. |
| `--files` | `find -type f` | Files only. |
| `--long` | `find -ls`, `find -printf '%s %TY… %p\n'` | Prints size, modification time in UTC, and the relative path (`src/report.rs:430-448`). |
| `--max-depth N` | `find -maxdepth` | Same N+1 offset as `search`. |
| `-g PATTERN`, `--glob` | `find -path`, `find -prune` | One glob, same include/`!`-exclude filter as `search`. |
| `--hidden` | no find equivalent | GNU find lists dotfiles like any other name; we skip them unless this flag is given. |
| `--no-ignore` | no find equivalent | Without it, `.gitignore`/`.ignore` and the built-in cache list apply to `find` too (`src/search.rs:190`). |
| `-x`, `--one-filesystem` | `find -xdev` | Device comparison per directory (`src/search.rs:195-203`). |
| `--json` | no find equivalent | `{"paths":[…],"count":N}` — see §4.3. |
| `--color[=WHEN]` | `ls --color`-ish | Colours the printed path only. |
| `--help`, `-V`, `--version` | — | `find -h` is rejected as an unknown option here, unlike grep, where `-h` is `--no-filename`. |

Absent relative to GNU find, and likely to matter: the expression language
(`-a`, `-o`, `!`, parentheses), `-exec`/`-execdir`, `-delete`, `-printf`,
`-print0`, `-prune`, `-regex`, `-iname`, `-mindepth`, `-newermt`, `-user`,
`-perm`, `-links`, `-empty`, `-depth`. There is no `-name` vs `-path` split: one
glob, matched as described above.

### 1.3 ripgrep flags we do not have

Each of these is genuinely absent — none is spelled differently, none has a
partial stand-in in the parser.

| rg flag | What it does in ripgrep | What its absence means here |
|---|---|---|
| `-P`, `--pcre2` | Switches to the PCRE2 engine "when you want to use features such as look-around or backreferences" ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-P, --pcre2`). | No lookaround and no backreferences, in any spelling. See §2.2. A pattern that needs them cannot be expressed; ripgrep gets them only if built with PCRE2. |
| `-U`, `--multiline` | Lifts the rule that a match cannot contain a line terminator, at a cost in speed and memory ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-U, --multiline`). | Matching is strictly per line: `scan_file` splits on `\n` and calls `handle_line` per line (`src/search.rs:446-486`). A pattern for a record that spans lines returns nothing. |
| `-z`, `--search-zip` | Detects gzip, bzip2, xz, LZ4, LZMA, Brotli and Zstd files and decompresses them before searching ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-z, --search-zip`). | Compressed files are searched as raw bytes: no match, or a `Binary file … matches` line if the literal happens to appear in the compressed stream. |
| `--pre COMMAND`, `--pre-glob` | Searches the standard output of `COMMAND PATH` instead of the file ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--pre`). | No preprocessor. PDFs, office documents and anything else needing extraction cannot be searched. |
| `--multiline-dotall` | Makes `.` match line terminators when `-U` is on ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--multiline-dotall`). | Meaningless without multiline mode, so also absent. The help text does advertise an inline `(?s)` flag (`src/cmd/search.rs:21`), whose effect within one line is **not verified**. |
| `-r`, `--replace` | Rewrites every match in the *output* using capture groups, never touching files ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-r REPLACEMENT`). | No output rewriting, and no `$1` capture substitution. `-o` prints the matched text only. |
| `--passthru` | Prints matching and non-matching lines, highlighting only matches ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--passthru`). | No way to emit a full file with highlights. |
| `-l` vs `--files-without-match` | rg spells the negative form `--files-without-match` and reserves `-L` for `--follow` ([rg(1)](https://man.archlinux.org/man/rg.1.en)). | We spell it `-L`. That is a direct letter collision: a script written for rg that passes `-L` to follow symlinks gets a list of files with no matches instead. |
| `--type-add`, `--type-clear`, `--type-not` | Extends or replaces the built-in type table at run time ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--type-add=TYPESPEC`). | The type table is compiled in (`src/cmd/search.rs:120-149`). An unlisted language is reachable only through `-g '*.ext'`, one glob at a time. |
| `--iglob` | A case-insensitive `-g`, which "always overrides any other ignore logic" ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--iglob`). | No case-insensitive glob, and our globs do not override ignore rules (§1.1, `-g`). |
| `--sort SORTBY`, `--sortr` | `path`, `modified`, `accessed`, `created`; forces single-threaded traversal ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--sort=SORTBY`). | Output is always sorted by relative path (`src/search.rs:301`), so the default already equals `--sort path`; sorting by mtime or size is not available. |
| `--no-require-git` | Respects `.gitignore` even outside a git repository ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--no-require-git`). | We always respect it — there is no repository test anywhere in `src/ignore.rs` — which is exactly `--no-require-git` behaviour, on by default. |

Also missing, and worth knowing: `--files`/`--type-list` (list instead of
search), `--null`/`-0` and `--null-data`, `--encoding`/`-E` (so no UTF-16
input), `--crlf`, `--binary`, `--follow`/`-L`, `--max-columns-preview`,
`--heading`, `--no-heading`, `--trim`, `--line-buffered`, `--block-buffered`,
`-f/--file`, `--engine`, `--no-unicode`, `-u/--unrestricted` (the `-uu`/`-uuu`
ladder has no equivalent; use `--no-ignore --hidden`), `-S/--smart-case`,
`-s/--case-sensitive`, multiple patterns (`-e` repeated or `-f PATTERNFILE`), and reading **stdin**:
with no paths we always search `.` (`src/cmd/search.rs:269-273`), and a `-` path
is treated as a file name that does not exist, so `cat x | dusage search foo`
searches the directory instead of the pipe.

## 2. Regex dialect

### 2.1 What is advertised and what is structural

The help text is the shipped contract for the dialect (`src/cmd/search.rs:13-21`):

> literals, `.` `*` `+` `?` `{n,m}` `[...]` `[^...]` `(` `)` `(?:` `)` `|` `^` `$`
> `\A` `\z` `\b` `\B` `\d` `\w` `\s` and their negations; inline flags `(?i)`
> `(?s)` `(?m)`. No lookaround or backreferences.

Structurally verified: the matcher receives raw bytes and returns byte offsets.
`Pattern::find_at(&self, hay: &[u8], from: usize) -> Option<Match>`
(`src/pattern.rs:101`), and `Match` is `{start, end}` in bytes, sliced directly
out of the line buffer for `-o` and colouring (`src/search.rs:595-615`). The
line is split on `b'\n'` only (`src/search.rs:446`), and a trailing `\r` is
removed so `^…$` works on CRLF files (`src/search.rs:618`). A pattern that fails to
compile returns `Err(String)`, which `run_query`'s `?` turns into exit 2
(`src/cmd/search.rs:305-314`).

The engine itself is `src/regex.rs`, which is not in the tree at `a42f96e`, so
the following are **not verified** and must be settled against that file when it
lands: whether `\d`, `\w`, `\s` and their negations are ASCII-only or
Unicode-aware; whether POSIX bracket expressions (`[[:alpha:]]`), `\p{…}` and
`\P{…}` are accepted; whether inline `(?i)` folds non-ASCII; and whether an
invalid construct is rejected at compile time or silently never matches.

Two things the engine *must* expose are visible from its callers, because
`src/pattern.rs` depends on them:

- `Regex::literal()` (`src/pattern.rs:54`) — if the whole pattern is a literal,
  the pattern is downgraded to the byte fast path and the engine is never
  consulted again.
- `Regex::required_byte()` (`src/pattern.rs:67`) — at most **one** byte that
  every match must contain, used as the per-line prefilter (`src/pattern.rs:105-113`).

### 2.2 Not supported

Lookaround (`(?=…)`, `(?!…)`, `(?<=…)`, `(?<!…)`) and backreferences (`\1`,
`\k<name>`) are outside the advertised grammar and have no opt-in engine. A
reader coming from ripgrep's `-P` ([rg(1)](https://man.archlinux.org/man/rg.1.en),
`-P, --pcre2`) or from GNU grep's `-P` (`grep --help`: "PATTERNS are Perl
regular expressions") loses both. A reader coming from `.NET` — PowerShell's
`Select-String` — loses them too, since .NET's regex language includes both, along
with `\p{…}` ([.NET regex quick reference](https://learn.microsoft.com/en-us/dotnet/standard/base-types/regular-expression-language-quick-reference)).
The pattern is not silently reinterpreted: an unsupported construct is a
compile error and exit 2, which is the one thing you want from a gap.

### 2.3 Byte-oriented, and what that costs

ripgrep's default engine is Unicode-aware with UTF-8 mode; its man page says the
engine is byte-oriented but documents Unicode character classes as an addition
to ERE ([rg(1)](https://man.archlinux.org/man/rg.1.en), REGEX SYNTAX). Measured
on the local binaries, with a file containing the three bytes `a`, `0xC3 0xA9`
(`é`) and `b`:

```
$ printf 'a\xc3\xa9b\n' > f.txt
$ rg -o --no-filename '.' f.txt | wc -l     # 3   (a, é, b)
$ LC_ALL=C grep -o '[^x]' f.txt | wc -l     # 4   (each byte separately)
$ LC_ALL=C.UTF-8 grep -o '[^x]' f.txt | wc -l  # 3
```

ripgrep is in the first camp: `.` consumes one code point, and a `[^x]` class
consumes a whole `é`. `grep`'s behaviour depends on the locale, which is why the
same command counts 4 in `C` and 3 in `C.UTF-8`.

Our matcher is handed bytes and never decodes UTF-8 in `pattern.rs` or
`search.rs`; every offset it reports is a byte offset into `text`. Whether the
engine's `.` and `[^x]` consume one byte or one code point is **not verified**
(because `src/regex.rs` is absent). If it is byte-wise, the consequences are:

- `-o` on `é` with pattern `.` prints two lines with half a character each
  (`0xC3`, then `0xA9`), which a terminal renders as `?` or as replacement
  characters.
- `[^x]` matches twice per `é`, so any count taken from `-o` output is a byte
  count wearing a character count's name. On CJK text the two differ by a
  factor of three, because each character is three bytes.
- `-w` is measured in ASCII: `is_word_byte` is `is_ascii_alphanumeric() || '_'`
  (`src/bytes.rs:244-257`), so the continuation bytes of a non-ASCII letter count
  as word *breaks*. `-w caf` therefore matches inside `café`, because the byte
  after `f` is `0xC3`. Measured: `rg -w caf` and `LC_ALL=C.UTF-8 grep -w caf`
  both reject that line; `LC_ALL=C grep -w caf` accepts it, and so would we.

The one place the byte orientation is certain and already bites is `-i`:
folding goes through the ASCII table only, whose comment states the intent —
"`0xC1` and `0xE1` stay distinct. A plain `| 0x20` would fold `@`/`` ` `` and
those Latin-1 letters together" (`src/bytes.rs:24-30`). Measured for comparison,
on a file containing `É` (`0xC3 0x89`):

```
$ rg -i 'é' up.txt                    # matches       (Unicode folding)
$ rg --no-unicode -i 'é' up.txt       # no match      (ASCII folding)
$ LC_ALL=C.UTF-8 grep -iq 'é' up.txt  # matches
$ LC_ALL=C grep -iq 'é' up.txt        # no match
```

So `dusage search -i é` behaves like `rg --no-unicode -i` and like `grep` in the
`C` locale: `É` is invisible to a search for `é`. Users on non-English source
trees will hit this, and there is no flag to change it.

**Worked example, end to end.** A UTF-8 file with two lines, `naïve café` and
`naive cafe`:

- `dusage search -F 'café' f.txt` — the literal path is used (`-F`, or the
  engine's `literal()`), the needle is `63 61 66 C3 A9`, and the match is exact,
  byte for byte. This works.
- `dusage search -i -F 'CAFÉ' f.txt` — folding maps `CAFÉ` to `café` in ASCII,
  leaving `É` as `C3 89` rather than `C3 A9`, so a file written in the other case
  does not match. rg matches here; grep matches in a UTF-8 locale.
- `dusage search -o 'caf.' f.txt` — with a byte-wise `.`, the span ends after
  `C3`, so `-o` prints `caf` plus one broken byte. With a code-point-wise `.`
  it prints `café`. **Not verified**; the one-line test that settles it is
  `dusage search -o '.' f.txt | wc -l` on the three-byte file above: 5 means
  bytes, 3 means code points.

## 3. Ignore semantics

The walker applies three layers, in this order (`src/ignore.rs:1-14`): built-in
cache names, ignore files discovered while walking, and explicit `-g` globs.

1. **Built-in names.** Thirteen exact base names are skipped wherever they
   appear: `.git`, `.hg`, `.svn`, `node_modules`, `target`, `__pycache__`,
   `.venv`, `venv`, `.tox`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`,
   `.gradle` (`src/ignore.rs:20-34`). Matching is on the last path component
   only, so `targets/` and `dist/` are *not* skipped, and the rule applies to
   files as well as directories (`src/ignore.rs:201-205`). `--no-ignore` turns
   the list off.
2. **Ignore files.** Only `.gitignore` and `.ignore`, in that priority order
   (`src/ignore.rs:37`), read from each directory as the walk enters it
   (`push_dir`), each applying to paths below it, with deeper files overriding
   shallower ones and later lines overriding earlier ones; `!` re-includes.
   Deliberately absent: `.git/info/exclude`, the global `core.excludesFile`
   (`$XDG_CONFIG_HOME/git/ignore`), `.rgignore`, and ignore files *above* the
   scan root. `ancestors()` exists in `src/ignore.rs:209` but has no caller, so
   a `.gitignore` in a parent directory has no effect on a scan rooted below it.
3. **`-g` globs.** Positive globs restrict, `!` globs exclude, and directories
   are never pruned by an include glob because their children may still match
   (`src/ignore.rs:88-98`). Because the walker combines these with the ignore
   verdict using `||` (`src/search.rs:190`), a negative glob cannot rescue a
   path an ignore file excluded, and a positive glob cannot either.

**No repository is required.** There is no `.git` detection anywhere in
`src/ignore.rs`: a directory with a `.gitignore` is filtered identically inside
and outside a repository. ripgrep does the opposite by default — "ripgrep will
only respect filter rules from source control ignore files when ripgrep detects
that the search is executed inside a source control repository" ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--no-require-git`) — which I confirmed: outside a git repository rg listed all three files despite a `.gitignore` excluding `*.log`, and inside one it listed only `keep.txt`.

**Hidden means a leading dot.** `--hidden` toggles the check `name.starts_with('.')`
(`src/search.rs:186`). This also means an ignore *file* is never itself a
result, and `.git` is skipped twice over (hidden and built-in).

### 3.1 Behaviour combinations

| Invocation | `.gitignore` / `.ignore` | Built-in caches | Dotfiles | Notes |
|---|---|---|---|---|
| `dusage search pat` | applied | applied | skipped | Default. Ripgrep additionally requires a git repo and honours `.git/info/exclude`, global gitignore and `.rgignore` ([GUIDE](https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md#automatic-filtering)); ours does not. |
| `dusage search --hidden pat` | applied | applied | **searched** | Same relationship as `rg --hidden`. |
| `dusage search --no-ignore pat` | **ignored** | **ignored** | skipped | Equivalent to rg's `-u` only; rg needs `-uu` to also take hidden files. |
| `dusage search --hidden --no-ignore pat` | ignored | ignored | searched | Ours: `rg -uu`. |
| `dusage search -g '*.rs' pat` | applied | applied | skipped | A positive glob narrows; it cannot re-include what the layers above excluded. |
| `dusage search -g '!node_modules' pat` | applied | applied | skipped | Redundant here but works after `--no-ignore`, which is the only way to descend into a built-in cache directory. |
| `dusage search -g '!x' -g '*.rs' pat` | applied | applied | skipped | Only the **last** `-g` survives (`src/cli.rs:90-92`), so this is just `*.rs`. rg accepts repeats and combines them. |
| `dusage find '*.rs'` | applied | applied | skipped | `find` (ours) inherits every ignore rule; GNU find has none of this and lists dotfiles too. |

Measured counterpart, for the built-in list: with no ignore file present at all,
ripgrep listed `./node_modules/p/i.js`; ours skips `node_modules` by name. A
repository full of `node_modules` therefore behaves differently under the two
tools, and `--no-ignore` is the escape hatch.

## 4. Exit codes and output contract

### 4.1 Exit codes

`0` = something was selected, `1` = nothing was, `2` = misuse or an unusable
invocation. That is grep's contract, including its quiet-mode rule: "if the `-q`
or `--quiet` or `--silent` option is used and a line is selected, the exit status
is 0 even if an error occurred" ([grep manual, Exit Status](https://www.gnu.org/software/grep/manual/html_node/Exit-Status.html));
our `-q` returns as soon as the first file matches (`src/search.rs:331-347`).

| Code | `search` | `find` |
|---|---|---|
| 0 | At least one line selected (or, with `-L`, at least one path printed). `report.any_match` is set in `src/search.rs:394-408` and consumed in `src/cmd/search.rs:439-443`. | At least one path survived the filters (`src/cmd/search.rs:417-419`). |
| 1 | No match. Returned with an empty message so nothing is printed (`src/cmd/search.rs:440-443`). | No path matched. Note GNU find exits 0 here — confirmed: `find . -name zzz` returned 0. A script that treats non-zero as failure will behave differently. |
| 2 | Unknown flag, missing or empty pattern, pattern that does not compile, malformed number or size, unknown `--type`, or a root path that does not exist (`src/cmd/search.rs:259-278`, `src/cmd/mod.rs:82-96`). | Same, including a glob that fails to validate (unterminated `[...]`, trailing `\`, bare `!`). |

The difference from grep that matters most: **per-file I/O errors never change
the exit code.** An unreadable file or directory is counted
(`stats.errors`) and the run continues (`src/search.rs:168-185`,
`src/search.rs:356-382`), so a run with `EACCES` on half the tree still exits 0
if one line matched. GNU grep exits 2 when any error occurs unless `-q` is given
(same manual section). A nonexistent *root* is checked up front and does exit 2
(`src/cmd/search.rs:274-278`), which covers the common scripting case but not the
permission case.

### 4.2 `--json` for `dusage search`

One JSON document, no pretty-printing, emitted by `render_search_json`
(`src/report.rs:379-428`). This is the real schema, with the field list taken
from the format strings:

```json
{
  "files": [
    {
      "path": "src/main.rs",
      "matches": 2,
      "binary": false,
      "lines": [
        {
          "line_number": 4,
          "context": false,
          "text": "    let needle = 1;",
          "submatches": [
            { "start": 8, "end": 14, "text": "needle" }
          ]
        }
      ]
    }
  ],
  "stats": {
    "files_scanned": 12,
    "files_matched": 1,
    "files_binary": 0,
    "bytes_scanned": 8134,
    "matches": 2,
    "errors": 0,
    "elapsed_secs": 0.0005
  }
}
```

Field notes, all from the source: `matches` counts matching **lines** per file;
`submatches` are byte offsets into `text` and are empty for `-v` and for context
lines; `context` is `true` for a line pulled in by `-A`/`-B`/`-C`;
`lines` is `[]` when `-c`, `-l` or `-L` is in effect, because those modes throw
the line data away (`src/search.rs:399-404`); `binary` is a per-file boolean
that stays `false` when no block was read; `stats.errors` is a single count, not
the per-category object the scan report uses; `elapsed_secs` is a float.
Non-UTF-8 line bytes go through `String::from_utf8_lossy`, so invalid
sequences become U+FFFD and a repaired line is indistinguishable in the JSON
from one that really contained the replacement character.

### 4.3 `--json` for `dusage find`

From `render_paths_json` (`src/report.rs:451-473`):

```json
{
  "paths": [
    {
      "rel": "src/deep/util.rs",
      "path": "./src/deep/util.rs",
      "size": 23,
      "is_dir": false,
      "modified": "2026-11-18 14:03:22"
    }
  ],
  "count": 1
}
```

`rel` is the path relative to the scan root with `/` separators, even on
Windows (`join_rel`, `src/search.rs:242-248`); `path` is the root exactly as
typed on the command line joined with the entry name (`dir.join(name)`,
`src/search.rs:222-240`), so with the default root `.` it keeps a `./` prefix;
`modified` is a UTC `YYYY-MM-DD HH:MM:SS` string (`fmt_time`,
`src/report.rs:276-290`), not an ISO-8601 timestamp with an offset.

### 4.4 Text output

`search`: `path:line:text` per match, `path:line-text` for context lines, `--`
between non-adjacent groups, `Binary file <path> matches` for a binary file with
a match unless `-a` (`src/report.rs:323-377`). `find`: one relative path per
line, directories with a trailing `/` only in `--long` mode (`src/report.rs:430-448`).

## 5. Performance model

### 5.1 The walk

`walk` is single-threaded and deterministic: for each root it lists candidates,
applying hidden, ignore and glob decisions as it descends, then sorts the whole
list by relative path (`src/search.rs:266-303`). Content mode drops directories
from the list (`src/search.rs:299-300`); path mode keeps them.

On Windows the size and mtime in that list come free with the name, because
`src/win/enumerate.rs` is `GetFileInformationByHandleEx` with
`FILE_ID_EXTD_DIR_INFO` — "one syscall returns many entries (name, sizes,
attributes, times, file id), so no per-file open or `stat` is needed"
(`src/win/enumerate.rs:1-5`). **On Linux it is one `lstat` per entry**: the
module doc says "`std::fs::read_dir` plus one `lstat` per entry"
(`src/unix/enumerate.rs:1-3`), and `AGENTS.md` records the same fact — "sizes
still need one `statx` per entry … so the Linux `batched` backend is 'one stat per
entry', not 'many entries per syscall'". So the walk is stat-free on Windows and
stat-per-entry on Linux; it is never dtype-only on either. `--max-filesize`
is applied from that already-fetched size (`src/search.rs:210-214`), and
`-x` costs one extra device lookup per directory (`src/search.rs:195-203`).

### 5.2 Matching

Two paths, chosen at compile time of the pattern:

- **Literal.** `-F`, or any regex the engine reduces to a literal
  (`src/pattern.rs:54-66`), goes to `bytes::memmem_from`. That scan broadcasts
  the first needle byte across a `u64`, XORs four words per iteration — 32 bytes
  — and tests the OR-ed zero flags once per block, only entering the byte-wise
  loop when a block holds a candidate (`src/bytes.rs:123-155`). Verification is
  tail-first for needles of four bytes or more, so a stray first-byte hit is
  usually rejected without reading the interior (`src/bytes.rs:164-178`). No
  `unsafe`, no allocation, no dependency.
- **Regex.** One byte the pattern requires gates each line: `is_match` asks
  `bytes::find_byte` for `re.required_byte()` before the engine runs
  (`src/pattern.rs:105-113`). A single required byte is a weak filter — it rules
  out lines that lack one specific character, nothing more — and a pattern with
  no required byte skips the prefilter entirely.

Files are read with `File::open` plus `read` into a 64 KiB buffer
(`src/search.rs:22`, `src/search.rs:415-429`). Binary detection looks for a NUL
in the first 8 KiB of the first block only (`src/search.rs:24`,
`src/search.rs:439`), and a file marked binary is still searched in full — only
the rendering changes (`src/report.rs:338`).

### 5.3 Parallelism and output order

`par_for_each` (`src/queue.rs:76`) runs the candidate list across `--threads`
workers. Each result is stored in a pre-allocated slot indexed by its position
in the sorted candidate list, and stats are aggregated under the same mutex
(`src/search.rs:352-382`). Output is therefore byte-identical regardless of
thread count and completion order: the sort happens before matching, not after.
`-q` bypasses the pool and scans sequentially so it can stop early
(`src/search.rs:331-347`).

### 5.4 What is not optimised

- **No mmap.** Every file is read with `File::open` plus `read` into a 64 KiB
  heap buffer allocated per file (`src/search.rs:416`); there is no `MAP_PRIVATE`
  path and no `--mmap`.
- **No incremental line reuse.** Each line is matched from offset 0
  (`handle_line` calls `find_at(text, 0)`, `src/search.rs:544`), and `carry` holds
  only the partial line that spans a read boundary (`src/search.rs:462-479`).
  A resumed scan across blocks exists; a resumable *matcher* across lines does not.
- **Every hit is copied and kept.** `LineHit.text` is `text.to_vec()`
  (`src/search.rs:545-615`), the whole `SearchReport` is built before anything is
  printed (`src/cmd/search.rs:425-436`), and rendering walks the finished report.
  Peak memory therefore scales with total output size, not with a fixed window:
  `dusage search foo` over a tree where `foo` appears in millions of lines costs
  gigabytes of resident memory, and the first line appears only when the last
  file is done. `-c`, `-l`, `-L` and `-q` avoid this by discarding line data
  (`want_lines`, `src/search.rs:427`).
- **One pattern per walk.** There is no multi-pattern engine: `-e` takes a
  single pattern (`src/cmd/search.rs:259-267`), so searching for five terms means
  five walks over the same tree. The 1024-match cap bounds `-o` and colouring
  output on minified files (`src/search.rs:27`).

### 5.5 Measured versus asserted

There are no recorded throughput numbers for search in this repository.
`docs/BENCHMARKS.md` does not exist; the scans benchmarked in `README.md` cover
the disk-usage backends only, and `docs/ROADMAP.md` §4 lists the outstanding
benchmark questions for the scanner. The single measurement harness in the
tree is a `#[test]` in `src/bytes.rs` (`bench_memmem_vs_naive`, `src/bytes.rs:889-941`)
that times `memmem` against `hay.windows(n).position()` over a 4 MiB
English-like buffer and asserts at least 1.5× speedup for needles of four bytes
or more in release builds. **I did not run it** — the branch does not compile
without `src/regex.rs` — so even the claim that the 32-byte scan beats a naive
loop rests on that test's threshold, not on a number in this document. The
required-byte prefilter's hit rate is unmeasured, and the effect of the
sort-before-match design on cache locality is unmeasured.

## 6. Can it replace X

**`grep -rn` in scripts.** For the shape most scripts use — `if grep -rq word
src/; then …` — yes: the exit triad matches, `-q` still stops at the first
match, `-r` is implicit, and output is deterministic. The gaps that bite are the
ones a script cannot see coming: an unreadable directory no longer produces exit
2 (per-file errors are counted, not fatal), `-x` means `--one-filesystem` here
rather than `--line-regexp`, and `-f patterns.txt`, `--include`, `--exclude-dir`
and `-I` have no equivalent, so the script's filter has to move into `-g` and
`-t` — one glob, one type, no repetition. Reading a pipe is not supported at
all. If the script leans on any of those, it will keep needing grep.

**`rg` interactively.** For code reading with literal or simple regex terms it
gets the same results faster to type than `grep`, and its built-in cache list
(`node_modules`, `target`, `.venv`) matches what most people reach for
`--glob '!node_modules'` to do. It is not a drop-in for rg: no `-U`, `-z`,
`--pre`, `-r`, `--passthru`, `-P`, `--iglob`, `--sort`, `--type-add`, no stdin,
one `-g` and one `-t` per run, and the `-L` letter means the opposite of follow.
Case-insensitive search silently misses non-ASCII case pairs, so `rg -i café`
finds `CAFÉ` and `dusage search -i café` does not. Keep rg for anything involving
multiline records, compressed or preprocessed input, or Unicode-aware patterns.

**`find -name`.** Substitute for a human listing names: the glob language is
richer (`**`, anchored path patterns), the filters `--size`, `--older`, `--newer`
and `--ext` cover the common `-size`/`-mtime` cases, and it is far faster than
find because the walker skips ignore-rule directories. Three things break
scripts. Ours prints paths relative to the root, so `dusage find '*.rs' .` prints
`src/main.rs` where `find . -name '*.rs'` prints `./src/main.rs`. Ours exits 1
when nothing matches while find exits 0, which inverts the meaning of
`if find …; then`. And there is no `-exec`, `-delete`, `-printf`, `-print0`,
`-prune` or `-mindepth`, so any pipeline that consumes `-print0` or deletes in
place stays on find. Also note `--older 7d` selects a file aged 7 days 1 hour
that `find -mtime +7` politely leaves out.

**Windows PowerShell `Select-String`.** This is the case the tool was built for,
and it wins on speed and on code-tree hygiene: `Select-String` has no recursion
of its own (the idiom is `Get-ChildItem -Recurse | Select-String`, [Select-String
docs](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/select-string)),
it does no ignore handling at all, and its output is `MatchInfo` objects that
carry `Path`, `LineNumber` and `Line` as properties rather than text lines. Two gaps survive. First, `.NET` regex supports lookaround and
backreferences, so a pattern using them has to be rewritten or left behind.
Second, `Select-String` decodes text — it detects a BOM and otherwise assumes
UTF-8 — while we match bytes, so a UTF-16LE file is marked binary by its NUL
bytes and never matches an ASCII pattern. That is the one class of input where
the old tool is simply right and this one has no answer. Where PowerShell wrote the files
itself, keep `Select-String`; for source trees, configs and logs that are UTF-8
or plain ASCII, `dusage search` is the faster and cleaner answer.

---

## Appendix: quirks found while writing this

These are behaviours a contributor will trip over, all verified in the source
and worth a follow-up issue rather than a silent fix here:

1. `--color=auto` forces colour on even when stdout is a pipe
   (`src/cmd/search.rs:110-118`): the `auto` arm returns `true` unconditionally,
   and only the *absent* flag consults `is_terminal()`.
2. `-g` cannot override ignore rules, contradicting the module doc that calls the
   glob layer "the only thing that can re-include a path hidden by 1 or 2"
   (`src/ignore.rs:11-12` versus `src/search.rs:190`).
3. `Pattern::new` sets `required` for a case-insensitive `-F` pattern
   (`src/pattern.rs:36-46`), but `find_at` only consults it on the regex branch
   (`src/pattern.rs:105`), so the value is dead for literals.
4. `ignore::ancestors` (`src/ignore.rs:209`) is unused, so ignore files above
   the scan root never apply.
5. `Candidate.is_link` is hard-coded `false` (`src/search.rs:237`), so no output
   path can tell a symlink from a regular file, and because
   `is_dir = e.is_dir() && !e.is_reparse()` (`src/search.rs:185`) a symlink to a
   directory is listed as a file by `dusage find`.
6. `-h` in `find` scope is an unknown-option error (only `--help` works), while
   in `search` scope `-h` means `--no-filename`; the asymmetry is easy to
   misread from the help texts.

## Sources

- ripgrep man page (text as fetched, ripgrep 14.x): <https://man.archlinux.org/man/rg.1.en>; verified against local `rg --help`, ripgrep 14.1.0.
- ripgrep user guide, "Automatic filtering": <https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md#automatic-filtering>.
- GNU grep manual: <https://www.gnu.org/software/grep/manual/grep.html>; exit-status section at <https://www.gnu.org/software/grep/manual/html_node/Exit-Status.html>; flag text verified against local `grep --help`, 3.11.
- GNU find(1): <https://man7.org/linux/man-pages/man1/find.1.html>. The `findutils/manual/html_mono/find.html` page named in the original brief returns 404 today, so the man page plus local findutils 4.9.0 probes are the reference used here.
- PowerShell `Select-String`: <https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/select-string>.
- This repository, branch `feat/search-suite` at `a42f96e`: `src/cmd/search.rs`, `src/search.rs`, `src/pattern.rs`, `src/glob.rs`, `src/ignore.rs`, `src/bytes.rs`, `src/report.rs`, `src/queue.rs`, `src/cli.rs`, `src/unix/enumerate.rs`, `src/win/enumerate.rs`, `AGENTS.md`.

# Search parity: `dusage search` and `dusage find` against grep, ripgrep and find

`dusage search` (aliases `grep`, `rg`) matches file contents, and `dusage find`
(alias `ls`) matches paths, both behind the same ignore-aware walker. This
document is a map of what they do relative to GNU grep, ripgrep and GNU find:
which flag means what, which syntax works, where the semantics silently
differ, and what would bite someone swapping one for the other.

**Evidence base.** Everything stated about this tool is read from the source of
branch `parity-polish` at commit `9491ecc` and, wherever the claim is about
behaviour, was reproduced by running `./target/release/dusage` (0.5.0) built
from that commit on Linux x86_64. File and line references are given for each
claim, and the commands behind the measurements are quoted. Claims about the
*other* three tools come from their own documentation (cited inline) and from
probing the local binaries: ripgrep 14.1.0, GNU grep 3.11, GNU findutils 4.9.0.
Numbers measured here came from a 2-vCPU Ubuntu 24.04 container with a warm page
cache.

The parser itself is shared with the rest of the tool: `Args::parse`
(`src/cli.rs:33`) accepts `--name value` and `--name=value`, a bare `--`
terminator, and treats a lone `-` or a negative number as a positional. Short
flags do not cluster — `-A2` is an unknown option, `-A 2` is not — and a repeated
*boolean* flag is idempotent. Repeated *value* flags keep their whole history
(`Args::values`, `src/cli.rs:97`), and what each caller does with it is its own
business: `-e` takes the last (`src/cmd/search.rs:270`), `--size`, `--older`,
`--newer`, `--max-depth`, `--max-filesize` and `--max-columns` take the last
(`Args::value`, `src/cli.rs:92`), while `-g`/`--glob` and `-t`/`--type`/`--ext`
combine. The option lists are `SEARCH_VALUE_FLAGS` / `SEARCH_BOOL_FLAGS`
(`src/cmd/search.rs:153-215`) for `search` and an inline pair for `find`
(`src/cmd/search.rs:217-247`); an unrecognised flag is `exit 2`, not a warning.

## 1. Flag map

### 1.1 `dusage search`

| Our flag | grep / rg / find equivalent | Notes |
|---|---|---|
| `<pattern>` positional, `-e`, `--regexp` | `grep PATTERN`, `rg PATTERN`, `rg -e` | One pattern only; `-e` exists so a pattern starting with `-` can be passed, it takes the next argument, and a second `-e` replaces the first (`src/cmd/search.rs:270-278`). There is no `grep -f PATTERN_FILE` / `rg -f`. |
| `-F`, `--fixed-strings` | `grep -F` / `rg -F` | Forces literal matching. A regex the engine can prove is a literal already takes the byte fast path without `-F` (`src/pattern.rs:52-76`). |
| `-i`, `--ignore-case` | `grep -i` / `rg -i` | Splits in two: a *literal* needle containing a non-ASCII byte is folded per character with `char::to_lowercase` on both sides (`src/pattern.rs:132-139`, `src/pattern.rs:168-189`); a *regex* folds only ASCII `A-Z`, via a per-byte-set fold that adds each letter's counterpart (`src/regex.rs:93-102`, applied at `src/regex.rs:766`). See §2.3. |
| `-w`, `--word-regexp` | `grep -w` / `rg -w` | Boundaries are `[0-9A-Za-z_]` plus every byte `>= 0x80` (`src/bytes.rs:244-260`), so a non-ASCII letter counts as word text. A rejected match is retried one byte later (`src/pattern.rs:115-127`). |
| `-v`, `--invert-match` | `grep -v` / `rg -v` | Selects lines with no match. `-o` and colour spans are suppressed when inverted (`src/search.rs:668-672`). |
| `-n`, `--line-number` | `grep -n` / `rg -n` | Off by default, including on a TTY. ripgrep differs: on a TTY it enables line numbers, colours and headings by default ([rg(1)](https://man.archlinux.org/man/rg.1.en), DESCRIPTION). |
| `-H`, `--with-filename` | `grep -H` / `rg -H` | Without either flag the file name is printed when more than one file is in the report or any root is a directory (`src/cmd/search.rs:472-476`). |
| `-h`, `--no-filename` | `grep -h` / `rg -h` | `-h` is *never* help here; `--help` is (`src/cmd/search.rs:61`). GNU grep makes the same choice; ripgrep does not — its `-h` prints help and its `--no-filename` short form is `-I` ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-I, --no-filename`). |
| `-l`, `--files-with-matches` | `grep -l` / `rg -l` | Prints paths only; the line data is dropped as the report is assembled (`src/search.rs:421-426`). |
| `-L`, `--files-without-match` | `grep -L` / `rg --files-without-match` | **Not ripgrep's `-L`**: in rg, `-L` is `--follow` ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-L, --follow`). We have no follow-symlink option at all. `exit 0` when at least one path is printed (`src/search.rs:424-426`). |
| `-c`, `--count` | `grep -c` / `rg -c` | Counts *matching lines* per file. `rg --count-matches` (occurrences) has no equivalent; `--count-matches` is an unknown option and exits 2. Source: `src/report.rs:344-351`. |
| `-o`, `--only-matching` | `grep -o` / `rg -o` | One output line per match, capped at 1024 matches per input line (`src/search.rs:28`, `src/search.rs:668-694`). Measured: a 3000-byte line of `x` with pattern `x` prints 1024 lines. |
| `-q`, `--quiet` | `grep -q` / `rg -q` | Prints nothing and stops at the first matching file. This path is sequential — it walks the candidate list in order (`src/search.rs:354-372`) — so it does not use the thread pool. |
| `-m N`, `--max-count N` | `grep -m` / `rg -m` | Per file, in matching lines; the file stops being read once the limit is reached (`src/search.rs:509-537`). |
| `-A N`, `-B N`, `-C N`, `--after-context`, `--before-context`, `--context` | `grep`/`rg` same letters | `-C` sets both and each falls back in the order `-A`/`--after-context`, then `-C` (`src/cmd/search.rs:373-382`). Non-adjacent groups are separated by `--`, and only when a context window was asked for (`src/report.rs:359-365`). |
| `-a`, `--text` | `grep -a` / `rg -a` | Binary files are always *searched*; the flag only replaces the one-line `Binary file … matches` summary with normal output (`src/report.rs:352-356`). ripgrep's default is to stop at the NUL ([GUIDE](https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md#automatic-filtering)). |
| `-j N`, `--threads N` | `rg -j` | Default is one worker per core, clamped to 4..16, with 8 on a machine that will not report a core count (`src/cli.rs:195-201`). Output order does not depend on it — see §5.3. |
| `-g PATTERN`, `--glob` | `rg -g` | Repeatable, and the repeats combine: `-g '!x' -g '*.rs'` means "not x, and a .rs file". `!` negates. A **positive** glob switches the ignore layers off entirely, exactly as in rg, so it can re-include a path a `.gitignore` hid and it descends into `node_modules` (`src/ignore.rs:83-90`, `src/search.rs:204-210`). A run of only negative globs keeps them on. |
| `-t NAME`, `--type` | `rg -t` | A fixed list of file types mapped to extension globs in `ext_globs` (`src/cmd/search.rs:122-151`): rust, python, js, ts, c, cpp, go, java, ruby, md, json, toml, yaml, sh, html, css, xml, txt, log, lock, each with one or two aliases. Repeatable. **Different from `-g`**: type filters narrow results and leave the ignore rules in force (`src/ignore.rs:69-82`, `src/ignore.rs:133-140`). No `--type-list`, no `--type-add`, no `--type-not`. |
| `--max-depth N` | `find -maxdepth` | Entries directly under a root are depth 0, and a directory at depth *d* is descended while `d < N` (`src/search.rs:227-229`). So our `N` equals `find -maxdepth N+1`; measured on a three-level tree, `--max-depth 0` lists the root's children and `find -maxdepth 1` lists the same entries. |
| `--max-filesize SIZE` | `rg --max-filesize` | Applied during the walk from the size the enumerator already returned (`src/search.rs:231-235`), so an oversized file is never opened. Sizes accept `K/M/G/T`, binary multiples (`src/cli.rs:104-136`). A file named explicitly on the command line bypasses it, because explicit roots are taken as given (`src/search.rs:296-307`). |
| `--max-columns N` | `rg -M`, `--max-columns` | Truncates printed lines at N *bytes*, after matching, and clips the colour spans to the cut (`src/search.rs:591-601`). No "N matches hidden" note like rg's. |
| `--encoding NAME` | `rg -E`, `--encoding` | `auto` (default), `utf-8`, `utf-16le`, `utf-16be`, `latin1`. Search always runs on UTF-8 bytes, so a forced encoding or a detected UTF-16 file is transcoded first (`src/search.rs:449-458`, `src/encodings.rs:1-13`). Auto-detection answers only UTF-8, UTF-16LE or UTF-16BE — Latin-1 is never guessed, it must be asked for (`src/encodings.rs:72-112`). An unknown name is exit 2. |
| `-Z`, `--null` | `rg --null` / `grep -Z` | A NUL instead of `:` after the file name, for `xargs -0` (`src/report.rs:336-342`, `src/report.rs:371-379`). |
| `--hidden` | `rg --hidden` / `rg -.` | Only governs names starting with `.` (`src/search.rs:200-203`); it does not disable ignore rules. |
| `--no-ignore` | `rg --no-ignore` | Disables both the built-in cache list and `.gitignore`/`.ignore`/`.git/info/exclude` (`src/search.rs:206`, `src/search.rs:273`). Hidden files are still skipped, exactly as in rg, where `--hidden` is a separate flag ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-u/--unrestricted`). |
| `-x`, `--one-filesystem` | `rg --one-file-system`, `find -xdev` | **Not grep's `-x`** (`--line-regexp`) and not rg's `-x` either — ripgrep also spells line-regexp `-x` ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-x, --line-regexp`). We have no line-regexp mode. Stays on the root's device (`src/search.rs:216-222`); measured on `/dev`, where `/dev/shm` is a separate tmpfs, `-x` drops the five entries under the mount point. |
| `--json` | `rg --json` | Different shape: one JSON document for the whole run, not JSON Lines. See §4.2. |
| `--color[=WHEN]` | `grep --color`, `rg --color` | `always`/`yes`/`true`, `never`/`no`/`false`, `auto`. Only the `--color=WHEN` spelling is parsed (`src/cmd/search.rs:99-112`); `auto` — and an unrecognised WHEN — consults `is_terminal()` on stdout, so a pipe stays plain (`src/cmd/search.rs:114-121`). |
| `--stats` | `rg --stats` | One summary line on **stderr**, not stdout (`src/cmd/search.rs:492-494`, format in `src/report.rs:516-527`): files scanned, binary count, matched, matches, errors, elapsed. Measured: `dusage: 1 files scanned (1 binary), 1 matched, 1 matches, 0 errors, 712.99µs`. |
| `--help` | — | Prints the help text. `-h` does not. |
| `-V`, `--version` | — | Prints `dusage <version>`. |

Not present on `search`, and rejected as unknown options: `--count-matches`,
`--ext`, `--size`, `--older`, `--newer`, `--dirs`, `--files`, `--long` (find-side
only, per the `run_query` flag lists at `src/cmd/search.rs:153-247`).

### 1.2 `dusage find`

| Our flag | grep / rg / find equivalent | Notes |
|---|---|---|
| `<glob>` positional | `find -name`, `find -path` | A glob with no `/` is matched against the entry's file name, gitignore-style; one with `/` is anchored at the root (`src/glob.rs:1-40`, `src/glob.rs:142-175`). Supports `*`, `?`, `**`, `[...]`, `[!...]`, `{a,b}` expansion, `\` escapes and a trailing `/` for directories. Prefix `!` inverts. |
| `[paths...]` | find starting points | Defaults to `.` (`src/cmd/search.rs:280-286`). |
| `-F`, `--fixed` | `grep -F`, `find -name 'x'` | Substring match on the relative path instead of a glob (`src/cmd/search.rs:398-403`, `src/cmd/search.rs:419-421`). |
| `-t NAME`, `--type` | `rg -t`, `find -name '*.rs'` | Same fixed list as `search`, repeatable. A type filter also implies files-only: `find -t js` never lists directories (`src/cmd/search.rs:413-415`). |
| `--ext rs,md` | `find -name '*.rs' -o -name '*.md'` | Comma-separated, translated to `*.ext` globs and combined with any `-t` (`src/cmd/search.rs:297-305`). |
| `--size +10M` / `-10M` / `10M` | `find -size +10M` | Ours is exact: `+10M` is `size >= 10 MiB` (`src/cmd/search.rs:503-527`). GNU find rounds *up* to the unit, so its `+10M` excludes a file of exactly 10 MiB while ours includes it — confirmed against findutils 4.9.0: of files at 10 MiB, 10.5 MiB and 11 MiB, `-size +10M` printed the last two and ours printed all three. A bare `10M` here means within ±5%, which find has no equivalent for; 10.5 MiB is the upper edge and is still inside (±524,288 bytes). Only the last `--size` is honoured despite the help text calling it repeatable. |
| `--older 7d` | `find -mtime +7` | Ours compares the exact elapsed duration with `>` (`src/cmd/search.rs:433-440`); `find -mtime +7` works in whole 24-hour periods. Confirmed against findutils 4.9.0 with a file aged 7 days 1 hour: our `--older 7d` selected it, `find -mtime +7` did not, and `find -mtime 7` did. Units: `s m h d w`, or a bare number meaning days (`src/cli.rs:139-176`). |
| `--newer 7d` | `find -mtime -7`, `find -newermt` | Exact duration `<` instead of find's integer periods. |
| `--dirs` | `find -type d` | Directories only. |
| `--files` | `find -type f` | Files only. A symlink to a directory counts as a file — see the appendix. |
| `--long` | `find -ls`, `find -printf '%s %TY… %p\n'` | Prints size, modification time in UTC, and the relative path, with a trailing `/` for directories (`src/report.rs:463-482`). Directory rows always show size `0 B` and a `-` timestamp, because the enumerator reports 0 for directories (`src/unix/enumerate.rs:61`) and the walk passes `None` for a directory's mtime (`src/search.rs:223-224`). |
| `--max-depth N` | `find -maxdepth` | Same N+1 offset as `search`. |
| `-g PATTERN`, `--glob` | `find -path`, `find -prune` | Same include/`!`-exclude filter as `search`, and the same override rule: a positive glob switches ignore rules off. |
| `--hidden` | no find equivalent | GNU find lists dotfiles like any other name; we skip them unless this flag is given. |
| `--no-ignore` | no find equivalent | Without it, the built-in cache list and the ignore files apply to `find` too. |
| `-x`, `--one-filesystem` | `find -xdev` | Device comparison per directory (`src/search.rs:216-222`). |
| `-Z`, `--null` | `find -print0` | NUL after each path instead of a newline (`src/report.rs:474-476`). |
| `--json` | no find equivalent | `{"paths":[…],"count":N}` — see §4.3. |
| `--color[=WHEN]` | `ls --color`-ish | Colours the printed path only. |
| `--help`, `-V`, `--version` | — | `find -h` is rejected as an unknown option here, unlike grep, where `-h` is `--no-filename`. |

Absent relative to GNU find, and likely to matter: the expression language
(`-a`, `-o`, `!`, parentheses), `-exec`/`-execdir`, `-delete`, `-printf`,
`-prune`, `-regex`, `-iname`, `-mindepth`, `-newermt`, `-user`, `-perm`,
`-links`, `-empty`, `-depth`, `-type` (we have `--dirs`/`--files` instead).
There is no `-name` vs `-path` split: one glob, matched as described above.

### 1.3 ripgrep flags we do not have

Each of these is genuinely absent — none is spelled differently, none has a
partial stand-in in the parser.

| rg flag | What it does in ripgrep | What its absence means here |
|---|---|---|
| `-P`, `--pcre2` | Switches to the PCRE2 engine "when you want to use features such as look-around or backreferences" ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-P, --pcre2`). | No lookaround and no backreferences, in any spelling. See §2.2. A pattern that needs them cannot be expressed; ripgrep gets them only if built with PCRE2. |
| `-U`, `--multiline` | Lifts the rule that a match cannot contain a line terminator, at a cost in speed and memory ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-U, --multiline`). | Matching is strictly per line: the scanner splits on `\n` and calls `handle_line` per line (`src/search.rs:515-542`). A pattern for a record that spans lines returns nothing. |
| `-z`, `--search-zip` | Detects gzip, bzip2, xz, LZ4, LZMA, Brotli and Zstd files and decompresses them before searching ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-z, --search-zip`). | Compressed files are searched as raw bytes: no match, or a `Binary file … matches` line if the literal happens to appear in the compressed stream. |
| `--pre COMMAND`, `--pre-glob` | Searches the standard output of `COMMAND PATH` instead of the file ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--pre`). | No preprocessor. PDFs, office documents and anything else needing extraction cannot be searched. |
| `--multiline-dotall` | Makes `.` match line terminators when `-U` is on ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--multiline-dotall`). | Meaningless without multiline mode, so also absent. The inline `(?s)` flag is accepted and widens `.` to `\n`, but since the haystack is one line it changes nothing in practice (`src/regex.rs:16`). |
| `-r`, `--replace` | Rewrites every match in the *output* using capture groups, never touching files ([rg(1)](https://man.archlinux.org/man/rg.1.en), `-r REPLACEMENT`). | No output rewriting, and no `$1` capture substitution. `-o` prints the matched text only. |
| `--passthru` | Prints matching and non-matching lines, highlighting only matches ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--passthru`). | No way to emit a full file with highlights. |
| `-l` vs `--files-without-match` | rg spells the negative form `--files-without-match` and reserves `-L` for `--follow` ([rg(1)](https://man.archlinux.org/man/rg.1.en)). | We spell it `-L`. That is a direct letter collision: a script written for rg that passes `-L` to follow symlinks gets a list of files with no matches instead. |
| `--type-add`, `--type-clear`, `--type-not` | Extends or replaces the built-in type table at run time ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--type-add=TYPESPEC`). | The type table is compiled in (`src/cmd/search.rs:122-151`). An unlisted language is reachable only through `-g '*.ext'`, which has the further side effect of switching ignore rules off. |
| `--iglob` | A case-insensitive `-g`, which "always overrides any other ignore logic" ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--iglob`). | No case-insensitive glob. Our positive globs do override ignore rules (§1.1, `-g`), but they match case-sensitively. |
| `--sort SORTBY`, `--sortr` | `path`, `modified`, `accessed`, `created`; forces single-threaded traversal ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--sort=SORTBY`). | Output is always sorted by relative path (`src/search.rs:324`), so the default already equals `--sort path`; sorting by mtime or size is not available. |
| `--no-require-git` | Respects `.gitignore` even outside a git repository ([rg(1)](https://man.archlinux.org/man/rg.1.en), `--no-require-git`). | We always respect it — nothing in `src/ignore.rs` tests for a repository — which is exactly `--no-require-git` behaviour, on by default. `.git/info/exclude` is the one rule file that *is* gated on a `.git` directory (§3). |

Also missing, and worth knowing: `--files`/`--type-list` (list instead of
search), `--null-data`, `--count-matches`, `--crlf`, `--binary`, `--follow`/`-L`,
`--max-columns-preview`, `--heading`, `--no-heading`, `--trim`,
`--line-buffered`, `--block-buffered`, `-f/--file`, `--engine`, `--no-unicode`,
`-u/--unrestricted` (the `-uu`/`-uuu` ladder has no equivalent; use
`--no-ignore --hidden`), `-S/--smart-case`, `-s/--case-sensitive`, multiple
patterns (a second `-e` replaces the first), and reading **stdin**: with no paths
we always search `.` (`src/cmd/search.rs:280-286`), so `cat x | dusage search foo`
searches the directory instead of the pipe. A `-` path is rejected as a
nonexistent path with exit 2 (`src/cmd/search.rs:286-289`), and `/dev/stdin`
fails the same way — measured: `dusage: 1 path(s) could not be read`.

## 2. Regex dialect

### 2.1 What is advertised and what is structural

The help text is the shipped contract for the dialect (`src/cmd/search.rs:20-22`):

> literals, `.` `*` `+` `?` `{n,m}` `[...]` `[^...]` `(` `)` `(?:` `)` `|` `^` `$`
> `\A` `\z` `\b` `\B` `\d` `\w` `\s` and their negations; inline flags `(?i)`
> `(?s)` `(?m)`. No lookaround or backreferences.

The engine is `src/regex.rs` — a Pike VM over a Thompson NFA, so there is no
backtracking and no pathological pattern (`src/regex.rs:1-30`). Its module
documentation is the fuller contract, and it is *wider* than the help text: the
help omits POSIX bracket expressions, lazy quantifiers, `{n,}`, the `\n \r \t \f
\v \0 \xHH` escapes and the `(?-i)` / `(?ism)` group flags, all of which work.
Every row below was run against the binary.

| Construct | Status | Evidence |
|---|---|---|
| literals, `.`, `*`, `+`, `?`, `{n}`, `{n,}`, `{n,m}` | works | `a{2,3}` on `aaaa` prints `aaa` |
| lazy `*?` `+?` `??` `{n,m}?` | works | `a+?` on `aaaa` prints `a` four times, `a+` prints `aaaa` once |
| alternation (vertical bar), `(...)`, `(?:...)` | works | `foo` or `bar` alternates and prints both matches; captures are never reported |
| `^`, `$`, `\A`, `\z`, `\b`, `\B` | works | `\Ax` matches at the start of the line |
| `[abc]`, `[^abc]`, `[a-z]`, multi-byte members | works | `[é]` matches the two-byte `0xC3 0xA9` as **one** character, not two |
| `\d \D \w \W \s \S` | works, **ASCII only** | `\d` does not match U+0663 (Arabic-Indic digit three) where rg does; `\w+` on `café noir` prints `caf` and `noir` where rg prints `café` and `noir` |
| `[[:alpha:]]` and 13 friends | works | see the table below |
| `\p{…}`, `\P{…}` | **rejected at compile time, exit 2** | `dusage: Unicode property classes (\p{..}) are not supported at offset 0 in pattern "\p{L}"` |
| lookaround `(?=…)` `(?!…)` `(?<=…)` `(?<!…)` | **rejected at compile time, exit 2** | `dusage: lookaround, named groups and comments are not supported at offset 2 in pattern "a(?=b)"` |
| backreferences `\1`, `\k<name>` | **rejected at compile time, exit 2** | `dusage: backreferences are not supported at offset 3 in pattern "(a)\1"` |
| unknown POSIX class name | **rejected at compile time, exit 2** | `dusage: unknown POSIX class "bogus" at offset 1 in pattern "[[:bogus:]]"` |
| unterminated `[` or `[[:alpha:]` | **rejected at compile time, exit 2** | `dusage: unterminated character class at offset 0 in pattern "[[:alpha:]"` |
| repetition above 1000 | **rejected at compile time, exit 2** | `a{1001}` → `repetition bound above 1000 at offset 1` (`src/regex.rs:57`), enforced at `src/regex.rs:684` |
| unknown alphabetic escape | **rejected at compile time, exit 2** | `\q` → `unknown escape at offset 0 in pattern "\\q"` (`src/regex.rs:968`); `\pL` is caught earlier as a Unicode property (`src/regex.rs:963-966`) |
| `(?i)` `(?s)` `(?m)` and negations, at the pattern or a group start | works | `(?m)^x` matches; `(?i)` is ASCII-only, see §2.3 |
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

They are defined as byte ranges (`src/regex.rs:977-1034`), so they are ASCII
classes even where the locale is UTF-8, and `[[:alpha:]]` will not match `é`. A
class name outside that list is the compile error above, not a silently empty
set — which is the important half of parity, because GNU grep accepts unknown
names in a UTF-8 locale and matches nothing.

Structurally verified: the matcher receives raw bytes and returns byte offsets.
`Pattern::find_at(&self, hay: &[u8], from: usize) -> Option<Match>`
(`src/pattern.rs:107`), and `Match` is `{start, end}` in bytes, sliced directly
out of the line buffer for `-o` and colouring (`src/search.rs:676-694`). The
line is split on `b'\n'` only (`src/search.rs:519`), and a trailing `\r` is
removed so `^…$` works on CRLF files (`src/search.rs:700-706`). A pattern that
fails to compile returns `Err(String)`, which propagates to exit 2
(`src/cmd/mod.rs:18-21`, `src/cmd/mod.rs:82-94`).

Two things the engine *must* expose are visible from its callers, because
`src/pattern.rs` depends on them:

- `Regex::literal()` — if the whole pattern is a literal, the pattern is
  downgraded to the byte fast path and the engine is never consulted again
  (`src/pattern.rs:52-76`).
- `Regex::required_byte()` — at most **one** byte that every match must
  contain, used as the per-line prefilter (`src/pattern.rs:112-118`).

### 2.2 Not supported

Lookaround and backreferences are outside the advertised grammar and have no
opt-in engine. A reader coming from ripgrep's `-P`
([rg(1)](https://man.archlinux.org/man/rg.1.en), `-P, --pcre2`) or from GNU
grep's `-P` (`grep --help`: "PATTERNS are Perl regular expressions") loses both.
A reader coming from `.NET` — PowerShell's `Select-String` — loses them too,
along with `\p{…}` ([.NET regex quick
reference](https://learn.microsoft.com/en-us/dotnet/standard/base-types/regular-expression-language-quick-reference)).

The pattern is never silently reinterpreted: every one of the constructs above
is a compile error naming the offset, with exit 2, and the error text is on
stderr. That is the one thing you want from a gap. It is also a hard difference
from GNU grep, which exits 2 for an invalid backreference but *silently* treats
an unknown POSIX class name as an empty set in a UTF-8 locale.

### 2.3 Byte-oriented, and what that costs

ripgrep's default engine is Unicode-aware with UTF-8 mode; its man page says the
engine is byte-oriented but documents Unicode character classes as an addition
to ERE ([rg(1)](https://man.archlinux.org/man/rg.1.en), REGEX SYNTAX). Measured
on the local binaries, with a file containing the three characters `a`, `é`
(`0xC3 0xA9`) and `b`:

```
$ printf 'a\xc3\xa9b\n' > f.txt
$ dusage search -o --no-filename '.' f.txt | wc -l       # 4  (a, 0xC3, 0xA9, b)
$ dusage search -o --no-filename '[^x]' f.txt | wc -l    # 4
$ rg -o --no-filename '.' f.txt | wc -l                  # 3  (a, é, b)
$ LC_ALL=C grep -o '[^x]' f.txt | wc -l                  # 4  (each byte separately)
$ LC_ALL=C.UTF-8 grep -o '[^x]' f.txt | wc -l            # 3
```

Our matcher is byte-wise, full stop: `.` and `[^x]` each consume one **byte**.
That is stated in the engine's own documentation (`src/regex.rs:24-26`: "every
atom consumes bytes, so `.` matches one byte of a multi-byte character") and it
is what the first two lines above measure. The consequences are concrete:

- `-o` with pattern `.` on `é` prints two lines, one holding `0xC3` and one
  holding `0xA9` — half a character each, which a terminal renders as `?` or as
  a replacement glyph. `dusage search -o --no-filename '.' f.txt | od -c` shows
  exactly that.
- `[^x]` matches twice per `é`, so any count taken from `-o` output is a byte
  count wearing a character count's name. On CJK text the two differ by a factor
  of three, because each character is three bytes.
- A character class written with a non-ASCII member is the exception that keeps
  `-o` usable: `[é]` matches the pair as one unit (`src/regex.rs:1039-1050`).
  `.` and negated classes have no such repair.

`-w` is *not* affected any more. `is_word_byte` is
`is_ascii_alphanumeric() || '_' || b >= 0x80`, with the comment explaining why —
"Every byte of a multi-byte character counts, so a non-ASCII letter is a letter
to `-w`" (`src/bytes.rs:253-260`). Measured: `dusage search -w caf` does **not**
match inside `café`, and neither does `rg -w caf` nor `LC_ALL=C.UTF-8 grep -w
caf`; only `LC_ALL=C grep -w caf` accepts it, which is the behaviour we now
diverge from. The word test is applied to the match's byte span, so `-w caf.`
does not match `café` either — the span ends after `0xC3` and `0xA9` is still a word
byte — while `-w 'caf..'` does, because two dots consume the whole character.

`\b` inside a regular expression is the odd one out: the assertion's word test is
`is_ascii_alphanumeric() || '_'`, without the `>= 0x80` clause (`src/regex.rs:401`).
So `\bcaf\b` **does** match inside `café` (measured; rg rejects it), while
`-w caf` does not. Two spellings of "whole word" disagree on non-ASCII text.
That is a bug, not a design choice, and it is listed in the appendix.

Case folding splits the same way as the rest of the dialect:

- A **literal** needle under `-i` — from `-F`, or from a pattern the engine
  reduces to a literal — is folded per character with `char::to_lowercase` on
  both sides, but only when the needle itself holds a non-ASCII byte; a pure-ASCII
  needle takes the byte-wise `memmem_ci` scan instead (`src/pattern.rs:132-139`,
  `src/pattern.rs:168-189`). `dusage search -i 'café' up.txt` matches
  a line containing `CAFÉ`, and `dusage search -i -F 'CAFÉ' low.txt` matches
  `café`; rg agrees on both.
- A **regex** under `-i` folds ASCII `A-Z` only: the compiler adds each ASCII
  letter's counterpart to every byte set it builds (`ByteSet::fold_ascii`,
  `src/regex.rs:93-102`) and leaves every other byte alone, so `É` (`0xC3 0x89`)
  is not folded to `é`. Measured: `dusage search -i 'CAF[É]' low.txt` exits 1 on a
  file containing `café` where `rg -i 'CAF[É]'` prints the line. The literal path
  is narrower for a different reason — its scanner folds through the 256-entry
  table whose comment states the intent, "`0xC1` and `0xE1` stay distinct. A
  plain `| 0x20` would fold `@`/`` ` `` and those Latin-1 letters together"
  (`src/bytes.rs:26-30`).
- The inline `(?i)` flag does **not** reach the Unicode literal path, because
  that path is chosen by the CLI flag, not by the pattern's flags. Measured:
  `dusage search -i 'café' up.txt` matches `CAFÉ`, and
  `dusage search '(?i)café' up.txt` does not. Two spellings of "ignore case"
  disagree, and only the CLI one is Unicode-aware.

Measured for comparison on a file containing `É` (`0xC3 0x89`):

```
$ rg -i 'é' up.txt                    # matches       (Unicode folding)
$ rg --no-unicode -i 'é' up.txt       # no match      (ASCII folding)
$ LC_ALL=C.UTF-8 grep -iq 'é' up.txt  # matches
$ LC_ALL=C grep -iq 'é' up.txt        # no match
```

So `dusage search -i é` behaves like `rg -i` and like `grep` in a UTF-8 locale,
and `dusage search -i 'CAF[É]'` behaves like `rg --no-unicode -i`. Users on
non-English source trees will hit the second case, and there is no flag to
change it.

**Worked example, end to end.** A UTF-8 file with two lines, `naïve café` and
`naive cafe`:

- `dusage search -F 'café' mix.txt` — the literal path is used, the needle is
  `63 61 66 C3 A9`, and the match is exact, byte for byte. This works.
- `dusage search -o 'caf.' mix.txt` — with a byte-wise `.`, the span ends after
  `C3`, so `-o` prints `caf` plus one broken byte, then `cafe` from the second
  line. Measured with `od -c`: `c a f 303 \n c a f e \n`.
- `dusage search -w caf mix.txt` — no match, because `0xC3` is a word byte.

## 3. Ignore semantics

The walker applies three layers (`src/ignore.rs:1-15`): built-in cache names,
ignore files discovered while walking, and explicit `-g` globs, of which a
positive one switches the first two off.

1. **Built-in names.** Thirteen exact names are skipped wherever they appear as a
   path component: `.git`, `.hg`, `.svn`, `node_modules`, `target`, `__pycache__`,
   `.venv`, `venv`, `.tox`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`,
   `.gradle` (`src/ignore.rs:20-35`). Matching is per component, not on the whole
   relative path (`builtin_hit`, `src/ignore.rs:256-259`), so `foo/target/x.rs`
   is skipped while `targets/` and `dist/` are not — measured: `foo/target` does
   not even appear in the listing. The rule applies to files as well as
   directories. `--no-ignore` turns the list off.
2. **Ignore files.** `.gitignore` and `.ignore`, read in that order from every
   directory the walk enters (`RULE_FILES`, `src/ignore.rs:37`;
   `push_dir`, `src/ignore.rs:206-230`), so `.ignore` overrides `.gitignore`
   within a directory. Each layer applies to paths below its own directory,
   deeper layers override shallower ones (`is_ignored`,
   `src/ignore.rs:165-199`), and `!` re-includes. A `.git/info/exclude` is read
   too, but **only at a directory that actually holds a `.git` directory**
   (`src/ignore.rs:212-215`); it is appended after both rule files, so it wins
   over both. Measured: with `.gitignore` naming `both.txt` and `.ignore` saying
   `!both.txt`, the file is re-included; with `.git/info/exclude` naming `b.txt`,
   a `!b.txt` in `.ignore` cannot rescue it. A stray `<dir>/.git/info/exclude`
   with no `.git` directory next to it has no effect (measured). Deliberately
   absent: the global `core.excludesFile` (`$XDG_CONFIG_HOME/git/ignore`),
   `.rgignore`, and ignore files *above* the scan root. `ancestors()` exists in
   `src/ignore.rs:264` but has no caller, so a `.gitignore` in a parent
   directory has no effect on a scan rooted below it.
3. **`-g` globs.** Positive globs restrict, `!` globs exclude, and directories
   are never pruned by an include glob because their children may still match
   (`GlobFilter::walk_allows`, `src/ignore.rs:107-130`). A positive glob is an
   **override**, not an addition: `IgnoreRules::whitelisted()` returns true and
   the walker skips `is_ignored` altogether for that run
   (`src/ignore.rs:83-90`, `src/search.rs:204-210`). So `-g '*.log'` finds a file
   a `.gitignore` hides, and `-g '*.js'` descends into `node_modules` — both
   measured. Negative globs alone do not do this: `-g '!node_modules'` on its own
   leaves the built-in list in force. `-t`/`--ext` never switch ignores off;
   they are pushed as a separate `GlobFilter::filter` set that only narrows
   (`src/ignore.rs:69-82`, `src/ignore.rs:133-140`).

**No repository is required.** Nothing in `src/ignore.rs` tests for a
repository, so a directory with a `.gitignore` is filtered identically inside
and outside one. ripgrep does the opposite by default — "ripgrep will only
respect filter rules from source control ignore files when ripgrep detects that
the search is executed inside a source control repository"
([rg(1)](https://man.archlinux.org/man/rg.1.en), `--no-require-git`) — which I
confirmed: outside a git repository rg listed all three files despite a
`.gitignore` excluding `*.log`, and inside one it listed only `keep.txt`.

**Hidden means a leading dot.** `--hidden` toggles the check
`name.starts_with('.')` (`src/search.rs:200-203`). This also means an ignore
*file* is never itself a result, and `.git` is skipped twice over (hidden and
built-in) — until a positive `-g` switches the built-in layer off, at which
point `--hidden -g '*.txt'` does list `.git` and `.git/info`.

### 3.1 Behaviour combinations

| Invocation | `.gitignore` / `.ignore` / `.git/info/exclude` | Built-in caches | Dotfiles | Notes |
|---|---|---|---|---|
| `dusage search pat` | applied | applied | skipped | Default. Ripgrep additionally requires a git repo and honours the global gitignore and `.rgignore` ([GUIDE](https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md#automatic-filtering)); ours does not. |
| `dusage search --hidden pat` | applied | applied | **searched** | Same relationship as `rg --hidden`. |
| `dusage search --no-ignore pat` | **ignored** | **ignored** | skipped | Equivalent to rg's `-u` only; rg needs `-uu` to also take hidden files. |
| `dusage search --hidden --no-ignore pat` | ignored | ignored | searched | Ours: `rg -uu`. |
| `dusage search -g '*.rs' pat` | **ignored** | **ignored** | skipped | A positive glob switches both ignore layers off (rg's override rule), so it re-includes what they hid and descends into cache directories. |
| `dusage search -g '!x' -g '*.rs' pat` | ignored | ignored | skipped | The globs **combine** — exclusion `x`, then the `*.rs` include — because `Args::values` keeps every occurrence (`src/cli.rs:97-99`). rg accepts repeats the same way. |
| `dusage search -g '!node_modules' pat` | applied | applied | skipped | Only negative globs: ignore rules stay on, so this particular exclusion is redundant. It matters after `--no-ignore` or beside a positive glob. |
| `dusage search -t rust pat` | applied | applied | skipped | A type filter narrows without touching ignore rules, so `-t js` finds nothing in a tree where the only `.js` files sit under `node_modules` or a gitignored `build/` (measured). |
| `dusage search -t rs -t js pat` | applied | applied | skipped | Repeated types combine into one alternative set. |
| `dusage find '*.rs'` | applied | applied | skipped | `find` (ours) inherits every ignore rule; GNU find has none of this and lists dotfiles too. |
| `dusage find --size +10M '*'` | applied | applied | skipped | `--size`, `--older`, `--newer`, `--dirs` and `--files` are applied after the walk to the candidates it kept, so they never widen it (`src/cmd/search.rs:416-450`). |

Measured counterpart, for the built-in list: with no ignore file present at all,
ripgrep listed `./node_modules/p/i.js`; ours skips `node_modules` by name. There
is no rg option that reproduces the cache list — the nearest is a hand-written
`--glob '!node_modules'` per directory name, and ripgrep has no built-in
equivalent. A repository full of `node_modules` therefore behaves differently
under the two tools, and `--no-ignore` (or any positive `-g`) is the escape
hatch.

## 4. Exit codes and output contract

### 4.1 Exit codes

`0` = something was selected, `1` = nothing was, `2` = misuse, an unusable
invocation, or **nothing was selected and some path could not be read**. That is
grep's contract, including its quiet-mode rule: "if the `-q` or `--quiet` or
`--silent` option is used and a line is selected, the exit status is 0 even if
an error occurred" ([grep manual, Exit
Status](https://www.gnu.org/software/grep/manual/html_node/Exit-Status.html));
our `-q` returns as soon as the first file matches (`src/search.rs:354-372`).

| Code | `search` | `find` |
|---|---|---|
| 0 | At least one line selected (or, with `-L`, at least one path printed). The `any_match` flag is set in `src/search.rs:409-432` and consumed in `src/cmd/search.rs:495-499`, **including when read errors occurred**: measured, a run that matched one readable file and failed on two unreadable ones exited 0. | At least one path survived the filters (`src/cmd/search.rs:464-468`), again even with read errors. |
| 1 | No match and no read error. Returned with an empty message so nothing extra is printed (`src/cmd/search.rs:495-499`). | No path matched and no read error. Note GNU find exits 0 here — confirmed: `find . -name zzz` returned 0. A script that treats non-zero as failure will behave differently. |
| 2 | Unknown flag, missing or empty pattern, pattern that does not compile, malformed number or size, unknown `--type` or `--encoding`, a root path that does not exist (`src/cmd/search.rs:280-289`), **or no match after at least one path could not be read**. | Same, including a glob that fails to validate (unterminated `[...]`, trailing `\`, bare `!`) and the read-error rule. |

The read-error rule is the one that changed from earlier drafts of this
document, and it cuts both ways. A failing path is reported on stderr as
`dusage: <N> path(s) could not be read` (`src/cmd/search.rs:477-480`,
`src/cmd/search.rs:460-463`) and, when something else matched, is otherwise
harmless: `exit 0`. When nothing matched, the failure decides the code, so a run
that selected nothing because half the tree was unreadable reports 2 rather than
1 — you cannot mistake it for "clean, no matches". GNU grep is blunter: any
error is exit 2 even when a line matched. With `-q` the count is neither printed
nor fatal, exactly as in grep. Measured with a `chmod 000` file and a `chmod 000`
directory: `dusage search needle .` → 0 with `2 path(s)` on stderr;
`dusage search zzz .` → 2; `dusage find 'zzz*' .` → 2; `dusage search -q needle .`
→ 0 and no stderr.

A nonexistent *root* is still caught up front and is exit 2 with its own message
(`src/cmd/search.rs:286-289`), which covers the common scripting case
separately from the permission case.

### 4.2 `--json` for `dusage search`

One JSON document, no pretty-printing, emitted by `render_search_json`
(`src/report.rs:401-460`). This is the real schema, with the field list taken
from the format strings:

```json
{
  "files": [
    {
      "path": "./src/main.rs",
      "matches": 1,
      "binary": false,
      "lines": [
        {
          "line_number": 2,
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
    "files_scanned": 1,
    "files_matched": 1,
    "files_binary": 0,
    "bytes_scanned": 24,
    "matches": 1,
    "errors": 0,
    "elapsed_secs": 0.000693531
  }
}
```

That is verbatim output from the binary, whitespace aside. Field notes, all from
the source: `matches` counts matching **lines** per file; `submatches` are byte
offsets into `text` — into the *transcoded* text when an encoding was applied,
so they do not index the bytes on disk — and are empty for `-v` and for context
lines (`src/search.rs:652-670`); `context` is `true` for a line pulled in by
`-A`/`-B`/`-C`; `lines` is `[]` when `-c`, `-l` or `-L` is in effect, because
those modes throw the line data away (`src/search.rs:421-428`, `src/report.rs:415-418`); `binary` is a
per-file boolean that stays `false` when no block was read, and also `false` for
a transcoded UTF-16 or Latin-1 file, which is never examined for NULs;
`stats.errors` is a single count, not the per-category object the scan report
uses; `elapsed_secs` is a float. `text` goes through
`String::from_utf8_lossy`, so invalid sequences become U+FFFD and a repaired
line is indistinguishable in the JSON from one that really contained the
replacement character — measured: a line `needle \xff\xfe bad` yields
`"needle \ufffd\ufffd bad"`. Control characters and quotes are escaped by
`json_escape` (`src/report.rs:155-171`), so a NUL in a `-a` line appears as
`\u0000`.

### 4.3 `--json` for `dusage find`

From `render_paths_json` (`src/report.rs:492-513`):

```json
{
  "paths": [
    {
      "rel": "src/main.rs",
      "path": "./src/main.rs",
      "size": 24,
      "is_dir": false,
      "modified": "2026-11-18 18:12:49"
    }
  ],
  "count": 1
}
```

`rel` is the path relative to the scan root with `/` separators, even on
Windows (`join_rel`, `src/search.rs:264-270`); `path` is the root exactly as
typed on the command line joined with the entry name, so with the default root
`.` it keeps a `./` prefix; `modified` is a UTC `YYYY-MM-DD HH:MM:SS` string
(`fmt_time`, `src/report.rs:276-284`), not an ISO-8601 timestamp with an offset,
and it is the literal `"-"` for directories and for anything whose time could
not be read (`src/report.rs:277`). Directory rows always carry `"size": 0`
(`src/unix/enumerate.rs:61`).

### 4.4 Text output

`search`: `path:line:text` per match, `path:line-text` for context lines, `--`
between non-adjacent groups **only when a context window was requested**, and
`Binary file <path> matches` for a binary file with a match unless `-a`
(`src/report.rs:326-398`). With `-Z` the separator after the path becomes a NUL
and the line number keeps its own colon. `find`: one relative path per line, or
`size  time  path` with a trailing `/` for directories in `--long` mode
(`src/report.rs:463-482`); `-Z` swaps the newline for a NUL.

## 5. Performance model

### 5.1 The walk

`walk` is single-threaded and deterministic: for each root it lists candidates,
applying hidden, ignore and glob decisions as it descends, then sorts the whole
list by relative path (`src/search.rs:289-326`). Content mode drops directories
from the list (`src/search.rs:322`); path mode keeps them.

On Windows the size and mtime in that list come free with the name, because
`src/win/enumerate.rs` is `GetFileInformationByHandleEx` with
`FILE_ID_EXTD_DIR_INFO` — "one syscall returns many entries (name, sizes,
attributes, times, file id), so no per-file open or `stat` is needed"
(`src/win/enumerate.rs:3-5`). **On Linux it is one `lstat` per entry**: the
module doc says "`std::fs::read_dir` plus one `lstat` per entry"
(`src/unix/enumerate.rs:1-3`), and `AGENTS.md` records the same fact — "sizes
still need one `statx` per entry … so the Linux `batched` backend is 'one stat per
entry', not 'many entries per syscall'". So the walk is stat-free on Windows and
stat-per-entry on Linux; it is never dtype-only on either. `--max-filesize`
is applied from that already-fetched size (`src/search.rs:231-235`), and `-x`
costs one extra device lookup per directory (`src/search.rs:216-222`).

### 5.2 Matching

Two paths, chosen at compile time of the pattern:

- **Literal.** `-F`, or any regex the engine reduces to a literal
  (`src/pattern.rs:52-76`), goes to `bytes::memmem_from`. That scan broadcasts
  the first needle byte across a `u64`, XORs four words per iteration — 32 bytes
  — and tests the OR-ed zero flags once per block, only entering the byte-wise
  loop when a block holds a candidate (`src/bytes.rs:123-162`). Verification is
  tail-first for needles of four bytes or more, so a stray first-byte hit is
  usually rejected without reading the interior (`src/bytes.rs:164-183`). No
  `unsafe`, no allocation, no dependency.
- **Regex.** One byte the pattern requires gates each line: `find_at` asks
  `bytes::find_byte` for `re.required_byte()` before the engine runs
  (`src/pattern.rs:112-118`). A single required byte is a weak filter — it rules
  out lines that lack one specific character, nothing more — and a pattern with
  no required byte skips the prefilter entirely.
- **Encoding.** A file whose detected encoding needs transcoding is read whole
  and converted to UTF-8 before scanning; UTF-8 streams through the 64 KiB block
  loop (`src/search.rs:449-458`). That is the one input class that costs an
  extra full-file copy (`src/encodings.rs:5-10`).

Files are read with `File::open` plus `read` into a 64 KiB buffer
(`src/search.rs:23`, `src/search.rs:439-461`). Binary detection looks for a NUL
in the first 8 KiB of the first block only (`src/search.rs:25`,
`src/search.rs:459-467`), and a file marked binary is still searched in full —
only the rendering changes (`src/report.rs:352-356`).

### 5.3 Parallelism and output order

`par_for_each` (`src/queue.rs:175`) runs the candidate list across `--threads`
workers. The default is `available_parallelism()` clamped to **4..16**, with 8 as
the fallback when the platform will not say (`src/cli.rs:195-201`). The comment
there explains the shape: the work is syscall-bound, so a few threads past the
core count hide latency while heavy oversubscription hurts. That replaced the old
`4 × cores` rule; on a 2-vCPU machine the default is now 4 workers, not 8, and on
a 4-vCPU machine it is 4, not 16.

Each result is stored in a pre-allocated slot indexed by its position in the
sorted candidate list, and stats are aggregated under the same mutex
(`src/search.rs:375-410`). Output is therefore byte-identical regardless of
thread count and completion order: the sort happens before matching, not after.
Measured: the same 860-line report from `--threads 1`, `2` and `8` has the same
MD5. `-q` bypasses the pool and scans sequentially so it can stop early
(`src/search.rs:354-372`).

### 5.4 What is not optimised

- **No mmap.** Every file is read with `File::open` plus `read` into a 64 KiB
  heap buffer allocated per file (`src/search.rs:441`); there is no `MAP_PRIVATE`
  path and no `--mmap`.
- **No incremental line reuse.** Each line is matched from offset 0
  (`handle_line` calls `find_at(text, 0)`, `src/search.rs:626`), and `carry`
  holds only the partial line that spans a read boundary
  (`src/search.rs:514-541`). A resumed scan across blocks exists; a resumable
  *matcher* across lines does not.
- **Every hit is copied and kept.** `LineHit.text` is `text.to_vec()`
  (`src/search.rs:676-696`), the whole `SearchReport` is built before anything is
  printed (`src/cmd/search.rs:482-492`), and rendering walks the finished report.
  Peak memory therefore scales with total output size, not with a fixed window:
  `dusage search foo` over a tree where `foo` appears in millions of lines costs
  gigabytes of resident memory, and the first line appears only when the last
  file is done. `-c`, `-l`, `-L` and `-q` avoid this by discarding line data
  (`want_lines`, `src/search.rs:501-511`).
- **One pattern per walk.** There is no multi-pattern engine: a second `-e`
  replaces the first (`src/cmd/search.rs:270-278`), so searching for five terms
  means five walks over the same tree. The 1024-match cap bounds `-o` and
  colouring output on minified files (`src/search.rs:28`).
- **Unicode case folding for literals allocates.** A case-insensitive needle with
  a non-ASCII byte builds a folded copy of the whole line plus a byte map before
  scanning it (`src/pattern.rs:168-189`), once per line.

### 5.5 Measured versus asserted

`docs/BENCHMARKS.md` exists and is the place the throughput numbers live: it
tabulates `bench/run.sh` output for `du`, `find`, `grep`, `rg` and `dusage` over
`/usr` and `/usr/local/go`, with the harness header and the raw CSV reproduced.
It was recorded with `dusage` at its old `4 × cores` default — its own preamble
says "4 threads per core — 16 worker threads on this box" — so its `dusage` rows
are a slight understatement of what the current 4..16 default gives on a
many-core machine, and it should be regenerated with `./bench/run.sh` before its
numbers are quoted as current.

Re-measured here on a 2-vCPU container (default 4 workers, warm cache, median of
three runs, times in ms, same trees as the harness):

```
dusage search -F asyncpreemptoff  358     rg 128     grep -rI 187
dusage search -F 'func '          535     rg 216     grep -rI 176
dusage search '[A-Z][a-zA-Z0-9_]*\.go'  2322  rg 112  grep -rIE 277
dusage search -i memmove          351     rg 111
dusage find '*.go'                108     find 62
```

The shape is consistent with the recorded table: a literal scan is roughly 2–3×
slower than rg, and a general regex is an order of magnitude slower, because the
Pike VM is unoptimised beyond the one-byte prefilter while rg runs a lazily
compiled DFA. None of that changes a result — §5.3 guarantees identical output —
so the cost is latency, not correctness, and it stays inside the same order of
magnitude as `grep`.

The one in-tree measurement harness is a `#[test]` in `src/bytes.rs`
(`bench_memmem_vs_naive`, `src/bytes.rs:893-945`) that times `memmem` against
`hay.windows(n).position()` over a 4 MiB English-like buffer and asserts at
least 1.5× speedup for needles of four bytes or more in release builds. I did not
run it here; the numbers above are end-to-end wall clock, which is the claim that
matters. The required-byte prefilter's hit rate is unmeasured, and the effect of
the sort-before-match design on cache locality is unmeasured.

## 6. Can it replace X

**`grep -rn` in scripts.** For the shape most scripts use — `if grep -rq word
src/; then …` — yes: the exit triad matches, `-q` still stops at the first
match, `-r` is implicit, and output is deterministic. The gaps that bite are the
ones a script cannot see coming: `-x` means `--one-filesystem` here rather than
`--line-regexp`, `-f patterns.txt`, `--include`, `--exclude-dir` and `-I` have
no equivalent, so the script's filter has to move into `-g` and `-t` — and a
positive `-g` quietly disables the repository's own ignore files. Reading a pipe
is not supported at all. The exit code for a partially unreadable tree is now 2
when nothing matched, which is closer to grep than it used to be, but it is 0
when something did match. If the script leans on any of those, it will keep
needing grep.

**`rg` interactively.** For code reading with literal or simple regex terms it
gets the same results faster to type than `grep`, and its built-in cache list
(`node_modules`, `target`, `.venv`) matches what most people reach for
`--glob '!node_modules'` to do. It is not a drop-in for rg: no `-U`, `-z`,
`--pre`, `-r`, `--passthru`, `-P`, `--iglob`, `--sort`, `--type-add`, no stdin,
one pattern per run, and the `-L` letter means the opposite of follow. Repeated
`-g` and `-t` do combine, so the common "several globs" habit carries over.
Case-insensitive search is the sharp edge: `-i` folds non-ASCII only for a
literal needle, so `rg -i 'CAF[É]'` finds `café` and `dusage search -i 'CAF[É]'`
does not — while a plain literal `dusage search -i café` does. Keep rg for anything involving
multiline records, compressed or preprocessed input, or regular expressions over
non-ASCII text.

**`find -name`.** Substitute for a human listing names: the glob language is
richer (`**`, anchored path patterns, `{a,b}`), the filters `--size`, `--older`,
`--newer` and `--ext` cover the common `-size`/`-mtime` cases, and it is usually
faster than find because the walker skips ignore-rule directories. Three things
break scripts. Ours prints paths relative to the root, so `dusage find '*.rs' .`
prints `src/main.rs` where `find . -name '*.rs'` prints `./src/main.rs`. Ours
exits 1 when nothing matches while find exits 0, which inverts the meaning of
`if find …; then`. And there is no `-exec`, `-delete`, `-printf`, `-prune` or
`-mindepth`, so any pipeline that consumes `-print0` or deletes in place stays on
find — `-Z` covers the NUL-separated listing half of `-print0` but there is
nothing to pipe it into. Also note `--older 7d` selects a file aged 7 days 1
hour that `find -mtime +7` politely leaves out, and `--size +10M` includes a file
of exactly 10 MiB that `find -size +10M` drops.

**Windows PowerShell `Select-String`.** This is the case the tool was built for,
and it wins on speed and on code-tree hygiene: `Select-String` has no recursion
of its own (the idiom is `Get-ChildItem -Recurse | Select-String`,
[Select-String
docs](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/select-string)),
it does no ignore handling at all, and its output is `MatchInfo` objects that
carry `Path`, `LineNumber` and `Line` as properties rather than text lines. Two
gaps survive. First, `.NET` regex supports lookaround and backreferences, so a
pattern using them has to be rewritten or left behind. Second, `Select-String`
decodes text — it detects a BOM and otherwise assumes UTF-8 — and we now do the
same: a UTF-16LE file is detected from its BOM or its alternating-NUL pattern and
transcoded, so `dusage search needle u16le.txt` finds the line and prints it as
UTF-8, where rg needs `--encoding utf-16le` (measured: rg 14.1.0 exits 1 on that
file without the flag). Latin-1 is the remaining difference, and it is opt-in:
auto-detection never guesses it, so pass `--encoding latin1` for files written by
legacy Windows tools. Where PowerShell wrote the files itself, keep
`Select-String`; for source trees, configs and logs, `dusage search` is the
faster and cleaner answer.

---

## Appendix: quirks found while writing this

These are behaviours a contributor will trip over, all verified against the
binary and the source, and worth a follow-up issue rather than a silent fix
here:

1. `\b` and `-w` disagree on non-ASCII text: the in-regex assertion tests
   `is_ascii_alphanumeric() || '_'` (`src/regex.rs:401`) while `-w` also counts
   every byte `>= 0x80` (`src/bytes.rs:257-260`). `dusage search -o '\bcaf\b'`
   prints `caf` out of `café`; `dusage search -w caf` does not match. rg refuses
   both.
2. `(?i)` inside a pattern never reaches the Unicode literal path, which only the
   `-i` CLI flag selects (`src/pattern.rs:37-49` versus `src/pattern.rs:168-189`),
   so `dusage search -i 'café'` matches `CAFÉ` and `dusage search '(?i)café'`
   does not.
3. The `find` help text calls `--size` repeatable, but `run_query` reads it with
   `Args::value` and only the last occurrence counts
   (`src/cmd/search.rs:406-409`). Measured: `--size +10M --size -11M` behaves as
   `-11M` alone.
4. `Pattern::new` sets `required` for a case-insensitive `-F` pattern
   (`src/pattern.rs:39-47`), but `find_at` only consults it on the regex branch
   (`src/pattern.rs:107-118`), so the value is dead for literals.
5. `ignore::ancestors` (`src/ignore.rs:264`) is unused, so ignore files above the
   scan root never apply — the function's own doc comment describes a behaviour
   nothing performs.
6. `Candidate.is_link` is hard-coded `false` (`src/search.rs:259`), so no output
   path can tell a symlink from a regular file, and because
   `is_dir = e.is_dir() && !e.is_reparse()` (`src/search.rs:199`) a symlink to a
   directory is listed as a file by `dusage find --files` (measured: a symlink
   named `linkdir` pointing at a directory appears under `--files` and not under
   `--dirs`).
7. `-h` in `find` scope is an unknown-option error (only `--help` works), while
   in `search` scope `-h` means `--no-filename`; the asymmetry is easy to misread
   from the help texts.
8. An unrecognised `--color=WHEN` value is not an error: it falls through to the
   terminal test in `color_enabled` (`src/cmd/search.rs:114-121`), so a typo like
   `--color=allways` silently means `auto`.

## Sources

- ripgrep man page (text as fetched, ripgrep 14.x): <https://man.archlinux.org/man/rg.1.en>; verified against local `rg --help`, ripgrep 14.1.0.
- ripgrep user guide, "Automatic filtering": <https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md#automatic-filtering>.
- GNU grep manual: <https://www.gnu.org/software/grep/manual/grep.html>; exit-status section at <https://www.gnu.org/software/grep/manual/html_node/Exit-Status.html>; flag text verified against local `grep --help`, 3.11.
- GNU find(1): <https://man7.org/linux/man-pages/man1/find.1.html>. The `findutils/manual/html_mono/find.html` page named in the original brief returns 404 today, so the man page plus local findutils 4.9.0 probes are the reference used here.
- PowerShell `Select-String`: <https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/select-string>.
- This repository, branch `parity-polish` at `9491ecc`: `src/cmd/search.rs`, `src/search.rs`, `src/pattern.rs`, `src/regex.rs`, `src/glob.rs`, `src/ignore.rs`, `src/bytes.rs`, `src/encodings.rs`, `src/report.rs`, `src/queue.rs`, `src/cli.rs`, `src/unix/enumerate.rs`, `src/win/enumerate.rs`, `AGENTS.md`, `docs/BENCHMARKS.md`. Every command quoted in this document was run against `./target/release/dusage` built from that commit.

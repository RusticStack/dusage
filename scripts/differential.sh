#!/usr/bin/env bash
# Differential correctness suite: cross-checks `dusage search` / `dusage find` against
# ripgrep and GNU grep on fixture trees built from scratch.
#
#   scripts/differential.sh [--keep] [--verbose] [--only <name-prefix>]
#
# Every content case compares sorted stdout of dusage with the other tool(s) and the exit
# code. `eq` cases must agree; a disagreement fails the run. `known:<id>` cases are the
# catalogue in scripts/README.md: they are expected to differ, and the run says `FIXED`
# when they start to agree so the entry can be retired. `doc:<id>` cases pin behaviour
# dusage documents deliberately (its own exit codes and error paths).
set -uo pipefail
set -f # no pathname expansion: flag strings split on spaces only, globs stay literal

REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN=${DUSAGE_BIN:-$REPO/target/release/dusage}
KEEP=0
VERBOSE=0
ONLY=""

while [ $# -gt 0 ]; do
    case "$1" in
    --keep) KEEP=1 ;;
    -v | --verbose) VERBOSE=1 ;;
    --only)
        shift
        ONLY=${1:-}
        ;;
    -h | --help)
        sed -n '2,10p' "$0"
        exit 0
        ;;
    *)
        echo "unknown argument: $1" >&2
        exit 2
        ;;
    esac
    shift
done

agreed=0
differed=0
failed=0
skipped=0
FAILED_NAMES=""

say() { printf '%s\n' "$*"; }
warn() { printf '%s\n' "$*" >&2; }
have() { command -v "$1" >/dev/null 2>&1; }

HAVE_RG=0
HAVE_GREP=0
have rg && HAVE_RG=1
have grep && HAVE_GREP=1
[ "$HAVE_RG" = 1 ] || warn "rg not found: ripgrep comparisons are skipped"
[ "$HAVE_GREP" = 1 ] || warn "grep not found: grep comparisons are skipped"

# `du` and `find` are compared against GNU coreutils/findutils only: BSD's versions print
# different units and accept a different expression list, so a case run against them would
# fail for a reason the harness cannot explain. `du --version` and `find --version` are the
# cheapest discriminator and both tools print their banner first.
have_gnu() { "$1" --version 2>/dev/null | head -n 1 | grep -q GNU; }
HAVE_DU=0
have du && have_gnu du && HAVE_DU=1
[ "$HAVE_DU" = 1 ] || warn "GNU du not found: du comparisons are skipped"
HAVE_FIND=0
have find && have_gnu find && HAVE_FIND=1
[ "$HAVE_FIND" = 1 ] || warn "GNU find not found: find comparisons are skipped"

RG_EXTRA=""
# ripgrep honours .gitignore only inside a git repository unless told otherwise.
if [ "$HAVE_RG" = 1 ] && ! have git; then
    if rg --no-require-git --version >/dev/null 2>&1; then
        RG_EXTRA="--no-require-git"
    fi
fi

if [ "${DUSAGE_SKIP_BUILD:-0}" != 1 ]; then
    say "building $BIN"
    if ! cargo build --release --manifest-path "$REPO/Cargo.toml" >/dev/null; then
        warn "cargo build failed"
        exit 2
    fi
fi
[ -x "$BIN" ] || {
    warn "no dusage binary at $BIN"
    exit 2
}

WORK=$(mktemp -d "${TMPDIR:-/tmp}/dusage-diff.XXXXXX") || exit 2
cleanup() {
    if [ "$KEEP" = 1 ]; then
        say "fixture kept at $WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

# --- fixture ---------------------------------------------------------------
# One directory per behaviour, so a case can only fail for the reason it tests:
#   plain/   LF-only text: ASCII, UTF-8, empty file, missing trailing newline, 1 MiB line
#   crlf/    CRLF line endings
#   enc/     Latin-1, a binary file, UTF-16LE with BOM
#   tree/    ignore rules (root and nested), builtin cache names, hidden entries
#   strict/  root-level ignore rules with no subdirectories
#   one/     a single file, to pin down file-name printing
#   links/   symlinks to a file and to a directory
#   wipe/    a rule-less directory beside ignored siblings
# Every directory in tree/ that is not itself ignored carries an ignore file that matches
# nothing: the walker drops its parents' rules when it leaves a directory with no ignore
# file (catalogue entry `ancestor-wipe`), so a natural tree cannot test anything else about
# ignore rules. `wipe/` is the tree that demonstrates that bug.
make_fixture() {
    local w=$1

    mkdir -p "$w/plain/sub/deep"
    printf 'alpha needle beta\nsecond line without it\nneedle again needle\n' >"$w/plain/ascii.txt"
    printf '# Notes\nA needle in markdown.\nfn other() {}\nneedle\there\nno trailing newline needle\n' >"$w/plain/notes.md"
    printf 'nested needle line\nanother nested line\n' >"$w/plain/sub/nested.txt"
    printf 'deep needle here\nterminal line\n' >"$w/plain/sub/deep/leaf.txt"
    printf 'alpha 123 needle\nalpha 1234 needle\nplain 12 needle\n' >"$w/plain/digits.txt"
    printf 'fn needle() { let x = 42; }\n' >"$w/plain/codes.rs"
    printf 'fn needle() {}\n' >"$w/plain/sub/more.rs"
    printf 'no trailing newline needle' >"$w/plain/nonl.txt"
    : >"$w/plain/empty.txt"
    printf 'caf\xc3\xa9 needle na\xc3\xafve \xe2\x80\x94 \xc3\xbcn\xc3\xafc\xc3\xb6d\xc3\xa9\ncaf\xc3\xa9\n' >"$w/plain/utf8.txt"
    printf 'caf\xc3\xa9 na\xc3\xafve at the top\ncaf\xc3\xa9\n' >"$w/plain/sub/utf8b.txt"
    printf 'needle\there\ntwo  spaces needle\n' >"$w/plain/ws.txt"
    { head -c 1048576 /dev/zero | tr '\0' 'x'; printf 'needle-tail\n'; } >"$w/plain/long.txt"

    mkdir -p "$w/crlf"
    printf 'crlf needle\r\nsecond line\r\n' >"$w/crlf/crlf.txt"
    printf 'another needle line\r\nsecond line\r\n' >"$w/crlf/other.txt"
    printf 'a plain needle line\ncrlf again\r\n' >"$w/crlf/plain.txt"

    mkdir -p "$w/enc"
    printf 'needle plain\n' >"$w/enc/plain.txt"
    printf 'header\x00needle\x00tail\n' >"$w/enc/blob.bin"
    printf 'caf\xe9 needle latin1\n' >"$w/enc/latin1.txt"
    # UTF-16LE with BOM: "needle utf16" as two-byte code units.
    printf '\xff\xfen\x00e\x00e\x00d\x00l\x00e\x00 \x00u\x00t\x00f\x001\x006\x00\r\x00\n\x00' >"$w/enc/utf16le.txt"

    mkdir -p "$w/tree/src" "$w/tree/build" "$w/tree/vendor" "$w/tree/node_modules/x" "$w/tree/hidden"
    printf '*.log\n!keep.log\nbuild/\nnode_modules/\n' >"$w/tree/.gitignore"
    printf 'ignored-by-dot-ignore\n' >"$w/tree/.ignore"
    printf 'in app\n' >"$w/tree/app.rs"
    printf 'in a log\n' >"$w/tree/app.log"
    printf 'in keep log\n' >"$w/tree/keep.log"
    printf 'in build output\n' >"$w/tree/build/out.txt"
    printf 'in lib source\n' >"$w/tree/src/lib.rs"
    printf 'in generated log\n' >"$w/tree/src/gen.log"
    printf 'generated.txt\n' >"$w/tree/src/.gitignore"
    printf 'in generated source\n' >"$w/tree/src/generated.txt"
    printf 'in vendor source\n' >"$w/tree/vendor/index.js"
    printf 'matches-nothing\n' >"$w/tree/vendor/.gitignore"
    printf 'in node_modules\n' >"$w/tree/node_modules/index.js"
    printf 'in hidden file\n' >"$w/tree/.dotfile.txt"
    printf 'in hidden dir\n' >"$w/tree/hidden/.secret.txt"
    printf 'matches-nothing\n' >"$w/tree/hidden/.gitignore"

    mkdir -p "$w/strict"
    printf '*.log\n!keep.log\n' >"$w/strict/.gitignore"
    printf 'in a log\n' >"$w/strict/a.log"
    printf 'in keep log\n' >"$w/strict/keep.log"
    printf 'in a text file\n' >"$w/strict/a.txt"

    mkdir -p "$w/dot"
    printf 'in hidden rust\n' >"$w/dot/.hidden.rs"
    printf 'in visible rust\n' >"$w/dot/visible.rs"

    mkdir -p "$w/one"
    printf 'only-here needle\nsecond line\n' >"$w/one/single.txt"

    mkdir -p "$w/links/sub"
    printf 'needle in target\n' >"$w/links/target.txt"
    printf 'needle in other\n' >"$w/links/sub/other.txt"
    ln -s target.txt "$w/links/link.txt" 2>/dev/null
    ln -s sub "$w/links/linkdir" 2>/dev/null

    mkdir -p "$w/wipe/p/d1" "$w/wipe/p/d2"
    printf '*.log\n' >"$w/wipe/.gitignore"
    printf 'needle root log\n' >"$w/wipe/top.log"
    printf 'needle d1 log\n' >"$w/wipe/p/d1/a.log"
    printf 'needle d2 log\n' >"$w/wipe/p/d2/b.log"

    if have git && ! git -C "$w" rev-parse --git-dir >/dev/null 2>&1; then
        git -C "$w" init -q >/dev/null 2>&1
    fi
}

make_fixture "$WORK"

# --- size fixture: du and find ---------------------------------------------
# `size/` is the one tree both the `du` and the `find` groups run against, so a case can
# name a size or an age and both tools see the same entry. Every size below is a whole
# number of KiB on purpose: dusage rounds each file *up into its block* while GNU du
# rounds a subtree's total up *once*, so sub-KiB sizes would make the `-k` rows disagree
# for a rounding reason that has nothing to do with the walk. The `rounding/` tree is the
# one that demonstrates that difference, deliberately.
#
# `many/` holds 400 files of 4096 bytes: the file sizes are already multiples of the 4 KiB
# allocation unit of the filesystem on purpose, so the only number that can differ between
# `du -k many` and `dusage du -k many` is the directory inode's own allocation.
mkfile() { head -c "$2" /dev/zero >"$1"; }

make_size_fixture() {
    local w=$1/size
    mkdir -p "$w/sub/deep" "$w/excl" "$w/hl" "$w/aged/empty-dir"
    mkfile "$w/a.txt" 1024
    mkfile "$w/b.bin" 5120
    mkfile "$w/exact.dat" 2048
    mkfile "$w/kb2.dat" 3072
    : >"$w/empty.zero"
    mkfile "$w/big.dat" 102400
    mkfile "$w/mib3.dat" 3145728
    mkfile "$w/sub/c.txt" 3072
    mkfile "$w/sub/deep/d.txt" 7168
    mkfile "$w/excl/skip.log" 2048
    mkfile "$w/excl/keep.txt" 1024
    mkfile "$w/hl/orig.txt" 4096
    ln "$w/hl/orig.txt" "$w/hl/link2.txt"
    # A holey file: apparent size 1 MiB, allocated near zero, which is the one row that
    # shows `du-allocation` without depending on how the filesystem tiles a small file.
    truncate -s 1048576 "$w/sparse.dat"
    # Ages are 10 and 100 days, and `-newermt` cuts at 50 days, so every time case has a
    # 40-day margin on both sides and cannot flip while the suite runs.
    mkfile "$w/aged/old.txt" 1024
    mkfile "$w/aged/ancient.txt" 1024
    mkfile "$w/aged/fresh.txt" 1024
    mkfile "$w/aged/perm640.txt" 1024
    chmod 640 "$w/aged/perm640.txt"
    touch -d '10 days ago' "$w/aged/old.txt"
    touch -d '100 days ago' "$w/aged/ancient.txt"

    mkdir -p "$1/rounding"
    printf 'abc' >"$1/rounding/s1"
    printf 'abcde' >"$1/rounding/s2"
    printf 'abcdefg' >"$1/rounding/s3"
    printf 'abcdefghi' >"$1/rounding/s4"

    mkdir -p "$1/many"
    local i=0
    while [ "$i" -lt 400 ]; do
        mkfile "$1/many/f$i.dat" 4096
        i=$((i + 1))
    done
}

# --- compressed fixture: -z -------------------------------------------------
# The decoder fixtures under tests/fixtures/ are checked in byte for byte, so `-z` is
# compared on streams a real encoder produced. They are copied into the work tree because
# the originals live inside this git repository, where rg's ignore rules apply.
#
# gzip has no checked-in fixture; it is built here with the gzip binary when one exists,
# because it is the one format every rg build can read and therefore the only one that can
# carry the `-z` cases on a machine where rg was built without the others.
make_zip_fixture() {
    local src=$REPO/tests/fixtures w=$1/zip
    mkdir -p "$w"
    # Two formats ship a plaintext called mixed.txt, so every copy is prefixed: the last
    # copy would otherwise silently become the expectation of the first.
    cp "$src/bz2/mixed_200k.bz2" "$w/bz2_200k.bz2" 2>/dev/null
    cp "$src/bz2/mixed_200k.txt" "$w/bz2_200k.txt" 2>/dev/null
    cp "$src/bz2/corrupt.bz2" "$w/bz2_corrupt.bz2" 2>/dev/null
    cp "$src/lz4/mixed-100k.lz4" "$w/lz4_100k.lz4" 2>/dev/null
    cp "$src/lz4/mixed-100k.txt" "$w/lz4_100k.txt" 2>/dev/null
    cp "$src/xz/mixed.xz" "$w/xz_mixed.xz" 2>/dev/null
    cp "$src/xz/mixed.txt" "$w/xz_mixed.txt" 2>/dev/null
    cp "$src/zstd/text3.zst" "$w/zstd_text3.zst" 2>/dev/null
    cp "$src/zstd/text.bin" "$w/zstd_text.bin" 2>/dev/null
    cp "$src/brotli/mixed-q11.br" "$w/br_mixed.br" 2>/dev/null
    cp "$src/brotli/mixed.txt" "$w/br_mixed.txt" 2>/dev/null
    printf 'gzip needle line one\ngzip second line needle\nplain tail needle\n' >"$w/gz.txt"
    if have gzip; then
        gzip -c "$w/gz.txt" >"$w/gz.txt.gz"
    fi
}

make_size_fixture "$WORK"
make_zip_fixture "$WORK"

# The cut `-newermt` uses: between the 10-day-old and the 100-day-old fixture files, so the
# row cannot flip while the suite runs.
NEWER_MT=$(date -u -d '50 days ago' +%Y-%m-%d 2>/dev/null)
[ -n "$NEWER_MT" ] || NEWER_MT=1970-01-01

# One probe for the whole find-expression group. `src/findexpr.rs` is a library; until the
# CLI takes an expression, `dusage find . -name '*'` rejects the first token and exits 2,
# and every expression case is the catalogue entry `findexpr-unwired`.
FINDEXPR=0
UNWIRED=0
if (cd "$WORK/one" && "$BIN" find . -name '*' >/dev/null 2>&1); then
    FINDEXPR=1
fi

# --- helpers ---------------------------------------------------------------
# Sorted stdout with the `./` prefix removed. Group separators are dropped here: dusage
# prints one between any two non-adjacent matches, grep and rg only when a context flag is
# given (catalogue entries `separator-no-context` and `separator-interfile`), and two
# cases compare them with the separators kept.
normalize() { sed -e 's|^\./||' -e '/^--$/d' | LC_ALL=C sort; }
# The same, keeping the separators, for the cases that test them.
normalize_kept() { sed -e 's|^\./||' | LC_ALL=C sort; }
# `-z` echoes lines out of a decoded stream, and the bzip2 fixture's plaintext holds CRLF
# lines: dusage drops the trailing `\r` of a CRLF line (catalogue entry `crlf-strip`) while
# rg echoes it. Only that byte is dropped, on both sides; nothing else is rewritten.
zip_normalize() { sed -e 's|^\./||' -e 's/\r$//' | LC_ALL=C sort; }

show_diff() {
    if have diff; then
        diff -u "$2" "$1" | sed -n '3,14p' | sed -e 's/^+/  dusage: /' -e 's/^-/  other : /'
    else
        say "  dusage:"; sed -n '1,12p' "$1" | sed 's/^/    /'
        say "  other :"; sed -n '1,12p' "$2" | sed 's/^/    /'
    fi
}

# run_one <out-prefix> <workdir> <cmd...>: writes <prefix>.out and <prefix>.code
run_one() {
    local prefix=$1 dir=$2
    shift 2
    (cd "$dir" && "$@") >"$prefix.out" 2>/dev/null
    printf '%s' $? >"$prefix.code"
}

selected() {
    [ -z "$ONLY" ] || [ "${1#"$ONLY"}" != "$1" ]
}

# Lines in a compared output. It rides in the `--verbose` detail of every du, find-expression
# and `-z` case, because two empty outputs compare equal and would otherwise look like a
# passing case.
count() { wc -l <"$1" | tr -d ' '; }

# verdict <expect> <state> <name> <detail>
verdict() {
    local expect=$1 state=$2 name=$3 detail=${4:-}
    case "$expect" in
    eq | doc:*)
        if [ "$state" = ok ]; then
            agreed=$((agreed + 1))
            [ "$VERBOSE" = 1 ] && say "ok    [$name] $detail"
        else
            failed=$((failed + 1))
            FAILED_NAMES="$FAILED_NAMES $name"
            say "FAIL  [$name] $detail"
        fi
        ;;
    known:*)
        if [ "$state" = ok ]; then
            say "FIXED [$name] ${expect#known:} no longer differs; retire it from the catalogue"
        else
            differed=$((differed + 1))
            [ "$VERBOSE" = 1 ] && say "known [$name] ${expect#known:}"
        fi
        ;;
    *)
        failed=$((failed + 1))
        FAILED_NAMES="$FAILED_NAMES $name"
        say "FAIL  [$name] unknown expectation $expect"
        ;;
    esac
    return 0
}

# content_case <name> <expect> <tools> <tree> <target> <pattern> <dusage> <rg> <grep>
content_case() {
    local name=$1 expect=$2 tools=$3 tree=$4 target=$5 pattern=$6
    local dflags=${7:-} rflags=${8:-} gflags=${9:-}
    selected "$name" || {
        skipped=$((skipped + 1))
        return 0
    }
    local dir="$WORK/$tree" out="$WORK/.case" keep=0
    case "$name" in separator_*) keep=1 ;; esac
    local state=ok detail="" outputs="" dcode

    # shellcheck disable=SC2086
    run_one "$out.d" "$dir" "$BIN" search $dflags "$pattern" "$target"
    if [ "$keep" = 1 ]; then
        normalize_kept <"$out.d.out" >"$out.d.norm"
    else
        normalize <"$out.d.out" >"$out.d.norm"
    fi
    dcode=$(cat "$out.d.code")

    if [ "$tools" = 3 ] || [ "$tools" = rg ]; then
        if [ "$HAVE_RG" = 0 ]; then
            skipped=$((skipped + 1))
            return 0
        fi
        # shellcheck disable=SC2086
        run_one "$out.r" "$dir" rg --no-heading --color never $RG_EXTRA $rflags "$pattern" "$target"
        if [ "$keep" = 1 ]; then
            normalize_kept <"$out.r.out" >"$out.r.norm"
        else
            normalize <"$out.r.out" >"$out.r.norm"
        fi
        if ! cmp -s "$out.d.norm" "$out.r.norm"; then
            state=mismatch
            detail="dusage vs rg"
            [ "$expect" = eq ] && show_diff "$out.d.norm" "$out.r.norm"
        elif [ "$dcode" != "$(cat "$out.r.code")" ]; then
            state=mismatch
            detail="exit $dcode vs rg $(cat "$out.r.code")"
        fi
        outputs="rg"
    fi

    if [ "$tools" = 3 ] || [ "$tools" = grep ]; then
        if [ "$HAVE_GREP" = 0 ]; then
            skipped=$((skipped + 1))
            return 0
        fi
        # shellcheck disable=SC2086
        run_one "$out.g" "$dir" grep $gflags "$pattern" "$target"
        if [ "$keep" = 1 ]; then
            normalize_kept <"$out.g.out" >"$out.g.norm"
        else
            normalize <"$out.g.out" >"$out.g.norm"
        fi
        if ! cmp -s "$out.d.norm" "$out.g.norm"; then
            state=mismatch
            detail="${detail:+$detail, }dusage vs grep"
            [ "$expect" = eq ] && show_diff "$out.d.norm" "$out.g.norm"
        elif [ "$dcode" != "$(cat "$out.g.code")" ]; then
            state=mismatch
            detail="${detail:+$detail, }exit $dcode vs grep $(cat "$out.g.code")"
        fi
        outputs="${outputs:+$outputs+}grep"
    fi

    verdict "$expect" "$state" "$name" "$detail${outputs:+ (against $outputs)}"
}

# find_case <name> <expect> <tree> <target> <dusage-find-args> <find(1)-args>
# find(1) exits 0 when nothing matches, so only output is compared.
find_case() {
    local name=$1 expect=$2 tree=$3 target=$4 dargs=${5:-} fargs=${6:-}
    selected "$name" || {
        skipped=$((skipped + 1))
        return 0
    }
    local dir="$WORK/$tree" out="$WORK/.case"

    # shellcheck disable=SC2086
    run_one "$out.d" "$dir" "$BIN" find $dargs "$target"
    normalize <"$out.d.out" >"$out.d.norm"
    # shellcheck disable=SC2086
    run_one "$out.f" "$dir" find $fargs
    normalize <"$out.f.out" >"$out.f.norm"

    local state=ok detail="dusage find vs find(1)"
    if ! cmp -s "$out.d.norm" "$out.f.norm"; then
        state=mismatch
        [ "$expect" = eq ] && show_diff "$out.d.norm" "$out.f.norm"
    fi
    verdict "$expect" "$state" "$name" "$detail"
}

# find_expr_case <name> <expect> <tree> <expr> [nul]
# The same GNU find argv is handed to both tools -- `dusage find . -name '*.txt'` against
# `find . -name '*.txt'` -- so no row needs translating once the expression language is
# wired to the CLI. Only stdout is compared, as in `find_case`: find(1) exits 0 even when
# nothing matched. `nul` switches the splitter from lines to `-print0`'s NUL-terminated
# records before sorting.
find_expr_case() {
    local name=$1 expect=$2 tree=$3 expr=$4 nul=${5:-}
    # `expr` is one expectation for a capability that may not exist yet: a real `eq` once
    # the CLI takes expressions, and the catalogue entry `findexpr-unwired` while it does
    # not. Written as a plain `known:` in the table, a half-wired evaluator would answer
    # FIXED and never fail the run, which defeats the point.
    if [ "$expect" = expr ]; then
        if [ "$FINDEXPR" = 1 ]; then expect=eq; else expect=known:findexpr-unwired; fi
    fi
    selected "$name" || {
        skipped=$((skipped + 1))
        return 0
    }
    if [ "$HAVE_FIND" != 1 ]; then
        skipped=$((skipped + 1))
        return 0
    fi
    [ "$FINDEXPR" = 1 ] || UNWIRED=$((UNWIRED + 1))
    local dir="$WORK/$tree" out="$WORK/.case" state=ok

    # shellcheck disable=SC2086
    run_one "$out.d" "$dir" "$BIN" find $expr
    # shellcheck disable=SC2086
    run_one "$out.f" "$dir" find $expr
    if [ "$nul" = nul ]; then
        tr '\0' '\n' <"$out.d.out" | LC_ALL=C sort >"$out.d.norm"
        tr '\0' '\n' <"$out.f.out" | LC_ALL=C sort >"$out.f.norm"
    else
        normalize <"$out.d.out" >"$out.d.norm"
        normalize <"$out.f.out" >"$out.f.norm"
    fi

    if ! cmp -s "$out.d.norm" "$out.f.norm"; then
        state=mismatch
        [ "$expect" = eq ] && show_diff "$out.d.norm" "$out.f.norm"
    fi
    verdict "$expect" "$state" "$name" "dusage find vs find(1), $(count "$out.f.norm") line(s)"
}

# du_case <name> <expect> <tree> <target> <dusage-flags> <du-flags>
# Both sides get the same start point, so the printed paths already agree and the only
# normalisation is the sort: dusage prints post-order and GNU du's order follows its own
# traversal. Sizes, units, the `total` label and the separator are compared as printed.
# The GNU side is asked for `--apparent-size` (except under `-b`, which implies it, and
# `--inodes`, which ignores size) because that is the mode dusage documents; plain GNU du
# reports allocation and is catalogued as `du-allocation`.
du_case() {
    local name=$1 expect=$2 tree=$3 target=$4 dflags=${5:-} uflags=${6:-}
    selected "$name" || {
        skipped=$((skipped + 1))
        return 0
    }
    if [ "$HAVE_DU" != 1 ]; then
        skipped=$((skipped + 1))
        return 0
    fi
    local dir="$WORK/$tree" out="$WORK/.case" state=ok

    # shellcheck disable=SC2086
    run_one "$out.d" "$dir" "$BIN" du $dflags $target
    # shellcheck disable=SC2086
    run_one "$out.u" "$dir" du $uflags $target
    LC_ALL=C sort <"$out.d.out" >"$out.d.norm"
    LC_ALL=C sort <"$out.u.out" >"$out.u.norm"

    if ! cmp -s "$out.d.norm" "$out.u.norm"; then
        state=mismatch
        [ "$expect" = eq ] && show_diff "$out.d.norm" "$out.u.norm"
    fi
    verdict "$expect" "$state" "$name" "dusage du vs du(1), $(count "$out.u.norm") line(s)"
}

# --- -z: one capability probe per format ------------------------------------
# A format is comparable only when the local rg can decode it faithfully. The probe asks
# `rg -z` on the stream and `rg` on the plaintext the stream was made from for the same
# count: that is ripgrep against itself, so it can never mask a dusage bug, and it catches
# both a build without the decoder (brotli in the default feature set: no matches at all)
# and a decoder that misreads the stream (this rg on lz4 frames: one match in 100 KiB of
# text). A format that fails the probe is named in the `note:` line before the summary and
# counted as skipped, never as agreed.
ZIPDIR="$WORK/.zip"
ZIP_SKIPS=""

zip_probe() { # zip_probe <fmt> <packed> <plain> <needle> [missing-reason]
    local fmt=$1 packed=$2 plain=$3 needle=$4 missing=${5:-fixture missing} rp rq
    mkdir -p "$ZIPDIR"
    if [ "$HAVE_RG" = 0 ]; then
        printf 'rg not installed' >"$ZIPDIR/$fmt.why"
        return 0
    fi
    if [ ! -f "$WORK/zip/$packed" ]; then
        printf '%s' "$missing" >"$ZIPDIR/$fmt.why"
        return 0
    fi
    rq=$(cd "$WORK/zip" && rg -c "$needle" "$plain" 2>/dev/null)
    rp=$(cd "$WORK/zip" && rg -z -c "$needle" "$packed" 2>/dev/null)
    if [ -n "$rq" ] && [ "$rp" = "$rq" ]; then
        : >"$ZIPDIR/$fmt.ok"
    elif [ -z "$rp" ]; then
        printf 'rg -z reads no %s stream from %s' "$fmt" "$packed" >"$ZIPDIR/$fmt.why"
    else
        printf 'rg -z counts %s in %s, its own plaintext search counts %s' \
            "$rp" "$packed" "$rq" >"$ZIPDIR/$fmt.why"
    fi
}

# zip_case <name> <expect> <fmt> <packed> <needle> <dusage-flags> <rg-flags>
zip_case() {
    local name=$1 expect=$2 fmt=$3 packed=$4 needle=$5 dflags=${6:-} rflags=${7:-}
    selected "$name" || {
        skipped=$((skipped + 1))
        return 0
    }
    if [ ! -f "$ZIPDIR/$fmt.ok" ]; then
        skipped=$((skipped + 1))
        ZIP_SKIPS="$ZIP_SKIPS${ZIP_SKIPS:+, }$name ($(cat "$ZIPDIR/$fmt.why" 2>/dev/null))"
        return 0
    fi
    local dir="$WORK/zip" out="$WORK/.case" state=ok

    # shellcheck disable=SC2086
    run_one "$out.d" "$dir" "$BIN" search $dflags "$needle" "$packed"
    zip_normalize <"$out.d.out" >"$out.d.norm"
    # shellcheck disable=SC2086
    run_one "$out.r" "$dir" rg $rflags "$needle" "$packed"
    zip_normalize <"$out.r.out" >"$out.r.norm"

    local detail="dusage -z vs rg -z, $(count "$out.r.norm") line(s)"
    if ! cmp -s "$out.d.norm" "$out.r.norm"; then
        state=mismatch
        [ "$expect" = eq ] && show_diff "$out.d.norm" "$out.r.norm"
    elif [ "$(cat "$out.d.code")" != "$(cat "$out.r.code")" ]; then
        state=mismatch
        detail="$detail, exit $(cat "$out.d.code") vs rg $(cat "$out.r.code")"
    fi
    verdict "$expect" "$state" "$name" "$detail"
}

# doc_case <name> <tree> <expected-exit> <dusage-args...>: a dusage contract, not a diff.
doc_case() {
    local name=$1 tree=$2 want=$3
    shift 3
    selected "$name" || {
        skipped=$((skipped + 1))
        return 0
    }
    local out="$WORK/.case" code state=ok
    run_one "$out.d" "$WORK/$tree" "$BIN" "$@"
    code=$(cat "$out.d.code")
    # An error code (2) must be silent; a success must print something.
    if [ "$code" != "$want" ]; then
        state=mismatch
        say "  exit $code, wanted $want"
    elif [ "$want" = 2 ] && [ -s "$out.d.out" ]; then
        state=mismatch
        say "  exit $want printed $(wc -c <"$out.d.out" | tr -d ' ') byte(s) of stdout, wanted none"
    elif [ "$want" != 2 ] && [ ! -s "$out.d.out" ]; then
        state=mismatch
        say "  exit $want printed nothing, wanted output"
    fi
    verdict "doc:$name" "$state" "$name" "exit $code"
}

# --- corpus ----------------------------------------------------------------
say ""
say "== content: dusage / rg / grep (plain/)"
content_case literal_basic eq 3 plain . needle "" "" -rE
content_case literal_absent eq 3 plain . zzzz-absent "" "" -rE
content_case line_numbers eq 3 plain . needle -n -n -rnE
content_case case_insensitive eq 3 plain . Needle -i -i -riE
content_case case_insensitive_upper eq 3 plain . NEEDLE "-i -n" "-i -n" -rniE
content_case word_flag eq 3 plain . needle -w -w -rwE
content_case word_flag_no_partial eq 3 plain . needles -w -w -rwE
content_case invert_match eq 3 plain . needle -v -v -rvE
content_case files_with_matches eq 3 plain . needle -l -l -rlE
content_case files_without_match eq 3 plain . needle -L --files-without-match -rLE
content_case only_matching eq 3 plain . needle -o -o -roE
content_case only_matching_numbers eq 3 plain . needle "-o -n" "-o -n" -rnoE
content_case max_count_1 eq 3 plain . needle "-m 1" -m1 "-rE -m 1"
content_case fixed_strings eq 3 plain . nee.le -F -F -rF
content_case regex_dot eq 3 plain . nee.le "" "" -rE
content_case anchored_start eq 3 plain . '^alpha' -n -n -rnE
content_case anchored_end eq 3 plain . 'line$' -n -n -rnE
content_case class_range eq 3 plain . '[0-9]{2,3}' -o -o -roE
content_case class_negated eq 3 plain . '[^a-z]needle' -n -n -rnE
content_case word_boundary_escape eq 3 plain . '\bneedle\b' -n -n -rnE
content_case alternation eq rg plain . 'alpha|needle' -c -c -rcE
content_case optional_group eq 3 plain . 'need(le)?' -o -o -roE
content_case one_or_more eq 3 plain . 'needle+' -o -o -roE
content_case space_class eq rg plain . '\sneedle' -c -c -rcE
content_case escaped_parens eq 3 plain . '\(\)' -n -n -rnE
content_case utf8_literal eq 3 plain . 'café' -n -n -rnE
content_case utf8_second_word eq 3 plain . 'naïve' -n -n -rnE
content_case tab_escape eq rg plain . 'needle\there' -n -n -rnE
content_case no_trailing_newline eq 3 plain . 'newline needle' -n -n -rnE
content_case two_file_roots eq 3 plain ascii.txt needle -n -n -nE
content_case invalid_regex eq 3 plain . '[a-' "" "" -rE
# `-r` is ripgrep's flag: in grep `-r` means recursive, so these compare with rg only.
content_case replace_literal eq rg plain . 'needle' '-r X' '-r X' ''
content_case replace_group eq rg plain . 'need(le)' '-r X$1' '-r X$1' ''
content_case replace_whole_match eq rg plain . 'needle' '-r A${0}B' '-r A${0}B' ''
content_case replace_only_matching eq rg plain . 'need(le)' '-o -r G$1' '-o -r G$1' ''
# `-U` is ripgrep's multiline flag: grep has no equivalent, so these compare with rg only.
content_case multiline_lines eq rg plain . 'alpha needle\nsecond' '-U -n' '-U -n' ''
content_case multiline_match_only eq rg plain . 'alpha needle\nsecond' '-U -o' '-U -o' ''
content_case multiline_no_flag eq rg plain . 'alpha needle\nsecond' '-n' '-n' ''
content_case multiline_count eq rg plain . 'needle\n' '-U -c' '-U -c' ''
content_case type_add eq rg plain . needle '--type-add web:*.md -t web -l' '--type-add web:*.md -t web -l' ''
content_case type_extend known:type-plus rg plain . needle '--type-add rust:+*.md -t rust -l' '--type-add rust:+*.md -t rust -l' ''
content_case type_clear eq rg plain . needle '--type-clear rust -t rust -l' '--type-clear rust -t rust -l' ''
content_case bre_literal_plus eq grep plain . 'nee+dle' '-G' '' -rG
content_case bre_operator_plus eq grep plain . 'nee\+dle' '-G' '' -rG
content_case bre_group eq grep plain . '\(nee\)dle' '-G' '' -rG
content_case grep_include eq grep plain . needle '--include *.txt -l' '' '-rl --include=*.txt'
content_case grep_exclude_dirs eq rg plain . needle '--exclude-dir sub -l' '--glob !sub -l' ''
content_case exact_repeat eq 3 plain . 'needle{1}' '-o' '-o' -roE
content_case quit_matched eq 3 plain . needle -q -q -rqE
content_case quit_absent eq 3 plain . zzzz-absent -q -q -rqE
content_case ctx_after eq 3 plain sub needle '-A 1' -A1 '-rE -A 1'
content_case ctx_before eq 3 plain sub needle '-B 1' -B1 '-rE -B 1'
content_case ctx_both eq 3 plain sub needle '-C 1' -C1 '-rE -C 1'
content_case count_per_file eq rg plain . needle -c -c -rcE
content_case count_inverted eq rg plain . needle "-c -v" "-c -v" -rcvE
content_case noncapturing_group eq rg plain . '(?:alpha|needle)' -c -c -rcE
content_case inline_case_flag eq rg plain . '(?i)NEEDLE' -n -n -rnE
content_case digit_class eq rg plain . '\d+' -o -o -roE
content_case long_line_paths eq 3 plain . 'needle-tail' -l -l -rlE
content_case glob_txt eq rg plain . needle '-g *.txt' '-g *.txt' -rE
content_case type_rust eq rg plain . needle '-t rust' '-t rust' -rE
content_case max_depth eq rg plain . needle '--max-depth 1' '--max-depth 1' -rE
content_case max_count_zero eq rg plain . needle '-m 0' '-m 0' '-rE -m 0'
content_case separator_no_context eq 3 plain ascii.txt needle -n -n -rnE
content_case separator_across_files eq 3 plain sub needle '-C 1' -C1 '-rE -C 1'

say ""
say "== line endings (crlf/)"
content_case crlf_start_anchor known:crlf-strip 3 crlf . '^crlf' -n -n -rnE
content_case crlf_plain_line known:crlf-strip 3 crlf . 'needle line' -n -n -rnE
content_case crlf_end_anchor known:crlf-anchor 3 crlf . 'second line$' -n -n -rnE

say ""
say "== ignore rules: dusage vs rg (tree/, strict/)"
content_case ignore_strict_default eq rg strict . needle "" "" -rE
content_case ignore_strict_no_ignore eq rg strict . needle --no-ignore --no-ignore -rE
content_case ignore_strict_paths eq rg strict . needle -l -l -rlE
content_case ignore_tree_default eq rg tree . in -l -l -rlE
content_case ignore_tree_all eq rg tree . in "" "" -rE
content_case ignore_tree_no_ignore eq rg tree . in --no-ignore --no-ignore -rE
content_case ignore_tree_hidden eq rg tree . in "--hidden -l" "--hidden -l" -rlE
content_case ignore_tree_glob_rs eq rg tree . in '-g *.rs' '-g *.rs' -rE
content_case ignore_tree_type_rust eq rg tree . in '-t rust' '-t rust' -rE
content_case ignore_tree_glob_exclude eq rg tree . in '--no-ignore -g !vendor/**' '--no-ignore -g !vendor/**' -rE
content_case ignore_nested_rules eq rg tree . source "" "" -rE
content_case ignore_parent_rules eq rg tree src in "" "" -rE
content_case ignore_glob_reinclude eq rg strict . in '-l -g *.log' '-l -g *.log' -rlE
content_case ignore_hidden_glob eq rg dot . in '-t rust' '-t rust' -rE

say ""
say "== find: dusage find vs find(1) (plain/)"
find_case find_glob_txt eq plain . '*.txt' '. -name *.txt'
find_case find_glob_recursive eq plain . '**/*.txt' '. -name *.txt'
find_case find_path_glob eq plain . 'sub/*.txt' 'sub -maxdepth 1 -name *.txt'
find_case find_substring eq plain . '-F deep' '. -path *deep*'
find_case find_dirs eq plain . '--dirs *' '. -mindepth 1 -type d'
find_case find_no_match eq plain . '*.nope' '. -name *.nope'
find_case find_mindepth eq plain . '--mindepth 2 *' '. -mindepth 2'
find_case find_name_only eq plain . '--name *.md' '. -name *.md'
find_case find_newermt eq plain . '--newermt 2000-01-01 *.txt' '. -newermt 2000-01-01 -name *.txt'
find_case find_empty eq plain . '--empty *' '. -empty'
# No `--null-data` cases here: a record may hold newlines, so the harness cannot sort the
# outputs into a comparable order, and ripgrep's file order is whatever its workers finish
# in. `tests/search_cli.rs` asserts the exact bytes instead.

say ""
say "== find expressions: dusage find vs GNU find (size/)"
# One row per test the expression language has to reproduce, run as the same argv on both
# sides. `expr` means: a real `eq` comparison once the CLI takes expressions, the catalogue
# entry `findexpr-unwired` while it does not. Today `src/findexpr.rs` is only a library, so
# `dusage find` takes a glob positional, answers the first expression token with "unknown
# option" and exits 2; the rows become live comparisons the moment that changes, without an
# edit here, and a half-wired evaluator then fails the run instead of passing quietly.
find_expr_case find_expr_name expr size '. -name *.txt'
find_expr_case find_expr_iname expr size '. -iname *.TXT'
find_expr_case find_expr_path expr size '. -path *aged*'
find_expr_case find_expr_type_file expr size '. -type f'
find_expr_case find_expr_type_dir expr size '. -type d'
find_expr_case find_expr_size_plus_k expr size '. -type f -size +4k'
find_expr_case find_expr_size_minus_k expr size '. -type f -size -4k'
find_expr_case find_expr_size_exact_k expr size '. -type f -size 3k'
find_expr_case find_expr_size_plus_m expr size '. -type f -size +1M'
find_expr_case find_expr_mtime_plus expr size '. -type f -mtime +5'
find_expr_case find_expr_mtime_minus expr size '. -type f -mtime -1'
find_expr_case find_expr_mtime_exact expr size '. -type f -mtime 0'
find_expr_case find_expr_mmin_plus expr size '. -type f -mmin +60'
find_expr_case find_expr_mmin_minus expr size '. -type f -mmin -60'
find_expr_case find_expr_newermt expr size ". -newermt $NEWER_MT"
find_expr_case find_expr_empty expr size '. -empty'
find_expr_case find_expr_perm expr size '. -type f -perm 640'
find_expr_case find_expr_links expr size '. -type f -links 2'
find_expr_case find_expr_maxdepth expr size '. -maxdepth 1'
find_expr_case find_expr_mindepth expr size '. -mindepth 2'
find_expr_case find_expr_prune expr size '. -name aged -prune -o -name *.txt -print'
find_expr_case find_expr_print0 expr size '. -name *.txt -print0' nul
find_expr_case find_expr_printf expr size '. -type f -printf %8s|%-28p|%.6f\n'
find_expr_case find_expr_combined expr size '( -name *.txt -o -name *.bin ) ! -path *aged*'

say ""
say "== du: dusage du vs GNU du (size/)"
# Both tools default to allocated blocks, so neither side passes a size switch except where
# the case names one: `-b` implies apparent size, and `--inodes` ignores size entirely.
# Paths, units and the `total` label are compared exactly; only the sort is applied, because
# the two tools print post-order in their own traversal order.
du_case du_default eq . size '' ''
du_case du_all eq . size '-a' '-a '
du_case du_summarize eq . size '-s' '-s '
du_case du_total eq . size '-c' '-c '
du_case du_max_depth_1 eq . size '-d 1' '-d 1 '
du_case du_bytes eq . size '-b' '-b'
du_case du_kib eq . size '-k' '-k '
du_case du_mib eq . size '-m' '-m '
du_case du_human eq . size '-h' '-h '
du_case du_si eq . size '--si' '--si '
du_case du_inodes eq . size '--inodes' '--inodes'
du_case du_separate_dirs eq . size '-S' '-S '
du_case du_threshold eq . size '-t 2K' '-t 2K '
du_case du_threshold_neg eq . size '-t -3K' '-t -3K '
du_case du_exclude eq . size '--exclude *.log' '--exclude=*.log '
du_case du_exclude_dir eq . size '--exclude excl' '--exclude=excl '
# One hardlink pair under -a: the directory is one inode's worth (4096 bytes, not 8192) and
# only one of the two names is listed, which is what both tools do with a deduplicated pair.
du_case du_hardlinks eq . size/hl '-a -b' '-a -b'
# A zero-length file and a 1 KiB one, named as two start points, so both a file root and
# the empty-file arithmetic are pinned.
du_case du_zero_length eq . 'size/empty.zero size/a.txt' '-b' '-b'
# Sizes below 1 KiB, where the two tools used to round differently: 4 files of 3..9 bytes are
# now the same 1 block on both sides, so this replaced its catalogue entry.
du_case du_block_rounding eq . rounding '' ''

say ""
say "== du: catalogued differences"
# Plain GNU du reports allocated blocks; dusage reports apparent size. The holey fixture
# file makes the gap structural rather than a rounding coincidence.
du_case du_allocation known:du-allocation . size '' ''
# 400 files of 4096 bytes: allocation equals apparent size per file, so the only difference
# left is the blocks the directory inode itself allocates (8 KiB here), which dusage counts
# as zero.
du_case du_dir_inode known:du-dir-inode . many '' ''

say ""
say "== -z: dusage search -z vs rg -z (zip/)"
zip_probe gzip gz.txt.gz gz.txt needle 'gzip not installed: no gz.txt.gz fixture'
zip_probe bzip2 bz2_200k.bz2 bz2_200k.txt needle
zip_probe lz4 lz4_100k.lz4 lz4_100k.txt needle
zip_probe xz xz_mixed.xz xz_mixed.txt needle
zip_probe zstd zstd_text3.zst zstd_text.bin alpha
zip_probe brotli br_mixed.br br_mixed.txt alpha
zip_case zip_gzip eq gzip gz.txt.gz needle '-z' '-z'
zip_case zip_bzip2 eq bzip2 bz2_200k.bz2 needle '-z' '-z'
zip_case zip_lz4 eq lz4 lz4_100k.lz4 needle '-z' '-z'
zip_case zip_xz eq xz xz_mixed.xz needle '-z' '-z'
zip_case zip_zstd eq zstd zstd_text3.zst alpha '-z' '-z'
zip_case zip_brotli eq brotli br_mixed.br alpha '-z' '-z'
# A stream that starts like bzip2 and is corrupt must fail on both sides, not answer from a
# half expansion. stdout is empty either way, so this row is carried by the exit status.
zip_case zip_corrupt eq bzip2 bz2_corrupt.bz2 needle '-z -F' '-z -F'
zip_case zip_count eq bzip2 bz2_200k.bz2 needle '-z -c' '-z -c'
zip_case zip_only_matching eq xz xz_mixed.xz needle '-z -o' '-z -o'

say ""
say "== known differences"
doc_case empty_pattern plain 2 search "" .
doc_case attached_short_value plain 0 search -C1 needle .
doc_case posix_class plain 0 search '[[:alpha:]]+' .
doc_case posix_class_unknown plain 2 search '[[:bogus:]]' .
content_case single_file_name eq 3 one . only-here -n -n -rnE
content_case symlink_not_followed eq 3 links . needle -n -n -rnE
content_case binary_report known:binary-report rg enc . needle "" "" -rE
content_case binary_text known:utf16-autodetect grep enc . needle -a -a '-raE -a'
content_case utf16_bom known:binary-report rg enc . needle "" "" -rE
content_case ignore_rule_leak eq rg wipe . needle "" "" -rE
content_case utf8_dot_byte known:byte-dot 3 plain . '^caf.$' -n -n -rnE
content_case unicode_case_fold eq 3 plain . 'CAFÉ' '-i -n' '-i -n' -rniE
content_case unicode_class_letters eq rg plain . '\p{L}{4,}' '-o -n' '-o -n --pcre2' -rE
content_case max_columns known:max-columns rg plain . needle '--max-columns 20' '--max-columns 20' -rE
content_case max_columns_preview eq rg plain . needle '--max-columns 20 --max-columns-preview' '--max-columns 20 --max-columns-preview' -rE

say ""
# A skipped case is never counted as agreed. The two notes below say why a case the table
# lists did not run, so a shrinking corpus cannot pass for a green run.
[ -n "$ZIP_SKIPS" ] && say "note: skipped, the reference cannot read the format -- $ZIP_SKIPS"
[ "$FINDEXPR" = 0 ] && [ "$UNWIRED" -gt 0 ] &&
    say "note: $UNWIRED find-expression case(s) are known differences -- src/findexpr.rs is not wired to the CLI yet"
[ -n "$FAILED_NAMES" ] && say "unexpected mismatches:$FAILED_NAMES"
say "summary: $agreed agreed, $differed known differences, $failed unexpected mismatch(es), $skipped skipped"
[ "$failed" -gt 0 ] && exit 1
exit 0

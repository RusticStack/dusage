#!/usr/bin/env bash
# dusage benchmark harness.
#
# Builds the release binary, then measures the median wall time of N runs
# (default 5) for dusage against the coreutils / ripgrep / grep equivalents.
#
# Every command writes to /dev/null: page cache is warm, nothing is dropped
# (that needs root), so these are warm-cache numbers.
#
# Environment:
#   N                          runs per command (default 5)
#   DUSAGE_BENCH_SCAN_ROOT     large tree for scan (default /usr on Linux)
#   DUSAGE_BENCH_SEARCH_ROOT   mid-size text tree for search (default /usr/local/go)
#   DUSAGE_BENCH_PAT_RARE      rare literal (default asyncpreemptoff)
#   DUSAGE_BENCH_PAT_COMMON    frequent literal (default 'func ')
#   DUSAGE_BENCH_PAT_REGEX     regex (default [A-Z][a-zA-Z0-9_]*\.go)
#   DUSAGE_BENCH_PAT_CI        case-insensitive literal (default memmove)
#   DUSAGE_BENCH_GLOB          glob for find (default *.go)
#
# Output: a machine description block (lines starting with '#') followed by
# CSV rows: section,tool,command,median_ms,min_ms,max_ms,n
#
# Requires: bash, coreutils, ripgrep (rg), grep, find, du, cargo.
# hyperfine is used for one cross-check when present, never required.

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN="$REPO/target/release/dusage"
N=${N:-5}
SCAN_ROOT=${DUSAGE_BENCH_SCAN_ROOT:-/usr}
SEARCH_ROOT=${DUSAGE_BENCH_SEARCH_ROOT:-}
PAT_RARE=${DUSAGE_BENCH_PAT_RARE:-asyncpreemptoff}
PAT_COMMON=${DUSAGE_BENCH_PAT_COMMON:-func }
PAT_REGEX=${DUSAGE_BENCH_PAT_REGEX:-'[A-Z][a-zA-Z0-9_]*\.go'}
PAT_CI=${DUSAGE_BENCH_PAT_CI:-memmove}
GLOB=${DUSAGE_BENCH_GLOB:-*.go}

if [ -z "$SEARCH_ROOT" ]; then
    if [ -d /usr/local/go ]; then
        SEARCH_ROOT=/usr/local/go
    else
        SEARCH_ROOT=$HOME
    fi
fi

echo "# dusage benchmark, $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
echo "# runs,$N (median reported)"
echo "# uname,$(uname -srm)"
echo "# cpus,$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN)"
echo "# rustc,$(rustc --version 2>/dev/null || echo absent)"
echo "# rg,$(rg --version 2>/dev/null | head -1 || echo absent)"
echo "# grep,$(grep --version 2>/dev/null | head -1 || echo absent)"
echo "# hyperfine,$(command -v hyperfine >/dev/null && hyperfine --version || echo absent)"

cargo build --release --manifest-path "$REPO/Cargo.toml" >&2 || {
    echo "bench: cargo build --release failed" >&2
    exit 2
}

for tree in "$SCAN_ROOT" "$SEARCH_ROOT"; do
    [ -d "$tree" ] || {
        echo "bench: $tree is not a directory" >&2
        exit 2
    }
    bytes=$(du -sb "$tree" 2>/dev/null | cut -f1)
    files=$(find "$tree" -type f 2>/dev/null | wc -l)
    echo "# tree,$tree,bytes,$bytes,files,$files"
done

# --- timing ----------------------------------------------------------------

now_us() {
    if [ -n "${EPOCHREALTIME:-}" ]; then
        # bash >= 5, already microseconds as "sec.uuuuuu"
        local t=${EPOCHREALTIME//[!0-9]/}
        echo $((t / 1))
    else
        echo $(($(date +%s%N) / 1000))
    fi
}

# One timed run of "$1" (a command line, evaluated in this shell so the
# measurement does not pay for an extra fork+exec of bash).
time_one() {
    local start end
    start=$(now_us)
    eval "$1" >/dev/null 2>&1
    end=$(now_us)
    echo $((end - start))
}

stat_row() {
    # section tool command samples...
    # Passed through the environment: awk -v would interpret backslash escapes
    # in the command line and split the CSV row.
    local section=$1 tool=$2 cmd=$3
    shift 3
    printf '%s\n' "$@" | sort -n |
        BSD_S="$section" BSD_T="$tool" BSD_C="$cmd" BSD_N="$N" awk '
        { v[NR] = $1 }
        END {
            if (NR == 0) exit
            mid = int((NR + 1) / 2)
            if (NR % 2 == 0) median = (v[mid] + v[mid + 1]) / 2
            else median = v[mid]
            printf "%s,%s,%s,%.1f,%.1f,%.1f,%d\n", ENVIRON["BSD_S"], ENVIRON["BSD_T"], ENVIRON["BSD_C"], median / 1000, v[1] / 1000, v[NR] / 1000, ENVIRON["BSD_N"]
        }'
}

# --- cases -----------------------------------------------------------------

section=()
tool=()
cmd=()
samples=()

add() {
    section+=("$1")
    tool+=("$2")
    cmd+=("$3")
    samples+=("")
}

S="$SCAN_ROOT"
T="$SEARCH_ROOT"

add scan "dusage scan" "$BIN scan '$S' --top 1"
add scan "du -sb" "du -sb '$S'"
add scan "find+awk" "find '$S' -printf '%s\\n' | awk '{s+=\$1} END{}'"
add scan "dusage scan (1 thread)" "$BIN scan '$S' --top 1 --threads 1"

add search-rare "dusage" "$BIN search --color=never -F '$PAT_RARE' '$T'"
add search-rare "rg" "rg --color=never -F '$PAT_RARE' '$T'"
add search-rare "grep -rI" "grep -rI --color=never -F '$PAT_RARE' '$T'"

add search-common "dusage" "$BIN search --color=never -F '$PAT_COMMON' '$T'"
add search-common "rg" "rg --color=never -F '$PAT_COMMON' '$T'"
add search-common "grep -rI" "grep -rI --color=never -F '$PAT_COMMON' '$T'"

add search-regex "dusage" "$BIN search --color=never '$PAT_REGEX' '$T'"
add search-regex "rg" "rg --color=never '$PAT_REGEX' '$T'"
add search-regex "grep -rIE" "grep -rIE --color=never '$PAT_REGEX' '$T'"

add search-case-insensitive "dusage" "$BIN search --color=never -i '$PAT_CI' '$T'"
add search-case-insensitive "rg" "rg --color=never -i '$PAT_CI' '$T'"
add search-case-insensitive "grep -rI" "grep -rI --color=never -i '$PAT_CI' '$T'"
add search-case-insensitive "dusage -F" "$BIN search --color=never -F -i '$PAT_CI' '$T'"

add search-files-with-matches "dusage -l" "$BIN search --color=never -l -F '$PAT_COMMON' '$T'"
add search-files-with-matches "rg -l" "rg --color=never -l -F '$PAT_COMMON' '$T'"
add search-files-with-matches "grep -rIl" "grep -rIl --color=never -F '$PAT_COMMON' '$T'"

add find-glob "dusage find" "$BIN find --color=never '$GLOB' '$S'"

add find-glob "find -name" "find '$S' -name '$GLOB'"

# The default is now one thread per core clamped to 4..16, so it is 4 here and equals
# the explicit 4-thread row; 16 is swept to show what oversubscription costs.
add threads-scan "dusage scan 1 thread" "$BIN scan '$S' --top 1 --threads 1"
add threads-scan "dusage scan 2 threads" "$BIN scan '$S' --top 1 --threads 2"
add threads-scan "dusage scan 4 threads" "$BIN scan '$S' --top 1 --threads 4"
add threads-scan "dusage scan 8 threads" "$BIN scan '$S' --top 1 --threads 8"
add threads-scan "dusage scan 16 threads" "$BIN scan '$S' --top 1 --threads 16"
add threads-scan "dusage scan default" "$BIN scan '$S' --top 1"

add threads-search "dusage search 1 thread" "$BIN search --color=never -F '$PAT_COMMON' --threads 1 '$T'"
add threads-search "dusage search 2 threads" "$BIN search --color=never -F '$PAT_COMMON' --threads 2 '$T'"
add threads-search "dusage search 4 threads" "$BIN search --color=never -F '$PAT_COMMON' --threads 4 '$T'"
add threads-search "dusage search 8 threads" "$BIN search --color=never -F '$PAT_COMMON' --threads 8 '$T'"
add threads-search "dusage search 16 threads" "$BIN search --color=never -F '$PAT_COMMON' --threads 16 '$T'"
add threads-search "dusage search default" "$BIN search --color=never -F '$PAT_COMMON' '$T'"

count=${#cmd[@]}

# Warm up every command once, then interleave rounds so a drift in machine
# load hits all tools alike.
for i in $(seq 0 $((count - 1))); do
    eval "${cmd[i]}" >/dev/null 2>&1
done

for _round in $(seq 1 "$N"); do
    for i in $(seq 0 $((count - 1))); do
        t=$(time_one "${cmd[i]}")
        samples[i]="${samples[i]} $t"
    done
done

echo "section,tool,command,median_ms,min_ms,max_ms,n"
for i in $(seq 0 $((count - 1))); do
    # shellcheck disable=SC2086
    stat_row "${section[i]}" "${tool[i]}" "${cmd[i]}" ${samples[i]}
done

# Optional: hyperfine cross-check on the largest scan, when it is installed.
if command -v hyperfine >/dev/null; then
    echo "# hyperfine cross-check, dusage scan $S"
    hyperfine --warmup 1 --runs "$N" --style basic \
        "$BIN scan '$S' --top 1" "du -sb '$S'" 2>&1 | grep -E 'Time|faster' | sed 's/^/# /' >&2
fi

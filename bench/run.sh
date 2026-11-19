#!/usr/bin/env bash
# dusage benchmark harness.
#
# Builds the release binary, then measures wall time and peak RSS for every case in
# bench/cases.sh against the coreutils / ripgrep / grep / find equivalents, on a warm page
# cache and (for the rows marked `both`) on a cold one.
#
# Wall time is a median of N alternating runs: one run of every row per round, so a drift
# in machine load hits all tools alike. Peak RSS comes from its own pass (`/usr/bin/time
# -v` when GNU time is installed, else a /proc/<pid>/status VmHWM sampler) so that
# measurement never perturbs the clock.
#
# The cold pass drops the measured tree's page cache before each run. Root can do that
# with /proc/sys/vm/drop_caches; without root the harness falls back to
# bench/evict.py, one posix_fadvise(DONTNEED) per file, which evicts file data exactly but
# not dentries or inodes — metadata-only cases (scan, du, find) stay effectively warm that
# way and the header says so. A last-resort fallback reads a large unrelated file to push
# pages out by LRU pressure; that one is an approximation and is labelled as such.
#
# Output: a machine description block (lines starting with '#'), then CSV rows:
#   section,tool,command,median_ms,min_ms,max_ms,n,cache,maxrss_kb
# Warm rows come first, then cold rows; a row appears only in the passes it was measured
# in, so warm and cold numbers are never mixed. The same data lands in bench/results.json.
#
# Environment:
#   N                          runs per row (default 5)
#   DUSAGE_BENCH_COLD_RUNS     runs per cold row (default 3; 0 skips the cold pass)
#   DUSAGE_BENCH_RSS_RUNS      RSS samples per row, max kept (default 3; 0 skips RSS)
#   DUSAGE_BENCH_COLD_METHOD   auto (default) | drop_caches | fadvise | read-evict
#   DUSAGE_BENCH_RESULTS       result file (default bench/results.json)
#   DUSAGE_BENCH_BIN           binary under test (default target/release/dusage)
#   DUSAGE_BENCH_WORK          scratch dir for the gzip tree and eviction file (default
#                              target/bench)
#   DUSAGE_BENCH_SCAN_ROOT     large tree for scan, du and find (default /usr on Linux)
#   DUSAGE_BENCH_SEARCH_ROOT   mid-size text tree for search (default /usr/local/go)
#   DUSAGE_BENCH_ZIP_ROOT      gzip tree for -z (default $DUSAGE_BENCH_WORK/gzip-tree,
#                              built from the search tree on first run)
#   DUSAGE_BENCH_ZIP_FILES     files copied into the gzip tree (default 1200)
#   DUSAGE_BENCH_PAT_RARE      rare literal (default asyncpreemptoff)
#   DUSAGE_BENCH_PAT_COMMON    frequent literal (default 'func ')
#   DUSAGE_BENCH_PAT_REGEX     regex (default [A-Z][a-zA-Z0-9_]*\.go)
#   DUSAGE_BENCH_PAT_CI        case-insensitive literal (default memmove)
#   DUSAGE_BENCH_PAT_NOMATCH   regex that never matches (default [Z]{6}[a-z]{9})
#   DUSAGE_BENCH_PAT_MULTILINE pattern for the -U case (default 'func \w+\(\) \{\n')
#   DUSAGE_BENCH_GLOB          glob for find (default *.go)
#
# Requires: bash, coreutils, ripgrep (rg), grep, find, du, cargo. gzip is needed for the
# -z case, python3 for the fadvise fallback; both are optional, and a missing one drops or
# relabels only what needs it. hyperfine is used for one cross-check when present.

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN=${DUSAGE_BENCH_BIN:-$REPO/target/release/dusage}
WORK=${DUSAGE_BENCH_WORK:-$REPO/target/bench}
N=${N:-5}
COLD_RUNS=${DUSAGE_BENCH_COLD_RUNS:-3}
RSS_RUNS=${DUSAGE_BENCH_RSS_RUNS:-3}
RESULTS=${DUSAGE_BENCH_RESULTS:-$REPO/bench/results.json}
SCAN_ROOT=${DUSAGE_BENCH_SCAN_ROOT:-/usr}
SEARCH_ROOT=${DUSAGE_BENCH_SEARCH_ROOT:-}
Z=${DUSAGE_BENCH_ZIP_ROOT:-$WORK/gzip-tree}
ZIP_FILES=${DUSAGE_BENCH_ZIP_FILES:-1200}
# The pattern and glob knobs are defaulted in bench/cases.sh, where the commands that use
# them live.

if [ -z "$SEARCH_ROOT" ]; then
    if [ -d /usr/local/go ]; then
        SEARCH_ROOT=/usr/local/go
    else
        SEARCH_ROOT=$HOME
    fi
fi

mkdir -p "$WORK" || exit 2

CPUS=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN)

# --- the gzip tree the -z case needs ---------------------------------------

ZIP_OK=0

build_zip_tree() {
    if [ -d "$Z" ] && [ -n "$(ls -A "$Z" 2>/dev/null)" ]; then
        ZIP_OK=1
        return
    fi
    if ! command -v gzip >/dev/null 2>&1 || ! command -v tar >/dev/null 2>&1; then
        echo "bench: gzip or tar missing, skipping the -z case" >&2
        return
    fi
    rm -rf "$Z"
    mkdir -p "$Z" || return
    # shellcheck disable=SC2016
    if ! (cd "$SEARCH_ROOT" && find . -type f -name '*.go' | head -n "$ZIP_FILES" >"$WORK/zip-files.txt") ||
        ! (cd "$SEARCH_ROOT" && tar -cf - -T "$WORK/zip-files.txt") | (cd "$Z" && tar -xf -); then
        echo "bench: could not build the gzip tree at $Z, skipping the -z case" >&2
        rm -rf "$Z"
        return
    fi
    find "$Z" -type f -exec gzip -q {} + || {
        echo "bench: gzip failed, skipping the -z case" >&2
        rm -rf "$Z"
        return
    }
    rm -f "$WORK/zip-files.txt"
    ZIP_OK=1
}

# --- peak RSS --------------------------------------------------------------

GNU_TIME=
if [ -x /usr/bin/time ] && /usr/bin/time -v true >/dev/null 2>&1; then
    GNU_TIME=/usr/bin/time
fi

if [ -n "$GNU_TIME" ]; then
    RSS_METHOD="gnu-time (/usr/bin/time -v, maxrss of the largest process in the command)"
else
    RSS_METHOD="proc-sampler (max VmHWM from /proc/<pid>/status, polled every 10ms; approximate)"
fi

# Peak RSS of one run, in KiB. Never used for timing: the wrapper and the sampler cost
# a few ms, so all of this happens in a pass of its own.
measure_rss() {
    local cmd=$1 out
    if [ -n "$GNU_TIME" ]; then
        out=$(mktemp "${TMPDIR:-/tmp}/dusage-bench-rss.XXXXXX") || {
            echo 0
            return
        }
        "$GNU_TIME" -v -o "$out" -- sh -c "$cmd" >/dev/null 2>&1
        awk '/Maximum resident set size/ { print $6 }' "$out" | head -1 | grep . || echo 0
        rm -f "$out"
    else
        measure_rss_sampled "$cmd"
    fi
}

measure_rss_sampled() {
    local cmd=$1 pid peak=0 child v
    sh -c "$cmd" >/dev/null 2>&1 &
    pid=$!
    while kill -0 "$pid" 2>/dev/null; do
        for child in "$pid" $(pgrep -P "$pid" 2>/dev/null); do
            v=$(awk '/^VmHWM:/ { print $2 }' "/proc/$child/status" 2>/dev/null)
            [ -n "${v:-}" ] && [ "$v" -gt "$peak" ] && peak=$v
        done
        sleep 0.01
    done
    wait "$pid" 2>/dev/null
    echo "$peak"
}

# --- cold cache ------------------------------------------------------------

case "${DUSAGE_BENCH_COLD_METHOD:-auto}" in
auto | drop_caches | fadvise | read-evict) ;;
*)
    echo "bench: unknown DUSAGE_BENCH_COLD_METHOD ${DUSAGE_BENCH_COLD_METHOD}" >&2
    exit 2
    ;;
esac

detect_cold_method() {
    [ "${DUSAGE_BENCH_COLD_METHOD:-auto}" = auto ] || {
        echo "$DUSAGE_BENCH_COLD_METHOD"
        return
    }
    if [ -w /proc/sys/vm/drop_caches ] && (echo 3 >/proc/sys/vm/drop_caches) 2>/dev/null; then
        echo drop_caches
    elif command -v python3 >/dev/null 2>&1 && python3 -c 'import os; os.posix_fadvise' >/dev/null 2>&1; then
        echo fadvise
    else
        echo read-evict
    fi
}

COLD_METHOD=$(detect_cold_method)

cold_label() {
    case "$1" in
    drop_caches) echo "drop_caches (echo 3 > /proc/sys/vm/drop_caches, whole system)" ;;
    fadvise) echo "fadvise (posix_fadvise DONTNEED per file: file data evicted exactly, dentries and inodes retained, so metadata-only cases stay effectively warm)" ;;
    read-evict) echo "read-evict (large unrelated file read to evict by LRU pressure: an approximation, not a cache drop)" ;;
    esac
}

read_evict() {
    local big="$WORK/evict.bin" mb
    if [ ! -f "$big" ]; then
        mb=$(awk '/^MemTotal:/ { print int($2 / 2048) }' /proc/meminfo 2>/dev/null)
        [ -n "${mb:-}" ] || mb=1024
        [ "$mb" -lt 256 ] && mb=256
        dd if=/dev/zero of="$big" bs=1M count="$mb" status=none 2>/dev/null || {
            rm -f "$big"
            return
        }
    fi
    dd if="$big" of=/dev/null bs=1M status=none 2>/dev/null
}

drop_cache_for() {
    local root=$1
    case "$COLD_METHOD" in
    drop_caches)
        sync 2>/dev/null
        echo 3 >/proc/sys/vm/drop_caches 2>/dev/null
        ;;
    fadvise) [ "$root" = "-" ] || python3 "$REPO/bench/evict.py" "$root" >/dev/null 2>&1 ;;
    read-evict) read_evict ;;
    esac
}

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

# median, min, max in milliseconds from microsecond samples.
stats_of() {
    printf '%s\n' "$@" | sort -n | awk '
        { v[NR] = $1 }
        END {
            if (NR == 0) exit
            mid = int((NR + 1) / 2)
            if (NR % 2 == 0) median = (v[mid] + v[mid + 1]) / 2
            else median = v[mid]
            printf "%.1f %.1f %.1f\n", median / 1000, v[1] / 1000, v[NR] / 1000
        }'
}

json_escape() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

# --- header ----------------------------------------------------------------

echo "# dusage benchmark, $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
echo "# runs,$N (median reported)"
echo "# cold-runs,$COLD_RUNS"
echo "# uname,$(uname -srm)"
echo "# cpus,$CPUS"
echo "# rustc,$(rustc --version 2>/dev/null || echo absent)"
echo "# rg,$(rg --version 2>/dev/null | head -1 || echo absent)"
echo "# grep,$(grep --version 2>/dev/null | head -1 || echo absent)"
echo "# find,$(find --version 2>/dev/null | head -1 || echo absent)"
echo "# du,$(du --version 2>/dev/null | head -1 || echo absent)"
echo "# rss-method,$RSS_METHOD"
if [ "$COLD_RUNS" -gt 0 ]; then
    echo "# cold-method,$(cold_label "$COLD_METHOD")"
else
    echo "# cold-method,disabled (DUSAGE_BENCH_COLD_RUNS=0)"
fi
echo "# hyperfine,$(command -v hyperfine >/dev/null && hyperfine --version || echo absent)"

cargo build --release --manifest-path "$REPO/Cargo.toml" >&2 || {
    echo "bench: cargo build --release failed" >&2
    exit 2
}
[ -x "$BIN" ] || {
    echo "bench: $BIN is not executable" >&2
    exit 2
}

trees_json=""

add_tree() {
    local tree=$1 note=$2 bytes files
    bytes=$(du -sb "$tree" 2>/dev/null | cut -f1)
    files=$(find "$tree" -type f 2>/dev/null | wc -l)
    if [ -n "$note" ]; then
        echo "# tree,$tree,bytes,$bytes,files,$files,note,$note"
    else
        echo "# tree,$tree,bytes,$bytes,files,$files"
    fi
    trees_json="${trees_json}    {\"root\": \"$(json_escape "$tree")\", \"bytes\": ${bytes:-0}, \"files\": $files},"$'\n'
}

for tree in "$SCAN_ROOT" "$SEARCH_ROOT"; do
    [ -d "$tree" ] || {
        echo "bench: $tree is not a directory" >&2
        exit 2
    }
    add_tree "$tree" ""
done

build_zip_tree
if [ "$ZIP_OK" = 1 ]; then
    add_tree "$Z" "gzip of $ZIP_FILES files from $SEARCH_ROOT"
fi

# --- cases -----------------------------------------------------------------

r_case=()
r_section=()
r_cache=()
r_role=()
r_tool=()
r_cmd=()
r_evict=()
r_rss=()
warm_samples=()
cold_samples=()

bench_row() {
    r_case+=("$1")
    r_section+=("$2")
    r_cache+=("$3")
    r_role+=("$4")
    r_tool+=("$5")
    r_cmd+=("$6")
    r_evict+=("$7")
    r_rss+=("0")
    warm_samples+=("")
    cold_samples+=("")
}

# shellcheck source=bench/cases.sh
S="$SCAN_ROOT"
T="$SEARCH_ROOT"
. "$REPO/bench/cases.sh"

count=${#r_cmd[@]}
[ "$count" -gt 0 ] || {
    echo "bench: bench/cases.sh declared no rows" >&2
    exit 2
}

warm_idx=()
cold_idx=()
for i in $(seq 0 $((count - 1))); do
    case "${r_cache[i]}" in
    warm | both) warm_idx+=("$i") ;;
    esac
    case "${r_cache[i]}" in
    cold | both) cold_idx+=("$i") ;;
    esac
done

# Warm up every command once, then interleave rounds so a drift in machine
# load hits all tools alike.
for i in $(seq 0 $((count - 1))); do
    eval "${r_cmd[i]}" >/dev/null 2>&1
done

for _round in $(seq 1 "$N"); do
    for i in "${warm_idx[@]}"; do
        t=$(time_one "${r_cmd[i]}")
        warm_samples[i]="${warm_samples[i]} $t"
    done
done

# Peak RSS on a warm cache, three samples, the largest kept: a peak that only shows up
# under repetition is still a peak.
if [ "$RSS_RUNS" -gt 0 ]; then
    for i in $(seq 0 $((count - 1))); do
        peak=0
        for _round in $(seq 1 "$RSS_RUNS"); do
            kb=$(measure_rss "${r_cmd[i]}")
            [ -n "${kb:-}" ] && [ "$kb" -gt "$peak" ] && peak=$kb
        done
        r_rss[i]=$peak
    done
fi

# Cold pass last, so it never disturbs a warm measurement. Every row of a round is
# preceded by its own cache drop, so each tool pays the same cold start.
if [ "$COLD_RUNS" -gt 0 ] && [ "${#cold_idx[@]}" -gt 0 ]; then
    for _round in $(seq 1 "$COLD_RUNS"); do
        for i in "${cold_idx[@]}"; do
            drop_cache_for "${r_evict[i]}"
            t=$(time_one "${r_cmd[i]}")
            cold_samples[i]="${cold_samples[i]} $t"
        done
    done
fi

# --- results ---------------------------------------------------------------

echo "section,tool,command,median_ms,min_ms,max_ms,n,cache,maxrss_kb"

rows_json="$WORK/rows.jsonl"
: >"$rows_json" || exit 2

emit() {
    local i=$1 pass=$2 samples=$3 med min max n rss
    # shellcheck disable=SC2086
    read -r med min max <<<"$(stats_of $samples)"
    [ -n "${med:-}" ] || return 0
    n=$(printf '%s\n' "$samples" | wc -w)
    rss=${r_rss[i]:-0}
    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "${r_section[i]}" "${r_tool[i]}" "${r_cmd[i]}" "$med" "$min" "$max" "$n" "$pass" "$rss"
    printf '{"case": "%s", "section": "%s", "cache": "%s", "role": "%s", "tool": "%s", "command": "%s", "median_ms": %s, "min_ms": %s, "max_ms": %s, "runs": %s, "maxrss_kb": %s}\n' \
        "$(json_escape "${r_case[i]}")" "$(json_escape "${r_section[i]}")" "$pass" \
        "${r_role[i]}" "$(json_escape "${r_tool[i]}")" "$(json_escape "${r_cmd[i]}")" \
        "$med" "$min" "$max" "$n" "$rss" >>"$rows_json"
}

for i in "${warm_idx[@]}"; do
    # shellcheck disable=SC2086
    emit "$i" warm "${warm_samples[i]}"
done
if [ "$COLD_RUNS" -gt 0 ]; then
    for i in "${cold_idx[@]}"; do
        # shellcheck disable=SC2086
        emit "$i" cold "${cold_samples[i]}"
    done
fi

{
    echo "{"
    echo '  "schema": 1,'
    echo "  \"generated\": \"$(date -u '+%Y-%m-%dT%H:%M:%SZ')\","
    echo '  "machine": {'
    echo "    \"uname\": \"$(uname -srm)\","
    echo "    \"cpus\": $CPUS,"
    echo "    \"rustc\": \"$(json_escape "$(rustc --version 2>/dev/null || echo absent)")\","
    echo "    \"rg\": \"$(json_escape "$(rg --version 2>/dev/null | head -1 || echo absent)")\","
    echo "    \"grep\": \"$(json_escape "$(grep --version 2>/dev/null | head -1 || echo absent)")\","
    echo "    \"find\": \"$(json_escape "$(find --version 2>/dev/null | head -1 || echo absent)")\","
    echo "    \"du\": \"$(json_escape "$(du --version 2>/dev/null | head -1 || echo absent)")\""
    echo "  },"
    echo "  \"runs\": {\"warm\": $N, \"cold\": $COLD_RUNS, \"rss\": $RSS_RUNS},"
    echo "  \"rss_method\": \"$(json_escape "$RSS_METHOD")\","
    echo "  \"cold_method\": \"$(json_escape "$(cold_label "$COLD_METHOD")")\","
    echo '  "trees": ['
    printf '%s\n' "${trees_json%,$'\n'}"
    echo "  ],"
    echo '  "rows": ['
    awk 'NR > 1 { printf ",\n" } { printf "    %s", $0 }' "$rows_json"
    echo ""
    echo "  ]"
    echo "}"
} >"$RESULTS.tmp" 2>/dev/null

if mv "$RESULTS.tmp" "$RESULTS" 2>/dev/null; then
    echo "# results,$RESULTS"
else
    echo "bench: could not write $RESULTS" >&2
    rm -f "$RESULTS.tmp"
fi
rm -f "$rows_json"

# Optional: hyperfine cross-check on the largest scan, when it is installed.
if command -v hyperfine >/dev/null; then
    echo "# hyperfine cross-check, dusage scan $SCAN_ROOT"
    hyperfine --warmup 1 --runs "$N" --style basic \
        "$BIN scan '$SCAN_ROOT' --top 1" "du -sb '$SCAN_ROOT'" 2>&1 | grep -E 'Time|faster' | sed 's/^/# /' >&2
fi

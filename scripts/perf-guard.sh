#!/usr/bin/env bash
# Performance guard: fail when dusage regresses against the box it is running on.
#
#   scripts/perf-guard.sh                  # measure, compare, exit 0 (pass) or 1 (regress)
#   scripts/perf-guard.sh --update-baseline  # re-record bench/baseline.json from this box
#   scripts/perf-guard.sh --case a,b --runs 7 --verbose
#
# Three cheap content cases from bench/cases.sh, warm cache, medians of N alternating runs.
# Each primary row (dusage) is compared with two limits:
#
#   1. the fastest reference row of the same case on this box (rg / grep): fail when dusage
#      is more than TOL percent slower;
#   2. its own recorded median in bench/baseline.json: fail when dusage is more than TOL
#      percent above it.
#
# The baseline is machine specific. A run on a box with a different CPU count still works
# but says so, because the comparison then carries the machine's own spread.
#
# Environment:
#   N / DUSAGE_PERF_RUNS          runs per row (default 5)
#   DUSAGE_PERF_TOL               tolerance in percent (default 25)
#   DUSAGE_PERF_CASES             comma separated case ids (default the three below)
#   DUSAGE_BENCH_BIN              binary under test (default target/release/dusage)
#   DUSAGE_BENCH_BASELINE         baseline file (default bench/baseline.json)
#   DUSAGE_BENCH_SEARCH_ROOT      text tree (default /usr/local/go, else $HOME)
#   DUSAGE_SKIP_BUILD=1           reuse target/release/dusage as built
#
# Exit status: 0 pass (or baseline updated), 1 a case regressed, 2 setup error, 3 a case
# or its reference tool is missing on this box.

set -uo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN=${DUSAGE_BENCH_BIN:-$REPO/target/release/dusage}
BASELINE=${DUSAGE_BENCH_BASELINE:-$REPO/bench/baseline.json}
TOL=${DUSAGE_PERF_TOL:-25}
RUNS=${DUSAGE_PERF_RUNS:-${N:-5}}
CASES=${DUSAGE_PERF_CASES:-search-rare,search-regex-nomatch,search-count}
UPDATE=0
VERBOSE=0

while [ $# -gt 0 ]; do
    case "$1" in
    --update-baseline) UPDATE=1 ;;
    --runs)
        shift
        RUNS=${1:-5}
        ;;
    --case | --cases)
        shift
        CASES=${1:-}
        ;;
    -v | --verbose) VERBOSE=1 ;;
    -h | --help)
        sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "perf-guard: unknown argument: $1" >&2
        exit 2
        ;;
    esac
    shift
done

SEARCH_ROOT=${DUSAGE_BENCH_SEARCH_ROOT:-}
if [ -z "$SEARCH_ROOT" ]; then
    if [ -d /usr/local/go ]; then
        SEARCH_ROOT=/usr/local/go
    else
        SEARCH_ROOT=$HOME
    fi
fi
[ -d "$SEARCH_ROOT" ] || {
    echo "perf-guard: $SEARCH_ROOT is not a directory" >&2
    exit 2
}

if [ "${DUSAGE_SKIP_BUILD:-0}" != 1 ]; then
    cargo build --release --manifest-path "$REPO/Cargo.toml" >&2 || {
        echo "perf-guard: cargo build --release failed" >&2
        exit 2
    }
fi
[ -x "$BIN" ] || {
    echo "perf-guard: $BIN is not executable" >&2
    exit 2
}

CPUS=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN)

# --- the case table --------------------------------------------------------

want=$(printf '%s' "$CASES" | tr ',' ' ')

selected() {
    local c
    for c in $want; do
        [ "$c" = "$1" ] && return 0
    done
    return 1
}

g_case=()
g_role=()
g_tool=()
g_cmd=()
g_samples=()

bench_row() {
    # <case> <section> <cache> <role> <tool> <command> <evict-root>
    selected "$1" || return 0
    g_case+=("$1")
    g_role+=("$4")
    g_tool+=("$5")
    g_cmd+=("$6")
    g_samples+=("")
}

S=$SEARCH_ROOT
T=$SEARCH_ROOT
Z=""
ZIP_OK=0
# shellcheck source=bench/cases.sh
. "$REPO/bench/cases.sh"

[ "${#g_cmd[@]}" -gt 0 ] || {
    echo "perf-guard: no rows matched cases '$CASES'" >&2
    exit 3
}

# A reference tool this box does not have would make its row unmeasurable; drop the row
# rather than compare dusage with a shell error from a missing binary.
for i in "${!g_cmd[@]}"; do
    [ "${g_role[i]}" = reference ] || continue
    command -v "${g_tool[i]%% *}" >/dev/null 2>&1 && continue
    echo "perf-guard: skipping the ${g_tool[i]} row of ${g_case[i]}, not installed here" >&2
    g_case[i]=""
done

have=0
for i in "${!g_cmd[@]}"; do
    [ -n "${g_case[i]}" ] && have=$((have + 1))
done
[ "$have" -gt 0 ] || {
    echo "perf-guard: every selected case lost its reference tool" >&2
    exit 3
}

# --- measure ---------------------------------------------------------------

now_us() {
    if [ -n "${EPOCHREALTIME:-}" ]; then
        local t=${EPOCHREALTIME//[!0-9]/}
        echo $((t / 1))
    else
        echo $(($(date +%s%N) / 1000))
    fi
}

time_one() {
    local start end
    start=$(now_us)
    eval "$1" >/dev/null 2>&1
    end=$(now_us)
    echo $((end - start))
}

median_us() {
    printf '%s\n' "$@" | sort -n | awk '
        { v[NR] = $1 }
        END {
            if (NR == 0) exit
            mid = int((NR + 1) / 2)
            if (NR % 2 == 0) m = (v[mid] + v[mid + 1]) / 2
            else m = v[mid]
            printf "%.1f\n", m / 1000
        }'
}

med=()

for i in "${!g_cmd[@]}"; do
    [ -n "${g_case[i]}" ] && eval "${g_cmd[i]}" >/dev/null 2>&1
done

for _round in $(seq 1 "$RUNS"); do
    for i in "${!g_cmd[@]}"; do
        [ -n "${g_case[i]}" ] || continue
        t=$(time_one "${g_cmd[i]}")
        g_samples[i]="${g_samples[i]} $t"
    done
done

for i in "${!g_cmd[@]}"; do
    [ -n "${g_case[i]}" ] || continue
    # shellcheck disable=SC2086
    med[i]=$(median_us ${g_samples[i]})
    [ "$VERBOSE" = 1 ] && echo "perf-guard: ${g_case[i]} ${g_tool[i]} ${med[i]} ms" >&2
done

# --- baseline --------------------------------------------------------------

baseline_ms() {
    [ -f "$BASELINE" ] || return 1
    local v
    v=$(grep -m1 "\"$1\": {" "$BASELINE" 2>/dev/null | sed -n 's/.*"dusage_ms": \([0-9.]*\).*/\1/p')
    [ -n "$v" ] || return 1
    echo "$v"
}

baseline_cpus() {
    [ -f "$BASELINE" ] || return 0
    sed -n 's/.*"cpus": \([0-9]*\).*/\1/p' "$BASELINE" | head -1
}

if [ "$UPDATE" = 1 ]; then
    {
        echo "{"
        echo '  "schema": 1,'
        echo "  \"generated\": \"$(date -u '+%Y-%m-%dT%H:%M:%SZ')\","
        echo "  \"machine\": {\"uname\": \"$(uname -srm)\", \"cpus\": $CPUS},"
        echo "  \"runs\": $RUNS,"
        echo "  \"tolerance_pct\": $TOL,"
        echo '  "cases": {'
        first=1
        for i in "${!g_cmd[@]}"; do
            [ -n "${g_case[i]}" ] || continue
            [ "${g_role[i]}" = primary ] || continue
            ref_tool=""
            ref_ms=""
            for j in "${!g_cmd[@]}"; do
                [ "${g_case[j]}" = "${g_case[i]}" ] && [ "${g_role[j]}" = reference ] || continue
                if [ -z "$ref_ms" ] || awk -v a="${med[j]}" -v b="$ref_ms" 'BEGIN { exit !(a < b) }'; then
                    ref_ms=${med[j]}
                    ref_tool=${g_tool[j]}
                fi
            done
            [ "$first" = 1 ] || echo ","
            first=0
            printf '    "%s": {"dusage_ms": %s, "reference_tool": "%s", "reference_ms": %s}' \
                "${g_case[i]}" "${med[i]}" "$ref_tool" "${ref_ms:-0}"
        done
        echo ""
        echo "  }"
        echo "}"
    } >"$BASELINE.tmp" && mv "$BASELINE.tmp" "$BASELINE" || {
        rm -f "$BASELINE.tmp"
        echo "perf-guard: could not write $BASELINE" >&2
        exit 2
    }
    echo "perf-guard: baseline updated at $BASELINE"
    exit 0
fi

# --- compare ---------------------------------------------------------------

fail=0
printf '%-24s %10s %10s %10s %10s  %s\n' case dusage_ms ref ref_ms baseline_ms verdict
printf '%-24s %10s %10s %10s %10s  %s\n' ------------------------ ---------- ---------- ---------- ------------ --------

for i in "${!g_cmd[@]}"; do
    [ -n "${g_case[i]}" ] || continue
    [ "${g_role[i]}" = primary ] || continue

    ref_tool=""
    ref_ms=""
    for j in "${!g_cmd[@]}"; do
        [ "${g_case[j]}" = "${g_case[i]}" ] && [ "${g_role[j]}" = reference ] || continue
        if [ -z "$ref_ms" ] || awk -v a="${med[j]}" -v b="$ref_ms" 'BEGIN { exit !(a < b) }'; then
            ref_ms=${med[j]}
            ref_tool=${g_tool[j]}
        fi
    done

    base=$(baseline_ms "${g_case[i]}") || base=""
    verdict=ok
    if [ -n "$ref_ms" ] && awk -v d="${med[i]}" -v r="$ref_ms" -v tol="$TOL" \
        'BEGIN { exit !(d > r * (1 + tol / 100)) }'; then
        verdict="SLOWER than $ref_tool by more than ${TOL}%"
        fail=1
    fi
    if [ -z "$base" ]; then
        [ "$verdict" = ok ] && verdict="ok (no baseline)"
    elif awk -v d="${med[i]}" -v b="$base" -v tol="$TOL" 'BEGIN { exit !(d > b * (1 + tol / 100)) }'; then
        verdict="ABOVE baseline by more than ${TOL}%"
        fail=1
    fi
    printf '%-24s %10s %10s %10s %10s  %s\n' \
        "${g_case[i]}" "${med[i]}" "${ref_tool:-–}" "${ref_ms:-–}" "${base:-–}" "$verdict"
done

base_cpus=$(baseline_cpus)
if [ -f "$BASELINE" ]; then
    if [ -z "$base_cpus" ]; then
        echo "perf-guard: baseline has no machine block" >&2
    elif [ "$base_cpus" != "$CPUS" ]; then
        echo "perf-guard: baseline was recorded on $base_cpus CPUs, this box has $CPUS; the comparison carries that difference" >&2
    fi
else
    echo "perf-guard: no baseline at $BASELINE, only the reference comparison applies" >&2
fi

if [ "$fail" = 1 ]; then
    echo "perf-guard: FAIL"
    exit 1
fi
echo "perf-guard: pass (tolerance ${TOL}%, $RUNS runs per row)"
exit 0

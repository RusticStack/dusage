#!/usr/bin/env bash
# Write bench/windows/results.json (schema 1) from the rows bench/windows/run.sh measured.
#
#   bash bench/windows/record.sh
#
# Separate from run.sh so the record can be rebuilt from the same rows without re-measuring
# anything: the rows live in bench/windows/.rows.jsonl (one compact JSON object per line) and
# this only wraps them in the header bench/summary.sh reads.
#
# Environment: DUSAGE_BENCH_ROWS, DUSAGE_BENCH_RESULTS, DUSAGE_BENCH_ROOT,
# DUSAGE_BENCH_REAL_ROOT, DUSAGE_BENCH_RUNS, DUSAGE_BENCH_DELETE_RUNS,
# DUSAGE_BENCH_ADMIN_ROWS.

set -u

REPO=$(cd "$(dirname "$0")/../.." && pwd)
ROWS=${DUSAGE_BENCH_ROWS:-$REPO/bench/windows/.rows.jsonl}
RESULTS=${DUSAGE_BENCH_RESULTS:-$REPO/bench/windows/results.json}
ROOT=${DUSAGE_BENCH_ROOT:-'D:\dusage-bench'}
REAL=${DUSAGE_BENCH_REAL_ROOT:-'C:\Windows'}
RUNS=${DUSAGE_BENCH_RUNS:-5}
DELETE_RUNS=${DUSAGE_BENCH_DELETE_RUNS:-3}
ADMIN_ROWS=${DUSAGE_BENCH_ADMIN_ROWS:-$REPO/bench/windows/admin-results.jsonl}

if [ ! -f "$ROWS" ]; then
    echo "record: no rows at $ROWS (run bench/windows/run.sh first)" >&2
    exit 2
fi
if [ -f "$ADMIN_ROWS" ] && ! grep -q 'protection-scan' "$ROWS" 2>/dev/null; then
    cat "$ADMIN_ROWS" >> "$ROWS"
fi

json_str() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

CPUS=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN)
RUSTC=$(rustc --version 2>/dev/null || echo unknown)
WINVER=$(cmd /c ver 2>/dev/null | tr -d '\r' | sed -n 's/.*Version \(.*\)]/\1/p')
[ -z "$WINVER" ] && WINVER=unknown
GENERATED=$(date -u +%Y-%m-%dT%H:%M:%SZ)

{
    printf '{\n'
    printf '  "schema": 1,\n'
    printf '  "generated": "%s",\n' "$GENERATED"
    printf '  "machine": {\n'
    printf '    "uname": "Windows %s x86_64 (DOOMBRINGER, i7-13700KF)",\n' "$WINVER"
    printf '    "cpus": %s,\n' "$CPUS"
    printf '    "rustc": "%s",\n' "$RUSTC"
    printf '    "volume": "%s",\n' "$(printf '%s' "$ROOT" | cut -c1-2)"
    printf '    "filesystem": "NTFS"\n'
    printf '  },\n'
    printf '  "runs": {"warm": %s, "cold": 3},\n' "$RUNS"
    printf '  "delete_runs": %s,\n' "$DELETE_RUNS"
    printf '  "rss_method": "not measured: Windows peak working set needs a per-process sampler this harness does not have",\n'
    printf '  "cold_method": "standby-list purge (NtSetSystemInformation SystemMemoryListInformation 2/3/4), elevated; bench/windows/admin.ps1",\n'
    printf '  "trees": [\n'
    printf '    {"root": "%s", "files": 1000000, "dirs": 1000, "note": "zero-length files, generated"},\n' "$(json_str "$ROOT\\tree1m")"
    printf '    {"root": "%s", "files": 100000, "dirs": 1, "note": "zero-length files, one directory"},\n' "$(json_str "$ROOT\\flat100k")"
    printf '    {"root": "%s", "files": 100000, "dirs": 1000, "note": "zero-length files"},\n' "$(json_str "$ROOT\\tree1k_d100")"
    printf '    {"root": "%s", "files": 20000, "dirs": 500, "note": "delete fixture, regenerated per run"},\n' "$(json_str "$ROOT\\srcA")"
    printf '    {"root": "%s", "files": 5000, "dirs": 100, "note": "delete fixture, 128 KiB files, regenerated per run"},\n' "$(json_str "$ROOT\\srcB")"
    printf '    {"root": "%s", "files": null, "dirs": null, "note": "real system tree"}\n' "$(json_str "$REAL")"
    printf '  ],\n'
    printf '  "rows": [\n'
    awk 'NR > 1 { printf ",\n" } { print "    " $0 }' "$ROWS"
    printf '\n  ]\n}\n'
} > "$RESULTS"

echo "record: wrote $RESULTS ($(grep -c '^{' "$ROWS") rows)"

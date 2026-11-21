#!/usr/bin/env bash
# dusage Windows benchmark harness.
#
#   bash bench/windows/run.sh [--quick] [--delete-only] [--keep-rows]
#
# Writes bench/windows/results.json in schema 1 through bench/windows/record.sh (the same
# shape bench/results.json uses, so bench/summary.sh renders it unchanged), and prints the
# CSV on stdout.
#
# What it measures, one section per open question in docs/ROADMAP.md section 4:
#
#   enumeration  FindFirstFileExW against the FileIdExtdDirectoryInfo batch, at three buffer
#                sizes and with/without FIND_FIRST_EX_LARGE_FETCH, on a generated 1M tree,
#                a 100k flat directory, a 1000x100 tree and a real system tree. The numbers
#                come from `examples/win_enum_perf.rs`, which times one whole walk with an
#                in-process clock; the driver never sees its own startup cost.
#   threads      dusage scan and dusage search at 1, 2, 4, 8, 16, 24 and 32 threads.
#   delete       serial std::fs::remove_dir_all, the handle-based serial walk under the
#                POSIX and classic dispositions, and the parallel chunked path in fastdel,
#                each on a fresh copy of the same tree.
#
# Cache state: this harness measures the warm cache only and labels every row `warm`. The
# cold pass needs a standby-list purge, which needs an elevated token; `bench/windows/
# admin.ps1` does that and appends its rows through DUSAGE_BENCH_ADMIN_ROWS, which this
# script merges when the file exists.
#
# Flags: --quick halves the matrix for a smoke run; --enum-only and --delete-only run one
# section on its own, and --keep-rows appends to the rows an earlier pass measured, which is
# how a section is re-measured without repeating the rest.
#
# Environment:
#   DUSAGE_BENCH_ROOT          fixture root           (default D:\dusage-bench)
#   DUSAGE_BENCH_REAL_ROOT     real tree for the enumeration and thread rows (default C:\Windows)
#   DUSAGE_BENCH_BIN           binary under test     (default target/release/dusage.exe)
#   DUSAGE_BENCH_RUNS          runs per timing row   (default 5)
#   DUSAGE_BENCH_DELETE_RUNS   runs per delete row   (default 3)
#   DUSAGE_BENCH_SETTLE        seconds to wait for the antivirus scan queue to go quiet
#                              after building fixtures (default 20)
#   DUSAGE_BENCH_ADMIN_ROWS    JSONL of extra rows from bench/windows/admin.ps1
#   DUSAGE_BENCH_RESULTS       result file           (default bench/windows/results.json)

set -u

REPO=$(cd "$(dirname "$0")/../.." && pwd)
ROOT=${DUSAGE_BENCH_ROOT:-'D:\dusage-bench'}
REAL=${DUSAGE_BENCH_REAL_ROOT:-'C:\Windows'}
BIN=${DUSAGE_BENCH_BIN:-$REPO/target/release/dusage.exe}
FASTDEL=${DUSAGE_BENCH_FASTDEL:-$REPO/target/release/fastdel.exe}
ENUM=${DUSAGE_BENCH_ENUM:-$REPO/target/release/examples/win_enum_perf.exe}
DELPERF=${DUSAGE_BENCH_DELPERF:-$REPO/target/release/examples/win_del_perf.exe}
RUNS=${DUSAGE_BENCH_RUNS:-5}
SETTLE=${DUSAGE_BENCH_SETTLE:-20}
RESULTS=${DUSAGE_BENCH_RESULTS:-$REPO/bench/windows/results.json}
ADMIN_ROWS=${DUSAGE_BENCH_ADMIN_ROWS:-$REPO/bench/windows/admin-results.jsonl}
DELETE_RUNS=${DUSAGE_BENCH_DELETE_RUNS:-3}
QUICK=0
DELETE_ONLY=0
ENUM_ONLY=0
KEEP_ROWS=0
for a in "$@"; do
    case "$a" in
        --quick) QUICK=1 ;;
        --delete-only) DELETE_ONLY=1 ;;
        --enum-only) ENUM_ONLY=1 ;;
        --keep-rows) KEEP_ROWS=1 ;;
    esac
done

for f in "$BIN" "$ENUM" "$DELPERF"; do
    if [ ! -x "$f" ]; then
        echo "run.sh: missing $f (cargo build --release --examples)" >&2
        exit 2
    fi
done

CSV=$REPO/bench/windows/.rows.csv
: > "$CSV"

json_str() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

# emit case section role tool command median min max runs [extra-json]
emit() {
    local case_=$1 section=$2 role=$3 tool=$4 cmd=$5 med=$6 min=$7 max=$8 runs=$9 extra=${10:-}
    local cache=${11:-warm}
    local line
    line=$(printf '{"case": "%s", "section": "%s", "cache": "%s", "role": "%s", "tool": "%s", "command": "%s", "median_ms": %s, "min_ms": %s, "max_ms": %s, "runs": %s%s}' \
        "$case_" "$section" "$cache" "$role" "$tool" "$(json_str "$cmd")" "$med" "$min" "$max" "$runs" "${extra:+, $extra}")
    printf '%s\n' "$line" >> "$ROWS"
    printf '%s,%s,%s,%s,%s,%s,%s\n' "$section" "$tool" "$(json_str "$cmd")" "$med" "$min" "$max" "$runs"
}

# med_of <file of ms values on one line>: median, min, max as "med min max"
med_of() {
    tr ' ' '\n' < "$1" | grep -v '^$' | sort -n | awk '
        { a[NR] = $1 }
        END {
            if (NR == 0) { print "0 0 0"; exit }
            if (NR % 2) { m = a[(NR + 1) / 2] } else { m = (a[NR / 2] + a[NR / 2 + 1]) / 2 }
            printf "%.1f %.1f %.1f\n", m * 1000, a[1] * 1000, a[NR] * 1000
        }'
}

# time_cmd <runs> <outfile> -- <cmd...>
time_cmd() {
    local n=$1 out=$2
    shift 3
    TIMEFORMAT='%3R'
    : > "$out"
    for _ in $(seq "$n"); do
        { time "$@" > /dev/null 2>&1; } 2>> "$out"
    done
}

files_of() {
    "$BIN" scan "$1" --threads 1 2>&1 | sed -n 's/.*files \([0-9]*\).*/\1/p' | head -1
}

ROWS=$REPO/bench/windows/.rows.jsonl
if [ "$KEEP_ROWS" = 1 ] && [ -f "$ROWS" ]; then :; else : > "$ROWS"; fi

echo "# dusage Windows benchmark: $(date -u +%Y-%m-%dT%H:%M:%SZ)" >&2
echo "# root=$ROOT real=$REAL runs=$RUNS delete_runs=$DELETE_RUNS" >&2

if [ "$QUICK" = 0 ] && [ "$SETTLE" -gt 0 ]; then
    echo "# settling ${SETTLE}s so the antivirus scan queue of the fresh fixtures is done" >&2
    sleep "$SETTLE"
fi

# ---------------------------------------------------------------- enumeration (question 1)
if [ "$DELETE_ONLY" = 0 ]; then
    ENUM_ROOTS="tree1m flat100k tree1k_d100"
    ENUM_BACKENDS="walk walk-large-fetch batch-64k batch-256k batch-1m"
    [ "$QUICK" = 1 ] && ENUM_ROOTS="flat100k tree1k_d100"
    # One process per backend. Measured inside one process, the 1M tree put the larger batch
    # buffers at twice the time the same buffer measured alone, so the sweep order was doing
    # work the row must not include.
    for r in $ENUM_ROOTS; do
        p="$ROOT\\$r"
        echo "# enumerating $p" >&2
        for backend in $ENUM_BACKENDS; do
            ms=$($ENUM "$p" "$RUNS" "$backend" 2>/dev/null | awk -v b="$backend" '$1 == b {print $2}')
            extra='"note": "one sequential walk, in-process clock, whole tree"'
            emit "enum-$backend-$r" enumeration primary dusage \
                "$ENUM $p $RUNS $backend" "$ms" "$ms" "$ms" "$RUNS" "$extra"
        done
    done

    if [ "$QUICK" = 0 ]; then
        echo "# enumerating $REAL (real tree, warming it first)" >&2
        $ENUM "$REAL" 1 walk > /dev/null 2>&1
        for backend in $ENUM_BACKENDS; do
            ms=$($ENUM "$REAL" "$RUNS" "$backend" 2>/dev/null | awk -v b="$backend" '$1 == b {print $2}')
            extra='"note": "one sequential walk, in-process clock, real system tree"'
            emit "enum-$backend-real" enumeration primary dusage \
                "$ENUM $REAL $RUNS $backend" "$ms" "$ms" "$ms" "$RUNS" "$extra"
        done
    fi

    # ------------------------------------------------------------- thread sweep (question 2)
    if [ "$ENUM_ONLY" = 0 ]; then
    THREAD_LIST="1 2 4 8 16 24 32"
    [ "$QUICK" = 1 ] && THREAD_LIST="1 4 16"
    for root in "$ROOT\\tree1m" "$REAL"; do
        [ "$QUICK" = 1 ] && [ "$root" = "$REAL" ] && continue
        tag=$(basename "$root" | tr -cd '[:alnum:]')
        n=$(files_of "$root")
        for t in $THREAD_LIST; do
            time_cmd "$RUNS" "$CSV.t" -- "$BIN" scan "$root" --threads "$t"
            read -r med min max <<< "$(med_of "$CSV.t")"
            extra=$(printf '"files": %s, "files_per_s": %d' "${n:-0}" "$(awk -v n="${n:-0}" -v m="$med" 'BEGIN{ if (m > 0) printf "%d", n / (m / 1000); else print 0 }')")
            emit "scan-t$t-$tag" threads primary dusage "dusage scan $root --threads $t" \
                "$med" "$min" "$max" "$RUNS" "$extra"
        done
    done

    for t in $THREAD_LIST; do
        [ "$QUICK" = 1 ] && [ "$t" != "1" ] && [ "$t" != "16" ] && continue
        time_cmd "$RUNS" "$CSV.t" -- "$BIN" search asyncpreemptoff "$ROOT\\text" --threads "$t"
        read -r med min max <<< "$(med_of "$CSV.t")"
        emit "search-t$t" threads primary dusage "dusage search asyncpreemptoff $ROOT\\text --threads $t" \
            "$med" "$min" "$max" "$RUNS" ''
    done
    fi
fi

# ----------------------------------------------------------------- delete strategy (question 4)
# fresh_copy <tree>: an empty $ROOT/run holding a fresh copy of $ROOT/<tree>. The copy is
# outside every clock in this section: the question is what the delete costs. `rmdir /s /q`
# does the clearing because msys `rm -rf` gives up part way through a large copied tree here,
# and a half-empty payload would time the wrong thing.
fresh_copy() {
    local tree=$1
    "$FASTDEL" "$ROOT\run" --age -1 --threads 8 > /dev/null 2>&1
    rm -rf "$ROOT/run" 2>/dev/null
    mkdir -p "$ROOT/run" 2>/dev/null
    robocopy "$ROOT\\$tree" "$ROOT\\run\\payload" /MIR /NFL /NDL /NJH /NJS /NP > /dev/null 2>&1
    if [ ! -d "$ROOT/run/payload" ]; then
        echo "run.sh: the copy of $tree did not land; refusing to record a delete row" >&2
        return 1
    fi
    return 0
}

# The error categories of one delete, read out of the harness's own JSON line.
del_errors() {
    local last ad sv nf dne ot
    last=$(tail -1 "$CSV.del")
    ad=$(printf '%s' "$last" | grep -o '"access_denied":[0-9]*' | cut -d: -f2)
    sv=$(printf '%s' "$last" | grep -o '"sharing_violation":[0-9]*' | cut -d: -f2)
    nf=$(printf '%s' "$last" | grep -o '"not_found":[0-9]*' | cut -d: -f2)
    dne=$(printf '%s' "$last" | grep -o '"dir_not_empty":[0-9]*' | cut -d: -f2)
    ot=$(printf '%s' "$last" | grep -o '"other":[0-9]*' | cut -d: -f2)
    printf '"errors": {"access_denied": %s, "sharing_violation": %s, "not_found": %s, "dir_not_empty": %s, "other": %s}' \
        "${ad:-0}" "${sv:-0}" "${nf:-0}" "${dne:-0}" "${ot:-0}"
}

delete_tree() {
    local tree=$1 tag=$2
    for mode in std serial-posix serial-classic; do
        : > "$CSV.t"
        : > "$CSV.del"
        for _ in $(seq "$DELETE_RUNS"); do
            fresh_copy "$tree" || return 1
            TIMEFORMAT='%3R'
            { time "$DELPERF" "$ROOT\\run\\payload" "$mode" >> "$CSV.del" 2>&1; } 2>> "$CSV.t"
        done
        read -r med min max <<< "$(med_of "$CSV.t")"
        local errs cmd
        errs=$(del_errors)
        cmd="$DELPERF $ROOT\\run\\payload $mode"
        printf '{"case": "delete-%s-%s", "section": "delete", "cache": "warm", "role": "primary", "tool": "dusage", "command": "%s", "median_ms": %s, "min_ms": %s, "max_ms": %s, "runs": %s, %s}\n' \
            "$mode" "$tag" "$(json_str "$cmd")" "$med" "$min" "$max" "$DELETE_RUNS" "$errs" >> "$ROWS"
        printf 'delete,%s,%s,%s,%s,%s,%s\n' "$mode" "$(json_str "$cmd")" "$med" "$min" "$max" "$DELETE_RUNS" >> "$CSV"
    done

    # The parallel chunked path is the fastdel binary itself, planned and deleted in one run.
    : > "$CSV.t"
    for _ in $(seq "$DELETE_RUNS"); do
        fresh_copy "$tree" || return 1
        TIMEFORMAT='%3R'
        { time "$FASTDEL" "$ROOT\\run" --age -1 --threads 16 > /dev/null 2>&1; } 2>> "$CSV.t"
    done
    read -r med min max <<< "$(med_of "$CSV.t")"
    local cmd="$FASTDEL $ROOT\\run --age -1 --threads 16"
    printf '{"case": "delete-parallel-%s", "section": "delete", "cache": "warm", "role": "primary", "tool": "dusage", "command": "%s", "median_ms": %s, "min_ms": %s, "max_ms": %s, "runs": %s, "note": "fastdel plans the root and deletes its children; the copy is outside the clock"}\n' \
        "$tag" "$(json_str "$cmd")" "$med" "$min" "$max" "$DELETE_RUNS" >> "$ROWS"
    printf 'delete,parallel,%s,%s,%s,%s,%s\n' "$(json_str "$cmd")" "$med" "$min" "$max" "$DELETE_RUNS" >> "$CSV"
}

: > "$CSV.del"
delete_tree srcA A
delete_tree srcB B

# ----------------------------------------------------- rows from the elevated run, if any
if [ -f "$ADMIN_ROWS" ]; then
    cat "$ADMIN_ROWS" >> "$ROWS"
    echo "# merged $(wc -l < "$ADMIN_ROWS") elevated rows from $ADMIN_ROWS" >&2
fi

# ------------------------------------------------------------------------ write the record
DUSAGE_BENCH_ROWS=$ROWS \
    DUSAGE_BENCH_RESULTS=$RESULTS \
    DUSAGE_BENCH_ROOT=$ROOT \
    DUSAGE_BENCH_REAL_ROOT=$REAL \
    DUSAGE_BENCH_RUNS=$RUNS \
    DUSAGE_BENCH_DELETE_RUNS=$DELETE_RUNS \
    bash "$(dirname "$0")/record.sh"
echo "section,tool,command,median_ms,min_ms,max_ms,runs"
cat "$CSV"

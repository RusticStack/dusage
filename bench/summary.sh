#!/bin/sh
# Render bench/results.json as the Markdown tables docs/BENCHMARKS.md holds.
#
#   bench/summary.sh [RESULTS]        (default bench/results.json)
#
# One table per cache pass, warm first, and never a mixed one: a number is either
# warm-cache or cold-cache and the heading says which. Within a case the fastest reference
# row is the one compared (its tool is named), because that is the competitor worth
# measuring against. Cases with no reference row — the thread sweeps — show a dash.
#
# POSIX sh plus awk; the JSON is one row per line with a fixed key order, which is what
# makes this parseable without a JSON library.

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
RESULTS=${1:-$REPO/bench/results.json}

if [ ! -f "$RESULTS" ]; then
    echo "summary: no result file at $RESULTS (run bench/run.sh first)" >&2
    exit 2
fi

grab() {
    grep -m1 -- "$1" "$RESULTS" 2>/dev/null | sed -n "$2"
}

generated=$(grab '"generated":' 's/.*"generated": "\([^"]*\)".*/\1/p')
uname=$(grab '"uname":' 's/.*"uname": "\([^"]*\)".*/\1/p')
cpus=$(grab '"cpus":' 's/.*"cpus": \([0-9]*\).*/\1/p')
runs=$(grab '"runs":' 's/.*"warm": \([0-9]*\), "cold": \([0-9]*\).*/\1 \2/p')
warm_runs=${runs% *}
cold_runs=${runs#* }
rss_method=$(grab '"rss_method":' 's/.*"rss_method": "\([^"]*\)".*/\1/p')
cold_method=$(grab '"cold_method":' 's/.*"cold_method": "\([^"]*\)".*/\1/p')

cat <<EOF
Generated $generated on $uname, $cpus CPUs. Warm rows are medians of $warm_runs alternating
runs; cold rows are medians of $cold_runs, each preceded by a cache drop. Times in
milliseconds; RSS is peak resident set, in MiB, measured in a pass of its own.

RSS method: $rss_method

EOF

awk -v cold="$cold_method" '
function field(key,    pat, i, rest, j, out) {
    pat = "\"" key "\": "
    i = index($0, pat)
    if (i == 0) return ""
    rest = substr($0, i + length(pat))
    if (substr(rest, 1, 1) == "\"") {
        rest = substr(rest, 2)
        j = index(rest, "\"")
        if (j == 0) j = length(rest) + 1
        return substr(rest, 1, j - 1)
    }
    j = index(rest, ",")
    if (j == 0) j = length(rest) + 1
    out = substr(rest, 1, j - 1)
    gsub(/ /, "", out)
    return out
}

function mb(kb) { return kb == "" || kb + 0 == 0 ? "-" : sprintf("%.0f", kb / 1024) }

function ratio(a, b) {
    if (a == "" || b == "" || b + 0 == 0) return "-"
    return sprintf("%.2fx", a / b)
}

/^ *\{/ && index($0, "\"case\": ") > 0 {
    cache = field("cache")
    if (cache == "" || cache != "warm" && cache != "cold") next
    if (!(cache in seen)) { passes[++npass] = cache; seen[cache] = 1 }
    c = field("case")
    key = cache SUBSEP c
    if (!(key in known)) {
        known[key] = 1
        seq[cache] = (cache in seq ? seq[cache] : 0) + 1
        cases[cache, seq[cache]] = c
        section[key] = field("section")
    }
    med = field("median_ms")
    rss = field("maxrss_kb")
    if (field("role") == "primary") {
        # keep the first primary of a case: variants live under their own case id
        if (!(key in hasp)) {
            hasp[key] = 1
            pmed[key] = med
            prss[key] = rss
        }
    } else {
        rl = field("tool")
        if (!(key in rmed) || med + 0 < rmed[key] + 0) {
            rmed[key] = med
            rlabel[key] = rl
            rrss[key] = rss
        }
    }
}

END {
    for (p = 1; p <= npass; p++) {
        cache = passes[p]
        printf("### %s cache\n\n", cache == "warm" ? "Warm" : "Cold")
        print("| Case | dusage ms | reference | ref ms | dusage/ref | dusage RSS MiB | ref RSS MiB |")
        print("| --- | --- | --- | --- | --- | --- | --- |")
        for (i = 1; i <= seq[cache]; i++) {
            c = cases[cache, i]
            key = cache SUBSEP c
            if (!(key in hasp)) continue
            lab = (key in rlabel) ? "`" rlabel[key] "`" : "-"
            r = (key in rmed) ? rmed[key] : ""
            printf("| %s | %s | %s | %s | %s | %s | %s |\n", c, pmed[key], lab, r, ratio(pmed[key], r), mb(prss[key]), mb(rrss[key]))
        }
        printf("\n%s: %s\n\n", cache == "cold" ? "Cold cache method" : "Notes", cache == "cold" ? cold : "warm pass, page cache untouched between runs")
    }
}
' "$RESULTS"

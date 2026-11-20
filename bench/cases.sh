# bench/cases.sh — the benchmark case table.
#
# Data only, not executable: the caller sources this file after defining `bench_row` and
# the variables the commands reference ($BIN, $S, $T, $Z and the $PAT_* knobs). Both
# bench/run.sh and scripts/perf-guard.sh do exactly that, so the suite and the CI guard
# measure the same command lines.
#
#   bench_row <case> <section> <cache> <role> <tool> <command> <evict-root>
#
#   case        stable id; bench/baseline.json and scripts/perf-guard.sh key off it
#   section     the group printed in the CSV, one heading per group in docs/BENCHMARKS.md
#   cache       warm | cold | both — which passes measure this row; nothing is reported
#               for a pass the row is not marked for, so warm and cold never mix
#   role        primary (dusage) | reference (the tool dusage replaces)
#   tool        the label printed in the tool column
#   command     one shell command line, eval'd by the harness; stdout goes to /dev/null
#   evict-root  tree whose page cache the cold pass drops before this row, or - if the
#               row is warm-only
#
# Each case names its reference tool: rg, grep, find or du. A case may carry more than one
# reference row (grep and rg), and a dusage variant of another case gets its own case id.

# Defaults for the knobs below, so a caller that only wants a few cases (the CI guard)
# does not have to restate them. bench/run.sh documents each one.
PAT_RARE=${DUSAGE_BENCH_PAT_RARE:-asyncpreemptoff}
PAT_COMMON=${DUSAGE_BENCH_PAT_COMMON:-func }
PAT_REGEX=${DUSAGE_BENCH_PAT_REGEX:-'[A-Z][a-zA-Z0-9_]*\.go'}
PAT_CI=${DUSAGE_BENCH_PAT_CI:-memmove}
PAT_NOMATCH=${DUSAGE_BENCH_PAT_NOMATCH:-'[Z]{6}[a-z]{9}'}
PAT_MULTILINE=${DUSAGE_BENCH_PAT_MULTILINE:-'func \w+\(\) \{\n'}
PAT_MULTILINE_UNICODE=${DUSAGE_BENCH_PAT_MULTILINE_UNICODE:-'type \w+ struct'}
GLOB=${DUSAGE_BENCH_GLOB:-*.go}

# --- scan ------------------------------------------------------------------

bench_row scan scan both primary "dusage scan" "$BIN scan '$S' --top 1" "$S"

bench_row scan scan both reference "du -sb" "du -sb '$S'" "$S"

bench_row scan scan both reference "find+awk" "find '$S' -printf '%s\\n' | awk '{s+=\$1} END{}'" "$S"

bench_row scan-t1 scan warm primary "dusage scan (1 thread)" "$BIN scan '$S' --top 1 --threads 1" -

# --- search: literals ------------------------------------------------------

bench_row search-rare search-rare both primary "dusage" "$BIN search --color=never -F '$PAT_RARE' '$T'" "$T"

bench_row search-rare search-rare both reference "rg" "rg --color=never -F '$PAT_RARE' '$T'" "$T"

bench_row search-rare search-rare both reference "grep -rI" "grep -rI --color=never -F '$PAT_RARE' '$T'" "$T"

bench_row search-common search-common both primary "dusage" "$BIN search --color=never -F '$PAT_COMMON' '$T'" "$T"

bench_row search-common search-common both reference "rg" "rg --color=never -F '$PAT_COMMON' '$T'" "$T"

bench_row search-common search-common both reference "grep -rI" "grep -rI --color=never -F '$PAT_COMMON' '$T'" "$T"

# --- search: regex ---------------------------------------------------------

bench_row search-regex search-regex both primary "dusage" "$BIN search --color=never '$PAT_REGEX' '$T'" "$T"

bench_row search-regex search-regex both reference "rg" "rg --color=never '$PAT_REGEX' '$T'" "$T"

bench_row search-regex search-regex both reference "grep -rIE" "grep -rIE --color=never '$PAT_REGEX' '$T'" "$T"

bench_row search-regex-nomatch search-regex-nomatch both primary "dusage" "$BIN search --color=never '$PAT_NOMATCH' '$T'" "$T"

bench_row search-regex-nomatch search-regex-nomatch both reference "rg" "rg --color=never '$PAT_NOMATCH' '$T'" "$T"

bench_row search-regex-nomatch search-regex-nomatch both reference "grep -rIE" "grep -rIE --color=never '$PAT_NOMATCH' '$T'" "$T"

# --- search: case folding --------------------------------------------------

bench_row search-ci search-case-insensitive both primary "dusage" "$BIN search --color=never -i '$PAT_CI' '$T'" "$T"

bench_row search-ci search-case-insensitive both reference "rg" "rg --color=never -i '$PAT_CI' '$T'" "$T"

bench_row search-ci search-case-insensitive both reference "grep -rI" "grep -rI --color=never -i '$PAT_CI' '$T'" "$T"

bench_row search-ci-fixed search-case-insensitive both primary "dusage -F" "$BIN search --color=never -F -i '$PAT_CI' '$T'" "$T"

# --- search: output modes --------------------------------------------------

bench_row search-count search-count both primary "dusage -c" "$BIN search --color=never -c -F '$PAT_COMMON' '$T'" "$T"

bench_row search-count search-count both reference "rg -c" "rg --color=never -c -F '$PAT_COMMON' '$T'" "$T"

bench_row search-count search-count both reference "grep -rIc" "grep -rIc --color=never -F '$PAT_COMMON' '$T'" "$T"

bench_row search-only-matching search-only-matching both primary "dusage -o" "$BIN search --color=never -o -F '$PAT_COMMON' '$T'" "$T"

bench_row search-only-matching search-only-matching both reference "rg -o" "rg --color=never -o -F '$PAT_COMMON' '$T'" "$T"

bench_row search-only-matching search-only-matching both reference "grep -rIo" "grep -rIo --color=never -F '$PAT_COMMON' '$T'" "$T"

bench_row search-multiline search-multiline both primary "dusage -U" "$BIN search --color=never -c -U '$PAT_MULTILINE' '$T'" "$T"

bench_row search-multiline search-multiline both reference "rg -U" "rg --color=never -c -U '$PAT_MULTILINE' '$T'" "$T"

# The same mode on a pattern whose `\w` is Unicode: matching this over bytes is only exact
# for a file with no non-ASCII byte, so the row measures what a file that has one costs.
bench_row search-multiline-unicode search-multiline both primary "dusage -U unicode" "$BIN search --color=never -c -U '$PAT_MULTILINE_UNICODE' '$T'" "$T"

bench_row search-multiline-unicode search-multiline both reference "rg -U unicode" "rg --color=never -c -U '$PAT_MULTILINE_UNICODE' '$T'" "$T"

bench_row search-files-with-matches search-files-with-matches warm primary "dusage -l" "$BIN search --color=never -l -F '$PAT_COMMON' '$T'" -

bench_row search-files-with-matches search-files-with-matches warm reference "rg -l" "rg --color=never -l -F '$PAT_COMMON' '$T'" -

bench_row search-files-with-matches search-files-with-matches warm reference "grep -rIl" "grep -rIl --color=never -F '$PAT_COMMON' '$T'" -

# `-z` needs a compressed tree; run.sh builds it from $T and skips these rows when gzip is
# absent, so the section only exists when it can be measured.
if [ "${ZIP_OK:-0}" = 1 ]; then
    bench_row search-gzip search-gzip both primary "dusage -z" "$BIN search --color=never -F '$PAT_COMMON' -z '$Z'" "$Z"

    bench_row search-gzip search-gzip both reference "rg -z" "rg --color=never -F '$PAT_COMMON' -z '$Z'" "$Z"
fi

# --- find ------------------------------------------------------------------

bench_row find-name find-glob both primary "dusage find" "$BIN find --color=never '$GLOB' '$S'" "$S"

bench_row find-name find-glob both reference "find -name" "find '$S' -name '$GLOB'" "$S"

bench_row find-path find-path-glob both primary "dusage find (path glob)" "$BIN find --color=never '**/src/strings/*.go' '$T'" "$T"

bench_row find-path find-path-glob both reference "find -path" "find '$T' -path '**/src/strings/*.go'" "$T"

bench_row find-type find-type both primary "dusage find -t go" "$BIN find --color=never -t go '*' '$T'" "$T"

bench_row find-type find-type both reference "find -name" "find '$T' -name '*.go'" "$T"

# The expression form is find's own language and a different walk from the glob form: one
# `DirReader` at a time, because `-prune` and `-depth` decide the next step from the entry just
# seen. It had no row of its own, which is how it stayed the slowest find path unmeasured.
bench_row find-expr find-expr both primary "dusage find (expression)" "$BIN find '$T' -name '*.go'" "$T"

bench_row find-expr find-expr both reference "find -name" "find '$T' -name '*.go'" "$T"

# --- du --------------------------------------------------------------------

bench_row du-s du-s both primary "dusage du -s" "$BIN du -s '$S'" "$S"

bench_row du-s du-s both reference "du -s" "du -sk --apparent-size '$S'" "$S"

bench_row du du both primary "dusage du" "$BIN du '$S'" "$S"

bench_row du du both reference "du" "du -k --apparent-size '$S'" "$S"

# --- thread sweep ----------------------------------------------------------

bench_row threads-scan-1 threads-scan warm primary "dusage scan 1 thread" "$BIN scan '$S' --top 1 --threads 1" -

bench_row threads-scan-2 threads-scan warm primary "dusage scan 2 threads" "$BIN scan '$S' --top 1 --threads 2" -

bench_row threads-scan-4 threads-scan warm primary "dusage scan 4 threads" "$BIN scan '$S' --top 1 --threads 4" -

bench_row threads-scan-8 threads-scan warm primary "dusage scan 8 threads" "$BIN scan '$S' --top 1 --threads 8" -

bench_row threads-scan-16 threads-scan warm primary "dusage scan 16 threads" "$BIN scan '$S' --top 1 --threads 16" -

bench_row threads-scan-default threads-scan warm primary "dusage scan default" "$BIN scan '$S' --top 1" -

bench_row threads-search-1 threads-search warm primary "dusage search 1 thread" "$BIN search --color=never -F '$PAT_COMMON' --threads 1 '$T'" -

bench_row threads-search-2 threads-search warm primary "dusage search 2 threads" "$BIN search --color=never -F '$PAT_COMMON' --threads 2 '$T'" -

bench_row threads-search-4 threads-search warm primary "dusage search 4 threads" "$BIN search --color=never -F '$PAT_COMMON' --threads 4 '$T'" -

bench_row threads-search-8 threads-search warm primary "dusage search 8 threads" "$BIN search --color=never -F '$PAT_COMMON' --threads 8 '$T'" -

bench_row threads-search-16 threads-search warm primary "dusage search 16 threads" "$BIN search --color=never -F '$PAT_COMMON' --threads 16 '$T'" -

bench_row threads-search-default threads-search warm primary "dusage search default" "$BIN search --color=never -F '$PAT_COMMON' '$T'" -

#!/bin/sh
# Render bench/windows/checks.json as the Markdown tables docs/BENCHMARKS.md holds.
#
#   bench/windows/summary-checks.sh [CHECKS]      (default bench/windows/checks.json)
#
# One table per check kind, in the order the checks were written. Same rule as
# bench/summary.sh: the document holds program output, never a hand-typed figure, so these
# tables are copied verbatim out of this script's stdout.
#
# POSIX sh plus awk. The JSON is one compact object per line with no spaces after the
# colons, which is what ConvertTo-Json -Compress writes.

set -u

REPO=$(cd "$(dirname "$0")/../.." && pwd)
CHECKS=${1:-$REPO/bench/windows/checks.json}

if [ ! -f "$CHECKS" ]; then
    echo "summary-checks: no check file at $CHECKS (run bench/windows/checks.ps1 first)" >&2
    exit 2
fi

awk '
function field(key,    pat, i, rest, j, out) {
    pat = "\"" key "\":"
    i = index($0, pat)
    if (i == 0) return ""
    rest = substr($0, i + length(pat))
    if (substr(rest, 1, 1) == " ") rest = substr(rest, 2)
    if (substr(rest, 1, 1) == "\"") {
        rest = substr(rest, 2)
        j = index(rest, "\"")
        if (j == 0) j = length(rest) + 1
        out = substr(rest, 1, j - 1)
        gsub(/\\\\/, "\\", out)
        return out
    }
    j = index(rest, ",")
    if (j == 0) j = length(rest)
    out = substr(rest, 1, j - 1)
    gsub(/ /, "", out)
    gsub(/\\\\/, "\\", out)
    return out
}

function dash(v) {
    gsub(/\\\\/, "\\", v)
    return v == "" || v == "null" ? "-" : v
}

function table_end() {
    if (in_table) { print ""; in_table = 0 }
}

/^ *\{/ {
    kind = field("kind")
    if (kind == "fidelity") {
        if (!fid_head) {
            table_end()
            print "Allocation fidelity: every number is the Win32 answer for the same file, read through the handle dusage uses."
            print ""
            print "| fixture | EndOfFile | AllocationSize | GetCompressedFileSizeW | named streams | attributes |"
            print "| --- | ---: | ---: | ---: | --- | --- |"
            fid_head = 1
        }
        in_table = 1
        printf "| %s | %s | %s | %s | %s | %s |\n", field("name"), dash(field("end_of_file")),
            dash(field("allocation_size")), dash(field("compressed_size")),
            dash(field("streams")), dash(field("attributes"))
        next
    }
    if (kind == "fileid") {
        table_end()
        print "File ids: " field("name") "."
        print ""
        print "| question | answer |"
        print "| --- | --- |"
        printf "| hardlinks share one FileId | %s |\n", dash(field("same_file_id"))
        printf "| FileId (high:low) | %s |\n", dash(field("file_id"))
        printf "| `fsutil file queryFileID <path>` | %s |\n", dash(field("fsutil_syntax"))
        printf "| fsutil output | `%s` |\n", dash(field("fsutil_output"))
        print ""
        next
    }
    if (kind == "fileid-volume") {
        if (!vol_head) {
            print "File id support per volume:"
            print ""
            print "| volume | filesystem | FileIdInfo accepted | FileId | error |"
            print "| --- | --- | --- | --- | --- |"
            vol_head = 1
            in_table = 1
        }
        printf "| %s | %s | %s | %s | %s |\n", field("volume"), field("filesystem"),
            dash(field("file_id_supported")), dash(field("file_id")), dash(field("error"))
        next
    }
    if (kind == "overcount") {
        if (!ov_head) {
            table_end()
            print "Hardlink overcount on real trees, `--dedup-hardlinks` as the reference:"
            print ""
            print "| root | names (naive) | names (dedup) | on disk naive GB | on disk dedup GB | overcount |"
            print "| --- | ---: | ---: | ---: | ---: | ---: |"
            ov_head = 1
            in_table = 1
        }
        printf "| `%s` | %s | %s | %s | %s | %s%% |\n", field("root"), dash(field("naive_files")),
            dash(field("dedup_files")), dash(field("naive_disk_gb")), dash(field("dedup_disk_gb")),
            dash(field("overcount_pct"))
        next
    }
    if (kind == "reparse") {
        table_end()
        print "Reparse points: " field("path") "."
        print ""
        print "| property | value |"
        print "| --- | --- |"
        printf "| LinkType | %s |\n", dash(field("link_type"))
        printf "| PSIsContainer | %s |\n", dash(field("is_dir"))
        printf "| attributes | %s |\n", dash(field("attributes"))
        printf "| FILE_STANDARD_INFO Directory flag | %s |\n", dash(field("directory_flag"))
        print ""
        next
    }
    if (kind == "reparse-scan") {
        printf "`dusage scan %s`: %s files, %s dirs; `find` lists `%s`; the link target appears %s.\n\n", \
            field("path"), dash(field("files")), dash(field("dirs")), dash(field("find_paths")), \
            (field("find_mentions_target_twice") == "false" ? "once" : "more than once")
        next
    }
    if (kind == "longpath") {
        table_end()
        print "Long paths, deepest path " field("plain_len") " characters:"
        print ""
        print "| question | answer |"
        print "| --- | --- |"
        printf "| LongPathsEnabled | %s |\n", dash(field("long_paths_enabled"))
        printf "| raw CreateFileW, no prefix | %s (error %s) |\n", dash(field("plain_open_ok")), dash(field("plain_open_error"))
        printf "| raw CreateFileW, verbatim prefix | %s (error %s) |\n", dash(field("verbatim_open_ok")), dash(field("verbatim_open_error"))
        print ""
        next
    }
    if (kind == "elevation") {
        if (!el_head) {
            print "Elevation, opening the volume device:"
            print ""
            print "| device | elevated | CreateFileW(GENERIC_READ) | error |"
            print "| --- | --- | --- | --- |"
            el_head = 1
            in_table = 1
        }
        printf "| `%s` | %s | %s | %s |\n", field("device"), dash(field("elevated")),
            dash(field("open_ok")), dash(field("open_error"))
        next
    }
    if (kind == "posix-delete") {
        table_end()
        print "POSIX disposition:"
        print ""
        print "| question | answer |"
        print "| --- | --- |"
        printf "| FILE_DISPOSITION_INFO_EX accepted | %s |\n", dash(field("set_info_ok"))
        printf "| a second open while the first handle is still open | %s |\n", dash(field("state_while_handle_open"))
        printf "| name gone after the last close | %s |\n", dash(field("name_gone_after_close"))
        print ""
        next
    }
    if (kind == "mft") {
        if (!mft_head) {
            table_end()
            print "MFT against the batched backend and robocopy (elevated):"
            print ""
            print "| volume | backend | files | dirs | bytes | robocopy bytes | note |"
            print "| --- | --- | ---: | ---: | ---: | ---: | --- |"
            mft_head = 1
            in_table = 1
        }
        printf "| %s | %s | %s | %s | %s | %s | %s |\n", field("volume"), field("backend"),
            dash(field("files")), dash(field("dirs")), dash(field("bytes")),
            dash(field("robocopy_bytes")), dash(field("note"))
        next
    }
    if (kind == "watch") {
        table_end()
        print "USN change feed (elevated): " field("step") "."
        print ""
        print "| field | value |"
        print "| --- | --- |"
        printf "| detail | %s |\n", dash(field("detail"))
        print ""
        next
    }
    if (kind == "cold") {
        table_end()
        print "Cold cache (standby-list purge, elevated): " field("case") "."
        print ""
        print "| state | median ms | runs |"
        print "| --- | ---: | ---: |"
        printf "| warm | %s | %s |\n", dash(field("warm_ms")), dash(field("runs"))
        printf "| cold | %s | %s |\n", dash(field("cold_ms")), dash(field("runs"))
        print ""
        next
    }
    if (kind == "protection") {
        if (!prot_head) {
            table_end()
            print "Real-time protection, the same tree with the fixture folder excluded and without (elevated):"
            print ""
            print "| operation | tree | protection | median ms | runs |"
            print "| --- | --- | --- | ---: | ---: |"
            prot_head = 1
            in_table = 1
        }
        printf "| %s | %s | %s | %s | %s |\n", field("operation"), field("tree"),
            field("protection"), dash(field("median_ms")), dash(field("runs"))
        next
    }
}
END { table_end() }
' "$CHECKS"

#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Count read requests and bytes for a fixed set of query shapes.
#
# Wall time on a warm local box says almost nothing about a query node reading
# from object storage, where the bill and the latency both track request count
# and bytes transferred.  This measures those two directly, using pread64 as the
# stand-in for a range GET.
#
# Usage: scripts/measure-query-io.sh <db_path> <symbol_with_callers> [label]
#
# Requires strace and a release build of examples/explain-filter.

set -u

DB="${1:?usage: measure-query-io.sh <db_path> <symbol> [label]}"
SYMBOL="${2:?need a symbol that has callers}"
LABEL="${3:-$(basename "$(dirname "$DB")")}"

BIN=./target/release/examples/explain-filter
if [[ ! -x "$BIN" ]]; then
    echo "building explain-filter..." >&2
    cargo build -q --release --example explain-filter || exit 1
fi

TRACE=$(mktemp)
OUT=$(mktemp)
trap 'rm -f "$TRACE" "$OUT"' EXIT

parse() {
    python3 - "$1" "$2" "$TRACE" <<'PY'
import re, sys
label, rows, trace = sys.argv[1], sys.argv[2], sys.argv[3]
reqs = total = opens = 0
for line in open(trace):
    if 'openat(' in line and '= -1' not in line:
        opens += 1
    if 'pread64' not in line or '<unfinished' in line:
        continue
    # strace -f splits calls across unfinished/resumed lines; the resumed line
    # carries the return value, so match on the trailing result either way.
    m = re.search(r'=\s*(\d+)\s*$', line.strip())
    if m:
        reqs += 1
        total += int(m.group(1))
print(f"{label:<38} {opens:>6} {reqs:>8} {total/1e6:>10.2f} {rows:>10}")
PY
}

run() {
    local label="$1" filter="$2" cols="$3"
    strace -f -e trace=pread64,openat -o "$TRACE" \
        "$BIN" "$DB" functions "$filter" "$cols" 1 >"$OUT" 2>/dev/null
    local rows
    rows=$(grep -o '[0-9]* rows' "$OUT" | head -1 | cut -d' ' -f1)
    parse "$label" "${rows:-?}"
}

echo "=== $LABEL  ($DB, symbol '$SYMBOL')"
printf '%-38s %6s %8s %10s %10s\n' "query" "opens" "reqs" "MB" "rows"

run "no-op (absent name)"        "name = 'zzz_absent_symbol'"            "name"
run "btree probe: name ="        "name = '$SYMBOL'"                      "name"
run "labellist probe: absent"    "array_has_any(calls, ['zzz_absent'])"  "name,calls"
run "labellist probe: present"   "array_has_any(calls, ['$SYMBOL'])"     "name,calls"
run "scan of calls column"       "array_length(calls) > 1000"            "name,calls"
run "full scan, all columns"     "line_start > 0"                        ""

#!/bin/bash
# compile every module with --annotate and collect the whole report per module
# usage: census.sh SP BY PY OUT [MODULE...]
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
root="$SP/census.$$"; rm -rf "$root"; mkdir -p "$root"
trap 'rm -rf "$root"' EXIT
: > "$OUT"
for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$root/w"; sweep_stage "$d" "$LIB" "$b"
  sweep_compile "$b" "$d" "$PY" "$BY" --annotate --emit-c-only
  report="$(sweep_out_dir "$d")/m.annotated"
  if [ ! -f "$report" ]; then printf '%s\tno-report\n' "$b" >> "$OUT"; continue; fi
  awk -v b="$b" '
    /^[0-9]+ compiled, [0-9]+ left interpreted$/ { print b "\tcounts\t" $1 "\t" $3 }
    /^- / && insec==1 { sub(/^- /,""); print b "\tdecline\t" $0 }
    /^## left to the interpreted definition/ { insec=1; next }
    /^## / && !/^## left to the interpreted definition/ { insec=0 }
    /^## class / { print b "\tclass\t" $3 }
  ' "$report" >> "$OUT"
done

#!/bin/bash
# every stdlib module, subpackages included, each compiled in a project of its own
#
# a stand-in for the missing isolated.sh: it counts what built and what panicked,
# which is the coverage question a differential suite cannot answer
#
# usage: buildsweep.sh SP BY PY OUT [MODULE...]
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
sweep_begin bsw || exit 1
for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$SWEEP_ROOT/w"; sweep_stage "$d" "$LIB" "$b"
  err=$(cd "$d" && PYTHON="$PY" "$BY" compile "$SWEEP_SRC" -o o 2>&1 >/dev/null)
  if echo "$err" | grep -qiE 'panicked|internal error|stack overflow'; then
    printf '%s\tPANIC\t%s\n' "$b" "$(echo "$err" | head -1)" >> "$OUT"
  elif sweep_built "$d"; then
    printf '%s\tbuilt\n' "$b" >> "$OUT"
  else
    printf '%s\tno-artifact\n' "$b" >> "$OUT"
  fi
done
sweep_end || exit 1
echo "modules: $(wc -l < "$OUT")   built: $(grep -c $'\tbuilt' "$OUT")   panics: $(grep -c PANIC "$OUT")   no-artifact: $(grep -c no-artifact "$OUT")"

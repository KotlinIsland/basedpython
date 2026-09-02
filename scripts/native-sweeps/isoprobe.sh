#!/bin/bash
# one module, staged and built the way every rung stages one, then asked a question
#
# the rungs each ask a fixed question of all 550 modules. this asks an arbitrary one of a
# single module, so a case that has been minimised out of a rung's `differing:` column can
# be re-run on its own without hand-rolling the staging — which is the part that goes
# wrong, because a package member staged outside a copy of its package has nothing for
# `from . import x` to resolve against
#
# usage: isoprobe.sh SP BY PY OUT MODULE 'python expression'
#
# the expression is evaluated with the staged module bound as `m`, and its `repr` printed.
# both legs answer it and the two are compared, exactly as `isoimport` compares an import
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; MODULE="$5"; EXPR="$6"
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
sweep_begin isoprobe || exit 1

# PROBE_TRACEBACK=1 reports where a raise came from rather than what it said. it is for
# reading a difference this has already found — the traceback names file and line, which
# the two legs spell differently, so a run with it on is not a comparison
report='print(type(error).__name__ + ": " + str(error))'
[ "${PROBE_TRACEBACK:-0}" = 1 ] && report='import sys, traceback; traceback.print_exc(file=sys.stdout)'

probe="import importlib, os, signal
signal.alarm(int(os.environ['SWEEP_IMPORT_BOUND']))
m = importlib.import_module(os.environ['SWEEP_MOD'])
try:
    print(repr($EXPR))
except BaseException as error:
    $report
"

leg() {
  sweep_capture "$1" "$PY" -c "$probe"; LEG_STATUS=$SWEEP_CAPTURE_STATUS
  if [ "${PROBE_TRACEBACK:-0}" = 1 ]; then
    printf -- '--- %s ---\n%s\n' "$1" "$SWEEP_CAPTURE_TEXT" >&2
  fi
  LEG_TEXT=$(sweep_canonical "$(printf '%s' "$SWEEP_CAPTURE_TEXT" | tail -1 | sed "s|$1/||g")")
}

# the walk is the one module named on the command line, and `sweep_end` holds a rung to
# the count of what it walked
printf '1\n' > "$OUT.walked"

d="$SWEEP_ROOT/w"; sweep_stage "$d" "$LIB" "$MODULE"
sweep_compile "$MODULE" "$d" "$PY" "$BY"
if ! sweep_built "$d"; then
  printf '%s\tno-artifact\n' "$MODULE" >> "$OUT"
else
  sweep_place "$d"
  leg "$SWEEP_RUN_I"; istat=$LEG_STATUS; i=$LEG_TEXT
  leg "$SWEEP_RUN_C"; cstat=$LEG_STATUS; c=$LEG_TEXT
  if [ "$istat" -gt 128 ] || [ "$cstat" -gt 128 ]; then
    printf '%s\tDIED\tinterpreted[%s]\tcompiled[%s]\n' "$MODULE" "$istat" "$cstat"
  elif [ "$i" = "$c" ]; then printf '%s\tsame\t%s\n' "$MODULE" "$i"
  else printf '%s\tDIFFERS\tinterpreted[%s]\tcompiled[%s]\n' "$MODULE" "$i" "$c"
  fi >> "$OUT"
fi
sweep_end || exit 1
cat "$OUT"

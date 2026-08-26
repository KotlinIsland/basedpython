#!/bin/bash
# how a resumable frame reports its own end, per module, over the same 550
#
# not a rung: nothing here is compared against an interpreted leg, and no
# difference it reports is a bug. it counts *which exit* every compiled generator
# and coroutine takes when its frame finishes — the structural one, which writes
# the value into the state object for `am_send` to hand back, or a raised
# `StopIteration` for the caller to take apart again.
#
# the population moves with anything that changes what is emitted, so re-run it
# rather than quoting a figure. `setnone` is the count worth watching beside it:
# after a finish stopped being a raise, the only `PyErr_SetNone(PyExc_StopIteration)`
# left in a module should be one its own source wrote
#
# usage: finishcensus.sh SP BY PY OUT [MODULE...]
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
sweep_begin finishcensus || exit 1
for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$SWEEP_ROOT/w"; sweep_stage "$d" "$LIB" "$b"
  sweep_compile "$b" "$d" "$PY" "$BY" --emit-c-only
  c="$(sweep_out_dir "$d")/m.c"
  if [ ! -f "$c" ]; then printf '%s\tno-c\n' "$b" >> "$OUT"; continue; fi
  finishes=$(grep -c -- '->by_returned = by_t;' "$c")
  setnone=$(grep -c 'PyErr_SetNone(PyExc_StopIteration)' "$c")
  sendslot=$(grep -c '\.am_send =' "$c")
  printf '%s\tfinishes\t%s\tsetnone\t%s\tsendslot\t%s\n' "$b" "$finishes" "$setnone" "$sendslot" >> "$OUT"
done
sweep_end || exit 1

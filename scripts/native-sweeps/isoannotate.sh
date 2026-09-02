#!/bin/bash
# one module, staged the way every rung stages one, and its *annotation* printed
#
# `--annotate` is what says which functions a module left interpreted and what layout each
# of its classes got, and both answers depend on the module being staged inside a copy of
# its package — a package member staged bare loses every relative import and declines for
# that instead. this is the same staging the rungs use, stopping at the annotation
#
# usage: isoannotate.sh SP BY PY OUT MODULE
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; MODULE="$5"
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
sweep_begin isoannotate || exit 1
printf '1\n' > "$OUT.walked"

d="$SWEEP_ROOT/w"; sweep_stage "$d" "$LIB" "$MODULE"
sweep_compile "$MODULE" "$d" "$PY" "$BY" --annotate
note=$(sweep_out_dir "$d")/m.annotated
if [ -f "$note" ]; then
  printf '%s\tannotated\n' "$MODULE" >> "$OUT"
  cp "$note" "$OUT.annotated"
else
  printf '%s\tno-annotation\n' "$MODULE" >> "$OUT"
fi
sweep_end || exit 1
[ -f "$OUT.annotated" ] && cat "$OUT.annotated"

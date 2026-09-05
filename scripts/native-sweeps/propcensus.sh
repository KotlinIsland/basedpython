#!/bin/bash
# what every `@property` a compiled module publishes is actually made of
# usage: propcensus.sh SP BY PY OUT [MODULE...]
#
# the compiled leg only, for the reason `fntwin.sh` gives: one import per module, no
# second leg and no construction alarm, so a busy machine can drop a module but cannot
# invent a difference. the counts are therefore a floor
#
# a `@property` is the one construct whose decline is invisible to `--annotate`. a group
# the lowering leaves alone is not declined at all — the ordinary method path takes the
# `def`, and `By_DecoratedMethod` then writes the *interpreted* `property` the class body
# built over the method table's entry. the native body is emitted and never reached, and
# the module's compiled/interpreted counts say nothing about it
#
# so the question is asked of the object: `type(prop.fget).__name__` is
# `method_descriptor` where a half is this module's own and `function` where it is the
# interpreted definition. `lone` is a group with a getter and neither of the other two
# halves, which is the shape the ordinary method path can carry and the pair cannot
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
sweep_begin propcensus || exit 1

cat > "$SWEEP_ROOT/propprobe.py" <<'PYEOF'
"""report every `@property` a compiled module's classes publish, and what each half is"""

import importlib
import os
import signal

signal.alarm(int(os.environ.get("SWEEP_IMPORT_BOUND", "60")))
m = importlib.import_module(os.environ["SWEEP_MOD"])
signal.alarm(0)

name = m.__name__
rows = []
for key, value in list(vars(m).items()):
    if not isinstance(value, type) or getattr(value, "__module__", None) != name:
        continue
    for member, held in list(vars(value).items()):
        if not isinstance(held, property):
            continue
        halves = []
        for word, half in (("get", held.fget), ("set", held.fset), ("del", held.fdel)):
            if half is None:
                continue
            halves.append(f"{word}={type(half).__name__}")
        rows.append(f"{key}.{member}:{','.join(halves) or '-'}")
print("\t".join([str(len(rows)), " ".join(sorted(rows)) or "-"]))

PYEOF

for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$SWEEP_ROOT/w"; sweep_stage "$d" "$LIB" "$b"
  sweep_compile "$b" "$d" "$PY" "$BY"
  if ! sweep_built "$d"; then printf '%s\tno-artifact\n' "$b" >> "$OUT"; continue; fi
  sweep_place "$d"
  cp "$SWEEP_ROOT/propprobe.py" "$SWEEP_RUN_C/by_propprobe.py"
  # read through `sweep_capture` rather than a command substitution, for the reason the
  # other rungs give: the probe can die in ways a substitution reports as empty output
  sweep_capture "$SWEEP_RUN_C" "$PY" by_propprobe.py
  st=$SWEEP_CAPTURE_STATUS
  text=$(printf '%s' "$SWEEP_CAPTURE_TEXT" | tail -1)
  if [ "$st" != 0 ]; then printf '%s\tfailed[%s]\t%s\n' "$b" "$st" "$text"
  else printf '%s\tok\t%s\n' "$b" "$text"
  fi >> "$OUT"
done
# to stdout, not into `$OUT`: `sweep_end` counts distinct first columns there against
# `$OUT.walked`, so a summary line written into it reads as one more module
{
  printf 'walked: %s\n' "$(cat "$OUT.walked" 2>/dev/null || echo '?')"
  for kind in ok failed no-artifact; do
    printf '%s: %s\n' "$kind" "$(grep -c "	$kind" "$OUT")"
  done
  awk -F'\t' '$2=="ok" {
    n = split($4, seen, " ")
    for (i = 1; i <= n; i++) {
      split(seen[i], part, ":")
      halves = part[2]
      lone = (halves ~ /^get=/ && halves !~ /,/)
      native = (halves ~ /get=method_descriptor/)
      total++
      if (lone) { lonely++; if (native) lone_native++ }
      else { paired++; if (native) pair_native++ }
    }
  } END {
    printf "properties: %d\n", total + 0
    printf "lone getters: %d, of which native: %d\n", lonely + 0, lone_native + 0
    printf "groups with another half: %d, of which native: %d\n", paired + 0, pair_native + 0
  }' "$OUT"
}
sweep_end || exit 1

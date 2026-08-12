#!/bin/bash
# each module in a project of its own, built and then *imported*
#
# the build sweep cannot see a module that compiles and then fails to import — a class
# whose metaclass rejects what was handed to it only says so at import. so both legs are
# imported and the last line of each compared: a module the interpreter cannot import
# standalone either is not a regression, it is the harness
#
# usage: isoimport.sh SP BY PY OUT [MODULE...]
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
root="$SP/isoimp.$$"; rm -rf "$root"; mkdir -p "$root"
trap 'rm -rf "$root"' EXIT
: > "$OUT"

# an import that never returns would stall the whole sweep, and a subpackage module is
# far likelier to start something than a top-level one. the alarm is left at its default
# disposition on purpose: it kills the leg, both legs are killed alike, and a killed pair
# reads as `same` rather than as a difference nobody can act on
probe='import signal; signal.alarm(30); import m'

for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$root/w"; sweep_stage "$d" "$f"
  sweep_compile "$d" "$PY" "$BY"
  if ! sweep_built "$d"; then printf '%s\tno-artifact\n' "$b" >> "$OUT"; continue; fi
  i=$(cd "$d" && "$PY" -c "$probe" 2>&1 | tail -1)
  c=$(cd "$d/o" && "$PY" -c "$probe" 2>&1 | tail -1)
  if [ "$i" = "$c" ]; then printf '%s\tsame\t%s\n' "$b" "$i" >> "$OUT"
  else printf '%s\tDIFFERS\tinterpreted[%s]\tcompiled[%s]\n' "$b" "$i" "$c" >> "$OUT"; fi
done
echo "walked: $(wc -l < "$OUT")   exercised: $(grep -c $'\tsame\t$' "$OUT")   differing: $(grep -c DIFFERS "$OUT")   import-failed: $(grep -cE $'\tsame\t.' "$OUT")   no-artifact: $(grep -c $'\tno-artifact' "$OUT")"

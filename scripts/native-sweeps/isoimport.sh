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

# an import that never returns would stall the whole sweep, and a package member is far
# likelier to start something than a top-level module. the alarm is left at its default
# disposition on purpose: it kills the leg, and a leg killed by it says nothing on the
# way out — so what tells the two cases apart is the *exit status*, not the output.
#
# a leg that dies prints nothing, and so does a leg that imports cleanly. reading only
# the text, this rung counted a killed leg as an import that worked — the one reading
# that must never be given, because it is agreement with a leg that answered nothing
#
# the name comes from the staging: `m` for a top-level module, `pkg.m` for a package
# member. both legs are handed the same one
probe='import importlib, os, signal; signal.alarm(int(os.environ["SWEEP_IMPORT_BOUND"])); importlib.import_module(os.environ["SWEEP_MOD"])'

# one leg, into LEG_STATUS and LEG_TEXT. the status has to come from the interpreter
# rather than from the end of a pipeline, so the text is trimmed afterwards. a leg that
# says nothing is the case this exists for, so the two are returned apart rather than
# packed into one string an empty half would collapse
leg() {
  local dir="$1" out
  out=$(cd "$dir" && "$PY" -c "$probe" 2>&1); LEG_STATUS=$?
  # the two legs run from different directories, so a message that names a file would
  # read as a difference the module never had. each is made relative to its own root
  LEG_TEXT=$(printf '%s' "$out" | tail -1 | sed "s|$dir/||g")
}

for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$root/w"; sweep_stage "$d" "$LIB" "$b"
  sweep_compile "$b" "$d" "$PY" "$BY"
  if ! sweep_built "$d"; then printf '%s\tno-artifact\n' "$b" >> "$OUT"; continue; fi
  sweep_place "$d"
  leg "$SWEEP_RUN_I"; istat=$LEG_STATUS; i=$LEG_TEXT
  leg "$SWEEP_RUN_C"; cstat=$LEG_STATUS; c=$LEG_TEXT
  # 142 is SIGALRM: the import outran the bound. a loaded machine loses it on one leg
  # and not the other, which says nothing about the compiler — so it is its own
  # category rather than a difference or an agreement
  if [ "$istat" = 142 ] || [ "$cstat" = 142 ]; then
    printf '%s\ttimed-out\tinterpreted[%s]\tcompiled[%s]\n' "$b" "$istat" "$cstat"
  elif [ "$istat" -gt 128 ] || [ "$cstat" -gt 128 ]; then
    # killed by something else: a leg that died mid-import has not answered, and the
    # empty text it leaves behind must not be read as agreement
    printf '%s\tDIED\tinterpreted[%s]\tcompiled[%s]\n' "$b" "$istat" "$cstat"
  elif [ "$i" = "$c" ]; then printf '%s\tsame\t%s\n' "$b" "$i"
  else printf '%s\tDIFFERS\tinterpreted[%s]\tcompiled[%s]\n' "$b" "$i" "$c"
  fi >> "$OUT"
done
echo "walked: $(wc -l < "$OUT")   exercised: $(grep -c $'\tsame\t$' "$OUT")   differing: $(grep -c $'\tDIFFERS' "$OUT")   died: $(grep -c $'\tDIED' "$OUT")   timed-out: $(grep -c $'\ttimed-out' "$OUT")   import-failed: $(grep -cE $'\tsame\t.' "$OUT")   no-artifact: $(grep -c $'\tno-artifact' "$OUT")"

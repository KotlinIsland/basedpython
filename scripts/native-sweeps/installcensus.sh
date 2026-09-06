#!/bin/bash
# what `--annotate` says a module compiled, against what its import actually installed
#
# every other rung compares the compiled leg against the interpreted twin, and a class
# that quietly left its interpreted definition standing answers *identically* to that
# twin — so it agrees with all of them at once while the report goes on counting it as
# compiled. that is how the compiled-class figures this project quotes became upper
# bounds rather than counts, and it went unnoticed for weeks.
#
# so this rung asks the two sides separately. `--annotate` names every class the module
# published a type for, and `BY_INSTALL_CENSUS` makes the import say which of them a type
# actually stands under. the classes marked `not published` in the report are left out on
# both sides: a closure's environment and a generator's state are real emitted classes
# that nothing can name, and neither side ever claims one installed.
#
# the row to read is `stood-down`: a class the report counts and the import left
# interpreted. it is not on its own a defect — the layout guard and the install gate
# exist to leave a class interpreted where installing it would be wrong — but it is
# exactly the population every coverage figure has been silently including
#
# usage: installcensus.sh SP BY PY OUT [MODULE...]
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
sweep_begin instcensus || exit 1

# the import writes the census itself, through the environment variable the emitted
# module reads. the bound is the one every rung imports under
probe='import importlib, os, signal; signal.alarm(int(os.environ["SWEEP_IMPORT_BOUND"])); importlib.import_module(os.environ["SWEEP_MOD"])'

for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$SWEEP_ROOT/w"; sweep_stage "$d" "$LIB" "$b"
  sweep_compile "$b" "$d" "$PY" "$BY" --annotate
  report="$(sweep_out_dir "$d")/m.annotated"
  if ! sweep_built "$d"; then printf '%s\tno-artifact\n' "$b" >> "$OUT"; continue; fi
  if [ ! -f "$report" ]; then printf '%s\tno-report\n' "$b" >> "$OUT"; continue; fi

  # every class the report says this module published a type for. read *before*
  # `sweep_place`, which lays the twin's copy of the package over the build's output
  # directory and takes a package member's report with it — leaving every class the
  # import found looking like one the report never named
  #
  # a heading with a parenthesised note is a class this module cannot publish under any
  # name — a closure's environment, a generator's state — and no import ever claims one
  # installed, so the two sides would never be equal if they were counted
  reported="$SWEEP_ROOT/reported"
  awk '/^## class / && $0 !~ /not published/ { print $3 }' "$report" |
    LC_ALL=C sort -u > "$reported"
  sweep_place "$d"

  census="$SWEEP_ROOT/census"
  : > "$census"
  BY_INSTALL_CENSUS="$census" sweep_capture "$SWEEP_RUN_C" "$PY" -c "$probe"
  status=$SWEEP_CAPTURE_STATUS
  text=$(sweep_canonical "$(printf '%s' "$SWEEP_CAPTURE_TEXT" | tail -1 | sed "s|$SWEEP_RUN_C/||g")")

  if [ "$status" = 142 ]; then printf '%s\ttimed-out\n' "$b" >> "$OUT"; continue; fi
  if [ "$status" -gt 128 ]; then printf '%s\tDIED\t%s\n' "$b" "$status" >> "$OUT"; continue; fi
  if [ "$status" != 0 ]; then
    # the check itself raises `ImportError`, and it is the one import failure this rung
    # is looking for. every other one is the module, and both are worth the row
    printf '%s\timport-failed\t%s\n' "$b" "$text" >> "$OUT"
    continue
  fi

  installed="$SWEEP_ROOT/installed"; stood="$SWEEP_ROOT/stood"
  # `twin` is a stand-down too — the construction was tried, could not be rebuilt, and
  # handed the interpreted definition back to stand as the class — but it is kept apart
  # because it is the one the report has no idea about
  awk -F'\t' '$3 == "installed" { print $2 }' "$census" | LC_ALL=C sort -u > "$installed"
  awk -F'\t' '$3 != "installed" { print $2 }' "$census" | LC_ALL=C sort -u > "$stood"
  while read -r name; do
    [ -n "$name" ] && printf '%s\ttwin\t%s\n' "$b" "$name" >> "$OUT"
  done < <(awk -F'\t' '$3 == "twin" { print $2 }' "$census" | LC_ALL=C sort -u)

  # a class the report counted that the import left interpreted, and one the report
  # never mentioned that the import stood a type under. the second should be empty:
  # a module cannot install a class it did not emit
  while read -r name; do
    [ -n "$name" ] && printf '%s\tstood-down\t%s\n' "$b" "$name" >> "$OUT"
  done < <(LC_ALL=C comm -12 "$reported" "$stood")
  while read -r name; do
    [ -n "$name" ] && printf '%s\tUNREPORTED\t%s\n' "$b" "$name" >> "$OUT"
  done < <(LC_ALL=C comm -13 "$reported" "$installed")
  printf '%s\tcounts\t%s\t%s\t%s\n' "$b" \
    "$(grep -c . "$reported")" "$(grep -c . "$installed")" "$(grep -c . "$stood")" >> "$OUT"
done
sweep_end || exit 1
echo "walked: $(cat "$OUT.walked")   reported: $(awk -F'\t' '$2=="counts"{n+=$3} END{print n+0}' "$OUT")   installed: $(awk -F'\t' '$2=="counts"{n+=$4} END{print n+0}' "$OUT")   stood-down: $(grep -c $'\tstood-down' "$OUT")   of-those-twins: $(grep -c $'\ttwin\t' "$OUT")   unreported: $(grep -c $'\tUNREPORTED' "$OUT")   import-failed: $(grep -c $'\timport-failed' "$OUT")   no-artifact: $(grep -c $'\tno-artifact' "$OUT")   died: $(grep -c $'\tDIED' "$OUT")   timed-out: $(grep -c $'\ttimed-out' "$OUT")"

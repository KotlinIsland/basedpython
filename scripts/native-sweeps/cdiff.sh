#!/bin/bash
# the C two compilers emit for the same module, compared module by module
# usage: cdiff.sh SP BY_A BY_B PY OUT [MODULE...]
#
# the behavioural rungs all cost 10-18 minutes and are only as trustworthy as the
# machine is idle — the construction alarms are 2 seconds, so a loaded host invents
# differences. this asks a question that has none of that in it: given two builds of the
# compiler, which modules do they emit *different C* for?
#
# that is the population a change can possibly have touched. a module whose C is
# byte-identical cannot import, construct, subclass or contain anything different, so
# the rungs only have to walk the ones listed here — which is usually a handful rather
# than 550, and can then be run as a base/new pair in minutes instead of hours.
#
# it is also the one check that sees a module the compiler stopped emitting altogether:
# `same` and `differs` both mean C was produced, and `a-only`/`b-only`/`neither` are
# each their own line
#
# the runtime header is compared too, and that is what makes this rung able to scope a
# change to `by.h`. the header is included rather than inlined into the translation unit,
# so a change to `By_GetItemTagged` leaves all 550 modules' C byte-identical while the
# behaviour of every module that calls it has moved — which used to read here as "nothing
# to walk". a module whose C is unchanged but which calls a helper whose definition
# changed is reported as `header`, so the population to pair the behavioural rungs over is
# `differs` plus `header`
SP="$1"; BY_A="$2"; BY_B="$3"; PY="$4"; OUT="$5"; shift 5
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
# unknown until the first module compiles on both legs; then `same` or `differs`, and in
# the `differs` case `header_scope` is an ERE alternation of the helpers that moved
header_state=unknown
header_scope=""
sweep_begin cdiff || exit 1
for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  # staged *once* and compiled twice out of the one directory. the emitted C carries a
  # `#line` for every statement, naming the source by absolute path — so a leg with a
  # staging directory of its own differs from the other in each of them, and every module
  # in the corpus reads as changed however little the compiler did
  d="$SWEEP_ROOT/w"; sweep_stage "$d" "$LIB" "$b"
  for leg in a b; do
    by=$BY_A; [ "$leg" = b ] && by=$BY_B
    rm -rf "$d/o"
    sweep_compile "$b" "$d" "$PY" "$by" --emit-c-only
    out=$(sweep_out_dir "$d")
    # the staged module is always `m`, so the emitted translation unit always `m.c`
    if [ -f "$out/m.c" ]; then cp "$out/m.c" "$SWEEP_ROOT/$leg.c"; else rm -f "$SWEEP_ROOT/$leg.c"; fi
    # `--emit-c-only` writes the runtime header beside the translation unit, and a given
    # compiler emits the same one for every module — so it is captured on whichever
    # module compiles first and not looked at again
    if [ -f "$out/by.h" ]; then cp "$out/by.h" "$SWEEP_ROOT/$leg.h"; fi
  done
  # which helpers changed between the two runtimes, worked out once and then reused
  #
  # the answer is `same` (nothing a module calls moved), `unscopable` (it moved in a way
  # no line-level reading can attribute, so no module may be scoped out), or an ERE
  # alternation of the helpers that changed
  if [ "$header_state" = unknown ] && [ -f "$SWEEP_ROOT/a.h" ] && [ -f "$SWEEP_ROOT/b.h" ]; then
    header_scope=$(sweep_header_scope "$SWEEP_ROOT/a.h" "$SWEEP_ROOT/b.h" "$SWEEP_ROOT")
    case "$header_scope" in
      same)       header_state=same;    header_scope="" ;;
      unscopable) header_state=differs; header_scope="" ;;
      *)          header_state=differs ;;
    esac
    printf 'header: %s\n' "$header_state${header_scope:+ (scoped)}" >&2
  fi
  if [ -f "$SWEEP_ROOT/a.c" ] && [ -f "$SWEEP_ROOT/b.c" ]; then
    if cmp -s "$SWEEP_ROOT/a.c" "$SWEEP_ROOT/b.c"; then
      # identical C, so the only way this module's behaviour moved is through the runtime
      calls=""
      if [ "$header_state" = differs ]; then
        if [ -n "$header_scope" ]; then
          calls=$(grep -oE "$header_scope" "$SWEEP_ROOT/a.c" | sort -u | paste -sd, -)
        else
          calls="unscopable"
        fi
      fi
      if [ -n "$calls" ]; then
        printf '%s\theader\t%s\n' "$b" "$calls" >> "$OUT"
      else
        printf '%s\tsame\n' "$b" >> "$OUT"
      fi
    else
      printf '%s\tdiffers\t%s\n' "$b" \
        "$(diff "$SWEEP_ROOT/a.c" "$SWEEP_ROOT/b.c" | grep -c '^[<>]')" >> "$OUT"
    fi
  elif [ -f "$SWEEP_ROOT/a.c" ]; then
    printf '%s\ta-only\n' "$b" >> "$OUT"
  elif [ -f "$SWEEP_ROOT/b.c" ]; then
    printf '%s\tb-only\n' "$b" >> "$OUT"
  else
    printf '%s\tneither\n' "$b" >> "$OUT"
  fi
done
{
  printf 'walked: %s\n' "$(cat "$OUT.walked" 2>/dev/null || echo '?')"
  for kind in same differs header a-only b-only neither; do
    printf '%s: %s\n' "$kind" "$(grep -c "	$kind" "$OUT")"
  done
}
# the summary goes to stdout, not into `$OUT`: `sweep_end` counts distinct first columns
# there against `$OUT.walked`, so six summary lines read as six extra modules and a
# 550-module walk reported 556 — no `.complete` marker was ever written and the rung
# always exited 1. every other rung echoes its summary for the same reason
sweep_end || exit 1

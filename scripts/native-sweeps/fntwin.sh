#!/bin/bash
# what a compiled module's body captured: how many module-level *function* twins it holds
# and where each one sits
# usage: fntwin.sh SP BY PY OUT [MODULE...]
#
# the compiled leg only. the interpreted leg has no twins by construction, so there is
# nothing to compare against and this rung does not try — which is exactly what makes it
# trustworthy on a busy machine, the same property `cdiff.sh` has. one import per module,
# no second leg, no 2-second construction alarm for contention to trip. load can *drop* a
# module here, and the count is therefore a floor; it cannot invent a twin.
#
# the question it answers is not "does a twin exist" but "can one reach a position where
# it matters". module init replaces the module dict's name with the forwarder onto the
# native, and nothing revisits what the body already captured — so a name the body put
# in a list, a class dict or a default argument still holds the *interpreted* definition.
# that is mostly harmless: both call the same way and answer the same value, and the only
# divergence is `is` against the function's own name.
#
# it used to matter for a second reason: what the module published was a `PyCFunction`,
# which is not a descriptor, so a twin in a class dict bound its receiver where the
# published object would not have. a module now publishes a real `function`, so both
# bind and `class-dict` is no longer a hazard — the column is still worth reading,
# because it is where an `is` against the name is likeliest to be asked
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
sweep_begin fntwin || exit 1

cat > "$SWEEP_ROOT/fnprobe.py" <<'PYEOF'
"""count the module-level *function* twins a compiled module's body captured, and say
where each one sits — the question is whether a captured twin can reach a descriptor
position, not merely whether it exists"""
import importlib, os, signal, sys, types

signal.alarm(int(os.environ.get("SWEEP_IMPORT_BOUND", "60")))
m = importlib.import_module(os.environ["SWEEP_MOD"])
signal.alarm(0)

d = vars(m)
name = m.__name__
# the file a published forwarder's frame reports, which is what tells it apart from the
# interpreted definition it replaced — both are a `function`, and that is the point of it
FORWARDER = "<by native forwarder>"


def forwarder(v):
    """v is what a compiled module publishes over one of its own natives"""
    return (
        isinstance(v, types.FunctionType)
        and getattr(v, "__module__", None) == name
        and v.__code__.co_filename == FORWARDER
    )


# what the module's own names now answer with, where that is a compiled function
compiled = {k: v for k, v in d.items() if forwarder(v)}


def twin(v):
    """v is an interpreted definition this module replaced under its own name"""
    if not isinstance(v, types.FunctionType) or forwarder(v):
        return False
    if getattr(v, "__module__", None) != name:
        return False
    try:
        own = v.__code__.co_qualname
    except AttributeError:
        own = v.__name__
    stands = compiled.get(own)
    return stands is not None


counts = {}
seen = set()


def note(where):
    counts[where] = counts.get(where, 0) + 1


def walk(v, where, depth):
    if depth < 0:
        return
    if id(v) in seen:
        return
    seen.add(id(v))
    if twin(v):
        note(where)
        return
    if isinstance(v, (list, tuple)):
        for item in v:
            walk(item, where, depth - 1)
    elif isinstance(v, (set, frozenset)):
        for item in v:
            walk(item, where, depth - 1)
    elif isinstance(v, dict):
        for k in list(v.keys()):
            walk(k, where, depth - 1)
            try:
                walk(v[k], where, depth - 1)
            except Exception:
                pass
    elif isinstance(v, types.FunctionType):
        for part in (v.__defaults__, v.__kwdefaults__):
            if part is not None:
                walk(part, "captured-by-a-function", depth - 1)
        for cell in v.__closure__ or ():
            try:
                walk(cell.cell_contents, "captured-by-a-function", depth - 1)
            except ValueError:
                pass


for k, v in list(d.items()):
    if twin(v):
        note("module-level-alias")
        seen.add(id(v))
        continue
    if isinstance(v, type) and (v.__flags__ & (1 << 9)):  # Py_TPFLAGS_HEAPTYPE
        for ck, cv in list(vars(v).items()):
            if twin(cv):
                note("class-dict")
                seen.add(id(cv))
            else:
                walk(cv, "inside-a-class-dict", 4)
        continue
    walk(v, "module-level-container", 4)

total = sum(counts.values())
print(
    "\t".join(
        [
            str(len(compiled)),
            str(total),
            ",".join(f"{k}={v}" for k, v in sorted(counts.items())) or "-",
        ]
    )
)

PYEOF

for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$SWEEP_ROOT/w"; sweep_stage "$d" "$LIB" "$b"
  sweep_compile "$b" "$d" "$PY" "$BY"
  if ! sweep_built "$d"; then printf '%s\tno-artifact\n' "$b" >> "$OUT"; continue; fi
  sweep_place "$d"
  cp "$SWEEP_ROOT/fnprobe.py" "$SWEEP_RUN_C/by_fnprobe.py"
  # read through `sweep_capture` rather than a command substitution, for the reason the
  # other rungs give: the probe can die in ways a substitution reports as empty output
  sweep_capture "$SWEEP_RUN_C" "$PY" by_fnprobe.py
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
  printf 'captured twins: %s in %s module(s)\n' \
    "$(awk -F'\t' '$2=="ok" {n+=$4} END {print n+0}' "$OUT")" \
    "$(awk -F'\t' '$2=="ok" && $4>0' "$OUT" | wc -l | tr -d ' ')"
}
sweep_end || exit 1

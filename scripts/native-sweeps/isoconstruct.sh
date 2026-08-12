#!/bin/bash
# each module in a project of its own, built, imported and then *constructed*
#
# the build sweep only builds and the import sweep only imports, so neither can see a
# constructor that binds its arguments wrongly — the boundary is not reached until
# something is made. this one calls every class the module defines with no arguments
# at all and compares the outcome, which is the cheapest call that reaches `tp_init`.
#
# a no-argument call is either the answer or an exception, and both are observations:
# the exception *text* is python's own arity wording, which is what a wrongly
# marked parameter changes. a construction that hangs or crashes truncates the leg's output,
# which reads as a difference rather than as a pass.
#
# usage: isoconstruct.sh SP BY PY OUT [MODULE...]
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
root="$SP/isocon.$$"; rm -rf "$root"; mkdir -p "$root"
trap 'rm -rf "$root"' EXIT
: > "$OUT"

cat > "$root/drive.py" <<'PYEOF'
# every class the module itself defines, called with no arguments. `signal.alarm`
# bounds a constructor that blocks, so one module cannot stall the sweep
import signal
import sys

class _Slow(Exception):
    pass

def _ring(signum, frame):
    raise _Slow('timed out')

signal.signal(signal.SIGALRM, _ring)

# a subpackage module is far likelier than a top-level one to fail this import: a
# `from . import x` has no parent package to resolve against once the file has been
# copied out on its own. both legs fail it alike, but an uncaught traceback would not
# read alike — the interpreted frame names `m.py` where the compiled one, running its
# fallback source through `PyRun_String`, names `<string>`. so the failure is caught
# and reported as one line that carries no path at all
try:
    signal.alarm(30)
    try:
        import m
    finally:
        signal.alarm(0)
except BaseException as error:
    print('IMPORT-FAILED', type(error).__name__, str(error), flush=True)
    raise SystemExit(0)

names = [
    name
    for name in sorted(vars(m))
    if isinstance(vars(m)[name], type) and getattr(vars(m)[name], '__module__', None) == 'm'
]

start = int(sys.argv[1]) if len(sys.argv) > 1 else 0

for name in names[start:]:
    value = vars(m)[name]
    try:
        signal.alarm(2)
        try:
            made = value()
        finally:
            signal.alarm(0)
    except BaseException as error:
        print(name, type(error).__name__, str(error), flush=True)
    else:
        print(name, 'built', type(made).__name__, flush=True)
PYEOF

# run one leg to completion, restarting past whatever killed it. a constructor that
# segfaults would otherwise truncate the leg's output, and a truncated leg reads as an
# ordinary difference — which is how ~40 modules' crashes went unreported
leg() {
  local dir="$1" start=0 attempts=0 text="" out=""
  while [ "$attempts" -lt 40 ]; do
    attempts=$((attempts+1))
    out=$(cd "$dir" && "$PY" drive.py "$start" 2>&1); local status=$?
    out=$(printf '%s' "$out" | sed "s|$dir/||g")
    [ -n "$out" ] && text="$text$out"$'\n'
    [ "$status" -eq 0 ] && break
    local done_lines
    done_lines=$(printf '%s' "$text" | grep -c '')
    text="$text""DIED signal=$status"$'\n'
    start=$((done_lines + 1))
  done
  printf '%s' "$text"
}

for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$root/w"; sweep_stage "$d" "$f"
  sweep_compile "$d" "$PY" "$BY"
  if ! sweep_built "$d"; then printf '%s\tno-artifact\n' "$b" >> "$OUT"; continue; fi
  cp "$root/drive.py" "$d/drive.py"; cp "$root/drive.py" "$d/o/drive.py"
  # a traceback names the file it came from, and the two legs run from different
  # directories — so the *path* would read as a difference the module never had
  i=$(leg "$d")
  c=$(leg "$d/o")
  if [ "$i" = "$c" ]; then
    # a module that cannot be imported standalone agrees on both legs and exercises
    # nothing — kept apart from `same` so the denominator stays honest
    case "$i" in IMPORT-FAILED*) printf '%s\timport-failed\t%s\n' "$b" "$i" ;;
      *) printf '%s\tsame\t%s\n' "$b" "$(printf '%s' "$i" | grep -c '')" ;;
    esac >> "$OUT"
  else
    printf '%s\tDIFFERS\n' "$b"
    diff <(printf '%s' "$i") <(printf '%s' "$c") | awk -v b="$b" '{print b "\t| " $0}'
  fi >> "$OUT"
done
echo "walked: $(grep -cE $'\t(same|DIFFERS|import-failed|no-artifact)' "$OUT")   exercised: $(grep -cE $'\t(same|DIFFERS)' "$OUT")   differing: $(grep -c $'\tDIFFERS' "$OUT")   crashed: $(grep -c 'DIED signal' "$OUT")   import-failed: $(grep -c $'\timport-failed' "$OUT")   no-artifact: $(grep -c $'\tno-artifact' "$OUT")"

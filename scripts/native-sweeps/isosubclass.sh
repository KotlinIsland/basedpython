#!/bin/bash
# each module in a project of its own, built, imported and then *subclassed*
#
# the build, import and construct sweeps all stay inside the module: they exercise
# what it defines. a consumer does one more thing — it derives from what it imported —
# and that is a boundary none of the three reach. `class Color(m.Enum): RED = 1` runs
# the metaclass, the descriptor protocol and `__set_name__` on the module's own
# compiled code, with a type the module never saw
#
# `types.new_class` is the class statement: it resolves the metaclass, calls
# `__prepare__`, runs the body against the namespace that returns, and hands the
# result over. a plain `type(name, bases, {})` is not — a prepared namespace is what
# turns `RED = 1` into whatever the metaclass wants
#
# a probe that crashes must not lose what it had already found. the driver prints one
# line per class, flushed, and takes the index to start from; a leg that dies is
# restarted past the class it died on, with the signal recorded in its place. so a
# segfault reads as one named line rather than as a truncated file
#
# usage: isosubclass.sh SP BY PY OUT [MODULE...]
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
root="$SP/isosub.$$"; rm -rf "$root"; mkdir -p "$root"
trap 'rm -rf "$root"' EXIT
: > "$OUT"

cat > "$root/drive.py" <<'PYEOF'
# every class the module itself defines, derived from. `signal.alarm` bounds a
# metaclass that blocks, so one module cannot stall the sweep
import importlib
import os
import signal
import sys
import types

# the staging says what to import: `m` for a top-level module, `pkg.m` for a package
# member, which is the only name its relative imports resolve against. both legs are
# handed the same one
MOD = os.environ['SWEEP_MOD']
# `by compile` names a module after its file, and an emitted class takes its
# `__module__` from the last component of that name — so it answers `m` where its
# interpreted twin answers `pkg.m`. both spellings mean "defined by the module under
# test", and nothing else in the staged tree answers to either
SELF = (MOD, MOD.rpartition('.')[2])


class _Slow(Exception):
    pass


def _ring(signum, frame):
    raise _Slow('timed out')


def _body(namespace):
    # a member the metaclass may want to do something with, which is what reaches
    # the descriptor protocol at all
    namespace['probe'] = 1


def main():
    signal.signal(signal.SIGALRM, _ring)

    # a module can still fail this import — a compiled extension that did not load, a
    # C accelerator this build has no source for. both legs fail it alike, but an
    # uncaught traceback would not read alike: the interpreted frame names `m.py` where
    # the compiled one, running its fallback source through `PyRun_String`, names
    # `<string>`. and a non-zero exit would send the restart loop below round forty
    # times for nothing. so the failure is caught and reported as one line carrying no
    # path at all
    try:
        signal.alarm(int(os.environ['SWEEP_IMPORT_BOUND']))
        try:
            m = importlib.import_module(MOD)
        finally:
            signal.alarm(0)
    except BaseException as error:
        print('IMPORT-FAILED', type(error).__name__, str(error), flush=True)
        raise SystemExit(0)

    start = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    names = [
        name
        for name in sorted(vars(m))
        if isinstance(vars(m)[name], type)
        and getattr(vars(m)[name], '__module__', None) in SELF
    ]

    for name in names[start:]:
        value = vars(m)[name]
        try:
            signal.alarm(2)
            try:
                made = types.new_class('Sub', (value,), {}, _body)
            finally:
                signal.alarm(0)
        except BaseException as error:
            print(name, type(error).__name__, str(error), flush=True)
        else:
            print(name, 'subclassed', made.__name__, flush=True)


# `multiprocessing` starts a worker by re-running this interpreter and importing this
# file, as `__mp_main__`. the package modules import now, so a module body that starts
# one is reachable, and without the guard the worker would walk the module again
if __name__ == '__main__':
    main()
PYEOF

# run one leg to completion, restarting past whatever killed it
leg() {
  local dir="$1" start=0 attempts=0 text="" out=""
  while [ "$attempts" -lt 40 ]; do
    attempts=$((attempts+1))
    out=$(cd "$dir" && "$PY" drive.py "$start" 2>&1); local status=$?
    # a warning names the file it came from, and the two legs run from different
    # directories — so the *path* would read as a difference the module never had
    out=$(printf '%s' "$out" | sed "s|$dir/||g")
    [ -n "$out" ] && text="$text$out"$'\n'
    [ "$status" -eq 0 ] && break
    # the class it died on is the one after everything reported so far
    local done_lines
    done_lines=$(printf '%s' "$text" | grep -c '' )
    text="$text""DIED signal=$status"$'\n'
    start=$((done_lines + 1))
  done
  printf '%s' "$text"
}

for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$root/w"; sweep_stage "$d" "$LIB" "$b"
  sweep_compile "$b" "$d" "$PY" "$BY"
  if ! sweep_built "$d"; then printf '%s\tno-artifact\n' "$b" >> "$OUT"; continue; fi
  sweep_place "$d"
  cp "$root/drive.py" "$SWEEP_RUN_I/drive.py"; cp "$root/drive.py" "$SWEEP_RUN_C/drive.py"
  i=$(leg "$SWEEP_RUN_I")
  c=$(leg "$SWEEP_RUN_C")
  if [ "$i" = "$c" ]; then
    # a module that cannot be imported here agrees on both legs and exercises nothing —
    # kept apart from `same` so the denominator stays honest
    case "$i" in IMPORT-FAILED*) printf '%s\timport-failed\t%s\n' "$b" "$i" ;;
      *) printf '%s\tsame\t%s\n' "$b" "$(printf '%s' "$i" | grep -c '')" ;;
    esac >> "$OUT"
  elif printf '%s%s' "$i" "$c" | grep -q '_Slow timed out'; then
    # a class the metaclass took more than two seconds over, or an import that outran
    # its thirty — a loaded machine loses either bound on one leg and not the other,
    # which says nothing about the compiler. kept out of `differing` so the headline
    # number is the same on a busy machine as an idle one, and out of `same` because a
    # leg that was killed answered nothing
    printf '%s\ttimed-out\n' "$b" >> "$OUT"
  else
    printf '%s\tDIFFERS\n' "$b"
    diff <(printf '%s' "$i") <(printf '%s' "$c") | awk -v b="$b" '{print b "\t| " $0}'
  fi >> "$OUT"
done
echo "walked: $(grep -cE $'\t(same|DIFFERS|timed-out|import-failed|no-artifact)' "$OUT")   exercised: $(grep -cE $'\t(same|DIFFERS)' "$OUT")   differing: $(grep -c $'\tDIFFERS' "$OUT")   crashed: $(grep -c 'DIED signal' "$OUT")   timed-out: $(grep -c $'\ttimed-out' "$OUT")   import-failed: $(grep -c $'\timport-failed' "$OUT")   no-artifact: $(grep -c $'\tno-artifact' "$OUT")"

#!/bin/bash
# each module in a project of its own, built, imported, and then asked what its classes
# *contain*
#
# the other sweeps all reach a class from the outside: that it builds, imports, can be
# called and can be subclassed. none of them looks inside, so a class whose body was
# lost on the way to its metaclass passes every one of them — it builds, imports,
# constructs and subclasses, and only its contents are wrong. that is what this sees.
#
# what is compared is chosen so that a difference is a defect rather than a design
# choice. the two builds differ by design in ways that would drown the signal: a
# compiled method is a descriptor where an interpreted one is a function, a spec type
# has no `__dict__`/`__weakref__`/`__doc__` and none of 3.13's compiler-set
# `__firstlineno__`/`__static_attributes__`, and a compiled class publishes a getset for
# every field its interpreted twin keeps on the instance. so:
#
#   - the mro, the metaclass, `__module__` and `__qualname__` are compared outright
#   - names are compared *one way* — the compiled class must not be missing anything its
#     interpreted twin has. gaining a field descriptor is by design; losing a member is
#     the bug
#   - the derived surface a metaclass produces — `_member_names_`, `__abstractmethods__`
#     — is compared outright, because that is precisely what a lost namespace changes
#   - every module-level value whose type this module owns is asked whether it is still
#     an instance of the class now under that name
#
# one cause currently dominates `differing`: an emitted class in a package reports its
# `__module__` without the package, so 215 of the 246 differ on nothing else. that is one
# defect, not 215 — `grep '| > .* named ' | grep -v \\.` separates them, and the rest is
# the number to watch
#
# usage: isosurface.sh SP BY PY OUT [MODULE...]
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
root="$SP/isosurf.$$"; rm -rf "$root"; mkdir -p "$root"
trap 'rm -rf "$root"' EXIT
: > "$OUT"

cat > "$root/drive.py" <<'PYEOF'
import importlib
import os
import signal

# the staging says what to import: `m` for a top-level module, `pkg.m` for a package
# member, which is the only name its relative imports resolve against. both legs are
# handed the same one
MOD = os.environ['SWEEP_MOD']
# `by compile` names a module after its file, and an emitted class takes its
# `__module__` from the last component of that name — so it answers `m` where its
# interpreted twin answers `pkg.m`. both spellings have to select the *same classes*, or
# the compiled leg would appear to define none of them. what a class says its module is
# stays compared outright, because that answer is wrong rather than merely differently
# spelled
SELF = (MOD, MOD.rpartition('.')[2])


class _Slow(Exception):
    pass


def _ring(signum, frame):
    raise _Slow('timed out')


signal.signal(signal.SIGALRM, _ring)

# a module can still fail this import — a compiled extension that did not load, a C
# accelerator this build has no source for. both legs fail it alike, but an uncaught
# traceback would not read alike: the interpreted frame names `m.py` where the compiled
# one, running its fallback source through `PyRun_String`, names `<string>`. so the
# failure is caught and reported as one line that carries no path at all
try:
    signal.alarm(int(os.environ['SWEEP_IMPORT_BOUND']))
    try:
        m = importlib.import_module(MOD)
    finally:
        signal.alarm(0)
except BaseException as error:
    print('IMPORT-FAILED', type(error).__name__, str(error), flush=True)
    raise SystemExit(0)


def show(line):
    print(line, flush=True)


def safe(text):
    # a repr carrying an address differs between two processes for no reason of ours
    if ' at 0x' in text or ' object at ' in text:
        return '<addr>'
    return text


def value_of(target, key):
    try:
        return safe(repr(getattr(target, key)))
    except BaseException as error:
        return '<raised %s>' % type(error).__name__


def owned(value):
    return isinstance(value, type) and getattr(value, '__module__', None) in SELF


# what a compiled type does not carry, by construction rather than by defect: a spec
# type has no instance `__dict__` and no `__weakref__` slot, and 3.13's compiler-set
# `__firstlineno__`/`__static_attributes__` come from a code object there is none of.
# `__module__` answers on the class either way, but only the interpreted one keeps it
# in the class dict
BY_DESIGN = {
    '__dict__', '__weakref__', '__firstlineno__', '__static_attributes__', '__module__',
}


classes = [(name, vars(m)[name]) for name in sorted(vars(m)) if owned(vars(m)[name])]

for name, cls in classes:
    try:
        show('%s mro %s' % (name, [base.__name__ for base in cls.__mro__]))
        show('%s meta %s' % (name, type(cls).__name__))
        # `__module__` is compared outright, and the two spellings above are *not*
        # collapsed here. a compiled class in a package answers `m` where its twin
        # answers `pkg.m`, and that is a wrong answer rather than a house style:
        # `dataclasses` does `sys.modules[cls.__module__].__dict__` and gets `None`
        show('%s named %s %s' % (name, cls.__module__, cls.__qualname__))
        # one way: a name the interpreted class has and the compiled one does not.
        # `MISSING` is printed by the compiled leg only, so an identical pair of legs
        # prints nothing and any loss prints a line the other leg cannot match
        for key in sorted((set(dir(cls)) | set(vars(cls))) - BY_DESIGN):
            show('%s has %s' % (name, key))
        # what a metaclass made of the namespace it was handed. both are unordered
        # collections, so they are sorted rather than repr'd — a `frozenset`'s repr
        # order differs between two processes for no reason of ours
        for key in ('_member_names_', '__abstractmethods__'):
            if hasattr(cls, key):
                try:
                    value = sorted(getattr(cls, key))
                except BaseException as error:
                    value = '<raised %s>' % type(error).__name__
                show('%s %s %s' % (name, key, value))
    except BaseException as error:
        show('%s <walk raised %s>' % (name, type(error).__name__))

# a value the module body made before the class under its name was replaced still
# claims the class it was made from
for name in sorted(vars(m)):
    value = vars(m)[name]
    if isinstance(value, type):
        continue
    kind = type(value)
    if getattr(kind, '__module__', None) not in SELF:
        continue
    claimed = getattr(m, kind.__name__, None)
    if not isinstance(claimed, type):
        continue
    try:
        answer = isinstance(value, claimed)
    except BaseException as error:
        answer = '<raised %s>' % type(error).__name__
    show('%s instanceof %s %s' % (name, kind.__name__, answer))
PYEOF

for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$root/w"; sweep_stage "$d" "$LIB" "$b"
  sweep_compile "$b" "$d" "$PY" "$BY"
  if ! sweep_built "$d"; then printf '%s\tno-artifact\n' "$b" >> "$OUT"; continue; fi
  sweep_place "$d"
  cp "$root/drive.py" "$SWEEP_RUN_I/drive.py"; cp "$root/drive.py" "$SWEEP_RUN_C/drive.py"
  # a traceback names the file it came from, and the two legs run from different
  # directories — so the *path* would read as a difference the module never had
  i=$(cd "$SWEEP_RUN_I" && "$PY" drive.py 2>&1 | sed "s|$SWEEP_RUN_I/||g")
  c=$(cd "$SWEEP_RUN_C" && "$PY" drive.py 2>&1 | sed "s|$SWEEP_RUN_C/||g")
  # a `has` line is one-directional: only its loss counts, so the compiled leg's extra
  # names are dropped before the comparison and the interpreted leg's are kept
  ionly=$(echo "$i" | grep -v ' has ')
  only=$(echo "$c" | grep -v ' has ')
  lost=$(comm -23 <(echo "$i" | grep ' has ' | sort) <(echo "$c" | grep ' has ' | sort))
  if [ "$ionly" = "$only" ] && [ -z "$lost" ]; then
    # a module that cannot be imported here agrees on both legs and exercises nothing —
    # kept apart from `same` so the denominator stays honest
    case "$i" in IMPORT-FAILED*) printf '%s\timport-failed\t%s\n' "$b" "$i" ;;
      *) printf '%s\tsame\t%s\n' "$b" "$(echo "$i" | wc -l | tr -d ' ')" ;;
    esac >> "$OUT"
  elif printf '%s%s' "$i" "$c" | grep -q '_Slow timed out'; then
    # the import bound is 30 seconds and a loaded machine loses it on one leg and not
    # the other. that says nothing about the compiler, so it is kept out of `differing`
    # — and out of `same`, because a leg that was killed answered nothing
    printf '%s\ttimed-out\n' "$b" >> "$OUT"
  else
    printf '%s\tDIFFERS\n' "$b"
    diff <(echo "$ionly") <(echo "$only") | awk -v b="$b" '{print b "\t| " $0}'
    echo "$lost" | awk -v b="$b" 'NF {print b "\t| lost " $0}'
  fi >> "$OUT"
done
echo "walked: $(grep -cE $'\t(same|DIFFERS|timed-out|import-failed|no-artifact)' "$OUT")   exercised: $(grep -cE $'\t(same|DIFFERS)' "$OUT")   differing: $(grep -c $'\tDIFFERS' "$OUT")   lost: $(grep -c $'\t| lost ' "$OUT")   timed-out: $(grep -c $'\ttimed-out' "$OUT")   import-failed: $(grep -c $'\timport-failed' "$OUT")   no-artifact: $(grep -c $'\tno-artifact' "$OUT")"

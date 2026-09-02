#!/bin/bash
# each module in a project of its own, built, imported, constructed — and then the
# instance *used*
#
# `isoconstruct` calls every class with no arguments and compares the outcome, so it
# sees a constructor that binds its arguments wrongly. it stops there: the instance it
# made is thrown away. `isosurface` asks the *class* what it contains, which is a
# question about the type object and not about anything the type was used for. so a
# whole class of divergence falls between them — the constructor succeeds, the class
# carries every name it should, and the object is wrong the first time a program touches
# it:
#
#     tempfile._RandomNameSequence.rng   a @property whose body does `self._rng = ...`
#       interpreted  <random.Random object at 0xX>
#       compiled     <raised AttributeError: '_RandomNameSequence' object has no
#                     attribute '_rng' and no __dict__ for setting new attributes>
#
# every other rung scores that module `same`, because none of them reads an attribute
# off an instance. this one does: it constructs each class and then touches what a
# program would touch — every property, every class-level attribute, and `repr`, `str`,
# `bool`, `len`, `hash`, `iter` and `next` where the class defines them — and compares
# the answer, value or exception, between the two legs.
#
# three things make a *value* comparison trustworthy where a name comparison did not
# need to be:
#
#   - the two legs must touch the same things. a compiled class publishes a getset for
#     every field its twin keeps on the instance, and a compiled property is not a
#     `property` object — so a leg left to choose for itself would probe a different set
#     and, worse, would skip the very member above. so the *plan* is drawn up once, from
#     the interpreted class, and both legs are handed it
#   - a value that moves on its own is not evidence. `next()` on the sequence above
#     returns eight random characters; a property can read the clock. so each leg is run
#     twice and any probe that does not agree with *itself* is dropped from the
#     comparison and counted as `unstable`, rather than reported as a difference. a
#     difference that survives that gets a third run of each leg before it is reported,
#     because a probe with few possible answers — a `__bool__` that flips a coin —
#     repeats itself often enough to pass a two-run filter by luck
#   - what is left still has to be rendered so that two processes agree when nothing is
#     wrong: `sweep_write_renderer` does that, and the reasons are with it
#
# usage: isoinstance.sh SP BY PY OUT [MODULE...]
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
sweep_begin isoinst || exit 1

# how long one construction or one probe is given. it is the same two seconds
# `isoconstruct` allows a constructor, for the same reason: a member that blocks must
# not stall the corpus
export SWEEP_PROBE_BOUND=${SWEEP_PROBE_BOUND:-2}
# a class's members are read in sorted order and this many are taken. the cap applies to
# both legs identically — it is applied when the plan is drawn up, and both legs run the
# same plan — so it can hide a difference past the cap and can never invent one. it is
# here because a class deriving from `dict` inherits about forty names that all render
# as `<callable ...>`, and a handful of modules define hundreds of classes
export SWEEP_MEMBER_LIMIT=${SWEEP_MEMBER_LIMIT:-40}

cat > "$SWEEP_ROOT/plan.py" <<'PYEOF'
"""what both legs will touch, decided once by the interpreted class

a leg that chose for itself would choose differently — a compiled class's members are
getset descriptors where its twin's are properties and functions — and the difference
would fall exactly where the defect is. so this runs against the interpreted module and
its answer is handed to both
"""

import importlib
import os
import signal

# the staging says what to import: `m` for a top-level module, `pkg.m` for a package
# member, which is the only name its relative imports resolve against
MOD = os.environ['SWEEP_MOD']
SELF = (MOD, MOD.rpartition('.')[2])
LIMIT = int(os.environ['SWEEP_MEMBER_LIMIT'])


def _ring(signum, frame):
    raise TimeoutError('timed out')


signal.signal(signal.SIGALRM, _ring)
try:
    signal.alarm(int(os.environ['SWEEP_IMPORT_BOUND']))
    try:
        m = importlib.import_module(MOD)
    finally:
        signal.alarm(0)
except BaseException as error:
    print('@IMPORT-FAILED\t%s\t%s' % (type(error).__name__, error), flush=True)
    raise SystemExit(0)

# what a compiled type does not carry by construction rather than by defect, and what
# 3.13's compiler writes into a class statement's namespace from a code object a spec
# has none of. reading any of these off an instance compares the two builds' plumbing
# rather than the module's meaning
BY_DESIGN = {
    '__dict__', '__weakref__', '__firstlineno__', '__static_attributes__', '__module__',
}

for name in sorted(vars(m)):
    cls = vars(m)[name]
    if not isinstance(cls, type) or getattr(cls, '__module__', None) not in SELF:
        continue
    try:
        members = dir(cls)
    except BaseException:
        members = []
    lines = [(name, 'repr', '-'), (name, 'str', '-'), (name, 'bool', '-')]
    # a default `__hash__` is derived from the address, so it differs between two
    # processes of the same build and says nothing. only a hash the class wrote is asked
    if getattr(cls, '__hash__', None) not in (None, object.__hash__):
        lines.append((name, 'hash', '-'))
    for dunder, kind in (('__len__', 'len'), ('__iter__', 'iter'), ('__next__', 'next')):
        if dunder in members:
            lines.append((name, kind, '-'))
    # `hasattr(object, ...)` drops what every object has; the dunder test drops the rest
    # of the protocol surface, which is compared above as behaviour rather than read as
    # a value — a method read off an instance is a bound method on one leg and a builtin
    # on the other, and that is spelling, not meaning
    attrs = [member for member in sorted(members)
             if not (member.startswith('__') and member.endswith('__'))
             and member not in BY_DESIGN
             and not hasattr(object, member)]
    for member in attrs[:LIMIT]:
        lines.append((name, 'attr', member))
    for line in lines:
        print('@PLAN\t%s\t%s\t%s' % line, flush=True)
PYEOF

cat > "$SWEEP_ROOT/probe.py" <<'PYEOF'
"""construct each class the plan names, then touch it the way a program would"""

import importlib
import os
import signal
import sys

from sweepcanon import Canon

MOD = os.environ['SWEEP_MOD']
SELF = (MOD, MOD.rpartition('.')[2])
BOUND = int(os.environ['SWEEP_PROBE_BOUND'])


class _Slow(Exception):
    pass


def _ring(signum, frame):
    raise _Slow('timed out')


def load():
    try:
        signal.alarm(int(os.environ['SWEEP_IMPORT_BOUND']))
        try:
            return importlib.import_module(MOD)
        finally:
            signal.alarm(0)
    except BaseException as error:
        print('@IMPORT-FAILED\t%s\t%s' % (type(error).__name__, error), flush=True)
        raise SystemExit(0)


def aliases(m):
    """the module prefix the two builds spell differently, and nothing else

    an emitted class takes its `__module__` from the last component of the file name, so
    it answers `m` where its twin answers `by_stage.pkg.m`. that spelling is inside every
    `repr`, every `<class ...>` and every AttributeError message a probe produces, so
    left alone it would be the only thing this rung ever reported. `isosurface` compares
    `__module__` outright and reports the defect once per class, which is where it
    belongs. here both spellings are removed, by exact class name rather than by pattern:
    a blanket `m.` substitution would also rewrite `m.py` inside a message
    """
    out = []
    quals = set()
    for name in sorted(vars(m)):
        cls = vars(m)[name]
        if not isinstance(cls, type) or getattr(cls, '__module__', None) not in SELF:
            continue
        quals.add(name)
        quals.add(getattr(cls, '__qualname__', name))
        try:
            inner_values = list(vars(cls).values())
        except BaseException:
            inner_values = []
        for inner in inner_values:
            if isinstance(inner, type):
                quals.add(inner.__name__)
                quals.add(getattr(inner, '__qualname__', inner.__name__))
    for qual in quals:
        for prefix in SELF:
            out.append(('%s.%s' % (prefix, qual), qual))
    return out


def instance(m, cache, name):
    """one instance per class, made once and reused by every probe against it

    a construction that fails is not this rung's finding — `isoconstruct` compares that
    outcome, wording and all — so only the fact is recorded, and the class's probes all
    answer with it. that keeps one line per plan entry, which is what lets a leg killed
    mid-probe be restarted at the right place
    """
    if name in cache:
        return cache[name]
    cls = getattr(m, name, None)
    if not isinstance(cls, type):
        made = (None, '<no such class>')
    else:
        try:
            signal.alarm(BOUND)
            try:
                made = (cls(), None)
            finally:
                signal.alarm(0)
        except BaseException as error:
            made = (None, '<unconstructed %s>' % type(error).__name__)
    cache[name] = made
    return made


def touch(canon, obj, kind, member):
    if kind == 'attr':
        return canon.render(getattr(obj, member))
    if kind == 'repr':
        return canon.scrub(repr(obj))
    if kind == 'str':
        return canon.scrub(str(obj))
    if kind == 'bool':
        return repr(bool(obj))
    if kind == 'len':
        return repr(len(obj))
    if kind == 'hash':
        return repr(hash(obj))
    if kind == 'iter':
        return canon.render(type(iter(obj)))
    if kind == 'next':
        return canon.render(next(obj))
    return '<unknown probe %s>' % kind


def main():
    signal.signal(signal.SIGALRM, _ring)
    m = load()
    canon = Canon(aliases(m))
    with open('plan.txt') as handle:
        plan = [line.rstrip('\n').split('\t') for line in handle if line.strip()]
    start = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    cache = {}
    for index in range(start, len(plan)):
        name, kind, member = plan[index]
        obj, failure = instance(m, cache, name)
        if failure is not None:
            answer = failure
        else:
            try:
                signal.alarm(BOUND)
                try:
                    answer = touch(canon, obj, kind, member)
                finally:
                    signal.alarm(0)
            except BaseException as error:
                answer = canon.raised(error)
        print('@P%d\t%s\t%s\t%s\t%s' % (index, name, kind, member, answer), flush=True)


# `multiprocessing` starts a worker by re-running this interpreter and importing this
# file, as `__mp_main__`. a constructor that makes a pool would otherwise have every
# worker import the module and probe every class in it again — pool included
if __name__ == '__main__':
    main()
PYEOF

# run one leg to completion, restarting past whatever killed it
#
# a probe that segfaults truncates the leg, and a truncated leg reads as an ordinary
# difference — which is exactly how 56 modules' crashes went unreported in
# `isoconstruct` before it grew this loop. the death is written as a line of its own
# carrying the index it happened at, so it both survives into the comparison and
# consumes the probe that caused it
#
# only the probe lines and an import failure are kept. a leg also writes whatever the
# module wrote — a traceback, an `Exception ignored in __del__` — and those lines carry
# none of the four fields the comparison pairs rows by, so keeping them would pair rows
# that are not the same probe. nothing is lost by dropping them: a leg that failed
# outright leaves a non-zero status, which becomes a `DIED` row of its own
leg() {
  local dir="$1" start=0 attempts=0 text="" out="" status=0 done_lines=0
  while [ "$attempts" -lt 40 ]; do
    attempts=$((attempts+1))
    # through `sweep_capture` rather than a command substitution around the probe: a
    # constructor is free to start a process that inherits the leg's stdout, and a pipe
    # one of those still holds never reaches end of file. the reasons are with the
    # helper, in `sweeplib.sh`
    sweep_capture "$dir" "$PY" probe.py "$start"; status=$SWEEP_CAPTURE_STATUS
    out=$(printf '%s' "$SWEEP_CAPTURE_TEXT" | grep -E '^@(P[0-9]+|IMPORT-FAILED)\t')
    # *both* legs' directories are replaced, in both legs, and the compiled one first
    # because it sits inside the interpreted one. scrubbing only the leg's own directory
    # would leave the other leg's spelling of the same idea standing: `wsgiref.handlers`
    # publishes `os.environ` as a class attribute, and the two legs necessarily run from
    # different directories, so `PWD` alone made that module differ
    out=$(printf '%s' "$out" | sed -e "s|$SWEEP_RUN_C|<leg>|g" -e "s|$SWEEP_RUN_I|<leg>|g")
    [ -n "$out" ] && text="$text$out"$'\n'
    [ "$status" -eq 0 ] && break
    done_lines=$(printf '%s' "$text" | grep -c '^@P')
    text="$text@P$done_lines"$'\t'"DIED"$'\t'"signal=$status"$'\t'"-"$'\t'"died"$'\n'
    start=$((done_lines + 1))
    # 137 is the whole-leg bound killing a leg that does not finish, which is a different
    # claim from a probe dying: restarting past one probe assumes the next will get
    # further, and a leg that hangs after its last answer would hang again forty times
    # over. the death row above carries it into the comparison either way
    [ "$status" -eq 137 ] && break
  done
  printf '%s' "$text"
}

# the probes a leg agrees with itself about. anything else moved on its own — a clock, a
# random draw, an address this rung did not scrub — and cannot be evidence either way
stable() {
  comm -12 <(printf '%s' "$1" | LC_ALL=C sort) <(printf '%s' "$2" | LC_ALL=C sort)
}

# keep the lines whose key — index, class, probe, member — is in the given key list
keep() {
  LC_ALL=C join -t $'\t' -j 1 \
    <(printf '%s' "$1" | awk -F'\t' 'NF {print $1 "\x01" $2 "\x01" $3 "\x01" $4 "\t" $0}' | LC_ALL=C sort -t $'\t' -k1,1) \
    <(printf '%s' "$2" | LC_ALL=C sort) \
    | cut -f2-
}

keys() {
  printf '%s' "$1" | awk -F'\t' 'NF {print $1 "\x01" $2 "\x01" $3 "\x01" $4}' | LC_ALL=C sort -u
}

for b in $(sweep_modules "$LIB" "$@"); do
  f="$LIB/$b"
  [ -f "$f" ] || continue
  d="$SWEEP_ROOT/w"; sweep_stage "$d" "$LIB" "$b"
  sweep_compile "$b" "$d" "$PY" "$BY"
  if ! sweep_built "$d"; then printf '%s\tno-artifact\n' "$b" >> "$OUT"; continue; fi
  sweep_place "$d"
  for run in "$SWEEP_RUN_I" "$SWEEP_RUN_C"; do
    cp "$SWEEP_ROOT/plan.py" "$run/plan.py"; cp "$SWEEP_ROOT/probe.py" "$run/probe.py"
    sweep_write_renderer "$run"
  done
  # the plan is drawn up on the interpreted leg and handed to both. a module the
  # interpreter cannot import here exercises nothing, and says so rather than agreeing
  #
  # importing a module runs it, and a module may print: `this.py` prints the whole zen of
  # python at import. so the plan is read off marked lines rather than off the output,
  # and so is everything the probe program says. without that, twenty lines of poetry
  # became twenty malformed plan entries, the probe program raised on the first of them,
  # and the module was reported as a crash on both legs
  #
  # it is read through `sweep_capture` for the same reason the probes are: it builds every
  # class in the module, so it starts — and leaks — whatever a constructor starts
  sweep_capture "$SWEEP_RUN_I" "$PY" plan.py; said=$SWEEP_CAPTURE_TEXT
  plan=$(printf '%s' "$said" | grep '^@PLAN'$'\t' | cut -f2-)
  if printf '%s' "$said" | grep -q '^@IMPORT-FAILED'$'\t'; then
    printf '%s\timport-failed\t%s\n' "$b" \
      "$(printf '%s' "$said" | grep '^@IMPORT-FAILED'$'\t' | head -1)" >> "$OUT"; continue
  fi
  if [ -z "$plan" ]; then
    # no class this module owns, or none with anything to touch. a rung that scored this
    # `same` would be reporting agreement it never looked for
    printf '%s\tnothing-to-probe\n' "$b" >> "$OUT"; continue
  fi
  printf '%s\n' "$plan" > "$SWEEP_RUN_I/plan.txt"
  printf '%s\n' "$plan" > "$SWEEP_RUN_C/plan.txt"
  i1=$(leg "$SWEEP_RUN_I"); i2=$(leg "$SWEEP_RUN_I")
  c1=$(leg "$SWEEP_RUN_C"); c2=$(leg "$SWEEP_RUN_C")
  steady=$(comm -12 <(keys "$(stable "$i1" "$i2")") <(keys "$(stable "$c1" "$c2")"))
  i=$(keep "$i1" "$steady"); c=$(keep "$c1" "$steady")
  if [ "$i" != "$c" ]; then
    # two runs are enough to expose a clock or a random draw, and not enough to expose a
    # probe with only a few possible answers: a `__bool__` that flips a coin repeats
    # itself half the time, and a rung that stopped here would report the coin as a
    # defect. a difference is therefore confirmed with a third run of each leg before it
    # is reported, which costs a run only on the modules that were going to be looked at
    # by hand anyway
    i3=$(leg "$SWEEP_RUN_I"); c3=$(leg "$SWEEP_RUN_C")
    steady=$(comm -12 <(keys "$(stable "$(stable "$i1" "$i2")" "$i3")") \
                      <(keys "$(stable "$(stable "$c1" "$c2")" "$c3")"))
    i=$(keep "$i1" "$steady"); c=$(keep "$c1" "$steady")
  else
    i3=""; c3=""
  fi
  # a leg killed by an alarm has not answered, and an empty answer must never be read as
  # agreement — so this is decided before any verdict is written.
  #
  # a *death* is decided before even that. the restart loop steps past one and carries on,
  # so a leg that crashed can still reach a probe that outruns its bound, and this test
  # would then be the one that matched — writing a `timed-out` row and calling no
  # `sweep_note_death`, which leaves `sweep_end`'s cross-check nothing to disagree with.
  # the death branch further down does both, so reaching it is the whole fix
  if ! sweep_pair_died "$i1$i2$i3" "$c1$c2$c3" \
     && printf '%s%s%s%s%s%s' "$i1" "$i2" "$c1" "$c2" "$i3" "$c3" | grep -q '_Slow timed out'; then
    printf '%s\ttimed-out\n' "$b" >> "$OUT"; continue
  fi
  # a compiled leg that cannot be imported at all has no probes to compare, and its one
  # line would read as a wholesale difference. it is a real finding, but it is
  # `isoimport`'s, so it is named rather than diffed
  if printf '%s' "$c1" | grep -q '^@IMPORT-FAILED'$'\t'; then
    printf '%s\tcompiled-import-failed\t%s\n' "$b" \
      "$(printf '%s' "$c1" | grep '^@IMPORT-FAILED'$'\t' | head -1)" >> "$OUT"; continue
  fi
  compared=$(printf '%s' "$steady" | grep -c '')
  unstable=$(( $(printf '%s' "$plan" | grep -c '') - compared ))
  # a leg that was killed answered nothing, and two legs killed the same way answer
  # nothing in the same words — which is not agreement. this was not hypothetical: a
  # broken copy of this script made both legs die identically forty times, and the first
  # version scored the pair `same`. so a death is its own verdict, and the diff goes with
  # it so the probe that caused it is named
  #
  # what a death *looks like* is `sweep_pair_died`'s to know, and the fact of one is
  # recorded where `sweep_end` cross-checks it against the verdict written here — this
  # rung gets that check right, and the check exists so that the next one need not
  if sweep_pair_died "$i1$i2$i3" "$c1$c2$c3"; then
    sweep_note_death "$b"
    { printf '%s\tCRASHED\t%s\tunstable=%s\n' "$b" "$compared" "$unstable"
      diff <(printf '%s\n' "$i") <(printf '%s\n' "$c") | awk -v b="$b" '{print b "\t| " $0}'
    } >> "$OUT"
  elif [ "$compared" -eq 0 ]; then
    # every probe moved on its own, so the two legs were never compared. this is the
    # reading `instancecensus` once gave silently as `0 of 0`, and it is not agreement
    printf '%s\tnothing-stable\t%s\n' "$b" "$(printf '%s' "$plan" | grep -c '')" >> "$OUT"
  elif [ "$i" = "$c" ]; then
    printf '%s\tsame\t%s\tunstable=%s\n' "$b" "$compared" "$unstable" >> "$OUT"
  else
    { printf '%s\tDIFFERS\t%s\tunstable=%s\n' "$b" "$compared" "$unstable"
      diff <(printf '%s\n' "$i") <(printf '%s\n' "$c") | awk -v b="$b" '{print b "\t| " $0}'
    } >> "$OUT"
  fi
done
sweep_end || exit 1
echo "walked: $(grep -cE $'\t(same|DIFFERS|CRASHED|timed-out|import-failed|compiled-import-failed|nothing-to-probe|nothing-stable|no-artifact)\t?' "$OUT")   exercised: $(grep -cE $'\t(same|DIFFERS)\t' "$OUT")   differing: $(grep -c $'\tDIFFERS\t' "$OUT")   crashed: $(grep -c $'\tCRASHED\t' "$OUT")   nothing-stable: $(grep -c $'\tnothing-stable' "$OUT")   nothing-to-probe: $(grep -c $'\tnothing-to-probe' "$OUT")   timed-out: $(grep -c $'\ttimed-out' "$OUT")   import-failed: $(grep -c $'\timport-failed' "$OUT")   compiled-import-failed: $(grep -c $'\tcompiled-import-failed' "$OUT")   no-artifact: $(grep -c $'\tno-artifact' "$OUT")"

#!/bin/bash
# each module in a project of its own, built, imported, constructed — and then the
# instance *changed*
#
# the six rungs below this one all read. `isoconstruct` calls a class and compares the
# outcome, `isosurface` asks the type what it carries, `isoinstance` builds an object and
# reads it back. not one of them ever writes to the object it made, and three real
# divergences had to be found by hand for want of a rung that did:
#
#   - `$sent` went stale across `next()` in a compiled generator
#   - `__repr__ = _repr` in a class body answered one thing through the slot and another
#     through the name
#   - `del obj.attr` had no corpus evidence at all behind it, only unit tests
#
# so this one constructs an instance per attribute and does to it what a program does:
# assigns over the attribute and reads it back, deletes it and reads it back, assigns
# again, and — once per class — sets a name the class never had.
#
# a rung that writes has one failure mode a rung that reads does not, and it is the one
# that has cost this project the most: **a leg that silently does nothing scores the same
# as a leg that works**. a probe reporting only "the assignment did not raise" cannot tell
# a store from a no-op, and two legs that both quietly dropped the write would agree. so
# every mutation here is judged by what a *later read* returns, and each probe carries an
# `eff` field naming which of its three writes demonstrably changed that read. two
# consequences follow:
#
#   - a compiled leg whose write is dropped reads back the old value, so its `eff` differs
#     from its twin's and the module is reported
#   - a module where nothing was demonstrably changed on *either* leg is not scored
#     `same`. it gets `nothing-mutated`, because agreement about a mutation that never
#     happened is not evidence about mutation
#
# one difference is architectural rather than a defect and is counted instead of diffed:
# an emitted class may have no instance dict, so a name the class never had has nowhere to
# go. its twin stores it and it refuses. that pair is folded — in that direction only — and
# the count lands on the module's row as `nodict=`. a compiled class that *accepts* what
# its twin refuses is the opposite finding and is left standing.
#
# from 3.13 on most classes carry a managed dict and this fires for none of them. what is
# left is a class that declares `__slots__`, a class whose layout is not its own, and every
# class below 3.13.
#
# two more readings that are not defects, and are told apart here rather than left to a
# reader to guess at:
#
#   - a value the *program* had no business assigning. an emitted class checks the type of
#     what is written to a field where its twin, holding the same field in an instance
#     dict, checks nothing — so one sentinel string written into every attribute makes the
#     compiled leg correctly refuse and reads as a difference. the promise this project
#     makes is about programs `by check` accepts, and that is not one. so a write is of the
#     attribute's own type wherever `writes` can make one, and a refusal is a finding again
#
#   - both legs dying the same death. `ssl.py`'s `del c.keylog_filename` segfaults stock
#     cpython with no compiled module involved, so the compiled leg segfaults by faithfully
#     doing what its twin does. `deaths` gives each leg's deaths a plan index and a signal
#     and compares them: identical is `both-died`, and anything else is still `CRASHED` and
#     says which leg it was
#
# usage: isomutate.sh SP BY PY OUT [MODULE...]
SP="$1"; BY="$2"; PY="$3"; OUT="$4"; shift 4
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"
LIB=$(sweep_lib "$PY")
sweep_begin isomutate || exit 1

# how long one construction or one step of one probe is given. the same two seconds
# `isoconstruct` allows a constructor, for the same reason: a member that blocks must not
# stall the corpus
export SWEEP_PROBE_BOUND=${SWEEP_PROBE_BOUND:-2}
# how many of a class's attributes are mutated. lower than `isoinstance`'s forty because
# each one here costs a construction and seven timed steps against that rung's single
# read. the cap is applied when the plan is drawn up and both legs run the same plan, so
# it can hide a difference past the cap and can never invent one
export SWEEP_MEMBER_LIMIT=${SWEEP_MEMBER_LIMIT:-12}

cat > "$SWEEP_ROOT/plan.py" <<'PYEOF'
"""what both legs will change, decided once by the interpreted object

a leg left to choose for itself would choose differently, and the difference would fall
exactly where the defect is: a compiled class publishes a getset descriptor where its twin
keeps an entry in the instance dict, so "the attributes this object has" is not the same
question on the two sides. the list is read off the interpreted object and handed to both
"""

import importlib
import os
import signal

MOD = os.environ['SWEEP_MOD']
SELF = (MOD, MOD.rpartition('.')[2])
LIMIT = int(os.environ['SWEEP_MEMBER_LIMIT'])
BOUND = int(os.environ['SWEEP_PROBE_BOUND'])

# what a compiled type does not carry by construction rather than by defect, and what
# 3.13's compiler writes into a class statement's namespace from a code object a spec has
# none of. changing any of these compares the two builds' plumbing rather than the
# module's meaning
BY_DESIGN = {
    '__dict__', '__weakref__', '__firstlineno__', '__static_attributes__', '__module__',
}


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


def slots(klass):
    named = getattr(klass, '__slots__', ())
    if isinstance(named, str):
        return (named,)
    try:
        return tuple(named)
    except BaseException:
        return ()


def fields(obj):
    """the names a program would treat as this object's own state

    a method and a class-level constant are deliberately not in here. assigning over
    either of them on an *instance* is not a question about that member — it is the
    question "may this object hold a name its class did not give it", which the `new`
    probe asks once per class and answers better. what is left is what a compiled class
    ought to be modelling as storage: entries in the instance dict, `__slots__` members,
    and the data descriptors a `property` compiles to
    """
    found = set()
    try:
        found.update(vars(obj))
    except BaseException:
        pass
    for klass in getattr(type(obj), '__mro__', ()):
        for name in slots(klass):
            found.add(name)
        try:
            members = list(vars(klass).items())
        except BaseException:
            members = []
        for name, value in members:
            kind = type(value)
            if hasattr(kind, '__get__') and hasattr(kind, '__set__'):
                found.add(name)
    return found


for name in sorted(vars(m)):
    cls = vars(m)[name]
    if not isinstance(cls, type) or getattr(cls, '__module__', None) not in SELF:
        continue
    # a class that cannot be built has nothing to change, and why it cannot is
    # `isoconstruct`'s answer rather than this rung's
    try:
        signal.alarm(BOUND)
        try:
            obj = cls()
        finally:
            signal.alarm(0)
    except BaseException:
        continue
    names = [member for member in sorted(fields(obj))
             if not (member.startswith('__') and member.endswith('__'))
             and member not in BY_DESIGN
             and not hasattr(object, member)]
    for member in names[:LIMIT]:
        print('@PLAN\t%s\tmutate\t%s' % (name, member), flush=True)
    print('@PLAN\t%s\tnew\t-' % name, flush=True)
PYEOF

cat > "$SWEEP_ROOT/probe.py" <<'PYEOF'
"""construct each class the plan names, then change it the way a program would"""

import importlib
import os
import signal
import sys

from sweepcanon import Canon

MOD = os.environ['SWEEP_MOD']
SELF = (MOD, MOD.rpartition('.')[2])
BOUND = int(os.environ['SWEEP_PROBE_BOUND'])

# the values written in. they are spelled so that no attribute could already hold one:
# a probe whose sentinel matched the value already there would read back the same text
# before and after and report a working write as a no-op
SET1 = '<isomutate-1>'
SET2 = '<isomutate-2>'
NEW = 'isomutate_new'


def another(value):
    """a fresh value of the attribute's own type, or nothing when one cannot be made

    the type is read off the value the attribute is already holding, because that is the
    only evidence a probe has about what the attribute will accept. calling that type with
    no arguments is the one construction that needs no knowledge of the class; a type that
    wants arguments declines here rather than having them guessed at

    nothing comes back when no value could be made, and the caller falls back to a sentinel
    rather than being handed a value the type never produced
    """
    kind = type(value)
    try:
        signal.alarm(BOUND)
        try:
            made = kind()
        finally:
            signal.alarm(0)
    except BaseException:
        return None
    # a `__new__` is free to answer anything at all, and the only value known to be
    # acceptable here is one of the type the attribute is already holding
    return made if type(made) is kind else None


def writes(value):
    """two values to assign, and whether seeing one come back is proof of a store

    a compiled class publishes its fields as getset descriptors that check what is
    assigned to them; its twin keeps the same field in an instance dict, which checks
    nothing. so writing one sentinel string into every attribute makes a compiled class
    *correctly* refuse a field holding an `int` or an instance of some class of its own,
    and the rung comes away knowing only that the field is typed — never whether a value
    it does accept is actually stored. that is exactly the probe this rung must not have:
    it cannot tell a working leg from a dead one, because both refuse. so the value
    written is of the attribute's own type wherever one can be had, and the sentinel is
    what is left when none can

    the third answer is why this is not just a pair. a value of the attribute's own type
    usually renders exactly like the one it replaced — a class with no `__repr__` prints
    `<... object at 0xX>`, and the address is scrubbed — so a probe judging by text alone
    would read a working write as a no-op, which is the one reading this rung exists to
    refuse. a value made here is an object nothing else in the process holds, so a read
    that answers it *is* the proof, and the flag says so. for a sentinel or a number the
    text is the only evidence and identity must not be consulted: a `bool`'s second write
    is the original value put back, and `is` would call that a store either way

    a string sentinel goes in *front* of an existing string rather than after it. the
    renderer caps a value at 200 characters, and a difference past the cap is invisible —
    appending to a long string would render identically before and after and report a
    working write as a no-op
    """
    if value is None:
        # an attribute holding `None` is the one case where the type says there is nothing
        # else to write: a compiled slot inferred from `self.x = None` accepts `None` and
        # refuses everything, correctly. so `None` goes back in, and neither leg can show
        # that it did anything — `eff` simply does not name the write, on both legs alike.
        # that is the rung declining, which is the right answer when no value exists that
        # is both acceptable and observable, and it is not the same as looking away: a leg
        # that *refused* `None` here would still differ from one that took it
        return None, None, False
    kind = type(value)
    if kind is bool:
        # two values is all a bool has, so the second write puts the original back and
        # the third step cannot show a change. that is a property of the type rather
        # than of either leg, and both legs report it identically
        return not value, value, False
    if kind is int:
        return value + 1, value + 2, False
    if kind is float:
        return value + 1.0, value + 2.0, False
    if kind is str:
        return SET1 + value, SET2 + value, False
    if kind is bytes:
        return b'<isomutate-1>' + value, b'<isomutate-2>' + value, False
    # the second is only attempted when the first worked. a type that declines costs the
    # probe its whole two-second bound, and asking a second time buys nothing
    first = another(value)
    second = another(value) if first is not None else None
    # neither value may be the one already there: assigning a value back over itself changes
    # nothing a later read could show, so a dropped write would look exactly like a working
    # one. the identity claim needs one thing more — an interned or cached type hands back
    # the same object twice, and `is` cannot then say which of the two writes a read is
    # answering, so those go back to being judged on their text as everything else is
    if (first is not None and second is not None
            and first is not value and second is not value):
        return first, second, second is not first
    return SET1, SET2, False


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
    `repr` and inside every AttributeError a deletion raises, which is most of what this
    rung prints — left alone it would be the only thing ever reported. `isosurface`
    compares `__module__` outright and reports the defect once per class, which is where
    it belongs. here both spellings are removed, by exact class name rather than by
    pattern: a blanket `m.` substitution would also rewrite `m.py` inside a message
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


def build(m, failures, name):
    """a fresh instance for every probe

    `isoinstance` makes one per class and shares it, which is right for a rung that only
    reads. it is wrong here: the probes write, so a shared object would carry the previous
    probe's changes into the next one and a single divergence would spread across every
    attribute after it. the *failure* is remembered instead of the object, so a class that
    cannot be built is not attempted once per attribute
    """
    if name in failures:
        return None, failures[name]
    cls = getattr(m, name, None)
    if not isinstance(cls, type):
        failures[name] = '<no such class>'
        return None, failures[name]
    try:
        signal.alarm(BOUND)
        try:
            return cls(), None
        finally:
            signal.alarm(0)
    except BaseException as error:
        # a construction that fails is not this rung's finding — `isoconstruct` compares
        # that outcome, wording and all — so only the fact is recorded
        failures[name] = '<unconstructed %s>' % type(error).__name__
        return None, failures[name]


def read(canon, obj, name):
    """what the attribute answers now — as text, and as the value itself

    every write in this file is judged by this and by nothing else. a write that returned
    without raising has not been shown to have stored anything, and the whole reason this
    rung exists is that a leg which quietly stores nothing looks exactly like a leg that
    works if you only watch the write

    the value comes back beside the text because `writes` needs its type to choose what to
    assign and `moved` needs its identity to recognise it coming back. it is read once and
    passed along rather than read again: an attribute may be a property whose body has an
    effect, and a probe that read it twice would be measuring its own second call
    """
    try:
        signal.alarm(BOUND)
        try:
            value = getattr(obj, name)
        finally:
            signal.alarm(0)
    except BaseException as error:
        return canon.raised(error), None, False
    return canon.render(value), value, True


def act(canon, change):
    """run one mutation and say what it did, not what it returned"""
    try:
        signal.alarm(BOUND)
        try:
            change()
        finally:
            signal.alarm(0)
    except BaseException as error:
        return canon.raised(error)
    return 'ok'


def has_dict(obj):
    """whether this object has anywhere to put a name its class did not give it

    asked of the object rather than read out of an error message. cpython's wording for
    the refusal happens to name `__dict__`, but matching on that would tie the rung to a
    sentence the interpreter is free to rewrite, and would answer wrongly for any other
    build that phrases it differently
    """
    try:
        vars(obj)
    except BaseException:
        return False
    return True


def moved(now, before, written):
    """whether a read answers something the read before it did not

    the text decides first, because the text is what the two legs are compared on and what
    a reader of the diff sees. identity is consulted only for `written` — a value `writes`
    made for this probe, which nothing else in the process holds — and only because two
    instances of a class with no `__repr__` render alike: without it an attribute holding
    such an object would look untouched however well the store worked. passing nothing
    there says "judge this step on the text", which is what a deletion and a sentinel both
    want
    """
    if now[0] != before[0]:
        return True
    return written is not None and now[1] is written


def mutate(canon, obj, name):
    """assign, read, delete, read, assign again, read

    the three `eff` flags are the rung's evidence that anything happened at all: `w` says
    the first assignment changed what the attribute answers, `d` says the deletion did,
    `r` says the second assignment did. an absent flag is not by itself a defect — a
    property with no setter is entitled to ignore a write — but the flags are compared
    between the legs like everything else, so a compiled leg that drops a write its twin
    performs differs here rather than agreeing
    """
    was = read(canon, obj, name)
    first, second, byid = writes(was[1]) if was[2] else (SET1, SET2, False)
    wrote = act(canon, lambda: setattr(obj, name, first))
    got = read(canon, obj, name)
    removed = act(canon, lambda: delattr(obj, name))
    gone = read(canon, obj, name)
    again = act(canon, lambda: setattr(obj, name, second))
    back = read(canon, obj, name)
    eff = ''
    if moved(got, was, first if byid else None):
        eff += 'w'
    # a deletion puts no value of its own anywhere to be recognised by, so it is judged on
    # the text alone: what an attribute answers once deleted is a different member or an
    # AttributeError, and either of those reads differently
    if moved(gone, got, None):
        eff += 'd'
    if moved(back, gone, second if byid else None):
        eff += 'r'
    return ' ~ '.join((
        'pre=' + was[0], 'set=' + wrote, 'got=' + got[0], 'del=' + removed,
        'gone=' + gone[0], 'set2=' + again, 'back=' + back[0], 'eff=' + (eff or '-'),
    ))


def fresh(canon, obj):
    """set a name the class never had

    three answers, and the middle one is the reason the read-back is here. `stored` is
    what an object with an instance dict does. `refused-nodict` is what an object without
    one does, and an emitted class is always in that case — so the pair (`stored`,
    `refused-nodict`) is the layout rather than a defect, and the rung folds it away by
    counting it. `dropped` is a set that succeeded and stored nothing, which no correct
    object does and which a rung watching only the write would have scored as success
    """
    outcome = act(canon, lambda: setattr(obj, NEW, SET1))
    if outcome != 'ok':
        if not has_dict(obj):
            return 'new=refused-nodict ~ eff=-'
        return 'new=refused ~ %s ~ eff=-' % outcome
    value = read(canon, obj, NEW)[0]
    if value == canon.render(SET1):
        return 'new=stored ~ eff=w'
    return 'new=dropped ~ got=%s ~ eff=-' % value


def main():
    signal.signal(signal.SIGALRM, _ring)
    m = load()
    canon = Canon(aliases(m))
    with open('plan.txt') as handle:
        plan = [line.rstrip('\n').split('\t') for line in handle if line.strip()]
    start = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    failures = {}
    for index in range(start, len(plan)):
        name, kind, member = plan[index]
        obj, failure = build(m, failures, name)
        if failure is not None:
            answer = '%s ~ eff=-' % failure
        elif kind == 'mutate':
            answer = mutate(canon, obj, member)
        elif kind == 'new':
            answer = fresh(canon, obj)
        else:
            answer = '<unknown probe %s> ~ eff=-' % kind
        print('@P%d\t%s\t%s\t%s\t%s' % (index, name, kind, member, answer), flush=True)


# `multiprocessing` starts a worker by re-running this interpreter and importing this
# file, as `__mp_main__`. a constructor that makes a pool would otherwise have every
# worker import the module and mutate every class in it again — pool included
if __name__ == '__main__':
    main()
PYEOF

# run one leg to completion, restarting past whatever killed it
#
# a probe that segfaults truncates the leg, and a truncated leg reads as an ordinary
# difference. the death is written as a line of its own carrying the index it happened at,
# so it both survives into the comparison and consumes the probe that caused it
#
# only the probe lines and an import failure are kept. a leg also writes whatever the
# module wrote — a traceback, an `Exception ignored in __del__` — and those lines carry
# none of the four fields the comparison pairs rows by, so keeping them would pair rows
# that are not the same probe
leg() {
  local dir="$1" start=0 attempts=0 text="" out="" status=0 done_lines=0
  while [ "$attempts" -lt 40 ]; do
    attempts=$((attempts+1))
    # through `sweep_capture` rather than a command substitution around the probe: this
    # rung's probes leak `multiprocessing` workers that inherit the leg's stdout, and a
    # pipe held open by one of those never reaches end of file. the reasons are with the
    # helper, in `sweeplib.sh`
    sweep_capture "$dir" "$PY" probe.py "$start"; status=$SWEEP_CAPTURE_STATUS
    out=$(printf '%s' "$SWEEP_CAPTURE_TEXT" | grep -E '^@(P[0-9]+|IMPORT-FAILED)\t')
    # *both* legs' directories are replaced, in both legs, and the compiled one first
    # because it sits inside the interpreted one. scrubbing only the leg's own directory
    # would leave the other leg's spelling of the same idea standing
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

# the deaths one leg suffered: which probe killed it, and what killed it
#
# a rung that reported only that *something* died leaves a reader to assume it was ours,
# and for the one crashing module in this corpus it is not. `ssl.py`'s
# `del c.keylog_filename` segfaults stock cpython — 3.9, 3.13 and 3.14 alike, with no
# compiled module anywhere near it — so the compiled leg dies because it faithfully does
# what its twin does, and two legs dying the same death is the twins agreeing
#
# "the same death" is the plan index *and* the signal, not the signal alone. a compiled leg
# that segfaults thirty probes before its twin does has agreed about nothing — it died
# early, and that is the finding — but its signal matches, so a rung comparing signals
# would have called it a match. the index is comparable because both legs run the one plan
# and `leg` numbers a death by the probe it consumed
#
# a death row is recognised by all three of its fixed fields rather than by `DIED` alone: a
# class in the corpus is free to be called that, and its probe rows would otherwise be read
# as deaths and cost the module its verdict
deaths() {
  printf '%s' "$1" | awk -F'\t' '$2 == "DIED" && $4 == "-" && $5 == "died" { print $1 "@" $3 }' \
    | LC_ALL=C sort -u | paste -sd, -
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

# the keys whose two answers differ only in the way the emitted-class layout guarantees
#
# an emitted class may have no instance dict, so a name the class never had has nowhere to
# go — a class declaring `__slots__`, a class whose layout is not its own, or any class
# below 3.13, where the managed dict is not available.
# every class in the corpus carries that, and left in the diff it would be the only thing
# this rung ever said. the direction matters and is the whole reason this is a pair test
# rather than a rule about the `new` probe: the fold fires when the interpreted leg stored
# it and the compiled leg refused, and never the other way round, so a compiled class that
# accepts a name its twin refuses is still reported
bydesign() {
  LC_ALL=C join -t $'\t' -j 1 -o 0,1.2,2.2 \
    <(printf '%s' "$1" | awk -F'\t' 'NF {print $1 "\x01" $2 "\x01" $3 "\x01" $4 "\t" $5}' | LC_ALL=C sort -t $'\t' -k1,1) \
    <(printf '%s' "$2" | awk -F'\t' 'NF {print $1 "\x01" $2 "\x01" $3 "\x01" $4 "\t" $5}' | LC_ALL=C sort -t $'\t' -k1,1) \
    | awk -F'\t' '$2 ~ /^new=stored/ && $3 ~ /^new=refused-nodict/ { print $1 }' \
    | LC_ALL=C sort -u
}

# take the by-design pairs out of `$i` and `$c`, and say how many there were
#
# it runs before the two are compared rather than after, because the comparison is what
# decides whether a third pair of runs is needed — folding afterwards would buy a third
# run for almost every module in the corpus and change no verdict
fold() {
  local dropped
  dropped=$(bydesign "$i" "$c")
  nodict=$(printf '%s' "$dropped" | grep -c '')
  [ "$nodict" -eq 0 ] && return 0
  steady=$(LC_ALL=C comm -23 <(printf '%s' "$steady") <(printf '%s' "$dropped"))
  i=$(keep "$1" "$steady"); c=$(keep "$2" "$steady")
  return 0
}

# which step of the probe the two legs answered differently
#
# a rung that said only `DIFFERS` would leave a reader to open five hundred diffs to find
# out that they all say one thing. naming the steps on the module's own row separates the
# architectural difference every class carries from a novel one: `steps=del,gone` is the
# emitted-class layout refusing a deletion, and any other step is something else
#
# the answer is split on the separator the probe joins its steps with. a rendered value is
# free to contain that text, and one that did would yield a step name that is really a
# fragment of a value — so this names where to look, and the diff written under the row
# stays the evidence
steps() {
  LC_ALL=C join -t $'\t' -j 1 -o 0,1.2,2.2 \
    <(printf '%s' "$1" | awk -F'\t' 'NF {print $1 "\x01" $2 "\x01" $3 "\x01" $4 "\t" $5}' | LC_ALL=C sort -t $'\t' -k1,1) \
    <(printf '%s' "$2" | awk -F'\t' 'NF {print $1 "\x01" $2 "\x01" $3 "\x01" $4 "\t" $5}' | LC_ALL=C sort -t $'\t' -k1,1) \
    | awk -F'\t' '
      function nameof(text,   at) { at = index(text, "="); return at ? substr(text, 1, at - 1) : text }
      {
        split("", left); split("", right)
        n = split($2, a, " ~ "); for (k = 1; k <= n; k++) left[nameof(a[k])] = a[k]
        n = split($3, b, " ~ "); for (k = 1; k <= n; k++) right[nameof(b[k])] = b[k]
        for (k in left) if (!(k in right) || left[k] != right[k]) print k
        for (k in right) if (!(k in left)) print k
      }' \
    | LC_ALL=C sort -u
}

# how many mutations were demonstrably carried out, across the whole corpus
#
# this is the rung's own liveness figure, and it is here because a detector that never
# says no is not detecting. if a change to the staging, the plan or the probe left every
# leg constructing nothing and mutating nothing, every module would come back `same` and
# the summary would read like a clean run. so the total is reported, and a run that
# changed nothing anywhere raises an alarm rather than a verdict
carried=0

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
  # the plan is drawn up on the interpreted leg and handed to both. importing a module
  # runs it, and a module may print — `this.py` prints the whole zen of python — so the
  # plan is read off marked lines rather than off the output
  #
  # it is drawn up *twice*, and only what both attempts named is kept. the plan is built
  # by constructing every class, under the same two-second bound a probe gets, and a
  # constructor that races that bound is in the plan on one run and not the next:
  # `multiprocessing/pool.py` starts worker processes in its constructor and moved between
  # 37 and 25 entries between two runs of this rung on one commit. because a probe is
  # keyed by its position in the plan, a plan that is one entry shorter renumbers every
  # probe after it — so a wobble of one construction rewrites the whole module's output
  # and reads as if the compiler had changed
  #
  # this is the same rule the rung already applies to answers, applied one level up: what
  # does not repeat is not evidence. `grep -x -F -f` keeps the first attempt's order, so
  # the surviving plan is the first attempt's list with the unrepeatable entries removed
  #
  # the plan is read through `sweep_capture` for the same reason the probes are: it builds
  # every class in the module, so it starts — and leaks — whatever a constructor starts.
  # `multiprocessing/pool.py` is where the hang was seen, and its pool is made here before
  # any probe runs
  sweep_capture "$SWEEP_RUN_I" "$PY" plan.py; said=$SWEEP_CAPTURE_TEXT
  sweep_capture "$SWEEP_RUN_I" "$PY" plan.py; again=$SWEEP_CAPTURE_TEXT
  if printf '%s\n%s' "$said" "$again" | grep -q '^@IMPORT-FAILED'$'\t'; then
    printf '%s\timport-failed\t%s\n' "$b" \
      "$(printf '%s\n%s' "$said" "$again" | grep '^@IMPORT-FAILED'$'\t' | head -1)" >> "$OUT"; continue
  fi
  plan=$(LC_ALL=C grep -x -F -f \
    <(printf '%s' "$again" | grep '^@PLAN'$'\t' | cut -f2-) \
    <(printf '%s' "$said" | grep '^@PLAN'$'\t' | cut -f2-))
  if [ -z "$plan" ]; then
    # no class this module owns could be built, so there was no object to change. a rung
    # that scored this `same` would be reporting agreement it never looked for
    printf '%s\tnothing-to-probe\n' "$b" >> "$OUT"; continue
  fi
  printf '%s\n' "$plan" > "$SWEEP_RUN_I/plan.txt"
  printf '%s\n' "$plan" > "$SWEEP_RUN_C/plan.txt"
  i1=$(leg "$SWEEP_RUN_I"); i2=$(leg "$SWEEP_RUN_I")
  c1=$(leg "$SWEEP_RUN_C"); c2=$(leg "$SWEEP_RUN_C")
  steady=$(comm -12 <(keys "$(stable "$i1" "$i2")") <(keys "$(stable "$c1" "$c2")"))
  # counted before the fold, because "how much of the plan held still" and "how much of
  # what held still was worth diffing" are two different questions. reading the second as
  # the first is what made a module whose only probe was the by-design pair report
  # `nothing-stable` — a claim about instability that never happened
  settled=$(printf '%s' "$steady" | grep -c '')
  i=$(keep "$i1" "$steady"); c=$(keep "$c1" "$steady")
  nodict=0
  fold "$i1" "$c1"
  if [ "$i" != "$c" ]; then
    # two runs are enough to expose a clock or a random draw, and not enough to expose a
    # probe with only a few possible answers. a difference is confirmed with a third run
    # of each leg before it is reported, which costs a run only on the modules that were
    # going to be looked at by hand anyway
    i3=$(leg "$SWEEP_RUN_I"); c3=$(leg "$SWEEP_RUN_C")
    steady=$(comm -12 <(keys "$(stable "$(stable "$i1" "$i2")" "$i3")") \
                      <(keys "$(stable "$(stable "$c1" "$c2")" "$c3")"))
    settled=$(printf '%s' "$steady" | grep -c '')
    i=$(keep "$i1" "$steady"); c=$(keep "$c1" "$steady")
    fold "$i1" "$c1"
  else
    i3=""; c3=""
  fi
  # a leg killed by an alarm has not answered, and an empty answer must never be read as
  # agreement — so this is decided before any verdict is written.
  #
  # a *death* is decided before even that, the same way `isoconstruct` and `isoinstance`
  # decide it: the restart loop steps past one and carries on, so a leg that crashed can
  # still reach a probe that outruns its bound, and this test would then match and write a
  # `timed-out` row calling no `sweep_note_death` — leaving `sweep_end`'s cross-check
  # nothing to disagree with
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
  compared=$((settled - nodict))
  unstable=$(( $(printf '%s' "$plan" | grep -c '') - settled ))
  # a probe whose `eff` names a flag changed what a later read answered. counting them on
  # the interpreted leg is what separates "the two legs agree about mutation" from "the
  # two legs agree that nothing happened"
  mutated=$(printf '%s' "$i" | grep -c 'eff=[wdr]')
  carried=$((carried + mutated))
  detail="$compared"$'\t'"unstable=$unstable"$'\t'"mutated=$mutated"$'\t'"nodict=$nodict"
  idied=$(deaths "$i1$i2$i3"); cdied=$(deaths "$c1$c2$c3")
  # a death is evidence whatever verdict it ends up under, so it is recorded once here
  # rather than in each branch. `sweep_end` cross-checks this against every module scored
  # `same`, and the branches below are ordered so that a module which lost a leg never is
  [ -n "$idied$cdied" ] && sweep_note_death "$b"
  if [ "$idied" != "$cdied" ]; then
    # one leg was killed where the other lived, or the two were killed at different probes
    # or by different signals. that is the finding this rung is for: a compiled module that
    # dies where its twin lives — or lives where its twin dies — is not its twin
    { printf '%s\tCRASHED\t%s\tinterpreted=%s\tcompiled=%s\n' \
        "$b" "$detail" "${idied:--}" "${cdied:--}"
      diff <(printf '%s\n' "$i") <(printf '%s\n' "$c") | awk -v b="$b" '{print b "\t| " $0}'
    } >> "$OUT"
  elif [ "$settled" -eq 0 ]; then
    # every probe moved on its own, so the two legs were never compared
    printf '%s\tnothing-stable\t%s\n' "$b" "$(printf '%s' "$plan" | grep -c '')" >> "$OUT"
  elif [ "$i" != "$c" ]; then
    changed=$(steps "$i" "$c")
    printf '%s' "$changed" | grep . >> "$SWEEP_ROOT/steps"
    named=$(printf '%s' "$changed" | paste -sd, -)
    { printf '%s\tDIFFERS\t%s\tsteps=%s\n' "$b" "$detail" "${named:--}"
      diff <(printf '%s\n' "$i") <(printf '%s\n' "$c") | awk -v b="$b" '{print b "\t| " $0}'
    } >> "$OUT"
  elif [ -n "$idied" ]; then
    # both legs were killed at the same probe by the same signal, and everything either of
    # them did answer agreed. the crash belongs to the code both of them run, and a
    # compiled module that reproduces its twin's crash is doing what it is asked to. it is
    # still not `same`: neither leg answered the probe that killed it, so this says what
    # was compared and what was not, rather than claiming agreement about all of it
    printf '%s\tboth-died\t%s\tdied=%s\n' "$b" "$detail" "$idied" >> "$OUT"
  elif [ "$mutated" -eq 0 ]; then
    # the two legs agree, and they agree about an object neither of them changed. that is
    # the reading this rung exists to refuse: it is what a pair of legs that both quietly
    # dropped every write would produce, and it is indistinguishable from correctness
    # unless it is given a name of its own
    printf '%s\tnothing-mutated\t%s\n' "$b" "$detail" >> "$OUT"
  else
    printf '%s\tsame\t%s\n' "$b" "$detail" >> "$OUT"
  fi
done
if [ "$carried" -eq 0 ]; then
  sweep_alarm isomutate 'carried out no mutation anywhere in the corpus — every verdict in this run is about an object nothing was done to'
fi
sweep_end || exit 1
# which steps the corpus differed in, most common first. one uniform architectural
# difference and one real defect both read as `DIFFERS` on a module's row, and this is
# what tells them apart at a glance
if [ -s "$SWEEP_ROOT/steps" ]; then
  # the count is taken off the front of the line rather than read as a field: a row whose
  # answer never had a `name=` at all — a class one leg could not construct — is named by
  # its whole text, spaces included, and reading field two would print half of it
  echo "steps: $(LC_ALL=C sort "$SWEEP_ROOT/steps" | uniq -c | sort -rn \
    | awk '{ count = $1; $1 = ""; sub(/^ +/, ""); printf "%s=%s   ", $0, count }')"
fi
echo "walked: $(cut -f1 "$OUT" | LC_ALL=C sort -u | grep -c .)   exercised: $(grep -cE $'\t(same|DIFFERS)\t' "$OUT")   differing: $(grep -c $'\tDIFFERS\t' "$OUT")   crashed: $(grep -c $'\tCRASHED\t' "$OUT")   both-died: $(grep -c $'\tboth-died\t' "$OUT")   mutations: $carried   nothing-mutated: $(grep -c $'\tnothing-mutated\t' "$OUT")   nothing-stable: $(grep -c $'\tnothing-stable' "$OUT")   nothing-to-probe: $(grep -c $'\tnothing-to-probe' "$OUT")   timed-out: $(grep -c $'\ttimed-out' "$OUT")   import-failed: $(grep -c $'\timport-failed' "$OUT")   compiled-import-failed: $(grep -c $'\tcompiled-import-failed' "$OUT")   no-artifact: $(grep -c $'\tno-artifact' "$OUT")"

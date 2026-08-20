#!/bin/bash
# the one module list every sweep walks, and the two things every sweep does with it
#
# each sweep used to carry its own glob. four of the five globbed `$LIB/*.py`, which is
# the 152 top-level modules; only `buildsweep.sh` descended, which is 559. so every
# subpackage module — `urllib/`, `email/`, `json/`, `asyncio/`, `xml/` — was compiled
# and then never imported, constructed, subclassed or surface-compared. the list lives
# here now so widening it once widens it everywhere

# a sweep imports every module in the corpus, so any module body that reaches out of the
# process does it once per rung per leg. `antigravity` opens https://xkcd.com/353/ through
# `webbrowser`, which is the stdlib's own easter egg and not a defect to route around —
# so neutralise the browser rather than drop the module, and every future module that
# reaches for one is covered too. `webbrowser` builds a `GenericBrowser` from $BROWSER and
# runs it with the url; `true` accepts the argument and does nothing. if it were somehow
# missing, `webbrowser.open` catches the OSError and returns False, so this cannot fail
# open. both legs get the same value, so nothing the sweep compares moves
export BROWSER=true

# the stdlib of the interpreter that is going to run the probes
sweep_lib() {
  "$1" -c 'import sysconfig;print(sysconfig.get_paths()["stdlib"])'
}

# one module per line, as a path relative to $LIB. extra arguments override the walk,
# so a caller can name `urllib/parse.py` and get just that
#
# the filter matches the relative path rather than the absolute one: a checkout parked
# under a directory called `test` would otherwise silently empty the whole sweep
#
# `__main__.py` is dropped as a category rather than by name. every rung's premise is
# "import this and compare it against its twin", and an entry point's contract is "run
# me" — importing one executes the program. seven of the nine are argparse plumbing
# behind a `__name__` guard and cost nothing to lose; the other two are why this exists:
# `tkinter/__main__.py` opens a Tk window and enters `mainloop()`, and `venv/__main__.py`
# builds a virtualenv and then calls `sys.exit()`
sweep_modules() {
  local lib="$1"; shift
  if [ "$#" -gt 0 ]; then printf '%s\n' "$@"; return 0; fi
  (cd "$lib" && find . -name '*.py' | sed 's|^\./||') \
    | grep -v -E 'test|__pycache__|site-packages|lib2to3|idlelib' \
    | grep -v -E '(^|/)__main__\.py$' \
    | LC_ALL=C sort
}

# the modules whose type inference does not terminate today
#
# they are still walked — a rung that skipped them would stop noticing the day they
# are fixed — but on a short leash, because the full bound is 180s and re-proving a
# known hang once per rung costs a quarter hour of every sweep cycle
#
# it is empty. three were on this list on the strength of "produced no artefact",
# and the stale-entry alarm caught all three on its first real run: on
# `origin/main` as much as here, `bdb.py` overflows the stack in 5s, and
# `pickletools.py` and `profile.py` finish in 5s with a salsa panic reported as a
# diagnostic. none of those is a hang, and none of them needs a leash
#
# `ast.py` came off when the return type it grew a constructor a round was bounded;
# it now compiles in 2s. `turtle.py` came off when a tuple whose elements were the
# cycle's own marker stopped alternating between the widened and the marked form of
# itself; it now compiles in 24s
#
# `${VAR-default}` rather than `${VAR:-default}`: an explicitly empty list is a
# caller saying "treat every hang as new", which is how this gets tested
BY_KNOWN_HANGS=${BY_KNOWN_HANGS-""}

# compile one staged module, bounded
#
# a module that has never terminated needs only long enough to confirm it still does
# not; anything else gets the full bound. the two cases that must never pass quietly:
# a module *not* on the list that hits its bound is a new hang, and a module *on* the
# list that finishes means the list is stale. both say so on stderr and land in
# `$OUT.alarms`, because a rung that swallows either stops being evidence
#
# SWEEP_BOUND is seconds (0 disables); SWEEP_BOUND_KNOWN is the short leash
sweep_compile() {
  local name="$1" dir="$2" py="$3" by="$4"; shift 4
  local bound=${SWEEP_BOUND:-180} known=0
  case " $BY_KNOWN_HANGS " in *" $name "*) known=1; bound=${SWEEP_BOUND_KNOWN:-15} ;; esac

  if [ "$bound" -eq 0 ]; then
    (cd "$dir" && PYTHON="$py" "$by" compile "$SWEEP_SRC" -o o "$@" >/dev/null 2>&1)
    return $?
  fi

  (cd "$dir" && PYTHON="$py" "$by" compile "$SWEEP_SRC" -o o "$@" >/dev/null 2>&1) &
  local pid=$!
  # the watchdog gets its own descriptors, and its `sleep` is killed with it.
  # killing only the subshell leaves the `sleep` orphaned holding the inherited
  # stdout, so a rung that finished in a second did not close its pipe until the
  # bound elapsed — a reader saw the summary 180s late for no reason at all
  { sleep "$bound"; kill -9 "$pid" 2>/dev/null; } >/dev/null 2>&1 &
  local killer=$!
  wait "$pid" 2>/dev/null
  local rc=$?
  pkill -9 -P "$killer" 2>/dev/null
  kill -9 "$killer" 2>/dev/null
  wait "$killer" 2>/dev/null

  # 137 is the kill; anything else is the compiler's own answer
  if [ "$rc" -eq 137 ]; then
    if [ "$known" -eq 0 ]; then
      sweep_alarm "$name" "did not finish in ${bound}s — a hang that is not on BY_KNOWN_HANGS"
    fi
  elif [ "$known" -eq 1 ]; then
    sweep_alarm "$name" "finished in under ${bound}s — it is on BY_KNOWN_HANGS and should come off"
  fi
  return $rc
}

# an alarm is not a result: it says the sweep itself learned something it cannot
# record in a row, so it goes where it will be seen rather than into the tsv
sweep_alarm() {
  printf '!! sweep alarm: %s %s\n' "$1" "$2" >&2
  [ -n "${OUT:-}" ] && printf '%s\t%s\n' "$1" "$2" >> "$OUT.alarms"
  return 0
}

# the package every staged tree sits inside
#
# a package member has to be staged *in its package* or its relative imports have
# nothing to resolve against. a copy laid out under the package's own name would be
# imported in place of the interpreter's own, and that goes wrong two ways: `encodings`
# is already in `sys.modules` before a probe starts, so `import encodings.m` searches
# the real stdlib and finds nothing; and a copy of `re` or `importlib` first on the path
# answers every later import in the process, the driver's as much as the module's. one
# outer package nobody else names keeps the copy reachable and the interpreter's own
# stdlib intact. it costs the module only a prefix on its `__name__`
SWEEP_WRAP=by_stage

# nothing the sweep runs benefits from a cache, and a `.pyc` is one more file that
# could answer in place of the one the sweep staged
export PYTHONDONTWRITEBYTECODE=1

# how long a leg is given to import the module, in seconds
#
# it is there to stop a module body that never returns, and thirty seconds is far more
# than any of them needs — but not more than the *machine* can take. macos scans a
# freshly written `.so` the first time it is loaded, and every module in the sweep
# builds a new one: on an idle machine that is under a second, and on a loaded one it
# has been measured at twenty-five, with the process asleep for all of it. the same
# extension then loads in 0.04s. so on a busy machine raise this rather than reading the
# timeouts as results
export SWEEP_IMPORT_BOUND=${SWEEP_IMPORT_BOUND:-30}

# true when every directory above the module is a package
#
# a directory holding python files is not necessarily one: `config-3.13-darwin` has no
# `__init__.py`, and its name is not even an identifier. a module under one of those is
# reached by path rather than by import and is staged on its own, which is what it is
sweep_in_package() {
  local lib="$1" rel="$2" prefix=""
  case "$rel" in */*) ;; *) return 1 ;; esac
  local rest="${rel%/*}"
  while [ -n "$rest" ]; do
    prefix="$prefix${rest%%/*}"
    [ -f "$lib/$prefix/__init__.py" ] || return 1
    case "$rest" in
      */*) rest="${rest#*/}"; prefix="$prefix/" ;;
      *) rest="" ;;
    esac
  done
  return 0
}

# lay one module out as a project of its own, and say where the two legs run
#
# the module is `m.py` wherever it came from, so both legs import the same name and
# `m.c`, `m.annotated` and `m*.so` keep theirs.
#
# a top-level module is staged alone, as it always was. a package member is staged in a
# copy of its whole package, so `from . import x` resolves — for the compiler as much as
# for the two legs. the siblings are on disk but kept out of the project's *file set*:
# `by compile` compiles every source in the project it is run from, and building all 122
# modules of `encodings` once per member of `encodings` is fifteen thousand builds
#
# the whole package rather than the part the module imports: which part that is, is the
# same import graph the sweep exists to test, and a stage that had to be right about it
# would fail in the direction that hides defects
#
# sets, for the caller:
#   SWEEP_MOD    the dotted module name both legs import
#   SWEEP_SRC    the staged source, relative to $dir
#   SWEEP_RUN_I  the directory the interpreted leg runs from
#   SWEEP_RUN_C  the directory the compiled leg runs from
sweep_stage() {
  local dir="$1" lib="$2" rel="$3" limit=""
  rm -rf "$dir" "$dir/o"; mkdir -p "$dir"
  export SWEEP_RUN_I="$dir" SWEEP_RUN_C="$dir/o"
  if ! sweep_in_package "$lib" "$rel"; then
    export SWEEP_MOD=m SWEEP_SRC=m.py
    cp "$lib/$rel" "$dir/m.py"
  else
    local pkg="${rel%%/*}" sub="${rel%/*}" dotted
    dotted=$(printf '%s' "$sub" | tr / .)
    export SWEEP_SRC="$SWEEP_WRAP/$sub/m.py"
    export SWEEP_MOD="$SWEEP_WRAP.$dotted.m"
    mkdir -p "$dir/$SWEEP_WRAP"
    : > "$dir/$SWEEP_WRAP/__init__.py"
    cp -R "$lib/$pkg" "$dir/$SWEEP_WRAP/$pkg"
    find "$dir/$SWEEP_WRAP" -name __pycache__ -type d -exec rm -rf {} + 2>/dev/null
    cp "$lib/$rel" "$dir/$SWEEP_SRC"
    # a few package names are in ty's default `src.exclude` — `venv` above all — so a
    # member of one is dropped before anything can compile it, and the rung reports
    # `no-artifact` for a module that is perfectly fine. the negation re-includes the
    # directory; it is written per-stage rather than as a blanket rule so the exclusion
    # still holds for everything the sweep did not deliberately stage
    limit=$(printf '[tool.ty.src]\ninclude=["%s"]\nexclude=["!**/%s/"]\n' \
      "$SWEEP_SRC" "${rel%%/*}")
  fi
  printf '[project]\nname="s"\nversion="0"\nrequires-python=">=3.13"\n%s' "$limit" \
    > "$dir/pyproject.toml"
}

# where the build wrote the staged module's artefacts
#
# `by compile` lays its output out as the *module* tree, so a package member's `m.c`,
# `m.annotated` and `m*.so` sit at the member's own place under `o` rather than at the
# top of it — which is where the staged source sits within the project
sweep_out_dir() {
  local rel="${SWEEP_SRC%/*}"
  if [ "$rel" = "$SWEEP_SRC" ]; then printf '%s/o' "$1"; else printf '%s/o/%s' "$1" "$rel"; fi
}

# put the twin's sources around the extension the compiled leg will import
#
# a package member's compiled leg needs the tree its twin has, and the build has already
# left the extension in the module's place inside it — so the extension is set aside
# while the twin's copy of the package is laid down over the top, then put back. the
# staged `m.py` is *removed* from it: python prefers an extension to a source of the same
# name, so leaving it there would mean an extension that failed to load was replaced by
# the interpreted module — and the leg would then agree with its twin for the one reason
# that makes the whole comparison meaningless
#
# call it after `sweep_built`, before running either leg
# the extension the build left, or nothing when it left none
#
# a glob rather than `ls`: what comes back is a path this file goes on to `mv` and to hand
# to python, and `ls` would mangle any name that needed quoting. an unmatched glob stays
# literal in bash, so the `-e` is what distinguishes "no artefact" from "one named `m*.so`"
sweep_artifact() {
  local matches
  matches=("$(sweep_out_dir "$1")"/m*.so)
  [ -e "${matches[0]}" ] || return 1
  printf '%s\n' "${matches[0]}"
}

sweep_place() {
  local dir="$1" so name
  so=$(sweep_artifact "$dir") || so=""
  if [ "$SWEEP_MOD" != m ]; then
    name=${so##*/}
    mv "$so" "$dir/$name"
    rm -rf "${dir:?}/o/$SWEEP_WRAP"
    cp -R "$dir/$SWEEP_WRAP" "$dir/o/$SWEEP_WRAP"
    rm -f "$dir/o/$SWEEP_SRC"
    so="$dir/o/${SWEEP_SRC%/*}/$name"
    mv "$dir/$name" "$so"
  fi
  sweep_warm "$so"
}

# pay the operating system's first-load cost before anything is timed
#
# macos validates a freshly built dylib the first time it is loaded, and the process is
# asleep for all of it: 0.42s on an idle machine against 0.04s for every load after, and
# measured at 17.8s under contention. every module in a sweep builds a new one, so that
# cost lands inside whatever bound the rung set and comes back as a `timed-out` — which
# reads as a result and is not one. a full `isoimport` reported 26 of them; re-running
# exactly those 26 with a larger bound gave 26 exercised and 0 differing, so it was the
# operating system every time
#
# `ctypes.CDLL` loads the object without calling `PyInit_`, so this pays the validation
# without running the module body. that matters: a warm-up that imported the module would
# run its import side effects an extra time, and one that hung would hang here instead
sweep_warm() {
  local so="$1"
  [ -n "$so" ] && [ -f "$so" ] || return 0
  "$PY" -c 'import ctypes, sys; ctypes.CDLL(sys.argv[1])' "$so" >/dev/null 2>&1
  return 0
}

# canonicalise a set literal in a leg's output, so its *order* is not read as a difference
#
# a set has none. `__abstractmethods__` is a frozenset, and cpython's own
# `DeprecationWarning: Unimplemented abstract methods {...}` prints it in whatever order
# the hashes fell — which differed between the two legs consistently, and the rungs compare
# text, so it read as `DIFFERS` for a module where both legs name the same two methods
#
# ⚠️ only a *set*. a dict prints in insertion order and that order is meaningful, so a
# `{...}` that parses as a dict is left exactly as written. the span is parsed with
# `ast.literal_eval` rather than matched with a regex, precisely so the two cannot be
# confused — a nested `{'a': 1}` inside a set of tuples would defeat any "contains a colon"
# test. anything that does not parse as a literal is left alone
sweep_canonical() {
  # an address is never a difference: two processes print two addresses for the same
  # object, so a leg that merely *mentions* one differs from itself. cpython prints them
  # unbidden — `Exception ignored in: <function X.__del__ at 0x101eb3880>` reaches a
  # comparison from the garbage collector, and `tempfile` scored a difference on that line
  # alone, which a perfect compiler would also have scored
  set -- "$(printf '%s' "$1" | sed -E 's/0x[0-9a-fA-F]+/0xX/g')"
  case $1 in *'{'*) ;; *) printf '%s' "$1"; return 0 ;; esac
  printf '%s' "$1" | "$PY" -c '
import ast, sys
text = sys.stdin.read()
out, i = [], 0
while i < len(text):
    if text[i] != "{":
        out.append(text[i]); i += 1; continue
    depth, j = 0, i
    while j < len(text):
        if text[j] == "{": depth += 1
        elif text[j] == "}":
            depth -= 1
            if depth == 0: break
        j += 1
    span = text[i:j+1]
    try:
        value = ast.literal_eval(span)
    except (ValueError, SyntaxError, TypeError, MemoryError, RecursionError):
        value = None
    if isinstance(value, (set, frozenset)):
        out.append("{" + ", ".join(sorted(repr(v) for v in value)) + "}")
    else:
        out.append(span)
    i = j + 1
sys.stdout.write("".join(out))
'
}

# true when the build actually left an extension module behind. `-d o` is not enough:
# a build that fails halfway leaves the directory and no artefact, and the compiled leg
# then fails to import for a reason that is not the defect the sweep is looking for
sweep_built() {
  sweep_artifact "$1" >/dev/null
}

# write the shared value renderer next to a leg, as `sweepcanon.py`
#
# named apart from `sweep_canonical` on purpose: that one takes *text* a leg already
# printed and normalises it, and three rungs call it that way. this one takes a
# *directory* and writes a python module into it. one name for both would have been
# resolved by bash in favour of whichever was defined last, and the text callers would
# have silently stopped canonicalising
#
# a rung that compares *values* rather than names has to answer one question first: what
# in a repr moves between two runs of the same program, and is therefore never a defect?
# two things do. an address — `<random.Random object at 0x104a2b3d0>` — differs between
# any two processes. and the order a `set` or a `frozenset` prints in differs with the
# hash seed and with insertion history, so the same set prints two ways. a `dict`'s order
# is *not* in that class: it is insertion order, it is part of the answer, and a compiled
# module that built one in a different order has a real defect. so sets are sorted and
# dicts are left exactly as they came
#
# the renderer also takes a list of aliases — text to substitute before comparing. the
# rungs use it for the one difference that is already reported elsewhere: an emitted
# class in a package answers `m` for its `__module__` where its twin answers `pkg.m`,
# and that spelling is inside every repr and every AttributeError message a probe
# produces. `isosurface` reports that defect once per class, which is where it belongs;
# repeating it inside every value would leave nothing else visible
sweep_write_renderer() {
  cat > "$1/sweepcanon.py" <<'PYEOF'
"""turn a value, or the exception reading it raised, into text two processes agree on"""

import re
import types

_ADDR = re.compile(r'0x[0-9a-fA-F]+')

# a container is rendered element by element rather than repr'd, so these bound the work
# and the output. they apply to both legs identically, so a cap can hide a difference
# past the cap but can never invent one
ELEMENTS = 50
CHARACTERS = 200
DEPTH = 3
# past this a set is described rather than rendered: sorting is what makes a set
# comparable, and sorting has to see all of it
SET_LIMIT = 2000

# read off an instance, a method is a `bound method` on the interpreted leg and a
# `builtin_function_or_method` on the compiled one — the same member, two spellings.
# so a callable is rendered by its name and its kind is dropped
_CALLABLE = (
    types.FunctionType, types.MethodType, types.BuiltinFunctionType,
    types.MethodDescriptorType, types.WrapperDescriptorType,
    types.MethodWrapperType, types.ClassMethodDescriptorType,
    types.GetSetDescriptorType, types.MemberDescriptorType,
    staticmethod, classmethod,
)


class Canon:
    def __init__(self, aliases=()):
        # longest first: `pkg.m.Outer.Inner` must not be half-replaced by `pkg.m.Outer`
        self.aliases = sorted(aliases, key=lambda pair: len(pair[0]), reverse=True)

    def scrub(self, text):
        text = _ADDR.sub('0xX', text)
        for old, new in self.aliases:
            text = text.replace(old, new)
        # a rung compares its two legs line by line, so a rendered value has to stay on
        # one line: `str()` of an object and of an exception both readily contain a
        # newline, and one that reached the output would split a probe's answer into
        # rows the comparison could pair up wrongly
        text = text.replace('\\', '\\\\').replace('\n', '\\n').replace('\t', '\\t')
        if len(text) > CHARACTERS:
            text = text[:CHARACTERS] + '...'
        return text

    def render(self, value, depth=0):
        try:
            return self._render(value, depth)
        except BaseException as error:
            return '<render raised %s>' % type(error).__name__

    def _render(self, value, depth):
        if depth > DEPTH:
            return '...'
        if value is None or value is True or value is False:
            return repr(value)
        kind = type(value)
        if kind in (int, float, complex, str, bytes, bytearray):
            return self.scrub(repr(value))
        if kind in (list, tuple):
            return self._sequence(value, depth, '[%s]' if kind is list else '(%s)')
        if kind in (set, frozenset):
            if len(value) > SET_LIMIT:
                return '<%s of %d>' % (kind.__name__, len(value))
            # the whole set is rendered before anything is dropped: capping first would
            # cap an arbitrary slice, which is the very thing sorting exists to defeat
            items = sorted(self.render(item, depth + 1) for item in value)
            return '{%s}' % ', '.join(self._cap(items, len(value)))
        if kind is dict:
            items = ['%s: %s' % (self.render(key, depth + 1), self.render(item, depth + 1))
                     for key, item in list(value.items())[:ELEMENTS]]
            return '{%s}' % ', '.join(self._cap(items, len(value)))
        if isinstance(value, type):
            return '<class %s>' % self.scrub(
                '%s.%s' % (getattr(value, '__module__', '?'),
                           getattr(value, '__qualname__', value.__name__)))
        if isinstance(value, types.ModuleType):
            return '<module %s>' % self.scrub(getattr(value, '__name__', '?'))
        if isinstance(value, _CALLABLE):
            return '<callable %s>' % self.scrub(str(getattr(value, '__name__', '?')))
        return self.scrub(repr(value))

    def _sequence(self, value, depth, shape):
        items = [self.render(item, depth + 1) for item in list(value)[:ELEMENTS]]
        return shape % ', '.join(self._cap(items, len(value)))

    def _cap(self, items, total):
        if total > ELEMENTS:
            return items[:ELEMENTS] + ['...+%d' % (total - ELEMENTS)]
        return items

    def raised(self, error):
        """an exception is an answer too, so its type *and* its wording are compared"""
        try:
            text = str(error)
        except BaseException:
            text = '<str raised>'
        return '<raised %s: %s>' % (type(error).__name__, self.scrub(text))
PYEOF
}

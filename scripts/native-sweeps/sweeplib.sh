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

# true when the build actually left an extension module behind. `-d o` is not enough:
# a build that fails halfway leaves the directory and no artefact, and the compiled leg
# then fails to import for a reason that is not the defect the sweep is looking for
sweep_built() {
  sweep_artifact "$1" >/dev/null
}

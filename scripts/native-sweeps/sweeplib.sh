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

# ── a run owns its output, and says when it finished ─────────────────────────────────
#
# three ways a rung has silently lost a real answer here. none of them was a wrong
# number on the screen; each was a number that looked exactly like a right one
#
#   two agents ran rungs at the same time through the same scratchpad and both wrote
#   `isoinstance.base.txt`. one of them happened to notice, killed its chain and re-ran
#   three rungs from scratch somewhere private. the other would not have noticed at all.
#   `scripts/bg.sh` was made per-checkout for this exact reason; the sweep outputs were
#   not
#
#   a finished run was compared against one still going, and every module the second had
#   not reached yet came back as a change. "100 modules changed" was a hundred modules
#   that did not exist in the file yet. that has happened at least three times, which is
#   what a rule of "always count the rows first" is worth
#
#   a leg that was killed prints what a leg that found nothing prints — nothing — so two
#   dead legs compared equal and were scored `same`
#
# the first two are one defect: nothing tells a file being written from a file that is
# finished. so one mechanism covers both — a run takes its output path and holds it for
# as long as it runs, and marks it complete, by content, on the way out. the third is
# checked here rather than in each rung, because three rungs learned it separately and
# the other three had not

# a run's claim on its output path. a *directory*, because `mkdir` either creates it or
# fails and there is no window in between: `[ -e ] && touch` has one, and two rungs
# starting together is the whole case this exists for
sweep_lock() { printf '%s.lock' "$1"; }

# true when a claim is held by a process that is still there, printing who
#
# liveness is checked by pid rather than by name, for the reason `bg.sh` gives: a number
# cannot match the command line of the process asking the question, and a pattern can. a
# claim whose owner is gone is rubble — a run that was killed never reached its trap —
# and the two cases want different words in both directions, so they are told apart here
# rather than assumed either way
sweep_lock_live() {
  local lock pid=""
  lock=$(sweep_lock "$1")
  [ -d "$lock" ] || return 1
  [ -f "$lock/pid" ] && pid=$(cat "$lock/pid")
  [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null || return 1
  [ -f "$lock/owner" ] && cat "$lock/owner"
  return 0
}

# claim $OUT, clear it, and give the rung a working root of its own
#
# a rung calls this once, in place of the `root=$SP/tag.$$; rm -rf; trap` it used to
# write for itself. the working roots never collided — they carry `$$` — but the output
# files did, and only the output file is what anybody reads
sweep_begin() {
  local tag="$1" lock owner="(unrecorded)"
  if [ -z "${OUT:-}" ] || [ -z "${SP:-}" ]; then
    printf 'sweep: SP and OUT must be set — a rung takes SP BY PY OUT\n' >&2
    return 1
  fi
  lock=$(sweep_lock "$OUT")
  if ! mkdir "$lock" 2>/dev/null; then
    if owner=$(sweep_lock_live "$OUT"); then
      printf 'sweep: %s refuses to start — %s is already being written by a live run\n' \
        "$tag" "$OUT" >&2
      printf '  held by: %s\n' "${owner:-(unrecorded)}" >&2
      printf '  nothing was touched. give this run an output path of its own\n' >&2
      return 1
    fi
    owner="(unrecorded)"
    [ -f "$lock/owner" ] && owner=$(cat "$lock/owner")
    # a rung killed outright never reaches its trap, so a lock whose owner is gone is
    # rubble rather than a claim. saying so beats both silently overwriting and
    # refusing forever
    printf 'sweep: taking %s over from a run that did not finish (%s)\n' "$OUT" "$owner" >&2
    rm -rf "$lock"
    mkdir "$lock" || return 1
  fi
  printf '%s\n' "$$" > "$lock/pid"
  printf '%s pid %s in %s since %s\n' \
    "$tag" "$$" "$PWD" "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" > "$lock/owner"

  SWEEP_TAG="$tag"
  # a marker left by an earlier run must not be standing to vouch for the file this run
  # is about to write over — that is a complete-looking file with a stranger's contents,
  # which is worse than an unmarked one
  rm -f "$OUT.complete" "$OUT.walked" "$OUT.deaths" "$OUT.alarms"
  : > "$OUT"
  SWEEP_ROOT="$SP/$tag.$$"
  rm -rf "$SWEEP_ROOT"
  mkdir -p "$SWEEP_ROOT" || return 1
  trap sweep_finally EXIT
  return 0
}

# the working root goes, and so does the claim. running on every exit — clean, failed or
# interrupted — is what keeps a killed rung from leaving a lock nobody can explain
sweep_finally() {
  [ -n "${SWEEP_ROOT:-}" ] && rm -rf "$SWEEP_ROOT"
  [ -n "${OUT:-}" ] && rm -rf "$(sweep_lock "$OUT")"
  return 0
}

# the two spellings a rung uses for "this leg was killed and restarted past it"
#
# `isoconstruct` and `isosubclass` write `DIED signal=N` into the leg's text;
# `isoinstance` writes a row whose second field is `DIED`. they mean the same thing, so
# they are recognised in one place rather than in three
SWEEP_DIED_ROW=$'\tDIED\t'
sweep_pair_died() {
  case "$1$2" in *'DIED signal='*) return 0 ;; esac
  case "$1$2" in *"$SWEEP_DIED_ROW"*) return 0 ;; esac
  return 1
}

# record that a leg of this module was killed, however the rung came to notice
#
# it goes beside the tsv rather than only into the row, because `isoconstruct` and
# `isosubclass` record an agreeing pair as a *line count* — the text, deaths and all, is
# dropped before the summary's `crashed:` could ever count it. this file is the evidence
# and the row is the rung's word for it, and `sweep_end` refuses to mark the run complete
# when the two disagree. so a rung that notices a death and then writes `same` anyway is
# caught in one place instead of being trusted six times
sweep_note_death() {
  [ -n "${OUT:-}" ] && printf '%s\n' "$1" >> "$OUT.deaths"
  return 0
}

# true when a pair of legs that compared *equal* is not evidence, recording why
#
# call it before writing `same`. two legs killed the same way are killed in the same
# words, and a rung comparing text has no other way to tell that from agreement. it has
# happened: a broken rung made both legs die forty times each and scored the pair `same`
sweep_hollow() {
  sweep_pair_died "$2" "$3" || return 1
  sweep_note_death "$1"
  return 0
}

# sha256 of a file, however this machine spells it
sweep_digest() {
  if command -v shasum > /dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  elif command -v sha256sum > /dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else cksum < "$1" | tr -d ' '
  fi
}

# refuse to mark a run complete unless it is one, then mark it by content
#
# these are the same questions of every rung, so they are asked once here and a rung
# written later inherits them:
#
#   did every module the walk handed out get a row? a run that stopped at 480 of 550 is
#   not a smaller corpus, it is an unfinished one, and read as a corpus figure it invents
#   a change for each of the seventy it never reached
#
#   did any module score `same` after one of its legs was killed? that is agreement with
#   a leg that answered nothing
#
# the marker carries the digest of the file it vouches for, so a truncation afterwards
# cannot pass as a finished run — which is the only thing that makes a reader's check
# cheaper than counting the rows by hand
sweep_end() {
  local walked=0 seen=0 hollow=""
  [ -f "$OUT.walked" ] && walked=$(cat "$OUT.walked")
  seen=$(cut -f1 "$OUT" | LC_ALL=C sort -u | grep -c .)
  if [ "$walked" -eq 0 ]; then
    printf 'sweep: %s walked no modules, so it measured nothing — %s is not marked complete\n' \
      "${SWEEP_TAG:-?}" "$OUT" >&2
    return 1
  fi
  if [ "$seen" -ne "$walked" ]; then
    printf 'sweep: %s walked %s modules and wrote rows for %s — %s is not marked complete\n' \
      "${SWEEP_TAG:-?}" "$walked" "$seen" "$OUT" >&2
    return 1
  fi
  if [ -f "$OUT.deaths" ]; then
    LC_ALL=C sort -u "$OUT.deaths" > "$SWEEP_ROOT/deaths"
    awk -F'\t' '$2 == "same" { print $1 }' "$OUT" | LC_ALL=C sort -u > "$SWEEP_ROOT/agreed"
    hollow=$(LC_ALL=C comm -12 "$SWEEP_ROOT/deaths" "$SWEEP_ROOT/agreed" | tr '\n' ' ')
    if [ -n "$hollow" ]; then
      printf 'sweep: %s scored a module the same on both legs after one of them was killed, which is agreement with a leg that answered nothing: %s\n' \
        "${SWEEP_TAG:-?}" "$hollow" >&2
      return 1
    fi
  fi
  {
    printf 'tag\t%s\n' "${SWEEP_TAG:-?}"
    printf 'walked\t%s\n' "$walked"
    printf 'modules\t%s\n' "$seen"
    printf 'lines\t%s\n' "$(grep -c '' < "$OUT")"
    printf 'digest\t%s\n' "$(sweep_digest "$OUT")"
    printf 'checkout\t%s\n' "$PWD"
    printf 'commit\t%s\n' "$(git rev-parse --short HEAD 2> /dev/null || printf '?')"
    printf 'python\t%s\n' "$("$PY" -c 'import platform;print(platform.python_version())' 2> /dev/null || printf '?')"
    printf 'finished\t%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  } > "$OUT.complete"
  return 0
}

# read one field out of a completion marker
sweep_marker() {
  awk -F'\t' -v key="$2" '$1 == key { print $2; exit }' "$1"
}

# the reader's half: refuse a file that is not a finished run
#
# this is what makes the phantom-change failure impossible rather than merely rare. a
# reader comparing two rungs cannot see from the rows whether a file is finished — every
# row in a partial file is a perfectly good row — so the question is answered by the
# marker, and the marker is bound to the bytes it was written for
sweep_require_complete() {
  local out="$1" marker="$1.complete" want got holder
  if [ ! -f "$out" ]; then
    printf 'sweep: %s does not exist\n' "$out" >&2
    return 1
  fi
  if [ ! -f "$marker" ]; then
    if holder=$(sweep_lock_live "$out"); then
      printf 'sweep: %s is still being written — a run in progress is not a corpus figure\n' "$out" >&2
      printf '  held by: %s\n' "${holder:-(unrecorded)}" >&2
    elif [ -d "$(sweep_lock "$out")" ]; then
      # the claim is still on disk but its owner is gone, which is what a rung killed
      # outright leaves behind. saying "a live run" here would send a reader looking for
      # a process that stopped hours ago
      printf 'sweep: %s was left by a run that was interrupted — it stops wherever the run stopped\n' "$out" >&2
    else
      printf 'sweep: %s carries no completion marker, so how much of the corpus it covers is unknown\n' "$out" >&2
      printf '  it was either interrupted, or written by something that is not a rung\n' >&2
    fi
    return 1
  fi
  want=$(sweep_marker "$marker" lines)
  got=$(grep -c '' < "$out")
  if [ "$want" != "$got" ]; then
    printf 'sweep: %s has %s rows and its marker vouches for %s — it was truncated or appended to after the run\n' \
      "$out" "$got" "$want" >&2
    return 1
  fi
  want=$(sweep_marker "$marker" digest)
  got=$(sweep_digest "$out")
  if [ "$want" != "$got" ]; then
    printf 'sweep: %s does not match the digest its marker vouches for — it was edited after the run\n' "$out" >&2
    return 1
  fi
  return 0
}

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
#
# the count is recorded beside the output as well as returned, because it is the
# denominator `sweep_end` holds the rung to. a rung cannot be trusted to count its own
# walk: the failure being guarded against is a rung that stopped early, and a rung that
# stopped early stops before whatever counting it was going to do
sweep_modules() {
  local lib="$1" list; shift
  if [ "$#" -gt 0 ]; then
    list=$(printf '%s\n' "$@")
  else
    list=$( (cd "$lib" && find . -name '*.py' | sed 's|^\./||') \
      | grep -v -E 'test|__pycache__|site-packages|lib2to3|idlelib' \
      | grep -v -E '(^|/)__main__\.py$' \
      | LC_ALL=C sort )
  fi
  # `grep -c .` rather than `wc -l`: an empty list is one empty line here, and counting
  # that as a module would let a walk that found nothing look like a walk that found one
  [ -n "${OUT:-}" ] && printf '%s\n' "$list" | grep -c . > "$OUT.walked"
  printf '%s\n' "$list"
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
  #
  # `&&` rather than `;` for the reason `sweep_capture` gives: with `;` a `sleep`
  # killed during teardown means the kill runs *now* rather than not at all, and
  # the pid it is aimed at has already been waited for
  { sleep "$bound" && kill -9 "$pid" 2>/dev/null; } >/dev/null 2>&1 &
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

# ── running a leg, and reading what it printed ───────────────────────────────────────
#
# every rung used to read its probe straight out of a command substitution:
#
#     out=$(cd "$dir" && "$PY" probe.py "$start" 2>&1 | grep …)
#
# which waits on a *pipe*, and a pipe is at end of file only once the last writer closes
# it. a probe that leaks a child hands that write end to a process nobody is waiting for,
# so the read has no bound at all. `multiprocessing` leaks them by construction — a pool
# worker is a fork that inherits the leg's stdout and stderr — and one corpus run stalled
# 18 minutes on `multiprocessing/pool.py` with the probe itself long finished and fifteen
# orphans holding fds 1 and 2. it is load-dependent: the same rung ran clean twice on the
# same commit, which is the worst way for it to be left, because it will not reproduce on
# demand
#
# a hang is worse than a crash. a crash is a row in the output and gets read; a hang
# stalls the corpus and looks like slowness
#
# so a leg writes to a *file*, and the file is read back afterwards. a leaked child holding
# that descriptor blocks nothing: reading a regular file stops at whatever has been written
# so far, however many other processes still have it open. what the pipe did usefully — wait
# for a child that is still writing — is kept as a bounded wait rather than an unbounded one

# how long one whole leg is given, in seconds, and 0 disables it
#
# the probes bound *themselves* — an import gets `SWEEP_IMPORT_BOUND` and a step gets
# `SWEEP_PROBE_BOUND` — so this is not what stops a slow member. what those alarms do not
# cover is interpreter shutdown, where a module joining a thread or a worker can wait for
# ever after its last answer is printed, and a C call that never returns to the eval loop,
# where the alarm is raised only once python is looking. so it sits far above any honest
# leg's running time and exists to turn a hang into a death the rung already knows how to
# report, rather than to time anything
export SWEEP_CAPTURE_BOUND=${SWEEP_CAPTURE_BOUND:-240}

# how long a leg's leaked children are given to finish writing, in seconds
#
# the command substitution this replaced waited for *every* writer to close the pipe, so a
# line a child printed after its parent exited was still part of the leg's answer.
# `multiprocessing`'s resource tracker prints exactly one such line — the `leaked
# shared_memory objects` warning that is the only thing `isoconstruct` has ever found in
# `multiprocessing/shared_memory.py`. reading the file the moment the leg exits loses it,
# and losing it on one leg while the other keeps it turns an agreeing module into a
# difference the compiler never caused: a *wrong answer*, which is worse than the hang
#
# so the wait is kept and bounded. a child that is on its way out exits in milliseconds and
# is waited for; a pool worker asleep for ever is not, and costs this and no more
export SWEEP_CAPTURE_LINGER=${SWEEP_CAPTURE_LINGER:-5}

# run one program in a directory and hand back what it printed
#
# sets, for the caller:
#   SWEEP_CAPTURE_TEXT     everything it wrote to stdout and to stderr
#   SWEEP_CAPTURE_STATUS   the program's own exit status, never a pipeline's
#
# the leg gets a process group of its own, because what it leaks is a real process on a
# real machine and a corpus is 550 modules times four legs — the pool workers that caused
# the hang were still asleep, holding a deleted working directory, long after the rung
# they belonged to had finished. `set -m` is what puts a background job in a group of its
# own here, and the group is reaped once the linger below is over. stdin is `/dev/null` so
# that a backgrounded group can never be stopped reading from a terminal it does not own
#
# a rung that needs to know whether the bound fired reads it off the status: the bound
# kills, so the leg comes back 137 and lands in whatever the rung already does with a leg
# that was killed
#
# the two names above are this function's return values, set here and read by every rung
# that calls it. shellcheck sees one file at a time, so it reads them as written and never
# used
# shellcheck disable=SC2034
sweep_capture() {
  local dir="$1"; shift
  local sink="${SWEEP_ROOT:?sweep_capture needs a working root}/capture" pid killer
  local left=$(( ${SWEEP_CAPTURE_LINGER:-0} * 5 ))
  : > "$sink"
  set -m
  ( cd "$dir" && exec "$@" ) > "$sink" 2>&1 < /dev/null &
  pid=$!
  set +m
  if [ "${SWEEP_CAPTURE_BOUND:-0}" -gt 0 ]; then
    # the group goes before the leader, so a leg waiting *on* its own children does not
    # outlive them and come back looking like a leg that finished
    #
    # `&&` rather than `;` between the two, and that is not a style choice. this watchdog is
    # torn down by killing its `sleep`, and with `;` a killed `sleep` simply means the next
    # command runs *now* — so tearing the watchdog down fired the very kill it was there to
    # delay, and wiped the process group this function is about to wait on. with `&&` a
    # `sleep` that was killed is a `sleep` that did not elapse, and the kill is cancelled
    { sleep "$SWEEP_CAPTURE_BOUND" \
      && { kill -9 -"$pid" 2>/dev/null; kill -9 "$pid" 2>/dev/null; }; } >/dev/null 2>&1 &
    killer=$!
  else
    killer=""
  fi
  wait "$pid" 2>/dev/null
  SWEEP_CAPTURE_STATUS=$?
  if [ -n "$killer" ]; then
    # the watchdog's own `sleep` is killed with it, for the reason `sweep_compile` gives:
    # an orphaned `sleep` holds the descriptors it inherited until its bound elapses. the
    # sleep goes first because killing the subshell first would orphan it, and `-P` cannot
    # then find it
    pkill -9 -P "$killer" 2>/dev/null
    kill -9 "$killer" 2>/dev/null
    wait "$killer" 2>/dev/null
  fi
  # what the leg leaked is given its moment to finish writing, in fifths of a second so
  # that the overwhelming case — a leg that leaked nothing, and whose group is already
  # empty — costs one `kill -0` and no waiting at all
  while [ "$left" -gt 0 ] && kill -0 -"$pid" 2>/dev/null; do
    sleep 0.2; left=$((left - 1))
  done
  # whatever is still there, now that the leg itself is long gone
  kill -9 -"$pid" 2>/dev/null
  SWEEP_CAPTURE_TEXT=$(cat "$sink")
  rm -f "$sink"
  return 0
}

# ── the text that moves on its own ───────────────────────────────────────────────────
#
# a rung compares its two legs as text, so anything in that text which two runs of the
# *same* program disagree about is reported as a difference the compiler did not cause.
# it is worse than one wrong verdict: it also means two runs of a rung are never
# byte-identical, so anyone diffing them sees a module that nothing changed in. that has
# already misled a comparison in this project
#
# how an entry earns its place: run a rung twice against itself, same binary and same
# corpus, and see the entry fire. nothing changed between the two runs, so whatever
# differs is something the rung made up. a scrub that does *not* fire that way is not
# removing noise — it is removing the rung's ability to answer, and this is the area
# where three rungs scored two dead legs as agreement for months
#
# what is deliberately *not* here matters as much. a `dict` prints in insertion order and
# that order is part of the answer. a `set` has no order at all, and is sorted rather
# than scrubbed. a value that merely looks random — a hash, a table of offsets, a cache
# size — is the module's own answer and is left exactly as it came
#
#   0x…            an address. two processes print two addresses for the same object, so
#                  a leg that merely *mentions* one differs from itself. cpython prints
#                  them unbidden: `Exception ignored in: <function X.__del__ at
#                  0x101eb3880>` reaches a comparison from the garbage collector, and
#                  `tempfile` scored a difference on that line alone
#   /psm_…         a posix shared-memory segment. `multiprocessing.shared_memory`'s
#                  `_make_filename` names one `secrets.token_hex(4)`, and the segment it
#                  leaks is printed by `resource_tracker`'s shutdown `UserWarning` —
#                  which was the *only* thing `isoconstruct` had ever found wrong with
#                  `multiprocessing/shared_memory.py`. `wnsm_` is the same name on
#                  windows. the prefix is kept rather than swallowed, so a leg that
#                  produced the wrong *kind* of name still differs
#   ………T……:……:……   the wall clock, in the basic ISO 8601 that `xmlrpc.client._strftime`
#                  writes for a `DateTime()` given no argument. the separated spelling
#                  `2026-08-24T12:01:46` is deliberately not matched: `_strftime` has a
#                  second branch that writes exactly that, and a leg that took the wrong
#                  branch has a real defect this must not hide
#
# each entry is an extended regular expression and its replacement, tab separated. every
# replacement is a fixed spelling that the pattern itself cannot match, so scrubbing
# twice says the same as scrubbing once
#
# the two places that scrub read this one table: `sweep_canonical` below, for the raw
# text a leg printed, and `sweepcanon.py`, for a value a probe rendered. it is one table
# because a scrub present in only one of them would be a corpus that answers differently
# depending on which rung asked — and the two are written in different languages, so
# there is nothing to keep them honest but sharing the source
SWEEP_NOISE='0x[0-9a-fA-F]+	0xX
(/psm_|wnsm_)[0-9a-f]+	\1X
[0-9]{8}T[0-9]{2}:[0-9]{2}:[0-9]{2}	<clock>'
export SWEEP_NOISE

# canonicalise one leg's output: sort what has no order, and scrub what has no meaning
#
# a set has no order. `__abstractmethods__` is a frozenset, and cpython's own
# `DeprecationWarning: Unimplemented abstract methods {...}` prints it in whatever order
# the hashes fell — which differed between the two legs consistently, and the rungs compare
# text, so it read as `DIFFERS` for a module where both legs name the same two methods
#
# ⚠️ only a *set*. a dict prints in insertion order and that order is meaningful, so a
# `{...}` that parses as a dict is left exactly as written. the span is parsed with
# `ast.literal_eval` rather than matched with a regex, precisely so the two cannot be
# confused — a nested `{'a': 1}` inside a set of tuples would defeat any "contains a colon"
# test. anything that does not parse as a literal is left alone
#
# a set's elements are scrubbed *after* it is parsed rather than before, one element at a
# time. scrubbing the text first would turn `{'/psm_a', '/psm_b'}` into a set holding one
# element, and a leg that leaked two segments would then read exactly like a leg that
# leaked one. the names are noise; how many of them there were is not
sweep_canonical() {
  local out
  # nothing to sort and nothing to scrub, and by far the commonest case — `isoimport`'s
  # legs are empty whenever the import worked
  [ -n "$1" ] || return 0
  out=$(printf '%s' "$1" | "$PY" -c '
import ast, os, re, sys

noise = [(re.compile(rule.split("\t")[0]), rule.split("\t")[1])
         for rule in os.environ["SWEEP_NOISE"].splitlines() if rule]


def scrub(text):
    for pattern, replacement in noise:
        text = pattern.sub(replacement, text)
    return text


text = sys.stdin.read()
out, plain, i = [], [], 0
while i < len(text):
    if text[i] != "{":
        plain.append(text[i]); i += 1; continue
    out.append(scrub("".join(plain))); plain = []
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
        out.append("{" + ", ".join(sorted(scrub(repr(v)) for v in value)) + "}")
    else:
        out.append(scrub(span))
    i = j + 1
out.append(scrub("".join(plain)))
sys.stdout.write("".join(out))
' 2>/dev/null)
  # a scrub turns text into text, so non-empty in and empty out means this failed rather
  # than found nothing. the caller compares two legs, and an empty leg compares equal to
  # any other empty leg — so failing quietly here would score a pair `same` on the
  # strength of two legs that said nothing. the raw text is handed back instead: it still
  # carries whatever the noise was, so the pair is reported as a difference and looked at,
  # which is the direction to fail in
  if [ -z "$out" ]; then
    sweep_alarm canonicalise 'produced nothing from a leg that printed something — that leg is compared unscrubbed'
    printf '%s' "$1"
    return 0
  fi
  printf '%s' "$out"
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
# the text of it is `SWEEP_NOISE`'s to say, and this reads that same table rather than
# keeping a second list — an address is not a difference here for the same reason it is
# not one there. what is particular to a *rendered* value is the order it comes in: a
# `set` or a `frozenset` prints in whatever order the hash seed and the insertion history
# left, so the same set prints two ways. a `dict`'s order is *not* in that class: it is
# insertion order, it is part of the answer, and a compiled module that built one in a
# different order has a real defect. so sets are sorted and dicts are left exactly as
# they came
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

import os
import re
import types

# the one table of text that moves on its own, read from the environment rather than
# repeated here — the reasons for each entry are with it, in `sweeplib.sh`. a missing
# table is a hard failure on purpose: the probe program cannot even be imported, so the
# leg exits non-zero and the rung reports a death. scrubbing that silently stopped would
# be the other kind of wrong, and this rung's whole subject is legs that say nothing and
# are read as agreement
_NOISE = [(re.compile(rule.split('\t')[0]), rule.split('\t')[1])
          for rule in os.environ['SWEEP_NOISE'].splitlines() if rule]

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
        for pattern, replacement in _NOISE:
            text = pattern.sub(replacement, text)
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

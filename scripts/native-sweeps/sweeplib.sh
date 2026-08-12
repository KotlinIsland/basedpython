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

# compile one staged module, bounded
#
# the compiler is unbounded and a module whose type inference does not terminate wedges
# a whole rung: `ast.py` alone held one run for 21 minutes with no progress. the five
# that do this today (`ast`, `bdb`, `pickletools`, `profile`, `turtle`) stopped
# terminating when the branch was rebased, and the cause is on main rather than here —
# so the rungs have to survive it rather than wait for it. a bounded module reports as
# `no-artifact`, which is what it is: the sweep learned nothing about it
#
# SWEEP_BOUND is seconds; 0 disables the bound
sweep_compile() {
  local dir="$1" py="$2" by="$3"
  local bound=${SWEEP_BOUND:-180}
  if [ "$bound" -eq 0 ]; then
    (cd "$dir" && PYTHON="$py" "$by" compile m.py -o o >/dev/null 2>&1)
    return $?
  fi
  (cd "$dir" && PYTHON="$py" "$by" compile m.py -o o >/dev/null 2>&1) &
  local pid=$!
  ( sleep "$bound"; kill -9 "$pid" 2>/dev/null ) &
  local killer=$!
  wait "$pid" 2>/dev/null
  local rc=$?
  kill -9 "$killer" 2>/dev/null
  wait "$killer" 2>/dev/null
  return $rc
}

# lay one module out as a project of its own. `m.py` regardless of where it came from,
# so an import of `m` names the same thing in both legs
sweep_stage() {
  local dir="$1" src="$2"
  rm -rf "$dir"; mkdir -p "$dir"; cp "$src" "$dir/m.py"
  printf '[project]\nname="s"\nversion="0"\nrequires-python=">=3.13"\n' > "$dir/pyproject.toml"
}

# true when the build actually left an extension module behind. `-d o` is not enough:
# a build that fails halfway leaves the directory and no artefact, and the compiled leg
# then fails to import for a reason that is not the defect the sweep is looking for
sweep_built() {
  ls "$1"/o/m*.so >/dev/null 2>&1
}

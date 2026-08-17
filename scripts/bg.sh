#!/bin/bash
# start a long job and wait for it across several turns, without burning the
# 600-second command bound on a wait that answers nothing
#
# a build, a test run or a sweep outlives the bound the tool harness puts on one
# command, so it has to run in the background and be polled. hand-rolled polling
# has been measured as **the single largest cost in this work**: across five
# agents and 12h27m of wall time, 7h13m went into `sleep`/`until` loops against
# 43m of `cargo build`. 3h of that was loops that ran to the bound and returned
# nothing, and 2h was loops that could never have returned at all:
#
#     until ! pgrep -f "native-sweeps/isoimport.sh"; do sleep 30; done
#
# `pgrep -f` matches against whole command lines, and that loop's own command
# line contains the pattern — so it matches itself, and waits for the bound no
# matter what the sweep is doing. this file exists so nobody writes that again
#
# usage:
#   scripts/bg.sh start <name> <command...>   launch, return immediately
#   scripts/bg.sh wait  <name> [seconds]      wait up to N (default 540), then
#                                             report `running` and return 0 so
#                                             the caller can simply call again
#   scripts/bg.sh status <name>               done/running/missing, no waiting
#   scripts/bg.sh log   <name> [lines]        tail the output
#
# `wait` distinguishes the two outcomes a bare sleep loop cannot: `done rc=N`
# means the job really finished and N is its exit status; `running` means it is
# still going and nothing is wrong. a job that dies still writes its marker, so
# a crash is never mistaken for slowness

set -u

dir() {
  # the scratchpad if the harness gave us one, else a temp dir. keeping the
  # markers out of the project matters: `by compile` compiles every source it
  # finds beside the file it was given
  printf '%s\n' "${BY_BG_DIR:-${TMPDIR:-/tmp}/by-bg}"
}

start() {
  local name="$1"; shift
  local d; d=$(dir); mkdir -p "$d"
  rm -f "$d/$name.done" "$d/$name.log" "$d/$name.pid"
  # the marker is written by the same shell that runs the command, after it
  # exits, so it cannot be missed and it carries the real status
  #
  # the single quotes are the point: `$@`, `$?` and `$0` are for the *inner* shell to
  # expand once it has run the command, not for this one to expand now
  # shellcheck disable=SC2016
  nohup bash -c '"$@" ; printf "%s\n" "$?" > "$0"' \
    "$d/$name.done" "$@" > "$d/$name.log" 2>&1 &
  printf '%s\n' "$!" > "$d/$name.pid"
  printf 'started %s (log %s)\n' "$name" "$d/$name.log"
}

# a job killed outright — OOM, SIGKILL, the machine giving up — never reaches the
# line that writes its marker, and without this it would report `running` until
# the caller gave up. so liveness is checked too, and by *pid*: `kill -0` takes a
# number, which cannot match the command line of the shell asking the question.
# that is the whole difference from `pgrep -f`, which can and does
status() {
  local name="$1" d; d=$(dir)
  if [ -f "$d/$name.done" ]; then
    printf 'done rc=%s\n' "$(cat "$d/$name.done")"
    return 0
  fi
  if [ ! -f "$d/$name.log" ]; then
    printf 'missing\n'
    return 0
  fi
  local pid=""
  [ -f "$d/$name.pid" ] && pid=$(cat "$d/$name.pid")
  if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then
    # the marker is written *before* the shell exits, so seeing the exit first
    # only means the write has not landed yet. re-read once before calling it a
    # death, or a job that merely finished quickly is reported as killed
    sleep 1
    if [ -f "$d/$name.done" ]; then
      printf 'done rc=%s\n' "$(cat "$d/$name.done")"
      return 0
    fi
    printf 'died (no exit status — killed, not finished)\n'
    return 0
  fi
  printf 'running\n'
}

wait_for() {
  local name="$1" bound="${2:-540}" d; d=$(dir)
  local waited=0 state
  while [ "$waited" -lt "$bound" ]; do
    state=$(status "$name")
    case "$state" in
      running) ;;
      *) printf '%s\n' "$state"; return 0 ;;
    esac
    sleep 5
    waited=$((waited + 5))
  done
  # not a failure — the caller polls again. returning 0 keeps a bounded wait
  # from reading as a broken command
  printf 'running (%ss elapsed, call again)\n' "$waited"
  return 0
}

case "${1:-}" in
  start)  shift; start "$@" ;;
  wait)   shift; wait_for "$@" ;;
  status) shift; status "$@" ;;
  log)    shift; tail -n "${2:-40}" "$(dir)/$1.log" ;;
  *) printf 'usage: %s {start|wait|status|log} <name> [...]\n' "$0" >&2; exit 2 ;;
esac

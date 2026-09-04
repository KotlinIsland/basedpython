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
#   scripts/bg.sh stop  <name>                end it, and everything it started
#
# `wait` distinguishes the two outcomes a bare sleep loop cannot: `done rc=N`
# means the job really finished and N is its exit status; `running` means it is
# still going and nothing is wrong. a job that dies still writes its marker, so
# a crash is never mistaken for slowness
#
# `stop` exists because there was no way to end a job, so it was done by hand
# with `pkill -f <the rung's name>`. that kills the *script* the wrapper is
# running and leaves the wrapper alive — and a wrapper running a loop simply
# advances to the next iteration and spawns the next thing. a sweep chain killed
# that way carried straight on through three more rungs and ran orphaned for 98
# minutes, while every benchmark in that session was being timed against the load
# it was making. `pkill` returning cleanly looked like success each time

set -u

# where a job's markers live: outside the project, and *per checkout*
#
# keeping them out of the project matters because `by compile` compiles every
# source it finds beside the file it was given. keeping them per checkout matters
# because `$TMPDIR` is per *user* on macos, not per session — several worktrees
# are worked in at once here, and two of them starting a job under the same
# obvious name (`relbuild`, `tests`) shared one set of markers. the second
# session's `start` deleted the first's, so the first waited on a log another
# process was writing and read a status that was never its own. that really
# happened
#
# the checkout's own path is used rather than a hash of it: it is exact, so two
# trees can never land on one directory, and each component is short enough that
# the nesting costs nothing
dir() {
  if [ -n "${BY_BG_DIR:-}" ]; then
    printf '%s\n' "$BY_BG_DIR"
    return
  fi
  local root
  root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$PWD
  printf '%s\n' "${TMPDIR:-/tmp}/by-bg$root"
}

start() {
  local name="$1"; shift
  local d; d=$(dir); mkdir -p "$d"
  # a name still in use is the caller's mistake, not something to paper over:
  # clearing the markers under a live job orphans it, and the log it goes on
  # writing then belongs to a job nobody is waiting for
  local state; state=$(status "$name")
  if [ "$state" = running ]; then
    printf '%s is already running — pick another name or wait for it\n' "$name" >&2
    return 1
  fi
  rm -f "$d/$name.done" "$d/$name.log" "$d/$name.pid" "$d/$name.stopped"
  # the marker is written by the same shell that runs the command, after it
  # exits, so it cannot be missed and it carries the real status
  #
  # the single quotes are the point: `$@`, `$?` and `$0` are for the *inner* shell to
  # expand once it has run the command, not for this one to expand now
  #
  # `set -m` is what makes the job stoppable: with job control on, the background
  # job leads a process group of its own whose id is its pid, so one signal to the
  # negative pid reaches the wrapper, the command, and everything the command went
  # on to spawn. without it the job shares this shell's group and there is nothing
  # to signal but the wrapper
  set -m
  # shellcheck disable=SC2016
  nohup bash -c '"$@" ; printf "%s\n" "$?" > "$0"' \
    "$d/$name.done" "$@" > "$d/$name.log" 2>&1 &
  local pid=$!
  set +m
  printf '%s\n' "$pid" > "$d/$name.pid"
  printf 'started %s (log %s)\n' "$name" "$d/$name.log"
}

# end a job and everything it started
#
# a deliberate stop is recorded, because otherwise it is indistinguishable from
# the machine killing the job: both leave a job that never wrote its exit status,
# and the whole point of `status` is that those two are not the same thing
#
# the group is given a chance to exit on `TERM` before `KILL`, so a rung that
# cleans up after itself gets to. and the group is re-checked afterwards rather
# than trusted — a single kill looked like success three times in a row here while
# the loop above it had already started the next child
stop() {
  local name="$1" d; d=$(dir)
  if [ -f "$d/$name.done" ]; then
    printf 'already done rc=%s\n' "$(cat "$d/$name.done")"
    return 0
  fi
  if [ ! -f "$d/$name.pid" ]; then
    printf 'missing\n'
    return 0
  fi
  local pid; pid=$(cat "$d/$name.pid")
  if ! kill -0 "$pid" 2>/dev/null; then
    printf 'not running\n'
    return 0
  fi
  # `kill -TERM -$pid` means "the process group numbered $pid", and a group id is only
  # this job's if this job leads it. a job started before `start` enabled job control
  # shares whatever group its caller was in, so the negative form would name a group
  # belonging to somebody else — quite possibly the shell asking. so the leadership is
  # checked rather than assumed, and where the job does not lead a group only the job
  # itself is signalled and the report says the children could not be reached
  local pgid group=""
  pgid=$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')
  [ "$pgid" = "$pid" ] && group="-"
  kill -TERM "$group$pid" 2>/dev/null
  local waited=0
  while [ "$waited" -lt 10 ] && kill -0 "$pid" 2>/dev/null; do
    sleep 1
    waited=$((waited + 1))
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -KILL "$group$pid" 2>/dev/null
    sleep 2
  fi
  if kill -0 "$pid" 2>/dev/null; then
    printf 'still running after KILL — pid %s\n' "$pid" >&2
    return 1
  fi
  if [ -z "$group" ]; then
    printf 'stopped %s, but it led no process group — anything it spawned is still running\n' \
      "$name" >&2
    printf 'stopped\n' > "$d/$name.stopped"
    return 1
  fi
  # anything left in the group is a straggler that outlived its leader, and it is
  # the thing this verb exists to catch
  local left
  left=$(ps -Ao pgid= 2>/dev/null | tr -d ' ' | grep -c "^$pid$") || left=0
  printf 'stopped\n' > "$d/$name.stopped"
  if [ "$left" -gt 0 ]; then
    printf 'stopped %s, but %s process(es) remain in its group\n' "$name" "$left" >&2
    return 1
  fi
  printf 'stopped %s\n' "$name"
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
    if [ -f "$d/$name.stopped" ]; then
      printf 'stopped (by request)\n'
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
  stop)   shift; stop "$@" ;;
  *) printf 'usage: %s {start|wait|status|log|stop} <name> [...]\n' "$0" >&2; exit 2 ;;
esac

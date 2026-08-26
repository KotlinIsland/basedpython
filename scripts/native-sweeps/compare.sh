#!/bin/bash
# what moved between two runs of the same rung, and nothing else
#
# every comparison in this project used to be a `diff` or an ad-hoc `join`, and the same
# reading went wrong at least three times: a finished run compared against one still
# going reported "100 modules changed", where all hundred were modules the second run had
# not reached yet. the rows in a partial file are perfectly good rows — nothing in them
# says the file stops early — so no amount of care reading them can catch it. the answer
# has to come from outside the rows, which is what a rung's completion marker is
#
# so this refuses to compare a file that is not a finished run, and refuses again if the
# file has changed since the run that finished it. that is the whole point: the check is
# not a habit a reader has to remember, it is a thing that has to be got past
#
# usage: compare.sh BEFORE AFTER
#   BEFORE, AFTER   two `$OUT` files written by the same rung
# shellcheck source=scripts/native-sweeps/sweeplib.sh
. "$(dirname "$0")/sweeplib.sh"

before="${1:-}"; after="${2:-}"
if [ -z "$before" ] || [ -z "$after" ]; then
  printf 'usage: %s BEFORE AFTER\n' "$0" >&2
  exit 2
fi

# both, rather than the first that fails: a reader who has pointed this at two unfinished
# files wants to hear about both of them now
ok=0
sweep_require_complete "$before" || ok=1
sweep_require_complete "$after" || ok=1
[ "$ok" -eq 0 ] || exit 1

btag=$(sweep_marker "$before.complete" tag)
atag=$(sweep_marker "$after.complete" tag)
if [ "$btag" != "$atag" ]; then
  printf 'sweep: %s is a %s run and %s is a %s run — different rungs answer different questions\n' \
    "$before" "$btag" "$after" "$atag" >&2
  exit 1
fi

bwalked=$(sweep_marker "$before.complete" walked)
awalked=$(sweep_marker "$after.complete" walked)
if [ "$bwalked" != "$awalked" ]; then
  # both are finished runs, so this is not a truncation — it is two different corpora,
  # and a module missing from one of them is not a change in the other
  printf 'sweep: %s walked %s modules and %s walked %s — these are two corpora, not two runs\n' \
    "$before" "$bwalked" "$after" "$awalked" >&2
  exit 1
fi

printf '%s %s -> %s\n' "$btag" \
  "$(sweep_marker "$before.complete" commit)" "$(sweep_marker "$after.complete" commit)"
printf '%s modules, both runs\n\n' "$bwalked"

# the verdict is the second field of a module's own row. the rungs also write diff
# detail as extra rows under the same module, and those carry `|` in that field — they
# are the *reason* for a verdict rather than a verdict, so they are not compared here
verdicts() {
  awk -F'\t' '$2 != "" && $2 !~ /^\|/ && !($1 in seen) { seen[$1]; print $1 "\t" $2 }' "$1" \
    | LC_ALL=C sort
}

moved=$(LC_ALL=C join -t $'\t' -j 1 -o 0,1.2,2.2 \
  <(verdicts "$before") <(verdicts "$after") \
  | awk -F'\t' '$2 != $3 { print $1 "\t" $2 " -> " $3 }')

if [ -n "$moved" ]; then
  printf '%s\n\n' "$moved"
else
  printf 'no module changed verdict\n\n'
fi

# the modules whose *rows* changed while their verdict did not
#
# a verdict is one word, and a rung says far more than that: the text a leg printed, the
# diff between the two legs, the count of probes that were compared. none of it is
# compared above, so two runs can be word-for-word different in every reason they give
# and still read here as "no module changed verdict"
#
# that matters most when the two files are two runs of the *same* rung on the *same*
# commit. nothing changed, so anything that differs is something the rung made up: a
# random name, a clock, a pid, an address it did not scrub. running a rung twice and
# reading this section is the whole detector, and on an unchanged tree the answer has to
# be zero. between two commits it is a report rather than a fault — the reasons are
# expected to move when the compiler does — so it is printed and does not decide the
# exit status
#
# comparing the sorted rows themselves rather than a digest of each module's block: the
# rows a rung writes for one module are already keyed by it in field one, so the lines
# `comm` reports as unique to either side name their own modules
drifted=$(LC_ALL=C comm -3 \
  <(LC_ALL=C sort "$before") <(LC_ALL=C sort "$after") \
  | sed 's/^\t//' | cut -f1 | LC_ALL=C sort -u \
  | LC_ALL=C comm -23 - <(printf '%s' "$moved" | cut -f1 | LC_ALL=C sort -u))

if [ -n "$drifted" ]; then
  printf 'kept its verdict and changed what it said (%s):\n' \
    "$(printf '%s' "$drifted" | grep -c '')"
  printf '%s\n' "$drifted" | sed 's/^/  /'
  printf '\n'
else
  printf 'no module changed what it said\n\n'
fi

# the counts either side, so a reader sees the shape of the run and not only its edges
printf '%-26s %8s %8s\n' verdict "$(basename "$before")" "$(basename "$after")"
LC_ALL=C join -t $'\t' -a 1 -a 2 -e 0 -o 0,1.2,2.2 \
  <(verdicts "$before" | cut -f2 | LC_ALL=C sort | uniq -c \
    | awk '{ print $2 "\t" $1 }' | LC_ALL=C sort) \
  <(verdicts "$after" | cut -f2 | LC_ALL=C sort | uniq -c \
    | awk '{ print $2 "\t" $1 }' | LC_ALL=C sort) \
  | awk -F'\t' '{ printf "%-26s %8s %8s%s\n", $1, $2, $3, ($2 == $3 ? "" : "   <-- moved") }'

[ -z "$moved" ]

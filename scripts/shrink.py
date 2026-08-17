#!/usr/bin/env python3
"""shrink a python file to the smallest one that still provokes a `by` failure

every cycle panic fixed in this repo was cracked by minimising first: 2714 lines to 21,
5055 to 4, 2904 to 3, 621 to 4. in each case the mechanism guessed beforehand turned out
wrong, and the small reproducer is what settled it.

usage:
    scripts/shrink.py <input.py> <output.py> --by <path/to/by> [--python <interpreter>]
                      [--marker "too many cycle iterations"] [--command check]

three traps guarded here, each of which produced a confident wrong answer before it was:

1. every candidate gets its own directory. `by check`/`by compile` operate on the whole
   *project*, so an original sitting beside a candidate is analysed together with it —
   the predicate is then true no matter what the candidate says, and this "shrank" 2714
   lines to 4 that reproduce nothing at all.

2. a candidate whose rendered source equals the current best is skipped. after a
   successful removal the cached statement holders can still hold nodes now detached from
   the tree; mutating those re-renders identical source, and the predicate "passes"
   without anything having changed. one run reported 8982 hits against 16 misses with the
   line count frozen.

3. the predicate is checked against a known-failing *and* a known-clean input before any
   shrinking starts. `timeout` does not exist on macos, so a script relying on it once
   answered "OK" for everything.

the tell for all three is the same: **a predicate that never says no is not minimising,
it is deleting**. the hit/miss ratio is printed for that reason — a healthy run is on the
order of 15 hits to 40 misses, not 8982 to 16.
"""

from __future__ import annotations

import argparse
import ast
import os
import shutil
import subprocess
import sys
import tempfile

PYPROJECT = '[project]\nname="s"\nversion="0"\nrequires-python=">=3.13"\n'
BODY_FIELDS = ("body", "orelse", "finalbody")


class Predicate:
    """does `by` still fail the way we are shrinking towards?"""

    def __init__(self, by, python, marker, command, suffix):
        self.by, self.python = by, python
        self.marker, self.command, self.suffix = marker, command, suffix
        self.hits = self.misses = 0

    def __call__(self, source):
        # trap 1: its own directory, holding only this candidate
        work = tempfile.mkdtemp(prefix="shrink-")
        try:
            name = f"m{self.suffix}"
            with open(os.path.join(work, name), "w") as handle:
                handle.write(source)
            with open(os.path.join(work, "pyproject.toml"), "w") as handle:
                handle.write(PYPROJECT)
            env = {**os.environ, "PYTHON": self.python}
            try:
                done = subprocess.run(
                    [self.by, self.command, name],
                    cwd=work,
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=300,
                )
            except subprocess.TimeoutExpired:
                self.misses += 1
                return False
            hit = self.marker in (done.stdout + done.stderr)
            self.hits += hit
            self.misses += not hit
            return hit
        finally:
            shutil.rmtree(work, ignore_errors=True)


def holders(tree):
    """every statement list in the tree, as (list, ) — one entry per body"""
    found = []
    for node in ast.walk(tree):
        for field in BODY_FIELDS:
            body = getattr(node, field, None)
            if (
                isinstance(body, list)
                and body
                and all(isinstance(s, ast.stmt) for s in body)
            ):
                found.append(body)
        for handler in getattr(node, "handlers", None) or []:
            found.append(handler.body)
    return found


def shrink(source, still_fails):
    """greedily drop statements at any depth while the predicate holds"""
    best = source
    changed = True
    rounds = 0
    while changed:
        changed, rounds = False, rounds + 1
        for index in range(len(holders(ast.parse(best)))):
            at = 0
            while True:
                tree = ast.parse(best)
                bodies = holders(tree)
                if index >= len(bodies) or at >= len(bodies[index]):
                    break
                body = bodies[index]
                body.pop(at)
                if not body:
                    body.append(ast.Pass())
                try:
                    candidate = ast.unparse(tree)
                except (ValueError, AttributeError, RecursionError):
                    at += 1
                    continue
                # trap 2: an identical rendering proves nothing, so it is not a trial
                if candidate == best:
                    at += 1
                    continue
                if still_fails(candidate):
                    best = candidate  # the list shifted down, so `at` stays put
                    changed = True
                else:
                    at += 1
        print(f"round {rounds}: {len(best.splitlines())} lines", flush=True)
    return best


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source")
    parser.add_argument("output")
    parser.add_argument("--by", required=True, help="a release-built `by`")
    parser.add_argument("--python", default=sys.executable)
    parser.add_argument("--marker", default="too many cycle iterations")
    parser.add_argument(
        "--command", default="check", help="check | compile | transpile"
    )
    args = parser.parse_args()

    with open(args.source) as handle:
        source = handle.read()
    suffix = os.path.splitext(args.source)[1] or ".py"
    still_fails = Predicate(args.by, args.python, args.marker, args.command, suffix)

    # trap 3: a predicate that cannot say no is not a predicate
    if not still_fails(source):
        sys.exit(f"the unmodified source does not produce {args.marker!r}")
    if still_fails("x = 1\n"):
        sys.exit(
            "a trivial clean file also matches — the predicate is not discriminating"
        )
    print("predicate validated on a failing and a clean input", flush=True)

    # normalise through `unparse` first, so the shrinker's own rendering is the baseline
    # and the first successful removal is not credited to a formatting change
    base = ast.unparse(ast.parse(source))
    if not still_fails(base):
        sys.exit(
            "the source stops failing once reformatted — shrink the original by hand"
        )

    best = shrink(base, still_fails)
    with open(args.output, "w") as handle:
        handle.write(best + "\n")
    print(
        f"final: {len(best.splitlines())} lines "
        f"({still_fails.hits} hits / {still_fails.misses} misses)",
        flush=True,
    )


if __name__ == "__main__":
    main()

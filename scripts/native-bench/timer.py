"""the timed half of the native benchmark suite, run under the target interpreter

`bench.py` stages and builds; this imports what it built and times it. the two
are separate programs because everything here has to happen inside one process:
the whole method rests on the four builds of a benchmark being timed in the same
process, in the same wall-clock window, round by round, so that a load spike
lands on all of them rather than on whichever one happened to be running.

that is possible at all only because each build is staged under a module name of
its own — `mandel_cpython`, `mandel_by`, `mandel_control`, `mandel_mypyc` — since
an extension module's init hook is found by name and two of them cannot answer to
the same one.

nothing here is timed until it has proved what it is. `load` refuses a module
whose file is not under this run's own root, is older than this run's build, or —
for a build that is supposed to be compiled — does not end in a real extension
suffix. a stale artefact and a build that silently did not happen are the two
ways this suite has lied before, and both are refusals rather than warnings.
"""

from __future__ import annotations

import gc
import importlib
import importlib.machinery
import json
import math
import pathlib
import sys
import time
from typing import Any, Protocol, cast

# the interpreter puts *this script's* directory first, and a benchmark that
# shared a name with anything beside it would be shadowed. nothing is imported
# from here, so the entry only costs correctness
sys.path.pop(0)


class Refused(Exception):
    """a build that cannot be proved to be the one that was just made"""


class Bench(Protocol):
    """what this harness needs of a benchmark module

    `load` refuses anything without it, so the modules that reach `sample` and
    the timing loops have it by construction — which `ModuleType` cannot say
    """

    def bench(self) -> object: ...


def load(leg: dict[str, Any], root: str, built_after: float) -> tuple[Bench, str]:
    """import one build, or refuse to"""
    sys.path.insert(0, leg["dir"])
    try:
        module = importlib.import_module(leg["module"])
    except Exception as error:
        raise Refused(f"import failed: {type(error).__name__}: {error}") from error

    origin = getattr(module, "__file__", None)
    if origin is None:
        raise Refused("the module has no __file__, so what ran cannot be identified")
    path = pathlib.Path(origin).resolve()

    if not path.is_relative_to(pathlib.Path(root).resolve()):
        raise Refused(f"imported {path}, which is outside this run's root")
    if path.stat().st_mtime < built_after:
        raise Refused(f"imported {path.name}, which predates this run's build")

    suffixes = tuple(importlib.machinery.EXTENSION_SUFFIXES)
    if leg["compiled"]:
        if not path.name.endswith(suffixes):
            raise Refused(f"imported {path.name}, which is not an extension module")
    elif path.name.endswith(suffixes):
        raise Refused(f"imported {path.name}, which is an extension module")

    if not hasattr(module, "bench"):
        raise Refused("the module has no bench()")
    return cast(Bench, module), str(path)


def sample(module, target: float, ceiling: int) -> tuple[float, int]:
    """one timed sample: however many calls it takes to fill `target` seconds

    the call count is not fixed in advance, and it is not shared between builds.
    it was both, once: one number was calibrated per benchmark from a single
    probe and then handed to every build of it. that is survivable on a quiet
    machine, and it is what most of this suite's published tables were taken
    with, but it leaves two ways for a busy one to ruin a row. a benchmark's four
    builds can be two hundred times apart, so one count cannot suit them — a
    count long enough for the interpreted build left the compiled one timing a
    third of a millisecond, and one descheduling at the scheduler's
    ten-millisecond granularity is then thirty times the reading. and the probe
    was one un-replicated sample taken minutes before the timing, so a machine
    that got busier in between chose the count from a speed the run no longer
    had: a `mandel_inline` count picked against a 16ms interpreted call was still
    in use when that call had become 77ms.

    running each sample to a *duration* removes both. every build's sample is
    long enough by construction, on whatever machine it turns out to be on, and
    nothing about it is decided ahead of time. measured against the count it
    replaced — two runs of one unchanged compiler each way — it left the median
    row where it was and pulled the worst row in from a 50% run-to-run
    disagreement to 8%.

    the clock is read once per chunk rather than once per call, and the next
    chunk is sized from the rate measured so far — so the reading carries no
    per-call timing overhead and reaches the target in a few steps. the growth
    is capped at eightfold a step so that one anomalously quick chunk cannot
    overshoot the target by an order of magnitude.

    `gc.collect()` sits outside the timed region rather than being disabled,
    because disabling it would change what an allocation-heavy benchmark
    measures. quiescing it only moves the collection that was going to happen
    anyway out of whichever build was unlucky enough to trigger it
    """
    gc.collect()
    calls = 0
    chunk = 1
    start = time.perf_counter()
    while True:
        for _ in range(chunk):
            module.bench()
        calls += chunk
        elapsed = time.perf_counter() - start
        if elapsed >= target or calls >= ceiling:
            return elapsed, calls
        # the clock cannot read zero for a chunk that ran a python call, but a
        # rate is being divided by it, so it is floored rather than trusted
        wanted = math.ceil((target - elapsed) * calls / max(elapsed, 1e-9))
        chunk = max(1, min(chunk * 8, wanted, ceiling - calls))


def main() -> int:
    spec = json.loads(pathlib.Path(sys.argv[1]).read_text())
    root, built_after = spec["root"], spec["built_after"]

    loaded: list[tuple[str, Bench]] = []
    refused: dict[str, str] = {}
    answers: dict[str, str] = {}
    origins: dict[str, str] = {}
    for leg in spec["legs"]:
        try:
            module, origin = load(leg, root, built_after)
        except Refused as refusal:
            refused[leg["name"]] = str(refusal)
            continue
        loaded.append((leg["name"], module))
        origins[leg["name"]] = origin

    if spec["mode"] == "probe":
        for name, module in loaded:
            try:
                answers[name] = repr(module.bench())
            except Exception as error:
                refused[name] = f"bench() raised {type(error).__name__}: {error}"
        print(json.dumps({"answers": answers, "refused": refused, "origins": origins}))
        return 0

    target, ceiling = spec["sample_target"], spec["max_calls"]
    rounds, warmup = spec["rounds"], spec["warmup"]
    order = [name for name, _ in loaded]
    modules = dict(loaded)
    timings: dict[str, list[float]] = {name: [] for name in order}
    counts: dict[str, list[int]] = {name: [] for name in order}

    for index in range(warmup + rounds):
        # rotate, so no build is systematically the one that pays for a cold
        # cache at the top of a round. deterministic rather than random: a
        # benchmark run should be reproducible
        shift = index % len(order)
        for name in order[shift:] + order[:shift]:
            elapsed, calls = sample(modules[name], target, ceiling)
            if index >= warmup:
                timings[name].append(elapsed / calls)
                counts[name].append(calls)

    print(
        json.dumps(
            {
                "timings": timings,
                "refused": refused,
                "origins": origins,
                "counts": counts,
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

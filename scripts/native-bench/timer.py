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

# the interpreter puts *this script's* directory first, and a benchmark that
# shared a name with anything beside it would be shadowed. nothing is imported
# from here, so the entry only costs correctness
sys.path.pop(0)


class Refused(Exception):
    """a build that cannot be proved to be the one that was just made"""


def load(leg: dict, root: str, built_after: float):
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
    return module, str(path)


def sample(module, count: int) -> float:
    """one timed sample: `count` calls, with the collector quiesced first

    `gc.collect()` sits outside the timed region rather than being disabled,
    because disabling it would change what an allocation-heavy benchmark
    measures. quiescing it only moves the collection that was going to happen
    anyway out of whichever build was unlucky enough to trigger it
    """
    gc.collect()
    start = time.perf_counter()
    for _ in range(count):
        module.bench()
    return time.perf_counter() - start


def main() -> int:
    spec = json.loads(pathlib.Path(sys.argv[1]).read_text())
    root, built_after = spec["root"], spec["built_after"]

    loaded, refused, answers, origins = [], {}, {}, {}
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

    if spec["mode"] == "calibrate":
        # how many calls make one sample. every build of a benchmark runs the
        # same number, so the pairing stays exact — which means one number has
        # to suit builds that can be forty times apart. it is chosen from both
        # ends: long enough that the fastest build's sample is well clear of the
        # clock, short enough that the slowest build's round does not dominate
        # the run. where the two disagree the cap wins, and the control column
        # then says out loud whether the resulting sample was too short
        per_call = {}
        for name, module in loaded:
            count = 1
            while True:
                elapsed = sample(module, count)
                if elapsed >= spec["probe_target"] or count >= spec["max_count"]:
                    break
                grow = max(2, min(8, int(spec["probe_target"] / max(elapsed, 1e-9))))
                count = min(count * grow, spec["max_count"])
            per_call[name] = elapsed / count

        fastest, slowest = min(per_call.values()), max(per_call.values())
        wanted = math.ceil(spec["min_sample"] / fastest)
        cap = max(1, int(spec["max_sample"] / slowest))
        count = max(1, min(wanted, cap, spec["max_count"]))
        print(json.dumps({"count": count, "per_call": per_call, "refused": refused}))
        return 0

    count, rounds, warmup = spec["count"], spec["rounds"], spec["warmup"]
    order = [name for name, _ in loaded]
    modules = dict(loaded)
    timings = {name: [] for name in order}

    for index in range(warmup + rounds):
        # rotate, so no build is systematically the one that pays for a cold
        # cache at the top of a round. deterministic rather than random: a
        # benchmark run should be reproducible
        shift = index % len(order)
        for name in order[shift:] + order[:shift]:
            elapsed = sample(modules[name], count)
            if index >= warmup:
                timings[name].append(elapsed / count)

    print(
        json.dumps(
            {
                "timings": timings,
                "refused": refused,
                "origins": origins,
                "count": count,
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

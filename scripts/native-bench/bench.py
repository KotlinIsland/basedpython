"""the native benchmark suite: stage, build, prove, time, compare

    scripts/native-bench/bench.py                    # everything
    scripts/native-bench/bench.py mandel dot         # only these
    scripts/native-bench/bench.py --json today.json
    scripts/native-bench/bench.py --baseline last-week.json

the method it enforces is written up in
`docs/basedpython/development/compilation/benchmarks.md`. the short version:

- **the ratio is the measurement.** absolute times on this suite move by a
  factor of four with machine load, so nothing is reported from one build's
  clock alone. every ratio is *paired* — the four builds of a benchmark are
  timed in one process, round by round, so a spike lands on all four and mostly
  cancels in the quotient
- **the table carries its own error bar.** every benchmark is built twice by the
  same compiler, and the second build is timed alongside the first. two builds
  of the same source are the same program, so the ratio between them is the
  suite's noise floor for that benchmark, measured rather than assumed. a
  difference smaller than the floor is not a difference
- **nothing is timed until it has proved what it is.** a compiled build has to
  import as a real extension module, from a file inside this run's own root,
  newer than this run's build, and it has to return the same answer as the
  interpreted one. every one of those is a refusal rather than a warning
- **a decline is a failure, not a footnote.** `programs.toml` records how many
  functions each benchmark is expected to leave interpreted, and a run where the
  count moved either way fails. otherwise a benchmark can quietly stop measuring
  compiled code and go on posting numbers
- **the harness cannot match nothing.** an unknown benchmark name, a program
  with no manifest entry, a manifest entry with no program, an empty selection:
  all of them exit non-zero. a measurement harness that cannot fail loudly is
  worse than none
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
PROGRAMS = HERE / "programs"
MANIFEST = HERE / "programs.toml"

# how long one sample runs. each build fills this for itself, by calling
# `bench()` until the time is up, so the number of calls differs between builds
# and is not decided before the run — see `timer.sample`
#
# the size is set by the scheduler rather than by the clock. `perf_counter`
# resolves nanoseconds, and on a quiet machine this suite read a ±0.9% floor off
# a 0.9ms sample, so shortness on its own is not the problem. what it has to
# swamp is a descheduling, which comes back roughly a ten-millisecond quantum
# late: at 50ms that is a 20% outlier the median absorbs, and at the 0.3ms this
# suite used to hand its compiled builds it was a factor of thirty. so the
# target buys the tail rather than the median
SAMPLE_TARGET = 0.050
MAX_CALLS = 1_000_000

# below this the median's confidence interval degenerates to the full range of
# what was seen, which is not a confidence interval. a table built from three
# rounds is how this suite published 0.2x for something that was 0.9x
MIN_ROUNDS = 9

# the control bounds the noise *within* a run: same process, same moment, same
# memory layout. it does not bound the drift *between* two runs, which is larger
# and which is what a baseline comparison is actually up against — two runs of
# an unchanged compiler, forty minutes apart on a 16-core laptop, disagreed by
# 8.4% on `words` while each reported a ±0.8% floor for itself. so the default
# bar for calling a change a change is set from that measurement rather than
# from the within-run floor. tighten it on a machine that does nothing else
BETWEEN_RUN_DRIFT = 0.10


class Failure(Exception):
    """something the run cannot honestly continue past"""


def digest(path: Path) -> str:
    """what the compiler being measured actually is, as a hash of its bytes

    a version string and a git description are both about the checkout, not
    about the file on disk, and both go on saying the same thing while the
    binary underneath them is rebuilt — or is not rebuilt when it should have
    been. an ablation harness here once compared a build against itself because
    two paths resolved to one file, and the honest reading of that data was the
    opposite of what was concluded from it. this is what makes that visible
    """
    hashed = hashlib.sha256()
    with path.open("rb") as handle:
        while block := handle.read(1 << 20):
            hashed.update(block)
    return hashed.hexdigest()


def load() -> str:
    """what else the machine is doing, recorded at both ends of the run and
    again before each row is timed

    it does not change any decision the harness makes — the noise floor does
    that, and it measures the effect rather than guessing at it. this is here so
    that a table someone kept can be read back with the reason it was wide
    """
    try:
        return f"{os.getloadavg()[0]:.2f}"
    except (OSError, AttributeError):
        return "unknown"


# ── statistics ───────────────────────────────────────────────────────────────
#
# the median throughout, never the mean and never the minimum. the mean is moved
# by the one round that hit a scheduler; the minimum is an extreme-value
# statistic whose expectation depends on how many rounds were run and on how
# quiet the machine happened to be, so it is not comparable between runs — which
# is the only thing this suite is for.


def median_interval(values: list[float], alpha: float = 0.05) -> tuple[float, float]:
    """a distribution-free confidence interval for the median

    the order statistics either side of the middle, chosen from the binomial
    tail. exact, assumes nothing about the shape of the noise, needs no
    resampling and no random numbers — so two runs over the same data agree
    """
    ordered = sorted(values)
    n = len(ordered)
    if n < 6:
        return ordered[0], ordered[-1]
    cumulative, k = 0.0, 0
    for i in range(n + 1):
        p = math.comb(n, i) / 2**n
        if cumulative + p > alpha / 2:
            break
        cumulative += p
        k = i + 1
    if k == 0:
        return ordered[0], ordered[-1]
    return ordered[k - 1], ordered[n - k]


@dataclass
class Ratio:
    """a paired ratio and how sure of it the run is"""

    median: float
    low: float
    high: float

    @property
    def spread(self) -> float:
        """half the interval, as a fraction of the median"""
        return (self.high - self.low) / 2 / self.median if self.median else 0.0

    def render(self, places: int = 2) -> str:
        return f"{self.median:.{places}f}x ±{self.spread * 100:.1f}%"

    def as_json(self) -> dict[str, float]:
        return {"median": self.median, "low": self.low, "high": self.high}


def ratio_json(result: Result, leg: str) -> dict[str, float] | None:
    """a leg's paired ratio as json, or `None` where there was no pairing

    the three json rows all read a ratio that may not exist — a leg the run did not
    time has none — and two of the three used to read it without asking. one place
    to forget rather than three
    """
    ratio = result.ratio(leg)
    return ratio.as_json() if ratio else None


def paired(numerator: list[float], denominator: list[float]) -> Ratio:
    """the ratio of two builds, round by round rather than time against time

    dividing one build's median by another's would let a load spike that only
    landed on one of them through unchallenged. these two lists are the same
    rounds, so the spike is in both quotients and cancels
    """
    ratios = [a / b for a, b in zip(numerator, denominator, strict=True)]
    low, high = median_interval(ratios)
    return Ratio(statistics.median(ratios), low, high)


# ── the manifest ─────────────────────────────────────────────────────────────


@dataclass
class Program:
    name: str
    group: str
    measures: str
    declines: int
    mypyc: bool


def load_manifest(selected: list[str]) -> list[Program]:
    """read `programs.toml`, and refuse to run against a corpus that has drifted

    the two lists — the files on disk and the entries in the manifest — are
    meant to agree exactly. a file with no entry has no declared decline count
    and so is unguarded; an entry with no file quietly measures nothing
    """
    entries = tomllib.loads(MANIFEST.read_text())["programs"]
    on_disk = {path.stem for path in PROGRAMS.glob("*.py")}
    declared = set(entries)

    if undeclared := on_disk - declared:
        raise Failure(
            f"programs with no manifest entry: {', '.join(sorted(undeclared))}"
        )
    if missing := declared - on_disk:
        raise Failure(f"manifest entries with no program: {', '.join(sorted(missing))}")

    if selected:
        if unknown := set(selected) - declared:
            raise Failure(f"no such benchmark: {', '.join(sorted(unknown))}")
        wanted = [name for name in entries if name in set(selected)]
    else:
        wanted = list(entries)

    if not wanted:
        raise Failure("nothing selected")

    return [
        Program(
            name=name,
            group=entries[name]["group"],
            measures=entries[name]["measures"],
            declines=entries[name]["declines"],
            mypyc=entries[name].get("mypyc", True),
        )
        for name in wanted
    ]


# ── staging and building ─────────────────────────────────────────────────────


@dataclass
class Leg:
    """one build of one benchmark, and everything known about it"""

    name: str
    module: str
    compiled: bool
    directory: Path
    built: bool = False
    error: str | None = None
    log: Path | None = None
    declines: list[str] = field(default_factory=list)

    def spec(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "module": self.module,
            "compiled": self.compiled,
            "dir": str(self.directory),
        }


def stage(root: Path, program: Program, leg: str, python_version: str) -> Path:
    """lay one build out as a project of its own, under a name of its own

    the name is what makes the whole method possible: four builds of the same
    benchmark have to coexist in one process to be timed against each other, and
    an extension module's init hook is found by its name, so they cannot share
    one
    """
    directory = root / program.name / leg
    (directory / "dist").mkdir(parents=True)
    module = f"{program.name}_{leg}"
    shutil.copy(PROGRAMS / f"{program.name}.py", directory / f"{module}.py")
    (directory / "pyproject.toml").write_text(
        f'[project]\nname = "bench"\nversion = "0"\n'
        f'requires-python = ">={python_version}"\n\n'
        # a `float` annotation admits an `int` under python's numeric promotion,
        # so without this every float place holds `int | float` and cannot be
        # unboxed. mypyc reads a `float` annotation as a machine double, so this
        # is what makes the two comparable at all — and it is recorded in the
        # run's metadata because it changes the numbers
        f"[tool.ty.analysis]\nstrict-float = true\n\n"
        f'[tool.ty.environment]\npython-version = "{python_version}"\n'
    )
    return directory


def build_by(
    by: Path, directory: Path, module: str, python: str
) -> tuple[bool, str | None, list[str]]:
    """compile one build, and read back what it refused to compile

    the decline list comes from the `--annotate` report rather than from the
    diagnostics: the diagnostic renderer wraps and truncates, and a count taken
    from it has been wrong before
    """
    log = directory / "build.log"
    result = subprocess.run(
        [str(by), "compile", f"{module}.py", "-o", "out", "--annotate"],
        cwd=directory,
        env={**os.environ, "PYTHON": python},
        capture_output=True,
        text=True,
    )
    log.write_text(result.stdout + result.stderr)
    if result.returncode != 0:
        return False, f"`by compile` exited {result.returncode}", []

    declines = []
    for report in (directory / "out").glob("*.annotated"):
        section = report.read_text().partition("## left to the interpreted definition")[
            2
        ]
        for line in section.partition("\n##")[0].splitlines():
            if line.startswith("- "):
                declines.append(line[2:])

    artefacts = [
        p
        for p in (directory / "out").iterdir()
        if p.suffix in {".so", ".pyd", ".dylib"}
    ]
    if not artefacts:
        return False, "`by compile` succeeded but left no extension module", declines
    for artefact in artefacts:
        shutil.copy(artefact, directory / "dist" / artefact.name)
    return True, None, declines


def build_mypyc(directory: Path, module: str, python: str) -> tuple[bool, str | None]:
    """compile one build with mypyc, and say so out loud when it will not

    the previous harness sent this to /dev/null, and a run in which mypyc was
    simply broken was read as mypyc being unable to compile the program
    """
    log = directory / "build.log"
    result = subprocess.run(
        [
            "uv",
            "run",
            "--no-project",
            "--with",
            "mypy",
            "--with",
            "setuptools",
            "--python",
            python,
            "mypyc",
            f"{module}.py",
        ],
        cwd=directory,
        capture_output=True,
        text=True,
    )
    log.write_text(result.stdout + result.stderr)
    artefacts = [
        p for p in directory.iterdir() if p.suffix in {".so", ".pyd", ".dylib"}
    ]
    if not artefacts:
        return (
            False,
            f"mypyc left no extension module (exit {result.returncode}); log at {log}",
        )
    for artefact in artefacts:
        shutil.copy(artefact, directory / "dist" / artefact.name)
    return True, None


# ── driving the timer ────────────────────────────────────────────────────────


def drive(python: str, spec: dict[str, Any], work: Path, tag: str) -> dict[str, Any]:
    path = work / f"spec-{tag}.json"
    path.write_text(json.dumps(spec))
    result = subprocess.run(
        [python, str(HERE / "timer.py"), str(path)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise Failure(
            f"the timer failed for {tag} (exit {result.returncode}):\n{result.stderr}"
        )
    return json.loads(result.stdout)


# ── the run ──────────────────────────────────────────────────────────────────


@dataclass
class Result:
    program: Program
    status: str
    legs: dict[str, Leg]
    times: dict[str, list[float]] = field(default_factory=dict)
    calls: dict[str, int] = field(default_factory=dict)
    # what the machine was doing while *this row* was timed. the run-level
    # reading is taken at the two ends and a run is minutes long: one of these
    # runs began at load 26 and finished at 193, so its early rows and its late
    # rows were measured on what amounted to two different machines, and the
    # table gave no way to tell which was which
    load: str = "unknown"
    notes: list[str] = field(default_factory=list)

    def median(self, leg: str) -> float | None:
        values = self.times.get(leg)
        return statistics.median(values) if values else None

    def ratio(self, leg: str) -> Ratio | None:
        if leg in self.times and "by" in self.times:
            return paired(self.times[leg], self.times["by"])
        return None

    @property
    def noise(self) -> float | None:
        """what the suite reported for two builds that are the same program

        both how far the control landed from 1.00 and how far it wandered: a
        build that reads consistently 2% away from its own twin is as much a
        problem as one that reads all over the place
        """
        control = self.ratio("control")
        if control is None:
            return None
        return max(abs(control.median - 1), control.spread)

    def noisy(self, limit: float) -> bool:
        return self.noise is not None and self.noise > limit


def run_program(
    args,
    program: Program,
    root: Path,
    python: str,
    python_version: str,
    built_after: float,
) -> Result:
    legs: dict[str, Leg] = {}

    directory = stage(root, program, "cpython", python_version)
    shutil.copy(
        directory / f"{program.name}_cpython.py",
        directory / "dist" / f"{program.name}_cpython.py",
    )
    legs["cpython"] = Leg(
        "cpython", f"{program.name}_cpython", False, directory / "dist", built=True
    )

    # `control` is a second, independent compile of the same source. it is the
    # null: two builds of one program are one program, so whatever ratio the
    # suite reports between them is noise it invented. that number is printed on
    # every row, and nothing smaller than it is a finding
    for leg in ("by", "control"):
        directory = stage(root, program, leg, python_version)
        module = f"{program.name}_{leg}"
        built, error, declines = build_by(args.by, directory, module, python)
        legs[leg] = Leg(
            leg,
            module,
            True,
            directory / "dist",
            built,
            error,
            directory / "build.log",
            declines,
        )

    if program.mypyc and not args.no_mypyc:
        directory = stage(root, program, "mypyc", python_version)
        module = f"{program.name}_mypyc"
        built, error = build_mypyc(directory, module, python)
        legs["mypyc"] = Leg(
            "mypyc",
            module,
            True,
            directory / "dist",
            built,
            error,
            directory / "build.log",
        )

    result = Result(program, "ok", legs)

    # a decline that nobody declared means the row below is not measuring what
    # its name says. exact match in both directions: a compiler that started
    # compiling something is as much a change as one that stopped
    found = len(legs["by"].declines)
    if legs["by"].built and found != program.declines:
        result.status = "declines"
        result.notes.append(
            f"expected {program.declines} declined function(s), found {found}"
            + (": " + "; ".join(legs["by"].declines) if legs["by"].declines else "")
        )
        return result

    for leg in legs.values():
        if not leg.built:
            if leg.name in ("by", "control"):
                result.status = "build"
                result.notes.append(f"{leg.name}: {leg.error}")
                return result
            result.notes.append(f"{leg.name}: {leg.error}")

    live = [leg for leg in legs.values() if leg.built]
    base = {
        "root": str(root),
        "built_after": built_after,
        "legs": [leg.spec() for leg in live],
    }

    probe = drive(python, {**base, "mode": "probe"}, root, f"{program.name}-probe")
    for name, why in probe["refused"].items():
        if name in ("by", "control", "cpython"):
            result.status = "refused"
            result.notes.append(f"{name}: {why}")
            return result
        result.notes.append(f"{name}: {why}")
        live = [leg for leg in live if leg.name != name]

    # a build that got a different answer is not a faster build. `bigint` leans
    # on this: a compiler that wrapped at 64 bits fails here rather than posting
    # a very good time
    expected = probe["answers"]["cpython"]
    for name, answer in probe["answers"].items():
        if answer != expected:
            result.status = "disagrees"
            result.notes.append(
                f"{name} answered {answer}, cpython answered {expected}"
            )
            return result

    if args.verify_only:
        return result

    base["legs"] = [leg.spec() for leg in live]
    result.load = load()
    timed = drive(
        python,
        {
            **base,
            "mode": "time",
            "sample_target": SAMPLE_TARGET,
            "max_calls": MAX_CALLS,
            "rounds": args.rounds,
            "warmup": args.warmup,
        },
        root,
        f"{program.name}-time",
    )
    result.times = timed["timings"]
    # kept for the record rather than for any decision: a build whose median
    # sample was a single call is one whose `bench()` already runs longer than
    # the target, which is worth being able to read back off an old run
    result.calls = {
        name: int(statistics.median(values))
        for name, values in timed["counts"].items()
        if values
    }
    return result


# ── reporting ────────────────────────────────────────────────────────────────


def render(
    results: list[Result], metadata: dict[str, Any], show_declines: bool, limit: float
):
    header = (
        f"{'benchmark':<15}{'group':<10}{'cpython':>10}{'by':>10}{'mypyc':>10}"
        f"  {'vs cpython':>15}{'vs mypyc':>16}{'noise':>10}{'dec':>5}"
    )
    print()
    print(f"python  {metadata['python']}  ({metadata['implementation']})")
    print(f"by      {metadata['by_version']}  from {metadata['git']}")
    print(f"        {metadata['by_sha256'][:16]}  {metadata['by_path']}")
    print(f"mypy    {metadata['mypy'] or 'unavailable'}")
    print(
        f"host    {metadata['host']}, {metadata['cpus']} cpus, load {metadata['load_before']}"
    )
    print(
        f"method  {metadata['rounds']} rounds after {metadata['warmup']} warmup, "
        f"{metadata['sample_target'] * 1000:.0f}ms samples, paired medians, strict-float on"
    )
    print()
    print(header)
    print("-" * len(header))

    for result in results:
        name, group = result.program.name, result.program.group
        if result.status != "ok":
            print(
                f"{name:<15}{group:<10}  {result.status.upper()}: {'; '.join(result.notes)}"
            )
            continue
        cpython, by = result.median("cpython"), result.median("by")
        mypyc = result.median("mypyc")
        against_cpython = result.ratio("cpython")
        against_mypyc = result.ratio("mypyc")
        noise = result.noise
        # `ok` says every leg ran, not that every leg was *timed* — a leg whose samples
        # were all discarded leaves no median behind. rather than assume the status
        # covers it, say so in the row: a blank number is a measurement nobody has,
        # which is not the same as a slow one
        if cpython is None or by is None or noise is None:
            print(f"{name:<15}{group:<10}  no timing recorded")
            continue
        print(
            f"{name:<15}{group:<10}"
            f"{cpython * 1000:>9.2f}m{by * 1000:>9.2f}m"
            f"{(f'{mypyc * 1000:.2f}m' if mypyc else '-'):>10}"
            f"  {(against_cpython.render() if against_cpython else '-'):>15}"
            f"{(against_mypyc.render() if against_mypyc else '-'):>16}"
            f"{f'±{noise * 100:.1f}%' + ('!' if result.noisy(limit) else ''):>10}"
            f"{len(result.legs['by'].declines):>5}"
        )
        if result.noisy(limit):
            print(f"{'':<25}note: load was {result.load} while this row was timed")
        for note in result.notes:
            print(f"{'':<25}note: {note}")

    print()
    floors = [r.noise for r in results if r.status == "ok" and r.noise is not None]
    if floors:
        print(
            f"noise floor: median ±{statistics.median(floors) * 100:.2f}%, "
            f"worst ±{max(floors) * 100:.2f}% — a change smaller than a row's own "
            f"floor is not a change"
        )
    failed = [r for r in results if r.status != "ok"]
    noisy = [r for r in results if r.status == "ok" and r.noisy(limit)]
    print(
        f"{len(results) - len(failed) - len(noisy)}/{len(results)} benchmarks measured"
    )
    if noisy:
        # the row's two identical builds disagreed by more than the limit, so
        # whatever else it says, it is not a measurement of anything. marked
        # rather than deleted: the numbers are still worth a glance, and the
        # `!` is there to stop one being quoted
        print(
            f"{len(noisy)} too noisy to trust (marked `!`): "
            f"{', '.join(r.program.name for r in noisy)} — rerun on a quieter machine"
        )

    if show_declines:
        for result in results:
            if result.legs.get("by") and result.legs["by"].declines:
                print(f"\n{result.program.name} declines:")
                for decline in result.legs["by"].declines:
                    print(f"  {decline}")


def render_verification(results: list[Result]):
    """everything the suite checks that does not involve a clock

    this half is deterministic, so it is the half that can run anywhere — a
    shared runner cannot time anything, but it can tell you that a benchmark
    stopped compiling, or that the compiled build started answering differently
    """
    print()
    for result in results:
        declines = len(result.legs["by"].declines) if "by" in result.legs else 0
        mark = "ok " if result.status == "ok" else result.status.upper()
        print(
            f"{mark:<10}{result.program.name:<15}{declines:>3} declined"
            + (f"   {'; '.join(result.notes)}" if result.notes else "")
        )
    failed = [r for r in results if r.status != "ok"]
    print(f"\n{len(results) - len(failed)}/{len(results)} verified")


def compare(
    results: list[Result], baseline: dict[str, Any], threshold: float, limit: float
) -> bool:
    """a regression is detected rather than eyeballed

    the quantity compared between runs is the speedup, not the time: absolute
    times are not comparable across machines or across a Tuesday, and this suite
    exists because they were compared anyway
    """
    print("\nagainst the baseline")
    for key in ("python", "implementation", "host"):
        if baseline["metadata"].get(key) != CURRENT[key]:
            print(
                f"  warning: {key} was {baseline['metadata'].get(key)!r} then and is "
                f"{CURRENT[key]!r} now — the two runs are not comparable"
            )

    # which compiler each run measured, said out loud rather than inferred from
    # the paths that were typed. a comparison of one binary against itself is a
    # null experiment and every row of it is noise — that is a useful thing to
    # run deliberately, and a disastrous one to run by accident, and the two look
    # identical from the outside. this suite's ancestor did run it by accident
    was = baseline["metadata"].get("by_sha256")
    if was is None:
        print("  the baseline did not record which compiler it measured")
    elif was == CURRENT["by_sha256"]:
        print(
            f"  both runs measured the same compiler ({was[:16]}), so every row "
            f"below is this machine's noise and nothing else"
        )
    else:
        print(f"  compiler {was[:16]} then, {CURRENT['by_sha256'][:16]} now")

    print(f"  {'benchmark':<15}{'then':>12}{'now':>12}{'change':>12}   verdict")
    regressed = False
    for result in results:
        previous = baseline["benchmarks"].get(result.program.name)
        if result.status != "ok" or previous is None or previous.get("status") != "ok":
            continue
        against_cpython = result.ratio("cpython")
        new_noise = result.noise
        # the same gap as in the table above: `ok` does not promise a timing survived.
        # a row with nothing to compare is left out of the comparison rather than
        # compared against a number that is not there
        if against_cpython is None or new_noise is None:
            continue
        old = previous["vs_cpython"]["median"]
        new = against_cpython.median
        change = new / old - 1
        old_noise = previous["noise"]
        # a row either run could not measure is skipped rather than compared
        # with a very wide bar. a pair of identical builds that disagreed by 70%
        # says the machine was preempting samples, and under preemption the
        # longer sample loses more — so the paired ratio does not merely get
        # noisier, it drifts upwards. widening the bar does not fix a bias
        if max(old_noise, new_noise) > limit:
            print(
                f"  {result.program.name:<15}{old:>11.2f}x{new:>11.2f}x"
                f"{change * 100:>11.1f}%   skipped: ±{max(old_noise, new_noise) * 100:.0f}% "
                f"noise in one of the two runs"
            )
            continue
        # the bar is whichever is larger: the noise the two runs measured for
        # themselves, or the floor asked for on the command line
        bar = max(old_noise + new_noise, threshold)
        if change < -bar:
            verdict, regressed = "REGRESSED", True
        elif change > bar:
            verdict = "improved"
        else:
            verdict = f"same (within ±{bar * 100:.1f}%)"
        print(
            f"  {result.program.name:<15}{old:>11.2f}x{new:>11.2f}x"
            f"{change * 100:>11.1f}%   {verdict}"
        )
    return regressed


CURRENT: dict[str, Any] = {}


# ── the harness's check on itself ────────────────────────────────────────────


def self_check(by: Path, python: str, python_version: str) -> int:
    """build each way this suite has lied before, and prove it is refused now

    an ablation harness here once matched nothing and reported that every edit
    cost nothing, and the reason it was believed is that a harness which cannot
    fail looks exactly like a harness that found nothing wrong. so the refusals
    are exercised rather than asserted, by handing the timer a leg that is
    wrong in each of the ways a leg has actually been wrong
    """
    root = Path(tempfile.mkdtemp(prefix="native-bench-selfcheck-"))
    failures = []

    def expect(what: str, refusal: str | None, wanted: str):
        if refusal is None:
            failures.append(f"{what}: was accepted, and should have been refused")
        elif wanted not in refusal:
            failures.append(f"{what}: refused with {refusal!r}, wanted {wanted!r}")
        else:
            print(f"  refused {what}: {refusal}")

    def refusal_for(leg: dict[str, Any], built_after: float = 0.0) -> str | None:
        answer = drive(
            python,
            {
                "mode": "probe",
                "root": str(root),
                "built_after": built_after,
                "legs": [leg],
            },
            root,
            "selfcheck",
        )
        return answer["refused"].get(leg["name"])

    # a real extension module to be wrong about
    stage = root / "real"
    (stage / "dist").mkdir(parents=True)
    (stage / "canary.py").write_text("def bench() -> int:\n    return 1\n")
    (stage / "pyproject.toml").write_text(
        f'[project]\nname = "c"\nversion = "0"\nrequires-python = ">={python_version}"\n'
    )
    built, error, _ = build_by(by, stage, "canary", python)
    if not built:
        print(
            f"error: the self-check could not build its own canary: {error}",
            file=sys.stderr,
        )
        return 2

    print("the guards:")
    expect(
        "an interpreted build passed off as a compiled one",
        refusal_for(
            {"name": "by", "module": "canary", "compiled": True, "dir": str(stage)}
        ),
        "not an extension module",
    )
    expect(
        "a compiled build passed off as an interpreted one",
        refusal_for(
            {
                "name": "cpython",
                "module": "canary",
                "compiled": False,
                "dir": str(stage / "dist"),
            }
        ),
        "is an extension module",
    )
    # the stale-output-directory failure: the build did not happen, and the
    # artefact left over from last time answered in its place
    expect(
        "an artefact older than this run's build",
        refusal_for(
            {
                "name": "by",
                "module": "canary",
                "compiled": True,
                "dir": str(stage / "dist"),
            },
            built_after=os.stat(stage).st_mtime + 3600,
        ),
        "predates this run's build",
    )
    outside = Path(tempfile.mkdtemp(prefix="native-bench-outside-"))
    for artefact in (stage / "dist").glob("canary*"):
        shutil.copy(artefact, outside / artefact.name)
    expect(
        "an artefact from outside this run's root",
        refusal_for(
            {"name": "by", "module": "canary", "compiled": True, "dir": str(outside)}
        ),
        "outside this run's root",
    )
    expect(
        "a build that is not there at all",
        refusal_for(
            {
                "name": "by",
                "module": "absent",
                "compiled": True,
                "dir": str(stage / "dist"),
            }
        ),
        "import failed",
    )

    # and the corpus guards, which are what stops a run measuring nothing
    for what, selection, wanted in (
        ("an unknown benchmark name", ["nosuchbenchmark"], "no such benchmark"),
    ):
        try:
            _ = load_manifest(selection)
            failures.append(f"{what}: was accepted")
        except Failure as failure:
            if wanted not in str(failure):
                failures.append(f"{what}: refused with {failure!r}")
            else:
                print(f"  refused {what}: {failure}")

    shutil.rmtree(root, ignore_errors=True)
    shutil.rmtree(outside, ignore_errors=True)
    for failure in failures:
        print(f"error: {failure}", file=sys.stderr)
    print(
        "\nevery guard held"
        if not failures
        else f"\n{len(failures)} guard(s) did not hold"
    )
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "programs", nargs="*", help="benchmarks to run (default: all of them)"
    )
    parser.add_argument(
        "--by",
        type=Path,
        default=ROOT / "target" / "release" / "by",
        help="the compiler to measure (default: this checkout's release build)",
    )
    parser.add_argument(
        "--python",
        default="3.13",
        help="interpreter to build against and run: a version for `uv python find`, or a path",
    )
    parser.add_argument(
        "--rounds",
        type=int,
        default=21,
        help="timed rounds per benchmark (default: 21)",
    )
    parser.add_argument(
        "--warmup", type=int, default=3, help="untimed rounds first (default: 3)"
    )
    parser.add_argument("--no-mypyc", action="store_true", help="skip the mypyc build")
    parser.add_argument(
        "--declines", action="store_true", help="list every declined function"
    )
    parser.add_argument(
        "--json", type=Path, help="write the full result for a later comparison"
    )
    parser.add_argument(
        "--baseline", type=Path, help="compare against a previous --json"
    )
    parser.add_argument(
        "--regression-threshold",
        type=float,
        default=BETWEEN_RUN_DRIFT,
        help=f"the smallest change worth calling one, when noise is smaller "
        f"(default: {BETWEEN_RUN_DRIFT})",
    )
    parser.add_argument(
        "--noise-limit",
        type=float,
        default=0.10,
        help="the noise a row may show and still count as measured (default: 0.10)",
    )
    parser.add_argument(
        "--keep", action="store_true", help="keep the build tree and say where it is"
    )
    parser.add_argument(
        "--self-check",
        action="store_true",
        help="prove the guards refuse what they are meant to, and run nothing else",
    )
    parser.add_argument(
        "--verify-only",
        action="store_true",
        help="build, prove and check declines, but time nothing (deterministic, so it runs anywhere)",
    )
    args = parser.parse_args()

    if args.rounds < MIN_ROUNDS:
        raise Failure(
            f"--rounds {args.rounds} is not a measurement: below {MIN_ROUNDS} rounds the "
            "confidence interval degenerates to the range of what was seen"
        )

    # self-check returns before anything reads this, but an empty list rather than an
    # unbound name says that here instead of leaving it to be inferred from control flow
    programs: list[Program] = []
    if not args.self_check:
        programs = load_manifest(args.programs)

    if not args.by.is_file() or not os.access(args.by, os.X_OK):
        raise Failure(
            f"no compiler at {args.by} — build one with `cargo build --release --bin by`.\n"
            "a debug build is far too slow to measure anything with"
        )

    if Path(args.python).is_absolute():
        python = args.python
    else:
        found = subprocess.run(
            ["uv", "python", "find", args.python], capture_output=True, text=True
        )
        if found.returncode != 0:
            raise Failure(f"no interpreter for {args.python!r}: {found.stderr.strip()}")
        python = found.stdout.strip()

    probe = subprocess.run(
        [
            python,
            "-c",
            "import platform,sys;"
            "print(platform.python_version());print(platform.python_implementation());"
            "print(sys.version.split()[0])",
        ],
        capture_output=True,
        text=True,
    )
    if probe.returncode != 0:
        raise Failure(f"{python} does not run: {probe.stderr.strip()}")
    # the probe below prints three fields, but a python that printed something else
    # would unpack into an unhelpful ValueError here rather than say what it answered
    fields = probe.stdout.split()
    if len(fields) < 2:
        raise Failure(
            f"{python} answered {probe.stdout.strip()!r}, "
            "not a version and an implementation"
        )
    full_version, implementation = fields[0], fields[1]
    python_version = ".".join(full_version.split(".")[:2])

    if args.self_check:
        return self_check(args.by, python, python_version)

    version = subprocess.run(
        [str(args.by), "--version"], capture_output=True, text=True
    ).stdout.strip()
    git = subprocess.run(
        ["git", "-C", str(ROOT), "describe", "--always", "--dirty"],
        capture_output=True,
        text=True,
    ).stdout.strip()
    mypy = "skipped" if args.no_mypyc else None
    if not args.no_mypyc:
        found = subprocess.run(
            [
                "uv",
                "run",
                "--no-project",
                "--with",
                "mypy",
                "--python",
                python,
                "mypy",
                "--version",
            ],
            capture_output=True,
            text=True,
        )
        mypy = found.stdout.strip() if found.returncode == 0 else None
        if mypy is None:
            print(
                f"warning: mypyc is unavailable, so nothing will be compared against it\n{found.stderr}",
                file=sys.stderr,
            )

    CURRENT.update(
        {
            "python": full_version,
            "implementation": implementation,
            "by_version": version,
            "by_sha256": digest(args.by),
            "by_path": str(args.by),
            "git": git or "unknown",
            "mypy": mypy,
            "host": f"{platform.system()} {platform.machine()}",
            "cpus": os.cpu_count(),
            "load_before": load(),
            "sample_target": SAMPLE_TARGET,
            "rounds": args.rounds,
            "warmup": args.warmup,
            "strict_float": True,
        }
    )

    # a fresh root every run, so a stale artefact cannot be picked up by
    # construction rather than by remembering to delete one
    root = Path(tempfile.mkdtemp(prefix="native-bench-"))
    built_after = root.stat().st_mtime

    results = []
    for index, program in enumerate(programs, 1):
        print(f"[{index}/{len(programs)}] {program.name}", file=sys.stderr)
        results.append(
            run_program(args, program, root, python, python_version, built_after)
        )

    CURRENT["load_after"] = load()
    CURRENT["noise_limit"] = args.noise_limit
    if args.verify_only:
        render_verification(results)
        if args.keep:
            print(f"build tree kept at {root}")
        else:
            shutil.rmtree(root, ignore_errors=True)
        failed = [r for r in results if r.status != "ok"]
        return 1 if failed else 0
    render(results, CURRENT, args.declines, args.noise_limit)

    payload = {
        "metadata": CURRENT,
        "benchmarks": {
            result.program.name: {
                "status": result.status,
                "group": result.program.group,
                "notes": result.notes,
                "calls": result.calls,
                "load": result.load,
                "declines": result.legs["by"].declines if "by" in result.legs else [],
                "times": result.times,
                **(
                    {
                        "vs_cpython": ratio_json(result, "cpython"),
                        "vs_mypyc": ratio_json(result, "mypyc"),
                        "control": ratio_json(result, "control"),
                        "noise": result.noise,
                        "noisy": result.noisy(args.noise_limit),
                    }
                    if result.status == "ok"
                    else {}
                ),
            }
            for result in results
        },
    }
    if args.json:
        args.json.write_text(json.dumps(payload, indent=2))
        print(f"written to {args.json}")

    regressed = False
    if args.baseline:
        regressed = compare(
            results,
            json.loads(args.baseline.read_text()),
            args.regression_threshold,
            args.noise_limit,
        )

    if args.keep:
        print(f"build tree kept at {root}")
    else:
        shutil.rmtree(root, ignore_errors=True)

    failed = [r for r in results if r.status != "ok"]
    noisy = [r for r in results if r.status == "ok" and r.noisy(args.noise_limit)]
    if failed:
        print(
            f"\n{len(failed)} benchmark(s) did not measure: "
            f"{', '.join(r.program.name for r in failed)}",
            file=sys.stderr,
        )
    # a run with a junk row in it exits non-zero even when every row it *could*
    # measure looks fine. the alternative is a table that is mostly trustworthy,
    # which is the kind nobody remembers to check before quoting
    return 1 if failed or noisy or regressed else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Failure as failure:
        print(f"error: {failure}", file=sys.stderr)
        raise SystemExit(2) from None

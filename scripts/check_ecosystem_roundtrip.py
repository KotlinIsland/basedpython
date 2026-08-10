#!/usr/bin/env -S uv run --script
#
# /// script
# requires-python = ">=3.11"
# dependencies = ["mypy-primer"]
#
# [tool.uv.sources]
# mypy-primer = { git = "https://github.com/hauntsaninja/mypy_primer" }
# ///

"""Round-trip ecosystem check for the basedpython transpiler.

For each target project, round-trip the whole tree with the directory-level
`by` commands::

    by transpile --reverse <project>   # python -> basedpython (in place)
    by build                           # basedpython -> python (-> out/)

`by build` uses one shared project db, so cross-module types resolve — the same
path real `.by` projects take. This is far cheaper than spawning `by` per file:
two processes per project, not thousands.

The point of the check is not whether the round-trip reproduces the original
source (the forward transpiler normalizes formatting and adds a preamble, so it
won't). The point is whether a change to the transpiler *moves* the output — so
we round-trip each project with two `by` binaries (the merge base and the PR)
and diff those two results against each other, exactly like the ty
ecosystem-analyzer diffs diagnostics between base and head.

A project is a **regression** when it built on the base but now fails to build;
that fails the check. Output that merely *changed*, or a project that now builds
when it used to fail, is surfaced in the report but does not fail — those want a
human's eyes, not a red build.

A project that fails the same way on *both* binaries is not a regression and
does not fail the check either, but it is still reported: the check is blind to
that project, and saying nothing would let the corpus rot silently.

Modes:

* ``check_ecosystem_roundtrip.py <by-new> --baseline <by-old>`` — diff the
  round-trip output of two binaries across the primer corpus. This is the CI
  mode; the markdown report on stdout is posted as a PR comment.

* ``check_ecosystem_roundtrip.py <by>`` — single binary; just report projects
  that fail to round-trip-build. Useful locally to find crashes.

* either form accepts a positional ``<project-dir>`` to round-trip a
  pre-cloned project on disk instead of fetching from mypy-primer.

For CI the corpus is sharded like the ty ecosystem-analyzer: each shard runs
``--shard N --num-shards M --json-out shard-N.json`` over its slice of the
project list, and a final ``--render shard-*.json`` pass merges the shard JSON
into the single markdown report.

The default project list is ``crates/ty_python_semantic/resources/primer/good.txt``.
"""

from __future__ import annotations

import argparse
import asyncio
import difflib
import json
import logging
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from asyncio.subprocess import PIPE, create_subprocess_exec
from contextlib import asynccontextmanager, nullcontext
from pathlib import Path
from signal import SIGINT, SIGTERM
from typing import TYPE_CHECKING, NamedTuple

if TYPE_CHECKING:
    from collections.abc import AsyncIterator

logger = logging.getLogger(__name__)

_PAGE_SIZE = os.sysconf("SC_PAGE_SIZE") if hasattr(os, "sysconf") else 4096


def _total_ram_bytes() -> int | None:
    """Total physical RAM, or None if it can't be determined."""
    try:
        return os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
    except (ValueError, OSError, AttributeError):
        return None


def _resolve_mem_limit_bytes(gb: float) -> int | None:
    """An explicit GB budget, else a fraction of total RAM that leaves the OS
    and the python harness room to breathe (so the watchdog kills a runaway
    build before the kernel OOM-kills the whole runner). None disables it."""
    if gb > 0:
        return int(gb * 1e9)
    if gb < 0:
        return None
    total = _total_ram_bytes()
    if total is None:
        return None
    return int(total * 0.70)


def _rss_bytes(pid: int) -> int | None:
    """Resident memory of `pid`, or None if it can't be read. Linux reads
    /proc; elsewhere falls back to `ps`."""
    try:
        with open(f"/proc/{pid}/statm") as f:
            return int(f.read().split()[1]) * _PAGE_SIZE
    except (OSError, ValueError, IndexError):
        pass
    try:
        r = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)],
            capture_output=True,
            text=True,
            check=False,
        )
        return int(r.stdout.strip()) * 1024
    except (ValueError, OSError):
        return None


# pin every pass to the highest supported version so compat lowering doesn't
# add or remove polyfills between binaries
ROUNDTRIP_MIN_VERSION = "3.15"

# embedded in the rendered report so the PR-comment workflow can find and
# update its own comment in place
COMMENT_MARKER = "<!-- by-ecosystem-roundtrip -->"

# label used for a whole-project build failure (no single file owns it)
BUILD_PATH = Path("<build>")

# GitHub rejects an issue comment body over 65536 characters, and truncates a
# step summary over 1MB. Stay under both with room for the omission notice.
COMMENT_CHAR_LIMIT = 65_536
_COMMENT_BUDGET = 60_000

# per-finding cap, so one project's enormous diagnostic dump can't crowd every
# other finding out of the total budget. shared out over the findings, within
# these bounds: a report with three findings can afford to show far more of
# each than one with two hundred
_MAX_DETAIL_CHARS = 12_000
_MIN_DETAIL_CHARS = 1_500

# the `<details>`/fence scaffolding wrapped around each detail, and room for the
# omission notice — both come out of the budget before it is shared out
_ENTRY_OVERHEAD = 120
_NOTICE_RESERVE = 250

# a project that fails identically on both binaries gets one line, not a dump
_SUMMARY_CHARS = 200

_SKIPPED_HEADING = "### ⏭️ skipped"

# directories that aren't first-party source (skipped when sizing a project)
_NON_SOURCE_DIRS = frozenset(
    {
        ".venv",
        "venv",
        "env",
        ".env",
        "site-packages",
        "__pycache__",
        ".git",
        ".tox",
        ".mypy_cache",
        ".ruff_cache",
        ".pytest_cache",
        "build",
        "dist",
        "node_modules",
        "out",
    }
)


# projects whose `by build` spikes past the runner's memory faster than the
# watchdog can react, so they OOM-kill the whole shard rather than failing
# cleanly. the file-count proxy below misses them — their per-file db cost is
# unusually high (e.g. spack's ~1160 deeply interconnected package modules) — so
# skip them by name regardless of size
_OVERSIZED_PROJECTS = frozenset({"spack"})


def count_source_py(root: Path) -> int:
    """Count first-party `.py` files under `root` (a proxy for build memory:
    `by build` holds the whole project's db in memory, so a giant project can
    OOM the runner)."""
    n = 0
    for p in root.rglob("*.py"):
        if not any(part in _NON_SOURCE_DIRS for part in p.relative_to(root).parts):
            n += 1
    return n


class ProjectOutcome(NamedTuple):
    """The result of round-trip-building a project with a single binary."""

    error: str | None
    # relpath under out/ -> built python; empty when error is set
    outputs: dict[str, bytes]


class FileDiff(NamedTuple):
    path: Path
    # "broken" | "fixed" | "changed" | "error-changed" | "error-unchanged"
    kind: str
    detail: str


class FileError(NamedTuple):
    path: Path
    error: str


class ProjectDiff(NamedTuple):
    name: str
    files_checked: int
    diffs: list[FileDiff]
    skipped: str | None


class ProjectErrors(NamedTuple):
    name: str
    files_checked: int
    errors: list[FileError]
    skipped: str | None


async def _run(
    program: Path,
    args: list[str],
    *,
    stdin: bytes | None = None,
    cwd: Path | None = None,
    mem_limit_bytes: int | None = None,
    timeout: float | None = None,
) -> tuple[int, bytes, bytes]:
    proc = await create_subprocess_exec(
        str(program),
        *args,
        stdin=PIPE if stdin is not None else None,
        stdout=PIPE,
        stderr=PIPE,
        cwd=str(cwd) if cwd is not None else None,
    )

    if mem_limit_bytes is None and timeout is None:
        out, err = await proc.communicate(stdin)
        return proc.returncode or 0, out, err

    # watchdog: kill the process if its resident set exceeds the budget or it
    # outlives the wall-clock timeout, so one runaway or hung `by` invocation is
    # recorded as a (canonical) error instead of OOM-killing or stalling the
    # whole shard. poll often so a fast memory spike can't outrun us
    comm = asyncio.ensure_future(proc.communicate(stdin))
    started = time.monotonic()
    killed: str | None = None
    while not comm.done():
        await asyncio.wait({comm}, timeout=0.25)
        if comm.done():
            break
        if timeout is not None and time.monotonic() - started > timeout:
            killed = f"timed out after {timeout:g}s"
            proc.kill()
            break
        if mem_limit_bytes is not None:
            rss = _rss_bytes(proc.pid)
            if rss is not None and rss > mem_limit_bytes:
                killed = f"exceeded {mem_limit_bytes / 1e9:g}GB memory budget"
                proc.kill()
                break
    out, err = await comm
    rc = proc.returncode or 0
    if killed is not None:
        # canonical message (drop the partial stderr) so the identical kill
        # under both binaries diffs to "no change" rather than a spurious
        # regression — only a kill that is new on head should read as broken
        err = f"killed: {killed}".encode()
        rc = 137
    return rc, out, err


def collect_outputs(out_dir: Path) -> dict[str, bytes]:
    """Read every built `.py` under `out_dir`, keyed by path relative to it."""
    outputs: dict[str, bytes] = {}
    if not out_dir.is_dir():
        return outputs
    for p in out_dir.rglob("*.py"):
        if p.is_symlink():
            continue
        try:
            outputs[str(p.relative_to(out_dir))] = p.read_bytes()
        except OSError as e:
            logger.warning("could not read built file %s: %s", p, e)
    return outputs


async def reset_project(root: Path):
    """Restore pristine source for the next binary: bring back the tracked
    `.py` files reverse deleted and drop the generated `.by`/`out`. Keep the
    venv (if any). The clones are git repos, so this is cheap."""
    await _run(Path("git"), ["-C", str(root), "checkout", "--", "."])
    await _run(Path("git"), ["-C", str(root), "clean", "-fdx", "-e", ".venv"])


async def roundtrip_project(
    by: Path,
    root: Path,
    *,
    build_mem_limit_bytes: int | None,
    build_timeout: float | None,
) -> ProjectOutcome:
    """Reverse the whole project (py->by) then build it (by->py via out/)."""
    rc, _, err = await _run(
        by,
        ["transpile", "--reverse", "--min-version", ROUNDTRIP_MIN_VERSION, str(root)],
        mem_limit_bytes=build_mem_limit_bytes,
        timeout=build_timeout,
    )
    if rc != 0:
        msg = err.decode(errors="replace").strip()
        return ProjectOutcome(error=f"reverse: {msg}", outputs={})

    rc, _, err = await _run(
        by,
        ["build", "--min-version", ROUNDTRIP_MIN_VERSION],
        cwd=root,
        mem_limit_bytes=build_mem_limit_bytes,
        timeout=build_timeout,
    )
    outputs = collect_outputs(root / "out")
    # a non-zero exit from ty diagnostics (e.g. unresolved third-party imports —
    # the corpus is cloned source-only, so those are expected and unavoidable)
    # is not a round-trip failure: the transpile still emitted `out/`. only a
    # build the watchdog killed (137), or one that produced nothing at all, is a
    # genuine failure
    if rc == 137 or (rc != 0 and not outputs):
        msg = err.decode(errors="replace").strip()
        return ProjectOutcome(error=f"build: {msg}", outputs={})

    return ProjectOutcome(error=None, outputs=outputs)


def _unified_diff(old: bytes, new: bytes, rel: Path) -> str:
    return "".join(
        difflib.unified_diff(
            old.decode("utf-8", errors="replace").splitlines(keepends=True),
            new.decode("utf-8", errors="replace").splitlines(keepends=True),
            fromfile=f"base/{rel}",
            tofile=f"head/{rel}",
            n=2,
        )
    )


# a panic header carries the thread's id: `thread 'main' (4094) panicked at ...`.
# not anchored: the harness prefixes the phase, as `reverse: thread 'main' ...`
_THREAD_ID_RE = re.compile(r"\b(thread '[^']*') \(\d+\)")

# `by`'s diagnostic footer names the binary and the commit it was built from,
# so it can never match between the base and head binaries
_VERSION_RE = re.compile(r"^info: Version: .*$", re.MULTILINE)
_ARGS_RE = re.compile(r"^info: Args: .*$", re.MULTILINE)

# salsa's opaque interning handles, in both panic messages and `Query stack:`
# entries. the query *names* are the signal; the ids shift whenever interning
# order does, which any unrelated change can cause
_SALSA_ID_RE = re.compile(r"\bId\([0-9a-f]+\)")

# everything from `stack backtrace:` to the trailing `note:` (or to the end).
# frame indices, source line numbers and recursion depth all move under edits
# that leave the panic itself untouched
_BACKTRACE_RE = re.compile(
    r"^stack backtrace:\n.*?(?=^note: Some details are omitted|\Z)",
    re.MULTILINE | re.DOTALL,
)


def canonical_error(msg: str) -> str:
    """Strip the parts of a build failure that differ between two runs of the
    *same* failure.

    The base and head binaries are separate processes built from separate
    commits, so a raw stderr comparison never matches once a project fails
    under both: the thread id, the version/argv footer and every backtrace line
    number differ by construction. That reported every erroring project as
    `error-changed` on every PR. Compare what identifies the failure — the
    panic message and the diagnostics — not the run that produced it."""
    msg = _THREAD_ID_RE.sub(r"\1", msg)
    msg = _VERSION_RE.sub("info: Version: <elided>", msg)
    msg = _ARGS_RE.sub("info: Args: <elided>", msg)
    msg = _SALSA_ID_RE.sub("Id(<elided>)", msg)
    return _BACKTRACE_RE.sub("stack backtrace: <elided>\n", msg)


def classify_project(
    name: str, old: ProjectOutcome, new: ProjectOutcome
) -> ProjectDiff:
    """Compare a project's round-trip build on the base vs the head binary."""
    diffs: list[FileDiff] = []

    if old.error is not None or new.error is not None:
        if old.error is not None and new.error is not None:
            if canonical_error(old.error) != canonical_error(new.error):
                diffs.append(
                    FileDiff(
                        BUILD_PATH,
                        "error-changed",
                        f"base: {old.error}\nhead: {new.error}",
                    )
                )
            else:
                # not a change, so not a finding — but the check is blind to
                # this project, and saying nothing reads as "all good"
                diffs.append(FileDiff(BUILD_PATH, "error-unchanged", new.error))
        elif new.error is not None:  # built on base, fails on head
            diffs.append(FileDiff(BUILD_PATH, "broken", new.error))
        else:  # failed on base, builds on head
            diffs.append(FileDiff(BUILD_PATH, "fixed", ""))
        return ProjectDiff(name, len(old.outputs) or len(new.outputs), diffs, None)

    # both built — diff the per-file output
    rels = sorted(set(old.outputs) | set(new.outputs))
    for rel in rels:
        o = old.outputs.get(rel)
        n = new.outputs.get(rel)
        if o == n:
            continue
        if o is None:
            diffs.append(FileDiff(Path(rel), "changed", "(only produced on head)"))
        elif n is None:
            diffs.append(FileDiff(Path(rel), "changed", "(only produced on base)"))
        else:
            diffs.append(FileDiff(Path(rel), "changed", _unified_diff(o, n, Path(rel))))
    return ProjectDiff(name, len(rels), diffs, None)


async def diff_project(
    old_by: Path,
    new_by: Path,
    project_root: Path,
    name: str,
    *,
    build_mem_limit_bytes: int | None,
    build_timeout: float | None,
) -> ProjectDiff:
    # base and head share the tree, so run them sequentially with a reset in
    # between to restore pristine `.py` source
    old = await roundtrip_project(
        old_by,
        project_root,
        build_mem_limit_bytes=build_mem_limit_bytes,
        build_timeout=build_timeout,
    )
    await reset_project(project_root)
    new = await roundtrip_project(
        new_by,
        project_root,
        build_mem_limit_bytes=build_mem_limit_bytes,
        build_timeout=build_timeout,
    )
    await reset_project(project_root)
    return classify_project(name, old, new)


async def check_project(
    by: Path,
    project_root: Path,
    name: str,
    *,
    build_mem_limit_bytes: int | None,
    build_timeout: float | None,
) -> ProjectErrors:
    outcome = await roundtrip_project(
        by,
        project_root,
        build_mem_limit_bytes=build_mem_limit_bytes,
        build_timeout=build_timeout,
    )
    await reset_project(project_root)
    errors = [] if outcome.error is None else [FileError(BUILD_PATH, outcome.error)]
    return ProjectErrors(name, len(outcome.outputs), errors, None)


@asynccontextmanager
async def setup_primer_project(name: str, parent: Path) -> AsyncIterator[Path]:
    """Clone a mypy-primer project into ``parent / name`` if not already there.

    `by build` resolves third-party types from the ambient Python environment
    rather than the project's virtualenv, so installing the project's
    dependencies wouldn't change its output — we clone source only.
    """
    target = parent / name
    if not target.exists():
        script = Path(__file__).with_name("setup_primer_project.py")
        logger.info("setting up primer project %s in %s", name, target)
        proc = await create_subprocess_exec(
            sys.executable,
            str(script),
            name,
            str(target),
            stdout=PIPE,
            stderr=PIPE,
        )
        _, err = await proc.communicate()
        if proc.returncode != 0:
            raise RuntimeError(
                f"setup_primer_project.py failed for {name!r}:\n"
                f"{err.decode(errors='replace')}"
            )
    yield target


def _detail_cap(n_findings: int) -> int:
    """How much of each finding we can afford to show. Aims at 90% of the
    budget so the usual report fits whole and nothing has to be dropped."""
    if n_findings <= 0:
        return _MAX_DETAIL_CHARS
    share = (_COMMENT_BUDGET * 9 // 10) // n_findings - _ENTRY_OVERHEAD
    return max(_MIN_DETAIL_CHARS, min(_MAX_DETAIL_CHARS, share))


def _truncate_detail(detail: str, cap: int) -> str:
    """Cap one finding's body, keeping both ends: a panic is identified by its
    header and topmost frames, a failed build by where it finally gave up."""
    if len(detail) <= cap:
        return detail
    marker = "\n... {} characters elided ...\n"
    head_budget = cap * 3 // 4
    tail_budget = max(0, cap - head_budget - len(marker) - 12)
    # snap both cuts to a line boundary so neither end starts or ends mid-line
    head = detail[:head_budget].rsplit("\n", 1)[0]
    tail = detail[len(detail) - tail_budget :].split("\n", 1)[-1] if tail_budget else ""
    out = head + marker.format(len(detail) - len(head) - len(tail)) + tail
    return out if len(out) <= cap else out[:cap]


def _one_line(text: str) -> str:
    """Flatten a multi-line reason so it renders as a single list item."""
    s = " ".join(text.split()).replace("`", "")
    return s[: _SUMMARY_CHARS - 1] + "…" if len(s) > _SUMMARY_CHARS else s


def _error_summary(err: str) -> str:
    """One line naming how a project failed. A project that fails the same way
    on both binaries isn't a finding, so it gets a line rather than a dump."""
    phase, _, rest = err.partition(": ")
    m = re.search(r"panicked at ([^\n]*?):\n([^\n]*)", err)
    if m:
        summary = f"panicked at {m.group(1).rsplit('/', 1)[-1]}: {m.group(2)}"
    else:
        summary = next((x for x in rest.splitlines() if x.strip()), rest)
    return _one_line(f"{phase}: {summary}")


def _details_entry(
    name: str, path: Path, detail: str, cap: int, fence: str = ""
) -> str:
    return "\n".join(
        [
            f"<details><summary>{name} — {path}</summary>",
            "",
            f"```{fence}",
            _truncate_detail(detail, cap).rstrip(),
            "```",
            "",
            "</details>",
        ]
    )


def _assemble(header: list[str], sections: list[tuple[str, list[str]]]) -> str:
    """Join the report under GitHub's comment size limit, dropping whole
    findings rather than cutting the report off mid-sentence.

    Sections are consumed in severity order, so what gets dropped first is the
    least important thing in the report — never a regression."""
    out = list(header)
    used = sum(len(line) + 1 for line in out) + _NOTICE_RESERVE
    dropped = 0
    for heading, entries in sections:
        placed = False
        pending = [heading, ""]
        pending_cost = sum(len(line) + 1 for line in pending)
        for entry in entries:
            cost = len(entry) + 1
            if used + (0 if placed else pending_cost) + cost > _COMMENT_BUDGET:
                dropped += 1
                continue
            if not placed:
                out.extend(pending)
                used += pending_cost
                placed = True
            out.append(entry)
            used += cost
        if placed:
            out.append("")
            used += 1
    if dropped:
        out.append(
            f"_{dropped} finding(s) omitted to fit GitHub's "
            f"{COMMENT_CHAR_LIMIT}-character comment limit. The full report is "
            f"the `comment.md` artifact of this run._"
        )
    return "\n".join(out).rstrip() + "\n"


def render_diff_report(
    results: list[ProjectDiff], old_label: str, new_label: str
) -> tuple[str, bool]:
    # sorted, not in shard-completion order: the report is read by diffing it
    # against the last run, and a stable order is what makes new entries visible
    def of_kind(kind: str) -> list[tuple[ProjectDiff, FileDiff]]:
        return sorted(
            ((r, d) for r in results for d in r.diffs if d.kind == kind),
            key=lambda rd: (rd[0].name, rd[1].path),
        )

    broken = of_kind("broken")
    fixed = of_kind("fixed")
    changed = of_kind("changed")
    error_changed = of_kind("error-changed")
    failing = of_kind("error-unchanged")
    skipped = sorted(
        (r for r in results if r.skipped is not None), key=lambda r: r.name
    )
    total_files = sum(r.files_checked for r in results)
    n_projects = len(results) - len(skipped)

    lines: list[str] = ["## by ecosystem round-trip", "", COMMENT_MARKER, ""]

    skipped_section = (
        _SKIPPED_HEADING,
        [f"- `{r.name}`: {_one_line(r.skipped or '')}" for r in skipped],
    )

    if broken or fixed or changed or error_changed:
        lines.append(f"base: `{old_label}` → head: `{new_label}`")
        lines.append("")
        lines.append(
            f"regressions: {len(broken)}, changed: {len(changed)}, "
            f"improvements: {len(fixed)}, error changes: {len(error_changed)} "
            f"(across {total_files} files in {n_projects} projects)"
        )
    else:
        lines.append(
            f"✅ no round-trip differences between `{old_label}` and `{new_label}` "
            f"across {total_files} files in {n_projects} projects."
        )

    # never silent: a project failing on both binaries is not a regression, but
    # the check is blind to it, and the count belongs in the header where
    # nothing can drop it
    if failing:
        lines.append("")
        lines.append(
            f"⚠️ {len(failing)} project(s) fail to round-trip on both base and "
            f"head, so this check says nothing about them."
        )
    lines.append("")

    cap = _detail_cap(len(broken) + len(changed) + len(error_changed))
    sections = [
        (
            "### ❌ regressions (built on base, now fails)",
            [_details_entry(r.name, d.path, d.detail, cap) for r, d in broken],
        ),
        (
            "### ℹ️ changed round-trip output",  # noqa: RUF001
            [_details_entry(r.name, d.path, d.detail, cap, "diff") for r, d in changed],
        ),
        (
            "### ⚠️ build error changed (failed before and after)",
            [_details_entry(r.name, d.path, d.detail, cap) for r, d in error_changed],
        ),
        (
            "### 💥 fails to round-trip (unchanged from base)",
            [f"- `{r.name}`: {_error_summary(d.detail)}" for r, d in failing],
        ),
        (
            "### ✅ improvements (failed on base, now builds)",
            [f"- `{r.name}` `{d.path}`" for r, d in fixed],
        ),
        skipped_section,
    ]

    return _assemble(lines, sections), not broken


def _project_diff_to_dict(p: ProjectDiff) -> dict:
    return {
        "name": p.name,
        "files_checked": p.files_checked,
        "skipped": p.skipped,
        "diffs": [
            {"path": str(d.path), "kind": d.kind, "detail": d.detail} for d in p.diffs
        ],
    }


def _project_diff_from_dict(d: dict) -> ProjectDiff:
    return ProjectDiff(
        d["name"],
        d["files_checked"],
        [FileDiff(Path(x["path"]), x["kind"], x["detail"]) for x in d["diffs"]],
        d["skipped"],
    )


def render_from_json(paths: list[Path], old_label: str, new_label: str) -> int:
    """Merge per-shard JSON results into the single markdown report."""
    projects: list[ProjectDiff] = []
    for path in paths:
        data = json.loads(path.read_text())
        projects.extend(_project_diff_from_dict(x) for x in data["projects"])
    report, _clean = render_diff_report(projects, old_label, new_label)
    print(report)
    # findings (broken/changed/error-changed) are surfaced in the PR comment for
    # humans to review — they don't fail the job. only a genuine harness crash
    # (an uncaught exception above) is a failure
    return 0


def render_error_report(results: list[ProjectErrors]) -> tuple[str, bool]:
    total_files = sum(r.files_checked for r in results)
    errors = [(r, e) for r in results for e in r.errors]
    skipped = [r for r in results if r.skipped is not None]

    lines: list[str] = ["## by ecosystem round-trip", "", COMMENT_MARKER, ""]
    if not errors:
        lines.append(
            f"✅ round-trip built cleanly across {total_files} files "
            f"in {len(results) - len(skipped)} projects."
        )
        return "\n".join(lines).rstrip() + "\n", True

    lines.append(f"❌ {len(errors)} project(s) failed to round-trip build")
    lines.append("")
    for r, e in errors:
        lines.append(f"- `{r.name}`: {e.error}")
    return "\n".join(lines).rstrip() + "\n", False


async def main_async(args: argparse.Namespace) -> int:
    by: Path = args.by.resolve()
    if not by.is_file():
        logger.error("by binary not found: %s", by)
        return 2

    baseline: Path | None = None
    if args.baseline is not None:
        baseline = args.baseline.resolve()
        if not baseline.is_file():
            logger.error("baseline by binary not found: %s", baseline)
            return 2

    sem = asyncio.Semaphore(args.project_concurrency)
    mem_limit = _resolve_mem_limit_bytes(args.build_mem_limit_gb)
    build_timeout = args.build_timeout if args.build_timeout > 0 else None
    if mem_limit is not None:
        logger.info("per-build memory budget: %.1f GB", mem_limit / 1e9)
    if build_timeout is not None:
        logger.info("per-build wall-clock timeout: %g s", build_timeout)

    async def run_one(name: str, root: Path) -> ProjectDiff | ProjectErrors:
        async with sem:
            if name in _OVERSIZED_PROJECTS:
                logger.info("skipping %s: known to OOM the runner", name)
                return skipped_result(
                    name,
                    "skipped: known to spike past the runner's memory during "
                    "`by build` (faster than the watchdog can react)",
                )
            if args.max_project_py_files:
                n_py = count_source_py(root)
                if n_py > args.max_project_py_files:
                    logger.info("skipping %s: %d .py files over limit", name, n_py)
                    return skipped_result(
                        name,
                        f"skipped: {n_py} .py files exceeds "
                        f"--max-project-py-files ({args.max_project_py_files}); "
                        f"`by build` would not fit in the runner's memory",
                    )
            logger.info("round-tripping %s", name)
            if baseline is not None:
                return await diff_project(
                    baseline,
                    by,
                    root,
                    name,
                    build_mem_limit_bytes=mem_limit,
                    build_timeout=build_timeout,
                )
            return await check_project(
                by,
                root,
                name,
                build_mem_limit_bytes=mem_limit,
                build_timeout=build_timeout,
            )

    def skipped_result(name: str, note: str) -> ProjectDiff | ProjectErrors:
        if baseline is not None:
            return ProjectDiff(name, 0, [], note)
        return ProjectErrors(name, 0, [], note)

    results: list[ProjectDiff | ProjectErrors] = []

    if args.project_dir is not None:
        root = args.project_dir.resolve()
        results.append(await run_one(root.name, root))
    else:
        if args.checkout:
            location_ctx = nullcontext(args.checkout)
        else:
            location_ctx = tempfile.TemporaryDirectory()

        with location_ctx as parent_str:
            parent = Path(parent_str)
            parent.mkdir(parents=True, exist_ok=True)

            primer_names = list(args.primer)
            if not primer_names:
                projects_file = (
                    args.projects
                    or Path(__file__).resolve().parent.parent
                    / "crates/ty_python_semantic/resources/primer/good.txt"
                )
                primer_names = [
                    line.strip()
                    for line in projects_file.read_text().splitlines()
                    if line.strip() and not line.startswith("#")
                ]
            if args.limit:
                primer_names = primer_names[: args.limit]
            # round-robin so adjacent (often similarly-sized) projects land on
            # different shards
            if args.num_shards > 1:
                primer_names = primer_names[args.shard :: args.num_shards]

            async def setup_and_run(name: str) -> ProjectDiff | ProjectErrors:
                try:
                    async with setup_primer_project(name, parent) as root:
                        return await run_one(name, root)
                except Exception as e:
                    # a clone/network failure affects base and head equally, so
                    # it's noise, not a regression: skip it without failing
                    logger.warning("project %s failed setup: %s", name, e)
                    return skipped_result(name, f"setup failed: {e}")
                finally:
                    # free the clone (source + generated `.by` + `out/`) so disk
                    # doesn't accumulate across the shard's projects. --checkout
                    # is for reuse, so only clean the temp-dir mode
                    if args.checkout is None:
                        shutil.rmtree(parent / name, ignore_errors=True)

            results = list(
                await asyncio.gather(*(setup_and_run(n) for n in primer_names))
            )

    # shard worker: emit machine-readable results for the render step to merge.
    # a shard never decides pass/fail (the render step aggregates and does), so
    # this returns 0 even when its slice contains a regression.
    if args.json_out is not None:
        diffs = [r for r in results if isinstance(r, ProjectDiff)]
        args.json_out.write_text(
            json.dumps({"projects": [_project_diff_to_dict(p) for p in diffs]})
        )
        return 0

    if baseline is not None:
        diffs = [r for r in results if isinstance(r, ProjectDiff)]
        report, clean = render_diff_report(diffs, args.old_label, args.new_label)
    else:
        errs = [r for r in results if isinstance(r, ProjectErrors)]
        report, clean = render_error_report(errs)

    print(report)
    return 0 if clean else 1


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "by",
        type=Path,
        nargs="?",
        help="path to the `by` binary (the head/new); omitted in --render mode",
    )
    parser.add_argument(
        "project_dir",
        type=Path,
        nargs="?",
        help="pre-cloned project directory to check (skips primer fetch)",
    )
    parser.add_argument(
        "--shard",
        type=int,
        default=0,
        help="0-based index of this shard (with --num-shards)",
    )
    parser.add_argument(
        "--num-shards",
        type=int,
        default=1,
        help="partition the project list into this many shards",
    )
    parser.add_argument(
        "--json-out",
        type=Path,
        help="write per-project diff results as JSON (shard worker mode); "
        "requires --baseline and suppresses the markdown report",
    )
    parser.add_argument(
        "--render",
        type=Path,
        nargs="+",
        help="merge these shard JSON files into the markdown report and exit",
    )
    parser.add_argument(
        "--baseline",
        type=Path,
        help="path to a second `by` binary (the merge base); enables diff mode",
    )
    parser.add_argument(
        "--old-label",
        default="base",
        help="display label for the baseline binary in the report",
    )
    parser.add_argument(
        "--new-label",
        default="head",
        help="display label for the head binary in the report",
    )
    parser.add_argument(
        "--primer",
        action="append",
        default=[],
        help="mypy-primer project name (repeatable). overrides --projects",
    )
    parser.add_argument(
        "--projects",
        type=Path,
        help="newline-delimited list of mypy-primer project names "
        "(default: crates/ty_python_semantic/resources/primer/good.txt)",
    )
    parser.add_argument(
        "--checkout",
        type=Path,
        help="reuse a directory for primer checkouts (default: temp dir)",
    )
    parser.add_argument(
        "--limit",
        type=int,
        help="limit the number of primer projects to check",
    )
    parser.add_argument(
        "--project-concurrency",
        type=int,
        default=1,
        help="how many projects to round-trip-build at once (each build holds a "
        "whole project's db in memory, so keep this modest)",
    )
    parser.add_argument(
        "--max-project-py-files",
        type=int,
        default=2000,
        help="skip projects with more first-party .py files than this — `by "
        "build` holds the whole project's db in memory, so the biggest projects "
        "(e.g. spack) would OOM the runner (0 disables the limit)",
    )
    parser.add_argument(
        "--build-mem-limit-gb",
        type=float,
        default=0.0,
        help="kill a `by` reverse/build whose resident memory exceeds this many "
        "GB and record it as a (canonical) error, so one runaway build can't OOM "
        "the runner and take down the shard. 0 (the default) auto-sizes to 70%% "
        "of total RAM; a negative value disables the watchdog",
    )
    parser.add_argument(
        "--build-timeout",
        type=float,
        default=900.0,
        help="kill a `by` reverse/build that runs longer than this many seconds "
        "and record it as a (canonical) error, so one hung invocation can't "
        "stall the whole shard until the job timeout (0 disables the timeout)",
    )
    parser.add_argument("-v", "--verbose", action="store_true")
    return parser.parse_args(argv)


def main() -> int:
    args = parse_args()
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
    )

    if args.render:
        return render_from_json(args.render, args.old_label, args.new_label)
    if args.by is None:
        logger.error("the `by` binary is required unless --render is given")
        return 2
    if args.json_out is not None and args.baseline is None:
        logger.error("--json-out requires --baseline (diff mode)")
        return 2

    if args.checkout:
        args.checkout.mkdir(parents=True, exist_ok=True)

    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)
    main_task = asyncio.ensure_future(main_async(args))
    for sig in (SIGINT, SIGTERM):
        loop.add_signal_handler(sig, main_task.cancel)
    try:
        return loop.run_until_complete(main_task)
    finally:
        loop.close()


if __name__ == "__main__":
    sys.exit(main())

# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "pyyaml>=6.0.3",
# ]
#
# [tool.ty.analysis]
# return-type-inference = "always"
#
# [tool.ty.rules]
# truthiness-test-of-none-union = "warn"
# blanket-ignore-comment = "warn"
# missing-type-argument = "warn"
# possibly-unresolved-reference = "warn"
# unsound-return-statement = "warn"
# unsound-yield = "warn"
# unsupported-dynamic-base = "warn"
# division-by-zero = "warn"
# dynamic-function-decorator-return = "warn"
# unsound-assignment = "warn"
# redundant-condition-strict = "warn"
# disjoint-cast = "warn"
# missing-direct-dependency = "warn"
#
# [tool.uv]
# no-build = true
# exclude-newer = "P7D"
# require-build-hashes = true
# preview-features = ["build-dependency-hashes", "missing-exclude-newer-package-lock"]
# ///

"""check that `build-binaries.yml` builds exactly the archives a cargo-dist plan lists

the release page and installers link to every archive in the plan, but the archives are
built outside cargo-dist, so nothing else ties the two together before a release. for
every planned archive this checks that one `build-binaries.yml` job archives its target
and uploads it as a release artifact, then runs the archive script against stand-in
binaries placed where cargo builds them, and checks the archive's name, checksum sidecar
and layout (one top-level directory in a `.tar.gz`, the binaries at the root of a `.zip`,
which is what cargo-dist's installers unpack)
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

ARCHIVE_SCRIPT = "scripts/archive_release_binaries.sh"
# where cargo builds `crates/basedpython`'s binaries, which is its own cargo workspace
RELEASE_DIR = "crates/basedpython/target/{target}/release"


@dataclass(frozen=True)
class PlannedArchive:
    name: str
    target: str
    binaries: tuple[str, ...]


@dataclass(frozen=True)
class ArchiveJob:
    job: str
    target: str
    upload_globs: tuple[str, ...]


def planned_archives(manifest: dict[str, Any]) -> dict[str, PlannedArchive]:
    archives: dict[str, PlannedArchive] = {}
    for name, artifact in manifest["artifacts"].items():
        if artifact["kind"] != "executable-zip":
            continue
        (target,) = artifact["target_triples"]
        binaries = tuple(
            asset["path"]
            for asset in artifact["assets"]
            if asset["kind"] == "executable"
        )
        archives[target] = PlannedArchive(name, target, binaries)
    return archives


def matrix_targets(job_name: str, job: dict[str, Any], expression: str) -> list[str]:
    """resolve an archive step's `TARGET` to the targets the job builds"""
    matrix = job.get("strategy", {}).get("matrix", {})
    if expression == "${{ matrix.target }}":
        return list(matrix["target"])
    if expression == "${{ matrix.platform.target }}":
        return [platform["target"] for platform in matrix["platform"]]
    if "${{" in expression:
        raise ValueError(f"job `{job_name}`: cannot resolve `TARGET: {expression}`")
    return [expression]


def archive_jobs(workflow: dict[str, Any]) -> list[ArchiveJob]:
    jobs = []
    for job_name, job in workflow["jobs"].items():
        steps = job.get("steps", [])
        archive_steps = [
            step for step in steps if ARCHIVE_SCRIPT in step.get("run", "")
        ]
        if not archive_steps:
            continue
        if len(archive_steps) != 1:
            raise ValueError(f"job `{job_name}` runs `{ARCHIVE_SCRIPT}` more than once")
        expression = str(archive_steps[0].get("env", {}).get("TARGET", ""))
        if not expression:
            raise ValueError(f"job `{job_name}`: the archive step sets no `TARGET`")
        upload_globs = tuple(
            line.strip()
            for step in steps
            if step.get("uses", "").startswith("actions/upload-artifact@")
            and str(step.get("with", {}).get("name", "")).startswith("artifacts-")
            for line in str(step["with"].get("path", "")).splitlines()
            if line.strip()
        )
        jobs.extend(
            ArchiveJob(job_name, target, upload_globs)
            for target in matrix_targets(job_name, job, expression)
        )
    return jobs


def check_archive(root: Path, planned: PlannedArchive) -> list[str]:
    """run the archive script for one target and check what it produced"""
    errors = []
    with tempfile.TemporaryDirectory() as scratch:
        workdir = Path(scratch)
        release_dir = workdir / RELEASE_DIR.format(target=planned.target)
        release_dir.mkdir(parents=True)
        for binary in planned.binaries:
            (release_dir / binary).write_text(binary, encoding="utf-8")
        result = subprocess.run(
            ["bash", str(root / ARCHIVE_SCRIPT), planned.target],
            cwd=workdir,
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            return [f"{planned.target}: `{ARCHIVE_SCRIPT}` failed:\n{result.stderr}"]

        archive = workdir / planned.name
        sidecar = workdir / f"{planned.name}.sha256"
        if not archive.is_file() or not sidecar.is_file():
            produced = sorted(path.name for path in workdir.glob("basedpython-*.*"))
            return [
                f"{planned.target}: expected `{planned.name}` and its `.sha256`, got {produced}"
            ]

        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        # `sha256sum` marks a file it read in binary mode with `*`, as it can on windows
        fields = sidecar.read_text(encoding="utf-8").split()
        if (
            len(fields) != 2
            or fields[0] != digest
            or fields[1].removeprefix("*") != planned.name
        ):
            errors.append(
                f"{planned.target}: `{sidecar.name}` does not name the archive's hash"
            )

        if planned.name.endswith(".zip"):
            with zipfile.ZipFile(archive) as zip_file:
                members = sorted(zip_file.namelist())
            expected = sorted(planned.binaries)
        else:
            with tarfile.open(archive) as tar_file:
                members = sorted(member.name for member in tar_file if member.isfile())
            top = planned.name.removesuffix(".tar.gz")
            expected = sorted(f"{top}/{binary}" for binary in planned.binaries)
        if members != expected:
            errors.append(
                f"{planned.target}: `{planned.name}` holds {members}, expected {expected}"
            )
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest", type=Path, required=True, help="a `dist plan` json manifest"
    )
    parser.add_argument(
        "--workflow", type=Path, default=Path(".github/workflows/build-binaries.yml")
    )
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent

    planned = planned_archives(json.loads(args.manifest.read_text(encoding="utf-8")))
    try:
        jobs = archive_jobs(yaml.safe_load(args.workflow.read_text(encoding="utf-8")))
    except ValueError as error:
        sys.exit(str(error))

    errors = []
    built: dict[str, ArchiveJob] = {}
    for job in jobs:
        if job.target in built:
            errors.append(
                f"{job.target}: archived by both `{built[job.target].job}` and `{job.job}`"
            )
        built[job.target] = job
    for target in sorted(built.keys() - planned.keys()):
        errors.append(
            f"{target}: archived by `{built[target].job}`, but cargo-dist plans no archive"
        )
    for target in sorted(planned.keys() - built.keys()):
        errors.append(
            f"{target}: cargo-dist plans `{planned[target].name}`, but no job archives it"
        )

    for target in sorted(planned.keys() & built.keys()):
        archive, job = planned[target], built[target]
        for name in (archive.name, f"{archive.name}.sha256"):
            if not any(fnmatch.fnmatch(name, glob) for glob in job.upload_globs):
                errors.append(
                    f"{target}: `{job.job}` does not upload `{name}` as an `artifacts-*` artifact"
                )
        errors.extend(check_archive(root, archive))

    if errors:
        sys.exit("\n".join(errors))
    print(f"checked {len(planned)} planned archive(s) against {args.workflow}")


if __name__ == "__main__":
    main()

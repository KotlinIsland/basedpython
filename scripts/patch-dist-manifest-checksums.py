# /// script
# requires-python = ">=3.10"
# dependencies = []
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
# require-build-hashes = true
# preview-features = ["build-dependency-hashes", "missing-exclude-newer-package-lock"]
# ///

"""Add sidecar SHA-256 checksums to a cargo-dist local manifest.

The release workflow builds archives outside cargo-dist. Its global installer
generation needs their checksums in the manifest, so copy them from the sidecars
uploaded by the binary build jobs. Require every archive the manifest plans, with
its checksum, so a target without a binary build job fails the release rather than
publishing a release page and installers that link to a missing archive
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path


def read_sha256(path: Path, archive: Path) -> str:
    """read the sidecar's checksum, which must name `archive` and match its contents"""
    lines = path.read_text(encoding="utf-8").strip().splitlines()
    if len(lines) != 1:
        raise ValueError(f"expected one checksum in {path}")
    fields = lines[0].split()
    if len(fields) != 2:
        raise ValueError(f"expected a checksum and a file name in {path}")
    checksum, name = fields[0], fields[1]
    if re.fullmatch(r"[0-9a-fA-F]{64}", checksum) is None:
        raise ValueError(f"invalid SHA-256 checksum in {path}: {checksum!r}")
    # `sha256sum` marks a file it read in binary mode with `*`, as it can on windows
    if name.removeprefix("*") != archive.name:
        raise ValueError(f"{path} names {name!r}, not {archive.name!r}")
    actual = hashlib.sha256(archive.read_bytes()).hexdigest()
    if checksum.lower() != actual:
        raise ValueError(
            f"{path} gives {checksum.lower()}, but {archive.name} hashes to {actual}"
        )
    return actual


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--artifacts-dir", type=Path, required=True)
    args = parser.parse_args()

    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    patched = 0
    missing = []
    for artifact_name, artifact in manifest["artifacts"].items():
        if artifact["kind"] != "executable-zip":
            continue
        if not (args.artifacts_dir / artifact_name).is_file():
            missing.append(artifact_name)
            continue
        checksum = read_sha256(
            args.artifacts_dir / f"{artifact_name}.sha256",
            args.artifacts_dir / artifact_name,
        )
        artifact.setdefault("checksums", {})["sha256"] = checksum
        patched += 1

    if missing:
        raise ValueError(
            f"planned archives missing from {args.artifacts_dir}: {', '.join(sorted(missing))}"
        )
    if patched == 0:
        raise ValueError(f"no executable archives found in {args.artifacts_dir}")

    args.manifest.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"patched {patched} archive checksum(s) in {args.manifest}")


if __name__ == "__main__":
    main()

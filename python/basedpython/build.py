"""The PEP 517 build backend for basedpython projects.

    [build-system]
    requires = ["basedpython"]
    build-backend = "basedpython.build"

A basedpython project is python once it is built, so this backend does the one
thing that makes it so — run `by build`, which transpiles the `.by` sources and
carries everything else across unchanged — and then hands the resulting tree to
`uv_build` to package. Splitting it there is deliberate: everything above the
transpile (core metadata, RECORD, wheel tags, entry points, editable installs) is
ordinary python packaging with nothing basedpython about it, and a backend that
reimplemented it would only be a second place for it to be subtly wrong.

The wheel is plain python: the transpiled `.py` for python to import, the `.by`
sources beside them, and a `by.typed` marker saying the `.by` are the
authoritative surface. A python consumer sees a python library; a basedpython
consumer sees the declarations that have no python spelling.

The source distribution keeps the `.by` untranspiled — it is the source — and is
written here rather than delegated, because no python backend knows that a `.by`
file is a source file.
"""

from __future__ import annotations

import base64
import hashlib
import io
import os
import re
import shutil
import subprocess
import sys
import sysconfig
import tarfile
import tempfile
import zipfile
from contextlib import contextmanager
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from collections.abc import Iterator, Mapping, Sequence

# the backend `uv_build` version this was written against. it is requested at
# build time rather than depended on outright: `by check` and `by run` are the
# overwhelming majority of what this distribution is used for, and neither of
# them packages anything
UV_BUILD_REQUIREMENT = "uv_build>=0.9,<10"

# the config setting that asks for a wheel lowered to one python rather than to
# the floor of what the project supports
TARGET_VERSION_SETTING = "python-version"

# where `build_editable` stages the project. it is `by build`'s own default
# output directory on purpose: an editable install points python at this tree, so
# a plain `by build` is what refreshes an editable install
EDITABLE_STAGING_DIRECTORY = "out"


class BuildError(Exception):
    """A build that cannot proceed, reported without a traceback into it."""


# ── PEP 517 / PEP 660 hooks ──────────────────────────────────────────────────


def get_requires_for_build_wheel(
    config_settings: Mapping[str, Any] | None = None,
) -> list[str]:
    return _requirements()


def get_requires_for_build_sdist(
    config_settings: Mapping[str, Any] | None = None,
) -> list[str]:
    return _requirements()


def get_requires_for_build_editable(
    config_settings: Mapping[str, Any] | None = None,
) -> list[str]:
    return _requirements()


def prepare_metadata_for_build_wheel(
    metadata_directory: str,
    config_settings: Mapping[str, Any] | None = None,
) -> str:
    with _staged() as staging:
        return _delegate(
            "prepare_metadata_for_build_wheel",
            staging,
            metadata_directory,
            config_settings,
        )


def prepare_metadata_for_build_editable(
    metadata_directory: str,
    config_settings: Mapping[str, Any] | None = None,
) -> str:
    with _staged() as staging:
        return _delegate(
            "prepare_metadata_for_build_editable",
            staging,
            metadata_directory,
            config_settings,
        )


def build_wheel(
    wheel_directory: str,
    config_settings: Mapping[str, Any] | None = None,
    metadata_directory: str | None = None,
) -> str:
    target = _target_version(config_settings)
    with _staged(target) as staging:
        built = _delegate("build_wheel", staging, wheel_directory, config_settings)
    if target is None:
        return built
    # a wheel lowered for one python says so, so that a newer interpreter can be
    # given a better one. see `_retag`
    return _retag(Path(wheel_directory) / built, target)


def build_editable(
    wheel_directory: str,
    config_settings: Mapping[str, Any] | None = None,
    metadata_directory: str | None = None,
) -> str:
    # an editable install is a pointer at a directory, so the directory has to
    # outlive the build. it is the same one `by build` writes, which is what
    # makes the install editable at all: transpiled python is what gets imported,
    # and re-running the build is what updates it
    staging = Path.cwd() / EDITABLE_STAGING_DIRECTORY
    target = _target_version(config_settings)
    built = _stage(staging, target)
    _write_staged_pyproject(staging, built)
    editable = _delegate("build_editable", staging, wheel_directory, config_settings)
    if target is None:
        return editable
    # an editable wheel is a pointer, but it is still selected on its tag, so it
    # is tagged for the python it was lowered for like any other
    return _retag(Path(wheel_directory) / editable, target)


def build_sdist(
    sdist_directory: str,
    config_settings: Mapping[str, Any] | None = None,
) -> str:
    project_root = Path.cwd()
    metadata = _read_project_metadata(project_root)
    name = _normalized_name(metadata["name"])
    version = metadata["version"]
    directory_name = f"{name}-{version}"

    with tempfile.TemporaryDirectory() as scratch:
        staging = Path(scratch) / "build"
        # one build answers both halves of a source distribution: what the
        # project is made of, and — through the metadata the wheel would carry —
        # what to say about it
        built = _stage(staging)
        _write_staged_pyproject(staging, built)
        metadata_directory = Path(scratch) / "metadata"
        metadata_directory.mkdir()
        dist_info = _delegate(
            "prepare_metadata_for_build_wheel", staging, str(metadata_directory), None
        )
        pkg_info = (metadata_directory / dist_info / "METADATA").read_bytes()

        sdist = Path(sdist_directory)
        sdist.mkdir(parents=True, exist_ok=True)
        target = sdist / f"{directory_name}.tar.gz"
        with tarfile.open(target, "w:gz", format=tarfile.PAX_FORMAT) as archive:
            for source in built.sources:
                if not (project_root / source).is_file():
                    continue
                archive.add(
                    project_root / source,
                    arcname=f"{directory_name}/{source}",
                    recursive=False,
                )
            info = tarfile.TarInfo(f"{directory_name}/PKG-INFO")
            info.size = len(pkg_info)
            archive.addfile(info, io.BytesIO(pkg_info))

    return target.name


# ── staging ──────────────────────────────────────────────────────────────────


@contextmanager
def _staged(python_version: str | None = None) -> Iterator[Path]:
    """The project, built as python, in a directory that lasts for one hook."""
    with tempfile.TemporaryDirectory() as directory:
        staging = Path(directory) / "build"
        built = _stage(staging, python_version)
        _write_staged_pyproject(staging, built)
        yield staging


def _target_version(config_settings: Mapping[str, Any] | None) -> str | None:
    """The python this wheel is being lowered for, if one was asked for.

    Without it the build targets what the project declares it supports, which is
    the wheel that runs everywhere. With it, the wheel is for one python and is
    tagged so that only that python — or the next one up with no wheel of its own
    — will choose it.
    """
    if not config_settings:
        return None
    target = config_settings.get(TARGET_VERSION_SETTING)
    if target is None:
        return None
    target = str(target).strip()
    if not re.fullmatch(r"3\.\d+", target):
        raise BuildError(
            f"`{TARGET_VERSION_SETTING}` has to be a python version like `3.12`, not `{target}`"
        )
    return target


class Staged:
    """What a build read, what it produced, and what the result needs to run."""

    __slots__ = ("packages", "requires", "sources")

    def __init__(
        self,
        sources: list[str],
        packages: list[str],
        requires: list[str] | None = None,
    ) -> None:
        self.sources = sources
        self.packages = packages
        self.requires = requires or []


def _stage(staging: Path, python_version: str | None = None) -> Staged:
    """Build the project into `staging`, and report what came of it.

    Which files the project is made of, and which packages it builds into, are
    both the build's answers rather than this backend's. Reading a staged tree
    back to guess at them would be a second answer to keep in step — and it would
    guess wrong, since a directory in the output is not necessarily something the
    project ships.
    """
    arguments = ["build", "--out", str(staging), "--print-manifest"]
    if python_version:
        arguments += ["--min-version", python_version]

    sources: list[str] = []
    packages: list[str] = []
    requires: list[str] = []
    for line in _run_by(*arguments).splitlines():
        kind, _, value = line.strip().partition(" ")
        if kind == "input":
            sources.append(value)
        elif kind == "package":
            packages.append(value)
        elif kind == "requires":
            requires.append(value)
    return Staged(sorted(set(sources)), sorted(set(packages)), sorted(set(requires)))


def _write_staged_pyproject(staging: Path, built: Staged) -> None:
    """Describe the staged tree to the backend that packages it.

    The project's own `pyproject.toml` was copied across by the build, and it
    names *this* backend — handing it back unchanged would build the project
    again, forever. So it is replaced by one describing what the staged tree
    actually is: plain python, packages at the top level, and a version that is
    settled rather than dynamic.
    """
    project_root = Path.cwd()
    metadata = _read_project_metadata(project_root)
    if not built.packages:
        raise BuildError(
            "this project has no package to build a wheel from — a wheel needs at "
            "least one importable package, so a top-level module like `app.by` has "
            "to become `app/__init__.by`"
        )

    metadata["dependencies"] = _merged_dependencies(metadata, built.requires)

    document = {
        "build-system": {
            "requires": [UV_BUILD_REQUIREMENT],
            "build-backend": "uv_build",
        },
        "project": metadata,
        "tool": {
            "uv": {
                "build-backend": {
                    # the staged tree *is* the module root: `by build` already
                    # resolved the project's layout, so `src/pkg` arrives as `pkg`
                    "module-root": "",
                    "module-name": built.packages,
                }
            }
        },
    }
    (staging / "pyproject.toml").write_text(_toml(document), encoding="utf-8")


# ── tagging a wheel for the python it was lowered for ────────────────────────


def _retag(wheel: Path, python_version: str) -> str:
    """Re-tag `wheel` for one python, and return its new name.

    A wheel's tag is what an installer selects on. Left as `py3-none-any`, every
    wheel of a release looks equally good and the first one found wins — so a
    3.13 user could be handed code lowered for 3.9, with a `typing_extensions`
    dependency they have no use for. Tagged `py313-none-any`, that same user gets
    the wheel built for them, and a python with no wheel of its own falls back to
    the newest one below it.

    The backend that built it has no option for this, so the tag is rewritten
    here: in the archive's `WHEEL`, in the `RECORD` line that carries that file's
    hash, and in the file name itself.
    """
    tag = _python_tag(python_version)
    entries = _read_wheel(wheel)

    wheel_metadata = _dist_info_entry(entries, "WHEEL")
    metadata_info, metadata = entries[wheel_metadata]
    entries[wheel_metadata] = (metadata_info, _replace_tag(metadata, tag))

    record = _dist_info_entry(entries, "RECORD")
    record_info, recorded = entries[record]
    entries[record] = (
        record_info,
        _rerecord(recorded, wheel_metadata, entries[wheel_metadata][1]),
    )

    renamed = wheel.with_name(_retagged_name(wheel.name, tag))
    _write_wheel(renamed, entries)
    if renamed != wheel:
        wheel.unlink()
    return renamed.name


def _python_tag(python_version: str) -> str:
    return "py" + python_version.replace(".", "")


def _retagged_name(name: str, tag: str) -> str:
    """`thing-1.0-py3-none-any.whl` under a new python tag.

    The three tag fields are the last three before the extension, whatever the
    name and version in front of them contain.
    """
    stem, _, extension = name.rpartition(".")
    parts = stem.split("-")
    if len(parts) < 4:
        raise BuildError(f"`{name}` is not a wheel name this can re-tag")
    parts[-3] = tag
    return "-".join(parts) + "." + extension


def _read_wheel(wheel: Path) -> dict[str, tuple[zipfile.ZipInfo, bytes]]:
    with zipfile.ZipFile(wheel) as archive:
        return {
            info.filename: (info, archive.read(info)) for info in archive.infolist()
        }


def _write_wheel(
    wheel: Path, entries: Mapping[str, tuple[zipfile.ZipInfo, bytes]]
) -> None:
    """Write the entries back, each as it arrived but for what changed.

    The original `ZipInfo` is reused rather than rebuilt, because it carries what
    a wheel means by it: the mode, and so whether a file in `.data/scripts/` is
    still executable once installed. Building a fresh one silently made every
    entry `0o644`.
    """
    with zipfile.ZipFile(wheel, "w", zipfile.ZIP_DEFLATED) as archive:
        for info, payload in entries.values():
            archive.writestr(info, payload)


def _dist_info_entry(
    entries: Mapping[str, tuple[zipfile.ZipInfo, bytes]], name: str
) -> str:
    matches = [
        entry
        for entry in entries
        if entry.endswith(f".dist-info/{name}") and entry.count("/") == 1
    ]
    if len(matches) != 1:
        raise BuildError(f"the wheel does not hold exactly one `{name}` to rewrite")
    return matches[0]


def _replace_tag(wheel_metadata: bytes, tag: str) -> bytes:
    """Rewrite every `Tag:` line to name `tag`.

    A pure-python wheel carries one; rewriting all of them means a wheel that
    somehow carried several is either re-tagged wholly or not at all.
    """
    lines = wheel_metadata.decode("utf-8").splitlines(keepends=True)
    rewritten = []
    seen = False
    for line in lines:
        if line.startswith("Tag:"):
            seen = True
            _, _, rest = line.partition(":")
            fields = rest.strip().split("-")
            fields[0] = tag
            rewritten.append("Tag: " + "-".join(fields) + "\n")
        else:
            rewritten.append(line)
    if not seen:
        raise BuildError("the wheel's `WHEEL` has no `Tag:` to rewrite")
    return "".join(rewritten).encode("utf-8")


def _rerecord(record: bytes, path: str, payload: bytes) -> bytes:
    """Restate `path`'s hash and size in `RECORD`.

    `RECORD` is what an installer verifies the archive against, so a file
    rewritten without it is a wheel that reports itself as corrupt.
    """
    digest = base64.urlsafe_b64encode(hashlib.sha256(payload).digest()).rstrip(b"=")
    line = f"{path},sha256={digest.decode('ascii')},{len(payload)}\n"

    rewritten = []
    seen = False
    for existing in record.decode("utf-8").splitlines(keepends=True):
        if existing.split(",")[0] == path:
            seen = True
            rewritten.append(line)
        else:
            rewritten.append(existing)
    if not seen:
        raise BuildError(f"`RECORD` does not mention `{path}`")
    return "".join(rewritten).encode("utf-8")


# ── delegation ───────────────────────────────────────────────────────────────


def _delegate(
    hook_name: str,
    staging: Path,
    out_directory: str,
    config_settings: Mapping[str, Any] | None,
) -> str:
    try:
        import uv_build
    except ImportError as error:  # pragma: no cover - only without build isolation
        raise BuildError(
            f"packaging a basedpython project needs `{UV_BUILD_REQUIREMENT}`, which "
            "is normally installed into the build environment automatically. it is "
            "missing, which usually means the build was run with isolation disabled"
        ) from error

    hook = getattr(uv_build, hook_name)
    # the settings are this backend's, not the one it delegates to: `uv_build`
    # supports none of its own and warns about every one it is handed
    config_settings = None
    out = os.path.abspath(out_directory)
    Path(out).mkdir(parents=True, exist_ok=True)
    previous = Path.cwd()
    os.chdir(staging)
    try:
        with _scripts_on_path():
            return hook(out, config_settings)
    finally:
        os.chdir(previous)


@contextmanager
def _scripts_on_path() -> Iterator[None]:
    """Make this environment's console scripts findable by name.

    `uv_build` shells out to a `uv-build` executable it finds on `PATH`. A
    frontend is supposed to put the build environment's scripts there, and they
    all do — but the cost of not relying on it is one line.
    """
    scripts = sysconfig.get_path("scripts")
    previous = os.environ.get("PATH", "")
    os.environ["PATH"] = os.pathsep.join(filter(None, (scripts, previous)))
    try:
        yield
    finally:
        os.environ["PATH"] = previous


def _requirements() -> list[str]:
    requirements = [UV_BUILD_REQUIREMENT]
    if sys.version_info < (3, 11):
        requirements.append("tomli>=2")
    return requirements


# ── the project's own metadata ───────────────────────────────────────────────


def _merged_dependencies(
    metadata: Mapping[str, Any], introduced: Sequence[str]
) -> list[str]:
    """The project's dependencies, plus what lowering needs at run time.

    Building for an older python can put a name in the output that only
    `typing_extensions` has there, and the project never asked for it, so it
    cannot have declared it. A wheel that shipped without it would install
    cleanly and fail on the first import of the module that needs it.

    A project that already names the distribution keeps its own constraint: it
    knows something about the version it wants that this does not.
    """
    declared = list(metadata.get("dependencies", []))
    already = {_requirement_name(requirement) for requirement in declared}
    return declared + [
        requirement
        for requirement in introduced
        if _requirement_name(requirement) not in already
    ]


def _requirement_name(requirement: str) -> str:
    """The distribution a requirement names, normalized."""
    name = re.split(r"[\s<>=!~;\[(]", requirement.strip(), maxsplit=1)[0]
    return _normalized_name(name)


def _read_project_metadata(project_root: Path) -> dict[str, Any]:
    """The `[project]` table, with anything dynamic settled.

    It is carried into the staged tree verbatim, because it is what becomes the
    wheel's core metadata. The one thing that cannot be carried is a dynamic
    version: the backend downstream has no way to compute one, and the place the
    version actually lives — a `.by` module — is not something it can read. So it
    is resolved here, from the python that module was transpiled into.
    """
    pyproject = project_root / "pyproject.toml"
    if not pyproject.is_file():
        raise BuildError("a basedpython project needs a `pyproject.toml`")

    document = _parse_toml(pyproject.read_bytes())
    metadata = dict(document.get("project", {}))
    if not metadata:
        raise BuildError("`pyproject.toml` has no `[project]` table to build from")

    dynamic = list(metadata.get("dynamic", []))
    if "version" in dynamic:
        metadata["version"] = _dynamic_version(project_root, document)
        dynamic.remove("version")
    if dynamic:
        raise BuildError(
            "this backend cannot resolve dynamic metadata "
            f"{', '.join(sorted(dynamic))} — declare it in `[project]` instead"
        )
    metadata.pop("dynamic", None)
    if "version" not in metadata:
        raise BuildError("`[project]` has neither a `version` nor a dynamic one")
    return metadata


def _dynamic_version(project_root: Path, document: Mapping[str, Any]) -> str:
    """Read `__version__` out of the module the project points at."""
    configured = (
        document.get("tool", {})
        .get("basedpython", {})
        .get("build", {})
        .get("version-from")
    )
    if not configured:
        raise BuildError(
            '`[project] dynamic = ["version"]` needs somewhere to read the version '
            'from — set `[tool.basedpython.build] version-from = "src/pkg/__init__.by"`'
        )
    source = project_root / configured
    if not source.is_file():
        raise BuildError(
            f"`build.version-from` points at `{configured}`, which is not a file"
        )

    for line in source.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        # `__version__ = "1.2.3"`, however it is spelled around the assignment.
        # reading the text rather than importing keeps this from executing a
        # module that has not been transpiled yet
        if stripped.startswith("__version__"):
            _, _, value = stripped.partition("=")
            value = value.strip().split("#")[0].strip()
            if len(value) >= 2 and value[0] in "\"'" and value[-1] == value[0]:
                return value[1:-1]
    raise BuildError(f"`{configured}` has no `__version__` to read the version from")


def _parse_toml(contents: bytes) -> dict[str, Any]:
    try:
        import tomllib
    except ImportError:  # python < 3.11
        import tomli as tomllib  # type: ignore[no-redef]
    return tomllib.loads(contents.decode("utf-8"))


def _normalized_name(name: str) -> str:
    return re.sub(r"[-_.]+", "_", name).lower()


# ── writing the staged pyproject ─────────────────────────────────────────────


def _toml(document: Mapping[str, Any]) -> str:
    """Render a mapping as TOML.

    Only what a `[project]` table holds has to survive this: strings, numbers,
    booleans, arrays, tables, and arrays of tables. That is the whole of PEP 621,
    and rendering it here is what lets the project's metadata reach the packaging
    backend unaltered.
    """
    lines: list[str] = []
    _render_table(document, [], lines)
    return "\n".join(lines) + "\n"


def _render_table(
    table: Mapping[str, Any], path: Sequence[str], lines: list[str]
) -> None:
    scalars = {key: value for key, value in table.items() if not _is_table_like(value)}
    tables = {key: value for key, value in table.items() if _is_table_like(value)}

    if path and (scalars or not tables):
        lines.append(f"[{_render_key_path(path)}]")
    for key, value in scalars.items():
        lines.append(f"{_render_key(key)} = {_render_value(value)}")
    if path and (scalars or not tables):
        lines.append("")

    for key, value in tables.items():
        nested = [*path, key]
        if isinstance(value, dict):
            _render_table(value, nested, lines)
        else:
            for element in value:
                lines.append(f"[[{_render_key_path(nested)}]]")
                for inner_key, inner_value in element.items():
                    lines.append(
                        f"{_render_key(inner_key)} = {_render_value(inner_value)}"
                    )
                lines.append("")


def _is_table_like(value: Any) -> bool:
    if isinstance(value, dict):
        return True
    # an array of tables is rendered as `[[name]]` blocks; an array of anything
    # else is an ordinary inline array
    return (
        isinstance(value, list)
        and len(value) > 0
        and all(isinstance(element, dict) for element in value)
    )


def _render_key_path(path: Sequence[str]) -> str:
    return ".".join(_render_key(part) for part in path)


def _render_key(key: str) -> str:
    if key and all(character.isalnum() or character in "-_" for character in key):
        return key
    return _render_string(key)


def _render_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, str):
        return _render_string(value)
    if isinstance(value, (int, float)):
        return repr(value)
    if isinstance(value, dict):
        rendered = ", ".join(
            f"{_render_key(key)} = {_render_value(inner)}"
            for key, inner in value.items()
        )
        return "{" + rendered + "}"
    if isinstance(value, (list, tuple)):
        rendered = ", ".join(_render_value(element) for element in value)
        return "[" + rendered + "]"
    raise BuildError(
        f"`pyproject.toml` holds a value this backend cannot carry over: {value!r}"
    )


def _render_string(value: str) -> str:
    escaped = (
        value.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        .replace("\t", "\\t")
    )
    return f'"{escaped}"'


# ── the `by` executable ──────────────────────────────────────────────────────


def _run_by(*arguments: str) -> str:
    executable = _by_executable()
    completed = subprocess.run(
        [executable, *arguments],
        stdout=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        raise BuildError(
            f"`by {' '.join(arguments)}` failed with exit code {completed.returncode}"
        )
    return completed.stdout.decode("utf-8")


def _by_executable() -> str:
    """Find the `by` that belongs to this installation.

    It ships in the same distribution as this module, so it is in this
    environment's scripts directory. Looking there first rather than on `PATH`
    means the build uses the transpiler it was installed with, not whichever one
    happens to be earlier in the shell's path.
    """
    name = "by.exe" if os.name == "nt" else "by"
    candidates = [
        Path(sysconfig.get_path("scripts")) / name,
        Path(sys.executable).parent / name,
    ]
    for candidate in candidates:
        if candidate.is_file():
            return str(candidate)
    found = shutil.which(name)
    if found:
        return found
    raise BuildError(
        "could not find the `by` executable, which is what builds a basedpython "
        "project. it is installed by the `basedpython` distribution — check that "
        "`[build-system] requires` names it"
    )

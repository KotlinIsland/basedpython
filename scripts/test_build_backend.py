"""Tests for the basedpython PEP 517 build backend.

Run with::

    uv run --no-project --with pytest pytest scripts/test_build_backend.py

What is covered here is everything the backend decides on its own: what the
staged `pyproject.toml` says, how a project's metadata survives being written
back out, and where a dynamic version comes from. The packaging itself is
`uv_build`'s, and the transpile is `by`'s; neither is reimplemented here, so
neither is tested here.
"""

from __future__ import annotations

import contextlib
import os
import re
import sys
import tomllib
from collections.abc import Iterator
from pathlib import Path
from typing import Any

# the backend is not part of this project — it ships in the `basedpython` wheel —
# so it is reached the way a test reaches it, and `[tool.ty.environment] root`
# tells the checker the same thing
sys.path.insert(0, str(Path(__file__).parent.parent / "python"))

from basedpython.build import (
    BuildError,
    Staged,
    _merged_dependencies,
    _python_tag,
    _read_project_metadata,
    _replace_tag,
    _rerecord,
    _retag,
    _retagged_name,
    _target_version,
    _toml,
    _write_staged_pyproject,
)


@contextlib.contextmanager
def reports(pattern: str) -> Iterator[None]:
    """Assert that the block fails, with a message matching `pattern`.

    `pytest.raises` would say this, but pytest is not a dependency of this
    project — it is what runs the file, not what the file is written against —
    and an import of it is one the checker over `scripts/` cannot resolve.
    """
    try:
        yield
    except BuildError as error:
        assert re.search(pattern, str(error)), f"{pattern!r} does not match {error}"
    else:
        raise AssertionError(f"expected a failure matching {pattern!r}")


def render_and_parse(document: dict[str, Any]) -> dict[str, Any]:
    """A document, through the writer and back."""
    return tomllib.loads(_toml(document))


# ── the TOML the staged tree is described by ─────────────────────────────────


def test_scalars_survive_the_round_trip():
    document = {
        "project": {
            "name": "thing",
            "version": "1.2.3",
            "requires-python": ">=3.12",
            "keywords": ["a", "b"],
        }
    }
    assert render_and_parse(document) == document


def test_a_nested_table_survives_the_round_trip():
    document = {
        "project": {
            "name": "thing",
            "version": "1.0.0",
            "urls": {"Repository": "https://example.com/thing"},
            "optional-dependencies": {"dev": ["pytest>=8"]},
        }
    }
    assert render_and_parse(document) == document


def test_an_array_of_tables_survives_the_round_trip():
    document = {
        "project": {
            "name": "thing",
            "version": "1.0.0",
            "authors": [
                {"name": "A Person", "email": "a@example.com"},
                {"name": "Another"},
            ],
        }
    }
    assert render_and_parse(document) == document


def test_a_key_that_is_not_bare_is_quoted():
    """`[project.entry-points."my.group"]` is an ordinary shape, and a key with a
    dot in it splits into two tables unless it is quoted."""
    document = {
        "project": {
            "name": "thing",
            "version": "1.0.0",
            "entry-points": {"my.group": {"plugin": "thing:main"}},
        }
    }
    assert render_and_parse(document) == document


def test_a_string_that_needs_escaping_survives_the_round_trip():
    document = {
        "project": {
            "name": "thing",
            "version": "1.0.0",
            "description": 'a "quoted" \\ backslash\nand a newline',
        }
    }
    assert render_and_parse(document) == document


def test_booleans_and_numbers_are_rendered_as_themselves():
    document = {"tool": {"example": {"flag": True, "count": 3}}}
    assert render_and_parse(document) == document


def test_an_empty_array_stays_an_array():
    """`dependencies = []` is not an array of tables, however it looks."""
    document = {"project": {"name": "thing", "version": "1.0.0", "dependencies": []}}
    assert render_and_parse(document) == document


def test_a_value_that_cannot_be_carried_over_is_reported():
    with reports("cannot carry over"):
        _toml({"project": {"name": "thing", "version": object()}})


# ── reading the project's own metadata ───────────────────────────────────────


def write_project(directory: Path, pyproject: str) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "pyproject.toml").write_text(pyproject, encoding="utf-8")
    return directory


def test_the_project_table_is_carried_over_whole(tmp_path: Path):
    root = write_project(
        tmp_path,
        """
        [build-system]
        requires = ["basedpython"]
        build-backend = "basedpython.build"

        [project]
        name = "thing"
        version = "1.0.0"
        dependencies = ["packaging>=24"]
        """,
    )
    metadata = _read_project_metadata(root)
    assert metadata == {
        "name": "thing",
        "version": "1.0.0",
        "dependencies": ["packaging>=24"],
    }


def test_a_project_without_a_pyproject_is_reported(tmp_path: Path):
    with reports(r"needs a \`pyproject\.toml\`"):
        _read_project_metadata(tmp_path)


def test_a_pyproject_without_a_project_table_is_reported(tmp_path: Path):
    root = write_project(tmp_path, '[build-system]\nrequires = ["basedpython"]\n')
    with reports("no `\\[project\\]` table"):
        _read_project_metadata(root)


def test_a_dynamic_version_is_read_from_the_module_it_points_at(tmp_path: Path):
    root = write_project(
        tmp_path,
        """
        [project]
        name = "thing"
        dynamic = ["version"]

        [tool.basedpython.build]
        version-from = "src/thing/__init__.by"
        """,
    )
    module = root / "src" / "thing"
    module.mkdir(parents=True)
    (module / "__init__.by").write_text(
        '"""a docstring"""\n\n__version__ = "4.5.6"  # the one that counts\n',
        encoding="utf-8",
    )

    metadata = _read_project_metadata(root)
    assert metadata["version"] == "4.5.6"
    assert "dynamic" not in metadata


def test_a_dynamic_version_with_nowhere_to_read_it_from_is_reported(
    tmp_path: Path,
):
    root = write_project(
        tmp_path,
        '[project]\nname = "thing"\ndynamic = ["version"]\n',
    )
    with reports("version-from"):
        _read_project_metadata(root)


def test_a_version_source_without_a_version_is_reported(tmp_path: Path):
    root = write_project(
        tmp_path,
        """
        [project]
        name = "thing"
        dynamic = ["version"]

        [tool.basedpython.build]
        version-from = "empty.by"
        """,
    )
    (root / "empty.by").write_text("x = 1\n", encoding="utf-8")
    with reports("no `__version__`"):
        _read_project_metadata(root)


def test_other_dynamic_metadata_is_reported_rather_than_dropped(tmp_path: Path):
    """Silently dropping it would ship a wheel missing what the project declared."""
    root = write_project(
        tmp_path,
        '[project]\nname = "thing"\nversion = "1.0.0"\ndynamic = ["readme"]\n',
    )
    with reports("readme"):
        _read_project_metadata(root)


# ── describing the staged tree to the backend that packages it ───────────────


def staged_pyproject(tmp_path: Path, pyproject: str, built: Staged) -> dict[str, Any]:
    """The document written into the staging directory for a project."""
    root = write_project(tmp_path / "project", pyproject)
    staging = tmp_path / "staging"
    staging.mkdir()

    previous = Path.cwd()
    os.chdir(root)
    try:
        _write_staged_pyproject(staging, built)
    finally:
        os.chdir(previous)
    return tomllib.loads((staging / "pyproject.toml").read_text(encoding="utf-8"))


PROJECT = """
[build-system]
requires = ["basedpython"]
build-backend = "basedpython.build"

[project]
name = "thing"
version = "1.0.0"
"""


def test_the_staged_project_is_packaged_by_the_delegate(tmp_path: Path):
    """The project's own `pyproject.toml` names this backend. Handed back
    unchanged it would build the project again, forever."""
    document = staged_pyproject(
        tmp_path, PROJECT, Staged(sources=[], packages=["thing"])
    )
    assert document["build-system"]["build-backend"] == "uv_build"
    assert document["project"]["name"] == "thing"


def test_the_staged_tree_is_its_own_module_root(tmp_path: Path):
    """`by build` already resolved the layout, so `src/pkg` arrives as `pkg`."""
    document = staged_pyproject(
        tmp_path, PROJECT, Staged(sources=[], packages=["alpha", "beta"])
    )
    assert document["tool"]["uv"]["build-backend"]["module-root"] == ""
    assert document["tool"]["uv"]["build-backend"]["module-name"] == ["alpha", "beta"]


def test_a_project_with_no_package_to_ship_is_reported(tmp_path: Path):
    with reports("no package to build a wheel from"):
        staged_pyproject(tmp_path, PROJECT, Staged(sources=[], packages=[]))


# ── what lowering needs at run time ──────────────────────────────────────────


def test_a_dependency_lowering_introduced_is_declared():
    """Building for an older python can put a name in the output that only
    `typing_extensions` has there. The project never asked for it, so it cannot
    have declared it — and a wheel without it fails on the first import."""
    merged = _merged_dependencies(
        {"dependencies": ["packaging>=24"]}, ["typing_extensions>=4.12"]
    )
    assert merged == ["packaging>=24", "typing_extensions>=4.12"]


def test_a_project_with_no_dependencies_still_gets_what_it_needs():
    assert _merged_dependencies({}, ["typing_extensions>=4.12"]) == [
        "typing_extensions>=4.12"
    ]


def test_nothing_is_added_when_lowering_needed_nothing():
    assert _merged_dependencies({"dependencies": ["packaging>=24"]}, []) == [
        "packaging>=24"
    ]


def test_a_constraint_the_project_already_declared_is_left_alone():
    """It knows something about the version it wants that this does not."""
    merged = _merged_dependencies(
        {"dependencies": ["typing_extensions==4.13.2"]}, ["typing_extensions>=4.12"]
    )
    assert merged == ["typing_extensions==4.13.2"]


def test_a_declaration_is_matched_however_it_is_spelled():
    """`typing-extensions` and `typing_extensions` are one distribution, and a
    requirement can carry an extra, a marker or a comparator after the name."""
    for spelling in (
        "typing-extensions",
        "Typing_Extensions >= 4.0",
        "typing-extensions[all]>=4",
        'typing_extensions>=4; python_version < "3.11"',
    ):
        merged = _merged_dependencies(
            {"dependencies": [spelling]}, ["typing_extensions>=4.12"]
        )
        assert merged == [spelling], spelling


# ── tagging a wheel for the python it was lowered for ────────────────────────


def test_the_target_version_comes_from_the_config_setting():
    assert _target_version({"python-version": "3.12"}) == "3.12"
    assert _target_version(None) is None
    assert _target_version({}) is None
    assert _target_version({"other": "x"}) is None


def test_a_target_that_is_not_a_python_version_is_reported():
    for bad in ("3", "py312", "3.12.1", "latest", ""):
        with reports("python version"):
            _target_version({"python-version": bad})


def test_a_version_becomes_the_tag_an_installer_selects_on():
    assert _python_tag("3.9") == "py39"
    assert _python_tag("3.13") == "py313"


def test_only_the_python_field_of_the_name_changes():
    assert (
        _retagged_name("thing-1.0-py3-none-any.whl", "py313")
        == "thing-1.0-py313-none-any.whl"
    )
    # a version with its own hyphens leaves the three tag fields where they are
    assert (
        _retagged_name("thing-1.0-rc1-py3-none-any.whl", "py39")
        == "thing-1.0-rc1-py39-none-any.whl"
    )


def test_a_name_that_is_not_a_wheel_is_reported():
    with reports("re-tag"):
        _retagged_name("nonsense.whl", "py39")


def test_the_tag_line_is_rewritten_and_the_rest_is_left_alone():
    original = b"Wheel-Version: 1.0\nGenerator: uv 0.12.5\nRoot-Is-Purelib: true\nTag: py3-none-any\n"
    rewritten = _replace_tag(original, "py311")
    assert b"Tag: py311-none-any\n" in rewritten
    assert b"Generator: uv 0.12.5\n" in rewritten
    assert b"Root-Is-Purelib: true\n" in rewritten


def test_a_wheel_with_no_tag_to_rewrite_is_reported():
    with reports("`Tag:`"):
        _replace_tag(b"Wheel-Version: 1.0\n", "py39")


def test_the_record_restates_the_file_that_changed():
    """`RECORD` is what an installer verifies against, so a file rewritten
    without it is a wheel that reports itself as corrupt."""
    record = (
        b"thing/__init__.py,sha256=AAA,0\n"
        b"thing-1.0.dist-info/WHEEL,sha256=STALE,10\n"
        b"thing-1.0.dist-info/RECORD,,\n"
    )
    rewritten = _rerecord(record, "thing-1.0.dist-info/WHEEL", b"Tag: py39-none-any\n")
    lines = rewritten.decode().splitlines()
    assert lines[0] == "thing/__init__.py,sha256=AAA,0"
    assert lines[1].startswith("thing-1.0.dist-info/WHEEL,sha256=")
    assert not lines[1].endswith("STALE,10")
    assert lines[1].endswith(",19")
    assert lines[2] == "thing-1.0.dist-info/RECORD,,"


def test_a_record_that_does_not_mention_the_file_is_reported():
    with reports("RECORD"):
        _rerecord(b"thing/__init__.py,sha256=AAA,0\n", "missing/WHEEL", b"")


def build_wheel_fixture(directory: Path) -> Path:
    """A minimal but valid wheel, tagged generically."""
    import zipfile

    wheel = directory / "thing-1.0-py3-none-any.whl"
    metadata = b"Wheel-Version: 1.0\nGenerator: test\nRoot-Is-Purelib: true\nTag: py3-none-any\n"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr("thing/__init__.py", "x = 1\n")
        archive.writestr("thing-1.0.dist-info/WHEEL", metadata)
        archive.writestr("thing-1.0.dist-info/METADATA", "Name: thing\n")
        archive.writestr(
            "thing-1.0.dist-info/RECORD",
            "thing/__init__.py,sha256=AAA,6\n"
            f"thing-1.0.dist-info/WHEEL,sha256=STALE,{len(metadata)}\n"
            "thing-1.0.dist-info/RECORD,,\n",
        )
    return wheel


def test_a_retagged_wheel_keeps_everything_but_its_tag(tmp_path: Path):
    import base64
    import hashlib
    import zipfile

    wheel = build_wheel_fixture(tmp_path)
    name = _retag(wheel, "3.13")

    assert name == "thing-1.0-py313-none-any.whl"
    assert not wheel.exists(), "the wheel under the old name is gone"

    with zipfile.ZipFile(tmp_path / name) as archive:
        assert archive.read("thing/__init__.py") == b"x = 1\n"
        assert b"Tag: py313-none-any" in archive.read("thing-1.0.dist-info/WHEEL")

        # and the record agrees with what is actually in the archive, or an
        # installer reports the wheel as corrupt
        payload = archive.read("thing-1.0.dist-info/WHEEL")
        digest = base64.urlsafe_b64encode(hashlib.sha256(payload).digest()).rstrip(b"=")
        recorded = archive.read("thing-1.0.dist-info/RECORD").decode()
        assert f"sha256={digest.decode()},{len(payload)}" in recorded


def test_a_retagged_wheel_keeps_the_modes_it_arrived_with(tmp_path: Path):
    """A wheel's entries carry their mode, and an installer honours it — so a
    script in `.data/scripts/` that arrives executable has to leave executable.
    Rebuilding each entry's metadata instead of reusing it made every one
    `0o644`."""
    import zipfile

    wheel = tmp_path / "thing-1.0-py3-none-any.whl"
    metadata = b"Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n"
    script = b"#!/bin/sh\necho hi\n"
    with zipfile.ZipFile(wheel, "w") as archive:
        executable = zipfile.ZipInfo("thing-1.0.data/scripts/tool")
        executable.external_attr = (0o755 << 16) | 0o100000
        archive.writestr(executable, script)
        archive.writestr("thing-1.0.dist-info/WHEEL", metadata)
        archive.writestr(
            "thing-1.0.dist-info/RECORD",
            f"thing-1.0.data/scripts/tool,sha256=x,{len(script)}\n"
            f"thing-1.0.dist-info/WHEEL,sha256=y,{len(metadata)}\n"
            "thing-1.0.dist-info/RECORD,,\n",
        )

    name = _retag(wheel, "3.13")

    with zipfile.ZipFile(tmp_path / name) as archive:
        mode = archive.getinfo("thing-1.0.data/scripts/tool").external_attr >> 16
        assert mode == 0o755, f"expected 0o755, got {mode:o}"
        assert archive.read("thing-1.0.data/scripts/tool") == script

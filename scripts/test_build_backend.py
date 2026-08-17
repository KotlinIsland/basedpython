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

# the backend is not part of this project — it ships in the `basedpython` wheel —
# so it is reached the way a test reaches it, and `[tool.ty.environment] root`
# tells the checker the same thing
sys.path.insert(0, str(Path(__file__).parent.parent / "python"))

from basedpython.build import (
    BuildError,
    Staged,
    _merged_dependencies,
    _read_project_metadata,
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


def render_and_parse(document: dict) -> dict:
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


def staged_pyproject(tmp_path: Path, pyproject: str, built: Staged) -> dict:
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

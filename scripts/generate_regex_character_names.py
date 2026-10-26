# /// script
# requires-python = ">=3.12"
# dependencies = []
#
# [tool.uv]
# no-build = true
# exclude-newer = "P7D"
# ///

r"""Generate `crates/ty_python_semantic/src/types/regex/character_names.rs`.

`re` looks the name in a `\N{...}` escape up in the `unicodedata` of the python
that compiles the pattern, and each python minor ships the unicode database of one
unicode version. Character names are never changed or withdrawn once assigned, so
what differs between two pythons is which code points have a name at all. This
asks each python which code points it names and writes the ones a python after
the oldest names first, with the minor version that first names them.

A name can also be an alias, and unicode adds aliases to characters that have long
had a name. So this also asks each python which of the aliases in the unicode data
`unicode_names2` is built from it resolves, and writes the ones a python after the
oldest resolves first, with the minor version that first resolves them.

The pythons are run with `uv run --managed-python`, which downloads any of them
that is not installed yet.
"""

from __future__ import annotations

import subprocess
import textwrap
from pathlib import Path
from typing import Final

MODULE_CRATE: Final = "ty_python_semantic"
MODULE_PATH: Final = (
    Path("crates") / MODULE_CRATE / "src" / "types" / "regex" / "character_names.rs"
)
PYTHON_VERSIONS: Final = range(8, 16)  # Python 3.8 through 3.15

# prints the unicode version, then each run of consecutive code points that have a
# name as `first last`, in hex. a `CJK UNIFIED IDEOGRAPH-` name is left out: those are
# derived from the code point by a rule each python implements a little differently
# (3.8 names `2EBE1` to `2EBEF`, which no unicode assigns), so the regex reader never
# reports one, and has no use for when each was first named
NAMED_RANGES: Final = """
import unicodedata
print(unicodedata.unidata_version)
start = None
for cp in range(0x110001):
    name = unicodedata.name(chr(cp), None) if cp < 0x110000 else None
    named = name is not None and not name.startswith("CJK UNIFIED IDEOGRAPH-")
    if named and start is None:
        start = cp
    elif not named and start is not None:
        print(f"{start:x} {cp - 1:x}")
        start = None
"""


NAME_ALIASES: Final = (
    Path("crates") / "ruff_unicode_names2" / "data" / "NameAliases.txt"
)

# reads `alias code` lines, in hex, from stdin and prints each alias it resolves to
# that code point
RESOLVED_ALIASES: Final = """
import sys
import unicodedata
for line in sys.stdin:
    alias, code = line.rstrip("\\n").split(";")
    try:
        resolved = unicodedata.lookup(alias)
    except KeyError:
        continue
    if resolved == chr(int(code, 16)):
        print(alias)
"""


def name_aliases() -> list[tuple[str, str]]:
    """every alias in the unicode data, with the code point it names in hex"""
    aliases = []
    for line in NAME_ALIASES.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        match line.split(";"):
            case [code, alias, _kind]:
                aliases.append((alias, code))
            case _:
                raise ValueError(f"not a NameAliases.txt entry: {line!r}")
    return aliases


def resolved_on_version(minor: int, aliases: list[tuple[str, str]]) -> set[str]:
    proc = subprocess.run(
        [
            "uv",
            "run",
            "--managed-python",
            "--no-project",
            "--python",
            f"3.{minor}",
            "python",
            "-I",
            "-c",
            RESOLVED_ALIASES,
        ],
        input="".join(f"{alias};{code}\n" for alias, code in aliases),
        check=True,
        text=True,
        capture_output=True,
    )
    return set(proc.stdout.splitlines())


def named_on_version(minor: int) -> tuple[str, set[int]]:
    proc = subprocess.run(
        [
            "uv",
            "run",
            "--managed-python",
            "--no-project",
            "--python",
            f"3.{minor}",
            "python",
            "-I",
            "-c",
            NAMED_RANGES,
        ],
        check=True,
        text=True,
        capture_output=True,
    )
    match proc.stdout.splitlines():
        case [unicode_version, *ranges]:
            named: set[int] = set()
            for line in ranges:
                match line.split():
                    case [first, last]:
                        named.update(range(int(first, 16), int(last, 16) + 1))
                    case _:
                        raise ValueError(f"not a code point range: {line!r}")
            return unicode_version, named
        case _:
            raise ValueError(f"python 3.{minor} printed no unicode version")


def generate_module() -> None:
    by_version = {minor: named_on_version(minor) for minor in PYTHON_VERSIONS}
    oldest = PYTHON_VERSIONS[0]

    # the code point -> the first minor that names it, for those the oldest does not
    first_named: dict[int, int] = {}
    previous = by_version[oldest][1]
    for minor in PYTHON_VERSIONS[1:]:
        named = by_version[minor][1]
        withdrawn = previous - named
        if withdrawn:
            raise SystemExit(
                f"python 3.{minor} names no code point {min(withdrawn):04X}, "
                f"which python 3.{minor - 1} names"
            )
        for cp in named - previous:
            first_named[cp] = minor
        previous = named

    runs: list[tuple[int, int, int]] = []
    for cp in sorted(first_named):
        minor = first_named[cp]
        if runs and runs[-1][1] == cp - 1 and runs[-1][2] == minor:
            runs[-1] = (runs[-1][0], cp, minor)
        else:
            runs.append((cp, cp, minor))

    aliases = name_aliases()
    resolved = {minor: resolved_on_version(minor, aliases) for minor in PYTHON_VERSIONS}
    newest = PYTHON_VERSIONS[-1]
    unresolved = {alias for alias, _ in aliases} - resolved[newest]
    if unresolved:
        raise SystemExit(
            f"python 3.{newest} does not resolve the alias {min(unresolved)}, "
            f"which {NAME_ALIASES} lists"
        )
    # the alias -> the first minor that resolves it, for those the oldest does not
    first_resolved: dict[str, int] = {}
    previous_aliases = resolved[oldest]
    for minor in PYTHON_VERSIONS[1:]:
        withdrawn = previous_aliases - resolved[minor]
        if withdrawn:
            raise SystemExit(
                f"python 3.{minor} does not resolve the alias {min(withdrawn)}, "
                f"which python 3.{minor - 1} resolves"
            )
        for alias in resolved[minor] - previous_aliases:
            first_resolved[alias] = minor
        previous_aliases = resolved[minor]
    alias_entries = "\n".join(
        f'    ("{alias}", {first_resolved[alias]}),' for alias in sorted(first_resolved)
    )

    versions = "\n".join(
        f"//! - python 3.{minor}: unicode {by_version[minor][0]}"
        for minor in PYTHON_VERSIONS
    )
    entries = "\n".join(
        f"    (0x{first:04X}, 0x{last:04X}, {minor})," for first, last, minor in runs
    )
    MODULE_PATH.write_text(
        textwrap.dedent(
            """\
            //! this file is generated by `scripts/generate_regex_character_names.py`
            //!
            //! the unicode database each python minor ships:
            //!
            {versions}

            /// the code points python 3.{oldest} has no name for that a later python does, as
            /// `(first, last, minor)` runs: every code point from `first` to `last` is named
            /// from python 3.`minor` on. sorted by `first`, and the runs do not overlap
            pub(super) const NAMED_SINCE: &[(u32, u32, u8)] = &[
            {entries}
            ];

            /// the aliases python 3.{oldest} does not resolve that a later python does, as
            /// `(alias, minor)`: `alias` resolves from python 3.`minor` on. sorted by alias
            pub(super) const ALIASED_SINCE: &[(&str, u8)] = &[
            {alias_entries}
            ];
            """
        ).format(
            versions=versions,
            oldest=oldest,
            entries=entries,
            alias_entries=alias_entries,
        )
    )

    subprocess.run(["rustfmt", "--edition", "2024", MODULE_PATH], check=True)


if __name__ == "__main__":
    generate_module()

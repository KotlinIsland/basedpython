"""check the basedpython feature docs against the docs site nav

three lists are meant to agree: the pages on disk under `docs/basedpython/features`,
the links in that directory's hand-written `index.md`, and the `features` group of
the `zensical.toml` nav. a page missing from the nav is orphaned in the built site,
and a page missing from `index.md` is undiscoverable from the reference index

`zensical build --strict` does not report either case, so this is the only guard
against the three drifting apart
"""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).parent.parent
FEATURES = Path("docs/basedpython/features")

# reachable through in-page links rather than the nav, deliberately
NAV_EXEMPT: set[str] = set()

# a link in `index.md` that leaves the features directory is prose, not an entry
# in the reference index, so it takes no part in the three-way comparison
SIBLING_LINK = re.compile(r"^[\w.-]+\.md$")


def nav_paths(nav: object) -> list[str]:
    """every document path in the nav, in nav order"""
    if isinstance(nav, str):
        return [nav]
    if isinstance(nav, list):
        return [path for entry in nav for path in nav_paths(entry)]
    if isinstance(nav, dict):
        return [path for value in nav.values() for path in nav_paths(value)]
    return []


def feature_nav_paths(nav: object) -> list[str]:
    """the paths under the nav's `features` group, in nav order"""
    if isinstance(nav, list):
        return [path for entry in nav for path in feature_nav_paths(entry)]
    if isinstance(nav, dict):
        for key, value in nav.items():
            if key == "features" and isinstance(value, list):
                return nav_paths(value)
            if found := feature_nav_paths(value):
                return found
    return []


def main() -> int:
    docs = ROOT / "docs/basedpython"
    config = tomllib.loads((ROOT / "zensical.toml").read_text())
    nav = config["project"]["nav"]

    on_disk = sorted(
        p.name for p in (ROOT / FEATURES).glob("*.md") if p.name != "index.md"
    )

    index_order = [
        link
        for link in re.findall(
            r"\]\(([^)\s]+\.md)\)", (ROOT / FEATURES / "index.md").read_text()
        )
        if SIBLING_LINK.match(link)
    ]
    indexed = set(index_order)

    all_nav = nav_paths(nav)
    nav_order = [
        Path(p).name for p in feature_nav_paths(nav) if Path(p).name != "index.md"
    ]
    navved = set(nav_order)

    problems: list[str] = []

    def report(label: str, items: list[str]) -> None:
        if items:
            problems.append(f"{label}:\n" + "\n".join(f"  - {i}" for i in items))

    report(
        f"on disk but not linked from {FEATURES}/index.md",
        sorted(set(on_disk) - indexed),
    )
    report("on disk but not in the zensical.toml nav", sorted(set(on_disk) - navved))
    report(
        f"linked from {FEATURES}/index.md but missing on disk",
        sorted(indexed - set(on_disk)),
    )
    report(
        "in the zensical.toml nav but missing on disk",
        sorted(p for p in all_nav if not (docs / p).is_file()),
    )
    report(
        "duplicated in the zensical.toml nav",
        sorted({p for p in all_nav if all_nav.count(p) > 1}),
    )
    # the rest of the docs tree only needs the orphan check; `features` is covered
    # precisely by the two checks above
    report(
        "outside `features`, on disk but neither in the nav nor exempt",
        sorted(
            relative
            for p in docs.rglob("*.md")
            if (relative := str(p.relative_to(docs))) not in set(all_nav) | NAV_EXEMPT
            and Path(relative).parent != Path(FEATURES.name)
        ),
    )

    # the nav ordering mirrors the index's section order
    if navved == indexed and nav_order != index_order:
        divergence = next(
            (n, i) for n, i in zip(nav_order, index_order, strict=True) if n != i
        )
        problems.append(
            f"nav order diverges from index order: "
            f"nav has {divergence[0]} where index has {divergence[1]}"
        )

    if problems:
        print("\n\n".join(problems), file=sys.stderr)
        print(
            f"\n{len(problems)} problem(s) — see {Path(__file__).name}",
            file=sys.stderr,
        )
        return 1

    print(f"{len(on_disk)} feature pages: index, nav, and disk agree")
    return 0


if __name__ == "__main__":
    sys.exit(main())

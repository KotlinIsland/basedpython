"""check the basedpython pygments lexer against the docs it highlights

`python/basedpython-pygments` approximates basedpython's soft keywords with
lookaheads rather than a parser, so it can drift from the language. three checks
keep it anchored:

1. every ```by``` block in `docs/basedpython` lexes without producing an error
   token
2. across every *other* fenced language, the only text that lexes to an error
   token is the docs' inlay-hint notation, which no stock lexer knows. the
   stylesheet leans on that: it renders an error token like the hint it stands
   for, which is only safe while nothing else produces one
3. a table of snippets, one per keyword, comes out classified as a keyword.
   this is what catches a keyword being added to the language and to the docs
   but never to the lexer

run directly, or through `prek`
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

from pygments.lexers import get_lexer_by_name
from pygments.token import Error, Keyword
from pygments.util import ClassNotFound

ROOT = Path(__file__).parent.parent
DOCS = ROOT / "docs/basedpython"

# a fence may be indented, inside a list item or an admonition
FENCE = re.compile(r"^([ \t]*)```([\w+-]*)\n(.*?)^\1```", re.DOTALL | re.MULTILINE)

# the docs write an inlay hint — what an editor renders, not what the file
# contains — between these. the `by` lexer knows them; no other lexer does
HINT_DELIMITERS = {"⟨", "⟩"}

# each entry is a snippet and the word in it that must lex as a keyword. keep
# the snippets minimal — they document the position that makes the word a
# keyword, which is exactly what the lexer's lookaheads encode
KEYWORDS = [
    ("let x = 1", "let"),
    ("def f(var name: str): ...", "var"),
    ("a: typeof b", "typeof"),
    ("b = a cast int", "cast"),
    ("b = a cast! int", "cast!"),
    ("b = a cast? int", "cast?"),
    ("sentinel MISSING", "sentinel"),
    ("extension list[int]:", "extension"),
    ("build:", "build"),
    ("implementation Show for Point:", "implementation"),
    ("def f(x: protocol(a: int)): ...", "protocol"),
    ("class C[reified T]: ...", "reified"),
    ("def check(x: int | None) -> asserts x: ...", "asserts"),
    ("def parse(t: str) -> int raises ValueError: ...", "raises"),
    ("def f(local xs: list[int]): ...", "local"),
    ("def f(once cb: () -> None): ...", "once"),
    ("from x export y", "export"),
    ("data class Point:", "data"),
    ("enum class Shape:", "enum"),
    ("sealed class Shape:", "sealed"),
    ("frozen data class D:", "frozen"),
    ("abstract class A:", "abstract"),
    ("open class A:", "open"),
    ("final def f(): ...", "final"),
    ("override def f(): ...", "override"),
    ("static let x: int", "static"),
    ("private type X = int", "private"),
    ("public let x = 1", "public"),
    ("late var x: int", "late"),
    ("class Mapping[out Key]: ...", "out"),
    ("def f(x: literal int): ...", "literal"),
    ("class C[T: constraints (int, str)]: ...", "constraints"),
    ("a: dynamic = 1", "dynamic"),
    ("    get() = 1", "get"),
    ("    set(value):", "set"),
    ("    field = value", "field"),
]

# words the lexer must leave alone, because they are ordinary names here. these
# are the cost of the soft-keyword lookaheads being wrong in the other
# direction, which is just as visible in the rendered page
NON_KEYWORDS = [
    ("os.environ.get(key)", "get"),
    ("def get(self) -> int: ...", "get"),
    ("def read(data: bytes): ...", "data"),
    ("import enum", "enum"),
    ("literal: object | None", "literal"),
    ("x = open(path)", "open"),
    ("cast(int, x)", "cast"),
    ("build: int = 3", "build"),
    ("print(build.GIT_SHA)", "build"),
]


def blocks() -> list[tuple[Path, str, str]]:
    """every fenced block in the docs, as (path, language, source)"""
    found = []
    for path in sorted(DOCS.rglob("*.md")):
        found += [
            (path, m.group(2), m.group(3)) for m in FENCE.finditer(path.read_text())
        ]
    return found


def main() -> int:
    by_lexer = get_lexer_by_name("by")
    problems: list[str] = []
    by_blocks = 0

    for path, language, source in blocks():
        if not language:
            continue
        try:
            lexer = get_lexer_by_name(language)
        except ClassNotFound:
            # a fence tagged with something pygments has no lexer for renders
            # as plain text, which is a deliberate choice, not a lexer bug
            continue
        by_blocks += language == "by"
        # a `by` block must lex cleanly; anything else may only fail on the
        # inlay-hint notation, which the stylesheet renders as a hint
        allowed: set[str] = set() if language == "by" else HINT_DELIMITERS
        errors = {
            value
            for token, value in lexer.get_tokens(source)
            if token is Error and value not in allowed
        }
        if errors:
            relative = path.relative_to(ROOT)
            problems.append(
                f"{relative}: `{language}` block lexes to error token(s): "
                f"{sorted(errors)}"
            )

    for snippet, word in KEYWORDS:
        tokens = [
            (token, value)
            for token, value in by_lexer.get_tokens(snippet)
            if value.strip()
        ]
        if not any(value == word and token in Keyword for token, value in tokens):
            actual = next(
                (str(token) for token, value in tokens if value == word),
                "not tokenized as one word",
            )
            problems.append(
                f"`{snippet}`: expected `{word}` to be a keyword, got {actual}"
            )

    for snippet, word in NON_KEYWORDS:
        tokens = [
            (token, value)
            for token, value in by_lexer.get_tokens(snippet)
            if value.strip()
        ]
        if any(value == word and token in Keyword for token, value in tokens):
            problems.append(
                f"`{snippet}`: expected `{word}` to be an ordinary name, got a keyword"
            )

    if problems:
        print("\n".join(problems), file=sys.stderr)
        print(
            f"\n{len(problems)} problem(s) — see {Path(__file__).name}",
            file=sys.stderr,
        )
        return 1

    print(
        f"{by_blocks} `by` blocks lex cleanly; "
        f"{len(KEYWORDS)} keywords and {len(NON_KEYWORDS)} names classified"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

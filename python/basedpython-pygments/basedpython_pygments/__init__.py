"""a pygments lexer for basedpython

the docs are almost entirely ```by``` code, and pygments has no idea what `by`
is — every block used to render as undifferentiated plain text. this lexer
extends pygments' python lexer with basedpython's surface syntax so the
reference reads like code instead of a wall of grey

basedpython's added keywords are all *soft*: `get`, `data`, `out` and friends
stay perfectly good identifiers, and the parser tells them apart by position.
a regex lexer has no parser to ask, so the rules below fall into two tiers:

- `RESERVED` — words with no python meaning and no plausible use as a name.
  matched anywhere
- everything else — matched only through a lookahead that mirrors the
  grammatical position the keyword is legal in, so `data class Point` reads as
  a modifier while `def f(data: bytes)` reads as a parameter

`scripts/check_by_lexer.py` runs this over every `by` block in the docs and
checks both tiers, so a new keyword that isn't wired up here gets caught
"""

from __future__ import annotations

from pygments.lexer import bygroups, inherit, words
from pygments.lexers.python import PythonLexer
from pygments.token import Comment, Keyword, Name, Operator, Whitespace

__all__ = ["BasedPythonLexer"]

#: keywords that shadow nothing in python and read as keywords wherever they
#: appear
RESERVED = (
    "asserts",
    "export",
    "extension",
    "implementation",
    "let",
    "raises",
    "reified",
    "typeof",
)

#: modifiers that may be written, in any order and any number, ahead of the
#: declaration they modify
MODIFIERS = (
    "abstract",
    "data",
    "enum",
    "final",
    "frozen",
    "late",
    "open",
    "override",
    "private",
    "public",
    "sealed",
    "static",
)

#: what a modifier chain is allowed to end in — a further modifier, or the
#: keyword that actually introduces the declaration
INTRODUCERS = (
    "async",
    "class",
    "def",
    "extension",
    "implementation",
    "let",
    "type",
    "var",
)

#: modifiers written ahead of a parameter, binding its lifetime or promoting it
#: to an attribute
PARAM_MODIFIERS = ("local", "once", "var", "let")

#: modifiers written ahead of a type in a type expression
TYPE_MODIFIERS = ("final", "literal")

#: accessor blocks inside a property construct. these share their spelling with
#: very common method names (`d.get(k)`), so they only count at the head of a
#: line, where a `def` would otherwise go
ACCESSORS = ("field", "get", "set")


def _any(candidates: tuple[str, ...]) -> str:
    return "|".join(candidates)


class BasedPythonLexer(PythonLexer):
    """basedpython — python's lexer plus basedpython's surface syntax"""

    name = "basedpython"
    url = "https://kotlinisland.github.io/basedpython/"
    aliases = ["by", "basedpython"]
    filenames = ["*.by", "*.byi"]
    mimetypes = ["text/x-basedpython"]

    tokens = {
        "keywords": [
            (words(RESERVED, prefix=r"\b", suffix=r"\b"), Keyword),
            # `x cast int`, `x cast! int`, `x cast? int` — infix, so never
            # followed by a call. `cast(...)` stays the ordinary `typing.cast`
            (r"\bcast[!?](?!\w)|\bcast\b(?!\s*\()", Keyword),
            # a modifier only binds when something modifiable follows it. the
            # lookahead accepts another modifier, which is what lets a chain
            # like `frozen data class` resolve one word at a time
            (
                rf"\b(?:{_any(MODIFIERS)})\b(?=\s+(?:{_any(MODIFIERS + INTRODUCERS)})\b)",
                Keyword,
            ),
            # `x: literal int`, `xs: final list[int]`
            (rf"(?<![\w.])(?:{_any(TYPE_MODIFIERS)})(?=\s+[A-Za-z_])", Keyword),
            # `def f(local xs: list[int])`, `init(var name: str)`
            (rf"\b(?:{_any(PARAM_MODIFIERS)})\b(?=\s+[A-Za-z_]\w*\s*[:,)=])", Keyword),
            # `class Mapping[out Key, out Value]`
            (r"(?<![\w.])out(?=\s+[A-Za-z_])", Keyword),
            # `sentinel MISSING`
            (r"\bsentinel(?=\s+[A-Za-z_])", Keyword),
            # `class C[T: constraints (int, str)]`
            (r"\bconstraints(?=\s*\()", Keyword),
            # `protocol(a: int; def f(self) -> int)`
            (r"\bprotocol(?=\s*\()", Keyword),
            inherit,
        ],
        "builtins": [
            # `dynamic` is basedpython's spelling of `Any`
            (r"(?<![\w.])dynamic\b", Keyword.Type),
            inherit,
        ],
        "expr": [
            # the docs write an inlay hint — what the editor renders, not what
            # the file contains — between angle brackets
            (r"⟨[^⟩\n]*⟩", Comment.Special),
            # `a?.b` optional chaining, `a ?? b` none-coalesce, `T?` optional
            (r"\?\.|\?\?|\?", Operator),
            # `a === b` / `a !== b` identity
            (r"===|!==", Operator),
            # `expr!` force-unwrap. `!=` stays python's inequality
            (r"!(?!=)", Operator),
            inherit,
        ],
        "root": [
            # `build:` opens the block of values the build settles. only at the
            # margin, and only where the colon opens a block: `build: int` is an
            # ordinary annotated assignment and `build.GIT_SHA` an ordinary read
            (r"^build(?=:[ \t]*(?:#.*)?$)", Keyword),
            # a property accessor stands where a `def` would, so it only counts
            # at the head of a line. `d.get(k)` and `def get(self)` keep their
            # name token
            (
                rf"^([ \t]*)({_any(ACCESSORS)})\b(?=\s*[(:=])",
                bygroups(Whitespace, Keyword),
            ),
            # `enum class Shape:` / `case Circle(radius: int)` — a variant
            # declaration reads like the class header it lowers to
            (
                r"^([ \t]*)(case)(\s+)([A-Za-z_]\w*)",
                bygroups(Whitespace, Keyword, Whitespace, Name.Class),
            ),
            inherit,
        ],
    }

    def analyse_text(self, text: str) -> float:
        # never win a fight with the python lexer over a `.py` file
        return 0.0

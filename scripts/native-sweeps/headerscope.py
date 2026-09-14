# /// script
# requires-python = ">=3.11"
# ///
"""which names two builds of the runtime header disagree about, for `cdiff.sh`

    headerscope.py A_HEADER B_HEADER

prints one line: `same` where nothing a module can reach differs, `unscopable` where a
difference cannot be pinned to names, and otherwise the names a module's C has to mention
to be affected, joined by `|` for `grep -owE`. why a difference is unscopable goes to
stderr

the header is included rather than inlined, so a module whose emitted C is byte-identical
under both compilers still moves wherever it reaches a helper whose definition moved. this
answer decides which of those modules the behavioural rungs walk, and a name missing from
it hides a module from every one of them. so everything errs one way: a difference this
cannot attribute exactly is `unscopable`, and a name that might be reached is in the answer

the header is split into its top-level definitions, each keyed by the name it defines, and
the two sides are compared definition by definition:

- a function, a variable, a top-level macro invocation, or a macro or a type the other side
  does not have at all names what it defines
- so does every definition that mentions a name already in the answer, until nothing new
  is added: a module calling an unchanged helper that calls a changed one is affected
  without spelling the changed one's name
- a macro that pastes tokens can produce a name nobody spells, so one that can paste a name
  in the answer is in the answer itself, and everything that expands it with it
- a macro or a type that changed or went away, a directive that is not a conditional, an
  `_Static_assert`, a definition that runs at load, and anything this cannot parse are
  unscopable: a macro is expanded in places its name is the only trace of, and a type's
  layout is read by everything that holds one, neither of which a name search can follow
- a definition moved under a new `#if` differs from its old self, since the condition is
  part of what it is
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass, field

KEYWORDS = frozenset(
    [
        "auto",
        "break",
        "case",
        "char",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extern",
        "float",
        "for",
        "goto",
        "if",
        "inline",
        "int",
        "long",
        "register",
        "restrict",
        "return",
        "short",
        "signed",
        "sizeof",
        "static",
        "struct",
        "switch",
        "typedef",
        "union",
        "unsigned",
        "void",
        "volatile",
        "while",
        "_Bool",
        "_Complex",
        "_Imaginary",
        "_Alignas",
        "_Alignof",
        "_Atomic",
        "_Generic",
        "_Noreturn",
        "_Static_assert",
        "_Thread_local",
    ]
)
# spellings that take a parenthesised argument in front of a declaration without being
# what the declaration names
ATTRIBUTES = frozenset(
    {"__attribute__", "__declspec", "_Alignas", "__typeof__", "typeof"}
)

TOKEN = re.compile(
    r"""
    (?P<space>[ \t\r\f\v]+)
  | (?P<newline>\n)
  | (?P<comment>/\*.*?\*/|//[^\n]*)
  | (?P<string>[LuU8]*"(?:\\.|[^"\\\n])*")
  | (?P<char>[LuU]?'(?:\\.|[^'\\\n])*')
  | (?P<number>\.?[0-9](?:[eEpP][+-]|[A-Za-z0-9_.])*)
  | (?P<ident>[A-Za-z_][A-Za-z0-9_]*)
  | (?P<punct>\#\#|\.\.\.|<<=|>>=|->|\+\+|--|<<|>>|<=|>=|==|!=|&&|\|\||[-+*/%&|^]=|[^\s])
    """,
    re.VERBOSE | re.DOTALL,
)

OPENING = frozenset("{([")
CLOSING = frozenset("})]")


class Unscopable(Exception):
    """a difference, or a header, this cannot attribute to names"""


@dataclass
class Token:
    kind: str
    text: str
    # whether whitespace stood before it, which is all that survives of the layout: `- -b`
    # and `--b` are different programs, and so are `#define F (x)` and `#define F(x)`
    spaced: bool
    # whether it is the first token on its line, which is what makes a `#` a directive
    first: bool


def tokenize(source: str) -> list[Token]:
    # a backslash-newline is removed before anything else reads the text, as a compiler
    # removes it, so a macro spanning several lines is one line here
    source = source.replace("\\\n", "")
    tokens: list[Token] = []
    spaced = True
    first = True
    at = 0
    while at < len(source):
        match = TOKEN.match(source, at)
        kind = None if match is None else match.lastgroup
        if match is None or kind is None:
            raise Unscopable(f"cannot read the header at offset {at}")
        at = match.end()
        if kind == "newline":
            spaced = True
            first = True
        elif kind in ("space", "comment"):
            spaced = True
            first = first or "\n" in match.group()
        else:
            tokens.append(Token(kind, match.group(), spaced, first))
            spaced = False
            first = False
    return tokens


def spelled(tokens: list[Token]) -> str:
    """the tokens as text, with one space wherever the source had any"""
    out = []
    for index, token in enumerate(tokens):
        if index and token.spaced:
            out.append(" ")
        out.append(token.text)
    return "".join(out)


def identifiers(tokens: list[Token]) -> set[str]:
    return {token.text for token in tokens if token.kind == "ident"}


def closes_at(tokens: list[Token], opening: int) -> int:
    """the index of the bracket that closes the one at `opening`, or -1"""
    depth = 0
    for index in range(opening, len(tokens)):
        if tokens[index].text in OPENING:
            depth += 1
        elif tokens[index].text in CLOSING:
            depth -= 1
            if depth == 0:
                return index
    return -1


def top_level(tokens: list[Token]) -> list[tuple[int, Token]]:
    """the tokens outside every bracket, with their indices, the brackets included"""
    out = []
    depth = 0
    for index, token in enumerate(tokens):
        if token.text in CLOSING:
            depth -= 1
        if depth == 0:
            out.append((index, token))
        if token.text in OPENING:
            depth += 1
    return out


def split_arguments(tokens: list[Token]) -> list[list[Token]]:
    """`(a, f(b, c), d)` as its three arguments"""
    arguments: list[list[Token]] = [[]]
    depth = 0
    for token in tokens[1:-1]:
        if token.text in OPENING:
            depth += 1
        elif token.text in CLOSING:
            depth -= 1
        if depth == 0 and token.text == ",":
            arguments.append([])
        else:
            arguments[-1].append(token)
    return arguments


@dataclass
class Definition:
    kind: str
    # the key the two sides are compared under
    key: str
    # every name it lets a module reach
    names: set[str]
    # every identifier it spells
    mentions: set[str]
    # the conditional directives it stands inside, outermost first
    conditions: tuple[str, ...]
    text: str
    tokens: list[Token]

    def content(self) -> tuple[tuple[str, ...], str]:
        return (self.conditions, self.text)


@dataclass
class Macro:
    """a function-like macro: its parameters and the tokens it expands to"""

    parameters: list[str]
    body: list[Token]

    def pastes(self) -> list[re.Pattern[str]]:
        """every name a `##` in the body can make, as a pattern over the finished name"""
        patterns = []
        at = 0
        while at < len(self.body):
            if at + 1 < len(self.body) and self.body[at + 1].text == "##":
                pieces = [self.body[at]]
                while at + 2 < len(self.body) and self.body[at + 1].text == "##":
                    pieces.append(self.body[at + 2])
                    at += 2
                patterns.append(
                    re.compile(
                        "".join(
                            r"\w*"
                            if piece.text in self.parameters
                            else re.escape(piece.text)
                            for piece in pieces
                        )
                    )
                )
            at += 1
        return patterns

    def declares(self, arguments: list[list[Token]]) -> set[str]:
        """the names an invocation at the top level of the header declares

        a declaration generator declares whatever stands before each parameter list at the
        top level of its body: a parameter's argument, a name pasted out of one, or a name
        written into the body itself
        """
        if len(arguments) != len(self.parameters):
            raise Unscopable("a macro is invoked with a different number of arguments")
        given = dict(zip(self.parameters, arguments, strict=True))

        def single(name: str) -> str:
            argument = given[name]
            if len(argument) != 1 or argument[0].kind != "ident":
                raise Unscopable(
                    f"`{name}` is pasted from an argument that is not one name"
                )
            return argument[0].text

        names: set[str] = set()
        for index, token in top_level(self.body):
            if token.text in ("=", ";"):
                raise Unscopable(
                    "a top-level macro declares something that is not a function"
                )
            if token.text != "(" or index == 0:
                continue
            before = self.body[index - 1]
            if before.text in ATTRIBUTES or before.kind != "ident":
                continue
            # the whole run `a ## b ## c` that ends at the parameter list
            run = [before]
            back = index - 1
            while back >= 2 and self.body[back - 1].text == "##":
                run.insert(0, self.body[back - 2])
                back -= 2
            names.add(
                "".join(
                    single(piece.text) if piece.text in self.parameters else piece.text
                    for piece in run
                )
            )
        if not names:
            raise Unscopable(
                "a top-level macro invocation declares no name this can see"
            )
        return names


@dataclass
class Header:
    definitions: dict[str, list[Definition]] = field(default_factory=dict)
    macros: dict[str, list[Macro]] = field(default_factory=dict)
    # every definition in the order the header writes them
    order: list[Definition] = field(default_factory=list)

    def add(self, definition: Definition):
        self.definitions.setdefault(definition.key, []).append(definition)
        self.order.append(definition)

    def expansions(self) -> set[tuple[str, str, str]]:
        """which `#define` or `#undef` of each macro every definition spelling it sees

        a macro is expanded only below where it is defined, so a `#define` that moves
        changes what the definitions between its two places mean while comparing equal to
        itself. `(user, macro, directive)` for each, with the directive empty for a user
        that stands above every one of them
        """
        macros = {
            definition.key for definition in self.order if definition.kind == "macro"
        }
        seen: dict[str, str] = {}
        out = set()
        for definition in self.order:
            out.update(
                (definition.key, name, seen.get(name, ""))
                for name in definition.mentions & macros
            )
            if definition.kind == "macro":
                seen[definition.key] = definition.text
            elif (
                definition.kind == "directive"
                and len(definition.tokens) > 2
                and definition.tokens[1].text == "undef"
            ):
                seen[definition.tokens[2].text] = definition.text
        return out

    def every(self) -> list[Definition]:
        return [each for group in self.definitions.values() for each in group]


def parse(source: str) -> Header:
    tokens = tokenize(source)
    header = Header()
    conditions: list[str] = []
    chunk: list[Token] = []
    depth = 0
    at = 0
    while at < len(tokens):
        token = tokens[at]
        if token.text == "#" and token.first and depth == 0:
            if chunk:
                raise Unscopable(f"a directive stands inside `{spelled(chunk)[:60]}`")
            end = at + 1
            while end < len(tokens) and not tokens[end].first:
                end += 1
            directive(header, tokens[at:end], conditions)
            at = end
            continue
        chunk.append(token)
        at += 1
        if token.text in OPENING:
            depth += 1
        elif token.text in CLOSING:
            depth -= 1
            if depth < 0:
                raise Unscopable(f"`{token.text}` closes nothing")
        if depth != 0:
            continue
        following = tokens[at] if at < len(tokens) else None
        on_its_own = following is None or following.first
        if token.text == ";":
            ends = True
        elif token.text == "}":
            # a function's body ends its definition, while a type's or an initialiser's
            # goes on to a name or a `;` on the same line
            ends = on_its_own
        elif token.text == ")":
            # `BY_DEFINE_INT_CMP(By_IntEq, ==, Py_EQ)` on a line of its own, which no `;`
            # follows, rather than a signature whose body opens on the next line
            ends = (
                on_its_own
                and invoked(chunk)
                and (following is None or following.text != "{")
            )
        else:
            ends = False
        if ends:
            header.add(classify(chunk, tuple(conditions)))
            chunk = []
    if chunk or depth or conditions:
        raise Unscopable("the header ends inside a definition or a conditional")
    for definition in header.every():
        if definition.kind == "invocation":
            macros = header.macros.get(definition.tokens[0].text)
            if macros is None:
                raise Unscopable(
                    f"`{definition.text[:60]}` invokes a macro this header lacks"
                )
            arguments = split_arguments(definition.tokens[1:])
            for macro in macros:
                definition.names |= macro.declares(arguments)
    return header


def invoked(chunk: list[Token]) -> bool:
    """whether a chunk is `NAME(...)` and nothing else"""
    return (
        len(chunk) > 2
        and chunk[0].kind == "ident"
        and chunk[1].text == "("
        and closes_at(chunk, 1) == len(chunk) - 1
    )


def directive(header: Header, line: list[Token], conditions: list[str]):
    word = line[1].text if len(line) > 1 else ""
    if word in ("if", "ifdef", "ifndef"):
        conditions.append(spelled(line))
    elif word in ("elif", "else"):
        if not conditions:
            raise Unscopable(f"`{spelled(line)}` closes nothing")
        conditions[-1] = f"{conditions[-1]} {spelled(line)}"
    elif word == "endif":
        if not conditions:
            raise Unscopable("an `#endif` closes nothing")
        conditions.pop()
    elif word == "define" and len(line) > 2 and line[2].kind == "ident":
        name = line[2].text
        body = line[3:]
        # a parameter list is one only where nothing stands between it and the name
        if body and body[0].text == "(" and not body[0].spaced:
            close = closes_at(body, 0)
            if close < 0:
                raise Unscopable(f"`{name}` has a parameter list that never closes")
            parameters = [
                spelled(argument) for argument in split_arguments(body[: close + 1])
            ]
            header.macros.setdefault(name, []).append(
                Macro(parameters, body[close + 1 :])
            )
        header.add(
            Definition(
                "macro",
                name,
                {name},
                identifiers(body),
                tuple(conditions),
                spelled(line),
                line,
            )
        )
    else:
        text = spelled(line)
        header.add(
            Definition(
                "directive",
                f"directive {text}",
                set(),
                identifiers(line),
                tuple(conditions),
                text,
                line,
            )
        )


def classify(chunk: list[Token], conditions: tuple[str, ...]) -> Definition:
    text = spelled(chunk)
    mentions = identifiers(chunk)

    def opaque(kind: str) -> Definition:
        return Definition(
            kind, f"{kind} {text}", set(), mentions, conditions, text, chunk
        )

    words = [token.text for token in chunk]
    if words[0] == "_Static_assert":
        return opaque("assert")
    if invoked(chunk):
        # what it declares is worked out once every macro in the header is known
        return Definition("invocation", text, set(), mentions, conditions, text, chunk)
    outside = top_level(chunk)
    # a type is defined by a `typedef`, or by a tagged body with no declarator after it —
    # `static struct PyModuleDef by_module = {...}` is a variable of a type
    if words[0] == "typedef" or (
        words[0] in ("struct", "union", "enum")
        and not any(token.text == "=" for _, token in outside)
        and ([token.text for _, token in outside][-2:] == ["}", ";"] or len(words) == 3)
    ):
        names = type_names(chunk)
        if not names:
            return opaque("unknown")
        return Definition("type", min(names), names, mentions, conditions, text, chunk)
    stops = [
        index
        for index, token in outside
        if token.text in ("=", "(", "[", ";", "{", ",")
    ]
    if not stops:
        return opaque("unknown")
    stop = stops[0]
    # an attribute's parenthesised argument in front of a function's name is not its
    # parameter list
    while chunk[stop].text == "(" and stop > 0 and chunk[stop - 1].text in ATTRIBUTES:
        later = [index for index in stops if index > closes_at(chunk, stop)]
        if not later:
            return opaque("unknown")
        stop = later[0]
    name = chunk[stop - 1] if stop > 0 else None
    if name is None or name.kind != "ident" or name.text in KEYWORDS:
        return opaque("unknown")
    kind = "function" if chunk[stop].text == "(" else "variable"
    # several variables in one declaration is one more shape than this reads
    if kind == "variable" and any(token.text == "," for _, token in outside):
        return opaque("unknown")
    return Definition(kind, name.text, {name.text}, mentions, conditions, text, chunk)


def type_names(chunk: list[Token]) -> set[str]:
    """the tag, the typedef names and the enumerators a type definition introduces"""
    names: set[str] = set()
    for index, token in enumerate(chunk[:-1]):
        if (
            token.text in ("struct", "union", "enum")
            and chunk[index + 1].kind == "ident"
        ):
            names.add(chunk[index + 1].text)
        if token.text == "enum":
            body = next(
                (at for at in range(index, len(chunk)) if chunk[at].text == "{"), None
            )
            if body is not None:
                for item in split_arguments(chunk[body : closes_at(chunk, body) + 1]):
                    if item and item[0].kind == "ident":
                        names.add(item[0].text)
    if chunk[0].text == "typedef":
        # the names a typedef introduces stand at its top level before each `,` and the
        # closing `;`, or inside `(*name)` for a pointer to a function
        outside = [token for _, token in top_level(chunk)]
        for index, token in enumerate(outside):
            if (
                token.text in (",", ";")
                and index > 0
                and outside[index - 1].kind == "ident"
            ):
                names.add(outside[index - 1].text)
        for index in range(len(chunk) - 2):
            if (
                chunk[index].text == "("
                and chunk[index + 1].text == "*"
                and chunk[index + 2].kind == "ident"
            ):
                names.add(chunk[index + 2].text)
    return names - KEYWORDS


# the kinds whose difference is attributed to the names they define
NAMED = frozenset({"function", "variable", "invocation"})
# the kinds that are attributed only where one side does not have them at all
NAMED_WHERE_NEW = frozenset({"macro", "type"})


def scope(old: str, new: str) -> str:
    if old == new:
        return "same"
    try:
        return "|".join(sorted(affected(parse(old), parse(new)))) or "same"
    except Unscopable as reason:
        # the reason goes where a person reading the rung's log sees it, and the answer
        # stays one word for the script reading it
        print(f"headerscope: unscopable, {reason}", file=sys.stderr)
        return "unscopable"


def affected(before: Header, after: Header) -> set[str]:
    names: set[str] = set()
    unchanged: set[str] = set()
    for key in before.definitions.keys() | after.definitions.keys():
        was = before.definitions.get(key, [])
        now = after.definitions.get(key, [])
        if sorted(each.content() for each in was) == sorted(
            each.content() for each in now
        ):
            unchanged.add(key)
            continue
        for definition in was + now:
            if definition.kind in NAMED or (
                definition.kind in NAMED_WHERE_NEW and not was
            ):
                names |= definition.names
            else:
                raise Unscopable(f"a {definition.kind} differs: {definition.text[:80]}")
    # a definition that sees a different `#define` of a macro it spells, because the
    # `#define` moved, means something else without its own text changing
    moved = {
        (user, macro)
        for user, macro, _ in before.expansions() ^ after.expansions()
        if user in unchanged and macro in unchanged
    }
    if moved:
        user, macro = min(moved)
        raise Unscopable(
            f"`{user}` sees a different definition of `{macro}` than it did"
        )
    everything = before.every() + after.every()
    pastes = [
        (name, pattern)
        for header in (before, after)
        for name, macros in header.macros.items()
        for macro in macros
        for pattern in macro.pastes()
    ]
    grew = bool(names)
    while grew:
        grew = False
        # a macro that can paste a name in the answer can stand for it wherever it is
        # expanded, spelled or not
        for macro, pattern in pastes:
            if macro not in names and any(pattern.fullmatch(name) for name in names):
                names.add(macro)
                grew = True
        for definition in everything:
            if not definition.mentions & names:
                continue
            if definition.kind not in NAMED | NAMED_WHERE_NEW:
                raise Unscopable(
                    f"a {definition.kind} reaches a name that moved: {definition.text[:80]}"
                )
            if definition.names <= names:
                continue
            names |= definition.names
            grew = True
    # a definition that runs at load runs in every module, whatever the module mentions
    for definition in everything:
        if definition.names & names and runs_at_load(definition.tokens):
            raise Unscopable(f"`{min(definition.names)}` runs when the module loads")
    return names


def runs_at_load(tokens: list[Token]) -> bool:
    """whether an attribute marks a definition a constructor or a destructor"""
    for index, token in enumerate(tokens[:-1]):
        if token.text == "__attribute__" and tokens[index + 1].text == "(":
            group = tokens[index + 1 : closes_at(tokens, index + 1) + 1]
            if identifiers(group) & {"constructor", "destructor"}:
                return True
    return False


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: headerscope.py A_HEADER B_HEADER", file=sys.stderr)
        return 2
    with (
        open(sys.argv[1], encoding="utf-8") as a,
        open(sys.argv[2], encoding="utf-8") as b,
    ):
        print(scope(a.read(), b.read()))
    return 0


if __name__ == "__main__":
    sys.exit(main())

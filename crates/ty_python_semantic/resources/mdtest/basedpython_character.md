# `Character` and the grapheme string surface

basedpython adds `Character`, a concrete subtype of `str` representing one extended grapheme cluster
(a user-perceived character). `Character` is defined in `ty_extensions` and is implicitly available
in type expressions.

Raw string operations — `s[i]`, `s[i:j]`, `for c in s`, `len(s)` — keep python's **code-point**
semantics; `Character`s come from the dedicated grapheme accessors (`first`, `last`, `character_at`,
`characters`) or from explicit construction (`Character(...)`).

## Raw indexing and iteration are code points

Integer indexing, slicing and iteration operate on code points and yield `str`, not `Character`:

```by
def f(s: str):
    reveal_type(s[0])  # revealed: str
    reveal_type(s[1:])  # revealed: str

    for c in s:
        reveal_type(c)  # revealed: str

    reveal_type([c for c in s])  # revealed: list[str]
    reveal_type(list(s))  # revealed: final list[str]
```

A `str` is therefore not assignable to `Character` — a general string element is a code point, not a
guaranteed single grapheme:

```by
def f(s: str):
    # error: [invalid-assignment] "Object of type `str` is not assignable to `Character`"
    x: Character = s[0]
```

Indexing a string literal keeps the precise literal, which *is* a single grapheme, so it is
assignable to `Character`:

```by
reveal_type("hello"[0])  # revealed: "h"
c: Character = "hello"[0]
```

## The grapheme accessors produce `Character`s

`first` / `last` / `character_at` / `characters` read the string in whole grapheme clusters:

```by
def f(s: str, i: int):
    reveal_type(s.first)  # revealed: Character | None
    reveal_type(s.last)  # revealed: Character | None
    reveal_type(s.character_at(i))  # revealed: Character
    reveal_type(s.characters)  # revealed: Sequence[Character]
    for c in s.characters:
        reveal_type(c)  # revealed: Character
```

## Constructing a `Character`

`Character` is a concrete class, so it can be constructed explicitly (this is how transpiled Python
materialises grapheme values at runtime):

```by
from ty_extensions import Character

def f(s: str):
    c = Character(s[0])
    reveal_type(c)  # revealed: final Character
    x: Character = c
```

## Assignability

`Character` is a `str`, and a string literal inhabits `Character` exactly when it is a single
grapheme cluster:

```by
def f(c: Character, s: str):
    ok: str = c

    a: Character = "a"
    # error: [invalid-assignment] "Object of type `"ab"` is not assignable to `Character`"
    b: Character = "ab"
    # error: [invalid-assignment] "Object of type `""` is not assignable to `Character`"
    empty: Character = ""
    # error: [invalid-assignment] "Object of type `str` is not assignable to `Character`"
    d: Character = s

def g(flag: bool):
    # a union of single-grapheme literals is assignable to `Character`
    c: Character = "a" if flag else "b"

def h(c: Character):
    # a `Character` is not necessarily a literal
    # error: [invalid-assignment]
    s: LiteralString = c
```

A grapheme cluster can span several code points — the US flag is two regional-indicator code points
but one user-perceived character, so it is a single `Character`:

```by
# the US flag: "\U0001F1FA\U0001F1F8", one grapheme cluster
flag: Character = "🇺🇸"

# two separate flags are two graphemes, not a Character
# error: [invalid-assignment]
two: Character = "🇺🇸🇬🇧"

# a base letter plus a combining accent is one grapheme
accented: Character = "é"
```

## What we know about a `Character`

A `Character` is one extended grapheme cluster, so its `character_count` (the grapheme count) is
always exactly 1 and it is never empty. Its `len()` — a code-point count — may be greater than 1,
and raw indexing / iteration (inherited from `str`) still yield code-point `str`:

```by
def f(c: Character):
    reveal_type(c.character_count)  # revealed: 1
    reveal_type(c.first)  # revealed: Character
    reveal_type(c.last)  # revealed: Character
    reveal_type(len(c))  # revealed: int
    reveal_type(c[0])  # revealed: str

def g(c: Character):
    reveal_type(c.upper())  # revealed: str
    reveal_type(c + c)  # revealed: str
```

## The grapheme string surface

The grapheme string surface is a builtin `extension str:` declared in the basedpython prelude, so it
is available on every `str` without an import. `character_count` is the number of grapheme clusters
(not code points, so it differs from `len`), and `first` and `last` read the string in whole
graphemes:

```by
def f(s: str):
    reveal_type(s.character_count)  # revealed: int
    reveal_type(s.first)  # revealed: Character | None
```

`reversed`, `drop_first`, `drop_last`, `prefix` and `suffix` are grapheme-safe transformations, and
`unicode_scalars` is the code-point (scalar) view:

```by
def f(s: str, n: int):
    reveal_type(s.reversed)  # revealed: str
    reveal_type(s.drop_first())  # revealed: str
    reveal_type(s.drop_last())  # revealed: str
    reveal_type(s.prefix(n))  # revealed: str
    reveal_type(s.suffix(n))  # revealed: str
    reveal_type(s.unicode_scalars)  # revealed: Iterator[str]
```

Because the surface is an extension, it never shadows a real `str` member: python's
occurrence-counting `count(sub)` method keeps its standard meaning:

```by
def f(s: str):
    reveal_type(s.count("a"))  # revealed: int
```

The extension only applies to strings — other sequences keep their python API:

```by
def f(xs: list[int]):
    reveal_type(xs.count(1))  # revealed: int
```

## Subtyping and disjointness

A single-grapheme literal inhabits `Character`, while the empty string and multi-grapheme literals
are disjoint from it:

```by
from typing import Literal

from typing_extensions import LiteralString

from ty_extensions import Character, static_assert
from ty_extensions._internal import is_disjoint_from, is_subtype_of

static_assert(is_subtype_of(Literal["a"], Character))
static_assert(is_subtype_of(Literal["🇺🇸"], Character))
static_assert(is_subtype_of(Character, str))
static_assert(not is_subtype_of(str, Character))
static_assert(is_disjoint_from(Literal["ab"], Character))
static_assert(is_disjoint_from(Literal[""], Character))
static_assert(not is_disjoint_from(LiteralString, Character))
```

## Iterating a `Character` is suspicious

Iterating over a `Character` yields its code points — for a multi-scalar grapheme that is *not* the
single character the reader sees — so it almost always indicates a logic error and is reported (as a
warning by default):

```by
def g(c: Character):
    for x in c:  # error: [iteration-over-character]
        ...

    [x for x in c]  # error: [iteration-over-character]
    # error: [iteration-over-character]
    # error: [refutable-unpacking] "`Character` may not have exactly 1 element, which would raise `ValueError` when unpacked"
    a, = c
    print(*c)  # error: [iteration-over-character]

def h(s: str):
    # iterating a plain `str` is fine
    for c in s:
        ...
```

## `Character` in type expressions

### Composition

`Character` is implicitly available in basedpython type expressions and composes like any other
class:

```by
def f(cs: list[Character], c: Character | None):
    reveal_type(cs[0])  # revealed: Character
    reveal_type(c)  # revealed: Character | None
```

### Shadowing

A local binding shadows the implicit name:

```by
Character = int

def f(c: Character):
    reveal_type(c)  # revealed: int
```

### Explicit import

It can also be imported explicitly, which is how transpiled Python spells it:

```by
from ty_extensions import Character as ExplicitChar

def f(c: ExplicitChar):
    reveal_type(c)  # revealed: Character
```

### Value positions

In a value position, bare `Character` is an ordinary (undefined) identifier — import it from
`ty_extensions` to construct one:

```by
def f():
    print(Character)  # error: [unresolved-reference]
```

## Python files

In plain Python files the implicit type name is not available (it must be imported), and raw
indexing stays code-point `str`:

`mod.py`:

```py
from ty_extensions import Character

def f(s: str, c: Character):
    reveal_type(s[0])  # revealed: str
    reveal_type(c)  # revealed: Character
    reveal_type(c.first)  # revealed: Character
```

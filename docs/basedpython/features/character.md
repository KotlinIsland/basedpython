# `str`ings and `Character`s

in python, a string is a sequence of code points, but these don't represent user facing characters,
which can be descried as **extended grapheme clusters** — the
characters a reader perceives — not code points. basedpython extends the `str` api to suite:
`character_count` is the number of grapheme clusters, and `first` / `last` read the string in whole
graphemes

```by
def f(s: str):
    if s:
        print(s.first, s.last, s.character_count)
```

the distinction is not cosmetic. a grapheme cluster can be several code points
— the US flag is two, a zwj emoji like the facepalm is five — so `character_count`
and `len` disagree:

```by
flag = "🇺🇸"           # "\U0001F1FA\U0001F1F8"
flag.character_count  # 1  — one visual character
len(flag)             # 2  — two code points

facepalm = "🤦🏼‍♂️"       # face + skin tone + zwj + male sign + variation selector
facepalm.character_count  # 1  — one visual character
len(facepalm)         # 5  — five code points
```

none of the surface exists at python runtime — every access is a
**compile-time transformation** into a plain python expression:

| basedpython         | Python output                                      | type                  |
| ------------------- | -------------------------------------------------- | --------------------- |
| `s.character_count` | `len(_by_graphemes(s))`                            | `int`                 |
| `s.first`           | `(Character(_by_graphemes(s)[0]) if s else None)`  | `Character \| None`   |
| `s.last`            | `(Character(_by_graphemes(s)[-1]) if s else None)` | `Character \| None`   |
| `s.characters`      | `[Character(c) for c in _by_graphemes(s)]`         | `Sequence[Character]` |
| `s.character_at(i)` | `Character(_by_graphemes(s)[i])`                   | `Character`           |
| `s.reversed`        | `"".join(_by_graphemes(s)[::-1])`                  | `str`                 |
| `s.drop_first()`    | `"".join(_by_graphemes(s)[1:])`                    | `str`                 |
| `s.drop_last()`     | `"".join(_by_graphemes(s)[:-1])`                   | `str`                 |
| `s.prefix(n)`       | `_by_prefix(s, n)`                                 | `str`                 |
| `s.suffix(n)`       | `_by_suffix(s, n)`                                 | `str`                 |
| `s.unicode_scalars` | `iter(s)`                                          | `Iterator[str]`       |

the `Character`-producing accessors (`first` / `last` / `character_at` /
`characters`) construct **real `Character` instances** — `Character` is a
concrete `str` subclass, not a type-only alias, and the transpiler emits a
runtime `class Character(str)` for it, so `isinstance(x, Character)` works

`_by_graphemes` is a small injected helper that splits a string into extended
grapheme clusters via the [`regex`](https://pypi.org/project/regex/) module's
`\X`. `regex` is the only widely available python engine that implements unicode
UAX #29 correctly (zwj emoji, regional-indicator flags, …), so it is a **runtime
dependency of the grapheme surface**: a program that uses `character_count` /
`first` / `last` / `characters` / `character_at` must have `regex` installed
(`uv add regex`). if it is missing, the helper raises a clear `ImportError`
rather than silently miscounting — `len`-style code-point splitting would give
`5` for the facepalm above, not `1`

`regex` is Matthew Barnett's work, licensed `Apache-2.0 AND CNRI-Python` — see
[credits](../credits.md#regex)

the rewrites are type-directed: they fire only when the receiver is a string
(`str`, `Character`, `LiteralString`, a literal, or a `str` subclass).
`character_count` on a list, or on a user-defined attribute, passes through
untouched

## the `Character` type

`Character` is a single extended grapheme cluster — a concrete subtype of `str`
from `ty_extensions`. it comes from the grapheme accessors (`first`, `last`,
`character_at`, `characters`) or from explicit construction; a raw `s[0]` is a
*code point* (`str`), not a `Character`:

```by
from ty_extensions import Character

def first_grapheme(s: str) -> Character:
    return s.character_at(0)   # a Character
    # `return s[0]` would be an error — s[0] is a `str` (code point)
```

a `Character` is exactly one grapheme cluster, which may span several code
points. ty knows this about it (though raw `str` operations still behave as they
do on any string):

| operation                     | type         | why                             |
| ----------------------------- | ------------ | ------------------------------- |
| `c.character_count`           | `Literal[1]` | always exactly one grapheme     |
| `len(c)`                      | `int`        | a grapheme may be >1 code point |
| `c.first`, `c.last`           | `Character`  | the only grapheme is `c` itself |
| `c[i]`, `iter(c)`             | `str`        | inherited `str` code-point ops  |
| `c + c`, `c * n`, `c.upper()` | `str`        | results can be longer than 1    |

a string literal inhabits `Character` exactly when it is a single grapheme
cluster:

```by
a: Character = "a"    # ok
flag: Character = "🇺🇸"  # ok — one grapheme, two code points
b: Character = "ab"   # error: two graphemes
c: Character = ""     # error: empty
```

string literals keep their precise literal types — `"hello"[0]` is
`Literal["h"]`, a subtype of `Character`. indexing a `LiteralString` keeps
returning `LiteralString`

### an annotation materialises a `Character`

because `Character` is a concrete class, a `Character`-annotated assignment
constructs a real instance — the transpiler wraps the value in `Character(...)`
so the runtime class is `Character`, not a plain `str`:

```by
x: Character = "a"        # → x: Character = Character("a")
print(type(x))           # <class 'Character'>, not <class 'str'>
```

a bare `"a"` on its own is still an ordinary `str` — the coercion is driven by
the annotation. the wrap is applied
only when the annotation is exactly `Character` and the value is not already one:
`x: str = "a"`, `x: Character | None = "a"`, and `x: Character = s.character_at(0)`
(already a `Character`) are all left untouched, and a local class named
`Character` shadows the coercion

## `character_count`, not `count`

python already owns `str.count(sub)` as the occurrence-counting method. because
the grapheme surface is a builtin `extension str:` (see
[transpilation](#transpilation)), it can never shadow a real `str` member — so
basedpython keeps python's `count` untouched and spells the grapheme count
`character_count`:

```by
"mississippi".count("ss")     # 2  — python's occurrence-counting method
"mississippi".character_count  # 11 — grapheme count
```

`len(s)` is a third thing again — the code-point count — so the three never
conflate: `len("🇺🇸") == 2`, `"🇺🇸".character_count == 1`

## the grapheme character view

the grapheme-aware surface reads and slices a string in whole `Character`s:

| accessor                          | result                | notes                                                         |
| --------------------------------- | --------------------- | ------------------------------------------------------------- |
| `s.character_count`               | `int`                 | number of grapheme clusters                                   |
| `s.first` / `.last`               | `Character \| None`   | first / last grapheme, `None` when empty                      |
| `s.characters`                    | `Sequence[Character]` | the grapheme counterpart to `list(s)` — indexable and sized   |
| `s.character_at(i)`               | `Character`           | `i`-th grapheme (negative allowed; `IndexError` out of range) |
| `s.reversed`                      | `str`                 | grapheme-safe reverse (unlike `s[::-1]`)                      |
| `s.drop_first()` / `.drop_last()` | `str`                 | all but the first / last grapheme                             |
| `s.prefix(n)` / `.suffix(n)`      | `str`                 | first / last `n` graphemes (clamped, so `prefix(0) == ""`)    |

```by
def f(s: str):
    for ch in s.characters:          # each `ch` is a whole grapheme
        print(ch)

    reveal_type(s.character_at(0))    # revealed: Character
    reveal_type(s.reversed)           # revealed: str
    reveal_type(s.prefix(3))          # revealed: str
```

for example, with `s = "a🇺🇸é"` (three graphemes, four code points):

```by
s.character_count  # 3
s.characters       # ['a', '🇺🇸', 'é']
s.character_at(1)  # '🇺🇸'  — the flag stays whole
s.reversed         # 'é🇺🇸a' — `s[::-1]` would corrupt the flag
s.prefix(2)        # 'a🇺🇸'
s.suffix(1)        # 'é'
```

## the scalar view

`str` also has a **scalar view** — the unicode code points — reached with plain
python string operations. `len(s)`, `s[i]`, `s[i:j]`, `for c in s` and
`reversed(s)` all operate on code points, and `s.unicode_scalars` is an explicit
code-point iterator:

```by
def f(s: str):
    n = len(s)                    # code-point count (the scalar length)
    for u in s.unicode_scalars:   # iterate code points
        print(u)
```

this is a deliberate two-view model — a grapheme (character) view and a
code-point (scalar) view. the two views count differently for any
multi-code-point grapheme:

| string | `s.character_count` (graphemes) | `len(s)` (scalars) |
| ------ | ------------------------------- | ------------------ |
| `"a"`  | 1                               | 1                  |
| `"🇺🇸"` | 1                               | 2                  |
| `"🤦🏼‍♂️"` | 1                               | 5                  |

the two views are cleanly separated in the type system too: raw `s[i]` and
`for c in s` are typed `str` (they *are* code-point operations at runtime), so a
raw index never masquerades as a `Character`. that is why `x: Character = s[0]`
is an error — a code point is a `str`, not a guaranteed single grapheme — while
`x: Character = s.character_at(0)` is fine. reach for the character view when you
need whole graphemes; use the scalar view (`s[i]`, `s.unicode_scalars`) when you
want code points

> a fuller model where `str` is declared a `Sequence[Character]` — with `len`,
> indexing, slicing and iteration all grapheme-based — is **planned but not yet
> implemented**. it needs those raw operations rewritten to graphemes at runtime
> to be sound, which is a larger change (see the transpiler design notes); until
> then `str` remains a `Sequence[str]` and the grapheme surface lives in the
> accessors above

## iterating a `Character` is reported

iterating over a `Character` yields its *code points* — for a multi-scalar
grapheme that is not the single character the reader sees — so it almost always
indicates a logic error (for example, code that meant to iterate the enclosing
string, or to treat the `Character` as an opaque unit). ty reports the
`iteration-over-character` lint (warn by default) at every syntactic iteration
site — `for` loops, comprehensions, unpacking, splats, and `yield from`:

```by
def f(c: Character):
    for x in c:  # warning: iteration over a `Character`
        ...
```

## scope

`Character` resolves implicitly only in type-expression positions —
annotations, return types, type aliases, class bases, and the other positions
the shared type-position walker recognises. in a value position it is an
ordinary identifier, and a local `Character = …` binding shadows the implicit
name

`Character` is a real class at runtime: the transpiler emits `class Character(str)` whenever `Character` is imported or a grapheme accessor
constructs one, so `isinstance(x, Character)` works

there is exactly **one** `Character` class per process, so its identity survives
module boundaries — a `Character` built in one module is still a `Character` in
another. the emitted class is interned in a `sys.modules` registry to guarantee
that: `isinstance` tests class identity, so a plain per-module
`class Character(str)` would hand every module a *different* class and quietly
fail `isinstance(value_from_another_module, Character)`

`first` / `last` re-emit the receiver in both branches of the conditional; an
impure receiver (a call) is hoisted into a `:=` temp so it is evaluated once.
`first` and `last` are one-way sugar — the reverse transpiler leaves their
python lowerings as plain expressions

## transpilation

a `from ty_extensions import Character` — whether written by the user or injected
by an accessor — is turned into a concrete `class Character(str)` in the
preamble; the grapheme accessors lower to `Character(...)` constructions over
that class

the grapheme surface is a builtin `extension str:` — the `character_count` /
`first` / `last` / `characters` / `reversed` / `unicode_scalars` properties and
the `character_at` / `drop_first` / `drop_last` / `prefix` / `suffix` methods are
declared on `str` by the basedpython **prelude**, a vendored
`ty_extensions/_prelude.byi` stub every basedpython file sees without importing
it (folded in by `applicable_extensions`). because it is an extension it never
shadows a real `str` member, so python's `str.count`, `str.__getitem__` and
`str.__iter__` are left exactly as typeshed declares them, and the `str` base
stays `Sequence[str]`

these members are **type-only**: they have no backing function, so the
extension-call rewrite skips prelude members and the dedicated `grapheme_string`
lowering (the table above) emits the runtime python instead. the `Character`
class definition lives in `ty_extensions`

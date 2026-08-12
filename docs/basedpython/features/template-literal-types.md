# template literal types

an f-string in a type position is the set of strings its pattern produces:

```by
a: f"asdf{int}fdsa" = "asdf5fdsa"
```

the text between the holes is fixed, and each hole stands for `str(x)` over the
values of the hole's own type. `"asdf5fdsa"` is one of those strings;
`"asdfXfdsa"` is not, and is reported where it is written

## what a hole stands for

a hole reads its type the way `str()` would render it:

```by
path: f"/{str}" = "/home"
version: f"v{int}.{int}" = "v1.20"
flag: f"--{str}={bool}" = "--debug=True"
```

`int` is the decimal rendering python actually produces — a leading `-` for a
negative number, and never a leading zero, so `"a07b"` is not an `f"a{int}b"`.
`Character` is one extended grapheme cluster. a type the reading does not model
stands for any string at all, which never rejects anything

## a pattern that says something simpler is that simpler thing

a hole that renders to exactly one string folds into the text, so a pattern with
nothing left to stand for is a plain literal:

```by
a: f"a{1}b"          # "a1b"
b: f"{None}"         # "None"
```

a union hole distributes, one arm per member:

```by
a: f"a{1 | 2}b"      # "a1b" | "a2b"
b: f"{bool}"         # "True" | "False"
```

and a lone hole that is already a set of strings is that set:

```by
a: f"{str}"          # str
b: f"{Character}"    # Character
```

so the pattern form only survives where nothing else could have been written

## assignability

a string literal is assignable to a pattern it matches. between two patterns,
one is assignable to the other when every string it produces is one the other
produces:

```by
def f(x: f"a{int}b", y: f"a{str}b"):
    ok: f"a{str}b" = x   # every `a{int}b` is an `a{str}b`
    no: f"a{int}b" = y   # error
```

this is decided by aligning the two patterns piece by piece. a pair it cannot
align is reported as unrelated rather than guessed at, so a pattern that is a
subset of another by some argument the alignment does not make is not accepted

every pattern is a `str`, and carries the whole `str` api:

```by
def f(a: f"x{int}"):
    b: str = a
    a.upper()        # str
    len(a)           # int
```

a pattern whose fixed text is non-empty — or whose holes cannot render empty —
is always truthy, and a pattern built only out of holes that are themselves
literal strings is a `LiteralString`

## producing one

an f-string *value* is the pattern it spells, whether or not anything is
expecting one:

```by
def route(path: f"/{str}") -> f"{str}-ok":
    return f"{path}-ok"

def version(n: int) -> f"v{int}":
    return f"v{n}"

def f(name: str, n: int):
    a = f"hello {name}"   # f"hello {str}"
    b = f"asdf_{n}"       # f"asdf_{int}"
```

the text between the holes is known, and each hole is `str(x)` over whatever it
interpolates, so the pattern is simply what the expression builds

a pattern read off a value **widens where a string literal would**. the element
type of a mutable list is invariant, so a pattern in one is no more usable than
the `str` it produces, and it widens exactly as `["abc"]` widens to `list[str]`:

```by
xs = [f"a{n}"]        # list[str], so `xs.append("zzz")` is fine
zs: str = f"a{n}"     # fine
ws: f"a{int}" = f"a{n}"   # fine — the pattern is kept where it is asked for
```

a pattern *written* in a type expression is a declared type and never widens

a hole carrying a conversion or a format specifier is not `str(x)` any more, so
it is rejected in a type position and drops the value back to `str`:

```by
def f(a: f"{int!r}"): ...      # error
def g(n: int) -> f"v{int}":
    return f"v{n:>3}"          # error: the value is `str`
```

## narrowing

comparing against a string decides what it can:

```by
def f(a: f"x{int}"):
    if a == "x5":
        reveal_type(a)   # "x5"
    if a == "zzz":
        reveal_type(a)   # Never — no `x{int}` is `"zzz"`
```

## type parameters

a hole may be a type parameter, and the pattern re-folds when it is specialized:

```by
def render[T](x: T) -> f"a{T}b":
    raise NotImplementedError

reveal_type(render(1))     # "a1b"
reveal_type(render("q"))   # "aqb"
```

## transpiled output

python has no template literal types. a pattern that is still a pattern widens
to `str` — every string it produces is one — and a pattern that folded to a
finite set of strings keeps that precision:

```by
path: f"/{str}"
kind: f"a{1 | 2}b"
```

→

```python
path: str
kind: Literal["a1b", "a2b"]
```

the emitted annotation is read back from the checker, so it cannot disagree with
what basedpython decided the pattern meant. the lowering is one-way: a `str`
annotation carries no evidence that it was ever a pattern, so
[reverse transpilation](../development/reverse-transforms.md) leaves it alone

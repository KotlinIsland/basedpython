# `type def` type functions

a `type def` is a function from types to a type. it is applied with `[]` in a
type expression, and the application is evaluated by *executing its body* — real
python, which may do anything python can. so a type can be read off the world:

```by
type def Row[Schema]:
    import json
    import pathlib

    kinds = {"integer": int, "string": str, "number": float, "boolean": bool}
    fields = json.loads(pathlib.Path(Schema.literal).read_text())
    return tuple[tuple(kinds[v] for v in fields.values())]


def parse(line: str) -> Row["row.schema.json"]:
    raise NotImplementedError


def use():
    reveal_type(parse("1,a,2.0"))   # (int, str, float)
    ident, name, score = parse("1,a,2.0")
    reveal_type(score)              # float
```

given `row.schema.json` on disk:

```json
{ "id": "integer", "name": "string", "score": "number" }
```

the checker opens that file, parses it, and builds the row type out of what it
found. edit the json and the type changes. there is no static description of
this anywhere — the answer exists only because the body ran.

where a [match type](match-types.md) chooses between cases by pattern, a
`type def` computes.

## the body

the parameters are the type arguments and the return value is the resulting
type. inside the body a type is a value, so the operators python already has
mean what they say:

| written     | means                                                              |
| ----------- | ------------------------------------------------------------------ |
| `X <= int`  | `X` is a subtype of `int` — the mro decides, not the numeric tower |
| `X \| None` | the union type                                                     |
| `X.literal` | the literal's value when `X` is a literal type, else `None`        |

`<=` is subtyping, so `bool` passes and `float` does not:

```by
type def IsInt[X]:
    if X <= int:
        return int
    return str


def f(a: IsInt[int], b: IsInt[bool], c: IsInt[str], d: IsInt[float]):
    reveal_type(a)  # int
    reveal_type(b)  # int
    reveal_type(c)  # str
    reveal_type(d)  # str
```

a literal argument carries its value, so a type function can branch on it — or
compute with it, which is what the header example does:

```by
type def Big[X]:
    if X.literal is not None and X.literal > 3:
        return str
    return int


type def Camel[Name]:
    head, *rest = Name.literal.split("_")
    return head + "".join(word.capitalize() for word in rest)


def f(n: Camel["user_id"]):
    reveal_type(n)  # "userId"
```

nothing restricts the body. it may import, read the filesystem, consult the
environment, or reach the network — the footgun belongs to the author:

```by
type def Configured[Key]:
    import os

    return str if os.environ.get(Key.literal) else None


type def Fields[Spec]:
    import json

    kinds = {"integer": int, "string": str}
    return tuple[tuple(kinds[v] for v in json.loads(Spec.literal).values())]


def f(a: Configured["HOME"], b: Fields['{"id": "integer", "name": "string"}']):
    reveal_type(a)  # str        — because `HOME` is set where the checker ran
    reveal_type(b)  # (int, str)
```

which is worth saying plainly: a type function makes checking depend on the
machine it runs on. two developers whose environment differs get two different
answers, and so do CI and a laptop. keep the inputs to one in the repository —
a checked-in schema, a vendored file — rather than in the ambient environment,
unless you mean the type to vary.

a body that fails is reported at the application, with its traceback, rather
than crashing the checker or quietly giving `Unknown`:

```by
def g(row: Row["missing.json"]):  # error: `Row` could not be evaluated:
    ...                          # FileNotFoundError: … 'missing.json'
```

a relative path is resolved by python, so it is relative to the directory the
checker was invoked from — not to the file the annotation is written in.

## where an application may appear

anywhere a type may — a signature, a variable annotation, a class body, nested
inside another type:

```by
type def Opt[X]:
    return X | None


x: Opt[int]                       # int | None


class Holder:
    y: Opt[str]                   # str | None


def f(xs: list[Opt[int]]): ...


type Alias = Opt[int]
```

the name itself is not a value. a `type def` is erased when transpiling, so
naming one outside a type expression would emit python that raises `NameError`,
and it is rejected in either spelling:

```by
def f():
    print(F)        # error: it can only be applied in a type expression
    t = F[int]      # error: it can only be applied in a type expression
```

## a deferred application

an application whose arguments are all known is evaluated immediately, and
memoized. one that still mentions a type parameter cannot run — the argument is
not known yet — so it stays symbolic and behaves as the function's **declared
return type**, then re-runs once the parameter is substituted:

```by
type def F[X] -> int | str:
    if X <= int:
        return int
    return str


def generic[T](x: T) -> F[T]:
    raise NotImplementedError


def unreduced[T](x: F[T]):
    reveal_type(x)              # int | str — only the declared return is known


def specialized():
    reveal_type(generic(True))  # int
    reveal_type(generic(1.5))   # str
```

a class type parameter defers the same way a function's does:

```by
class Holder[T]:
    x: F[T]


def unreduced[T](h: Holder[T]):
    reveal_type(h.x)            # int | str


def specialized(h: Holder[bool]):
    reveal_type(h.x)            # int
```

this is why the declared return matters: without one there is nothing an
unreduced application can be, so generic code using the function is unusable.

```by
type def G[X]:
    return int


def unreduced[T](x: G[T]):
    reveal_type(x)  # Unknown
```

## execution

the body runs in a real python interpreter, so `python3` must be on `PATH`. only
a **first-party** type function is executed — one from a dependency is not run —
and setting `BY_NO_TYPE_FUNCTIONS` disables execution entirely.

the body is not itself type-checked yet. see
[the design note](../development/type-def-design.md) for what that leaves open.

## lowering

a type function has no runtime meaning: every application is resolved before
anything runs, and the declaration is erased.

```by
type def F[X]:
    return int


def f(a: F[bool]): ...
```

transpiles to:

```python
def f(a: int): ...
```

an application that could not be reduced lowers to the declared return type,
the same reduced form every [symbolic operation](symbolic-type-ops.md) falls
back to.

## see also

- [match types](match-types.md) — choosing a type by pattern instead of by code
- [symbolic operations in types](symbolic-type-ops.md) — arithmetic and
    comparisons kept symbolic until specialization
- [attribute types](attribute-types.md) — naming a member's type on a parameter

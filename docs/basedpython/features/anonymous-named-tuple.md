# anonymous named tuple types

basedpython supports an inline syntax for `typing.NamedTuple` types directly in
type-expression positions, so a structural record needs no separate class:

```by
def swap(p: (name: str, age: int)) -> (name: str, age: int):
    return (p.name.upper(), p.age + 1)

def main():
    let point = (name="ada", age=36)
    print(point.name, point.age)
```

the surface syntax avoids the boilerplate of declaring a separate class while
still producing a real `NamedTuple` subclass at runtime, so field-name access and
the standard `NamedTuple` API (`._asdict()`, `._replace()`) continue to work

## syntax

two surface forms — type and value:

```by
# type form: in annotation positions
def foo(x: (name: str, age: int)) -> (name: str, age: int):
    return ("asdf", 1)

a = (name: str, age: int)  # type alias

# value form: construct a named tuple inline
b = (name="asdf", age=20)
```

each type-form field is `name: type`; each value-form field is `name=expr`.
fields are comma-separated; a trailing comma is allowed. the syntax is
recognized inside `(` `)` whenever any element uses `name :` or `name =` —
which means a tuple that starts with a positional field can still become an
anonymous named tuple if a later field is named:

```by
a = (1, name="a")             # value form: positional first, named second
b: (int, name: str) = (1, "a")  # type form: positional first, named second
```

positional fields are valid in both forms. they get auto-generated field
names `arg0`, `arg1`, … in the synthesized `NamedTuple` class so that
positional access (`a[0]`) works alongside named access (`a.name`).
`NamedTuple` disallows leading-underscore field names so we use the
unprefixed `arg<i>` convention. if a user-named field collides with one of
those synthetic names (e.g. `(1, arg0=2)`), transpilation **fails** with a
clear error rather than emitting a malformed `NamedTuple` class — rename the
colliding field to resolve. the same hard-error behavior applies to any
duplicate named field (`(name=1, name=2)`)

within a single tuple, all named fields must use the same separator; you
cannot mix `:` and `=` in the same anonymous named tuple

## plain-tuple coercion

basedpython auto-wraps plain tuple literals as constructor calls when the
surrounding annotation says they should be anonymous named tuples. coercion
fires in three positions:

**return statements** inside a function whose return annotation is an
anonymous named tuple:

```by
def f() -> (age: int, name: str):
    return (1, "a")  # transpiled to: return _AnonNamedTuple_xxx(1, "a")

f().name  # works at runtime
```

**annotated assignments** whose annotation is an anonymous named tuple:

```by
a: (name: str, age: int) = ("asdf", 1)
# transpiled to: a: _AnonNamedTuple_xxx = _AnonNamedTuple_xxx("asdf", 1)
```

**list/set literals** whose annotation is `list[anon-NT]`, `set[anon-NT]`,
or `frozenset[anon-NT]` — every plain tuple element is wrapped:

```by
a: list[(age: int, name: str)] = [(1, "a"), (2, "b")]
# transpiled to:
# a: list[_AnonNamedTuple_xxx] = [_AnonNamedTuple_xxx(1, "a"), _AnonNamedTuple_xxx(2, "b")]
```

if a tuple literal's arity doesn't match the annotation it's left alone, so
ty diagnoses the mismatch rather than the transpiler silently constructing
the wrong shape

## semantics

an anonymous named tuple is a sugar for a `typing.NamedTuple` subclass:

```python
class _Anon(NamedTuple):
    name: str
    age: int
```

the type checker treats `(name: str, age: int)` as the equivalent
heterogeneous `tuple[str, int]` for assignability and inference. plain tuple
literals can therefore be assigned to or returned from positions annotated
with an anonymous named tuple — at runtime they remain plain tuples, so
field-name access (`x.name`) requires constructing through the synthesized
class explicitly

## structural identity

two anonymous named tuples with the same field names *and* the same field
types in the same order resolve to the same synthesized class. distinct shapes
get distinct classes:

```by
a: (name: str, age: int)
b: (name: str, age: int)  # same shape — same synthesized class as `a`
c: (label: int, count: str)  # different shape — different class
```

field-name comparison is exact: `(name: str, age: int)` and
`(label: str, age: int)` are different shapes, even though they have the same
ordered list of types

## limitations

- field defaults aren't yet supported in the surface syntax. if you need
    defaults, declare a `NamedTuple` class explicitly
- auto-coercion of plain tuple literals only fires on `return`,
    annotated assignment, and `list/set/frozenset[anon-NT]` literal sites.
    nested positions like `dict[K, anon-NT]` values or call arguments need
    the explicit value form `(name=…)` instead

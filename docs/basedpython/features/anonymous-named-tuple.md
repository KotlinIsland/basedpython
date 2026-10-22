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

## plain tuples where one is expected

a plain tuple literal written where an anonymous named tuple is expected *is* one: it is
constructed as that class, so its fields are there at runtime

```by
def take(p: (name: str, age: int)) -> str:
    return p.name

take(("ada", 36))
```

transpiles to:

```python
def take(p: _AnonNamedTuple_xxx) -> str:
    return p.name

take(_AnonNamedTuple_xxx("ada", 36))
```

the expectation can come from anywhere the type checker gives a literal its expected type, so
every one of these is constructed:

```by
type P = (name: str, age: int)

def make() -> P:
    return ("ada", 36)                    # a return, through an alias or not

def greet(p: P = ("bob", 1)): ...         # a default
def each(*ps: P): ...

greet(p=("cy", 2))                        # a keyword argument
each(("dee", 3), ("eve", 4))              # each element of `*args`

people: list[P] = [("fay", 5)]            # an element of a collection literal
people.append(("gus", 6))                 # an argument of a method call
by_name: dict[str, list[P]] = {"h": [("hal", 7)]}  # at any depth

class Team:
    var lead: P = ("ivy", 8)              # an attribute's default

team = Team()
team.lead = ("jo", 9)                     # a write to an attribute, or to a property
                                          # whose setter takes a `P`
```

a `yield` in a generator typed `Iterator[P]` is constructed the same way, and a field that is itself
an anonymous named tuple constructs its own literal: `((1, 2), "a")` passed as
`(point: (x: int, y: int), label: str)` builds both classes

only a literal is constructed. a value that already has a type is checked as it is: a variable
holding a plain tuple is not an anonymous named tuple, and a named tuple passed where a plain
`tuple` is expected stays the named tuple it is. a literal whose length doesn't match the expected
shape is left alone, so the type checker reports the mismatch

### several expected shapes

when the expected type is a union, the literal is constructed as the one anonymous named tuple that
accepts it: one with as many fields as the literal has elements, whose field types accept them

```by
a: (age: int) | None = (36,)              # constructed as `(age: int)`
b: (age: int) | (name: str) = ("ada",)    # constructed as `(name: str)`
```

a literal that more than one of them accepts is an error (`ambiguous-conversion`): the class decides
which field names the value has, and a plain tuple doesn't say which one was meant. name the fields
with the value form instead

```by
c: (age: int) | (year: int) = (36,)       # error: [ambiguous-conversion]
d: (age: int) | (year: int) = (age=36)    # fine
```

an anonymous named tuple whose fields don't accept the literal is passed over, and the literal is
checked against the rest of the expected type as a plain tuple:

```by
e: (a: list[int]) | tuple[list[object]] = ([1, "s"],)   # a plain tuple
```

an anonymous named tuple is a subtype of the plain tuple of its field types, so a union with that
tuple *is* that tuple, and a literal there stays a plain tuple:

```by
f: (age: int) | tuple[int] = (36,)        # the expected type is `tuple[int]`: a plain tuple
```

for the same reason, writing a literal to an attribute of a union whose members would build
different anonymous named tuples from it is an error (`invalid-assignment`): no single value is
both. members that would each build the same one — `P` and `P | None` alike — build it, and a
value with no plain tuple literal in it is checked against each member's own declaration as usual

### `.py` files

a `.py` file is python, where a plain tuple is never a named tuple. passing one where a `.by`
module expects an anonymous named tuple is an error there (`invalid-argument-type`). construct the
value through a name the `.by` module binds the type to:

```by
# shapes.by
Point = (x: int, y: int)

def norm(p: (x: int, y: int)) -> int:
    return p.x * p.x + p.y * p.y
```

```python
# main.py
from shapes import Point, norm

norm((3, 4))         # error: [invalid-argument-type]
norm(Point(3, 4))    # fine
```

## semantics

an anonymous named tuple is a sugar for a `typing.NamedTuple` subclass:

```python
class _Anon(NamedTuple):
    name: str
    age: int
```

it is a subclass of the heterogeneous `tuple[str, int]`, so it can be used anywhere that tuple
type is expected

the class is declared ahead of the module, where a named tuple's field annotations are read as the
class is made. a field naming something the module binds — a class or an import of the module —
is written as a string there, which `typing.get_type_hints` resolves once the module has run:

```by
class Point: ...

a: (origin: Point, label: str)
```

```python
class _Anon(NamedTuple):
    origin: "Point"
    label: str
```

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

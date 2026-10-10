# default argument re-evaluation

a long-standing python footgun is that mutable default values (`[]`, `{}`,
`set()`, etc.) are evaluated once at function-definition time and shared
across calls. basedpython removes the footgun: every non-scalar default is
re-evaluated per call, so each call gets a fresh value:

```by
def append_one(items=[]):
    items.append(1)
    return items
```

`append_one()` returns `[1]` every time, never the accumulating list.

scalar literals — numbers, bools, `None`, strings, `...` — stay as plain
python defaults (they're immutable, cheap, and carry no hidden state).
everything else is rewritten so the default expression runs at call time.

a useful side effect: `def g(a, b=a + 1)` becomes valid late-bound default
syntax, with `b` computed fresh from the actual `a` at each call

a default that names a value is read when the call runs, too, not where the
`def` stands:

```by
LIMIT = 2

def cap(n=LIMIT):
    return n

LIMIT = 3
cap()   # 3, where python's own default would still be 2
```

## what `inspect.signature` shows

a default re-evaluated per call has no value until a call evaluates it, so the
signature holds a marker in its place, and the marker's `repr` is the default as
basedpython reads it:

```by
import inspect

LIMIT = 2

def f(a: int, b: list[int] = [], c: int = LIMIT, d: int = 0):
    ...

print(inspect.signature(f))
# (a: int, b: list[int] = <default: []>, c: int = <default: LIMIT>, d: int = 0)
```

transpiles to:

```python
_MISSING: Any = _by_default("[]", __name__, "_MISSING")
_MISSING2: Any = _by_default("LIMIT", __name__, "_MISSING2")

def f(a: int, b: list[int] = _MISSING, c: int = _MISSING2, d: int = 0):
    if b is _MISSING:
        b = []
    if c is _MISSING2:
        c = LIMIT
    ...
```

- a scalar default is python's own, so the signature holds its value: `d` above
    is `0`
- a marker is not the value. `parameters["c"].default == 2` is `False`, and no
    value could be honest there: `LIMIT` is whatever it is when the call runs,
    `[]` is a new list each time, and a default reading an earlier parameter,
    such as `b=a + 1`, has none without an `a`
- the text is the default after the rewrites basedpython makes ahead of this one,
    on one line: `= Red` naming an enum member shows as `<default: Color.Red>`, a
    `??` stays `??`, the comments written inside a default are dropped, and a line
    break between two of its tokens becomes a space, or nothing just inside a
    bracket. a string literal keeps its own text, line breaks and all
- each default has a marker of its own. handed back as an argument, a marker is
    the argument not given: calling `f` with the arguments
    `BoundArguments.apply_defaults()` filled in evaluates each default as leaving
    the parameter out would, compiled with [`by compile`](../cli-reference.md#by-compile) or
    not. handed to any other parameter, it is a value like any other. a `.by` call
    passing one gets the [runtime check](soundness.md) any other value would, and
    a marker is not an `int`
- a copy of a marker is the marker, and a pickled one comes back as the marker it
    was, read from the name the module binds it under
- a required parameter after a defaulted one, which python cannot declare where
    it stands, shows as `<required>`. handed back, it raises the `TypeError`
    leaving it out does
- a parameter with no annotation and such a default records no type at runtime,
    so a [structural protocol test](parametric-type-tests.md#a-default-evaluated-per-call-records-no-type)
    against its method raises rather than answer

## a class body is not a default

the same reading does not extend to a class-body declaration, because a class
body is not a call — it runs once, when the class is made:

```by
class Fight:
    var last_contact: int = 0
    var seen: set[int] = set()
```

`last_contact` behaves per instance, because the only way to change a number is
to rebind it and `fight.last_contact = 5` binds on the instance. `seen` does not:
`fight.seen.add(1)` reaches through to the one set the class body built, and
every `Fight` ever made sees it. the two lines look the same and mean different
things

this is python's own behaviour and basedpython keeps it. the linter reports it —
[`RUF012`](linter.md), which reads a `let`/`var` declaration the same way it reads a plain
annotated assignment. to get a value per instance, declare the field and assign
it in `init`:

```by
class Fight:
    var seen: set[int]

    init():
        self.seen = set()
```

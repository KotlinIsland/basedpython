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
things, so the second is reported as [`shared-mutable-default`](../../rules.md).

the report is keyed on the value's own type rather than the shape it was written
in, so a computed number is left alone and an optional field initialised to
`None` is too — what is shared is the `None`. to get a value
per instance, declare the field and assign it in `init`:

```by
class Fight:
    var seen: set[int]

    init():
        self.seen = set()
```

to share one on purpose, say so with `class var` (or `class let` for a read-only
one), which is the [class-variable form](modifiers.md) and is not reported

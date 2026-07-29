# destructuring

a pattern can bind wherever a name can — the `let` statement, a `for` target, a
`with` item, a parameter:

```by
def area(Rect(w, h): Rect) -> int:
    return w * h

def report(shapes: list[Rect]):
    for Rect(w, h) in shapes:
        print(w * h)

def load(path: str) -> int:
    with open_config(path) as Config(size):
        return size

def first_word(text: str) -> str:
    let [word, *rest] := text.split() else:
        return ""
    return word
```

each one is a single `match` case in disguise: it binds the pattern's captures in
the enclosing scope and narrows exactly as that case would

## the `let` statement

```text
let_stmt ::= "let" patterns ":=" expression ["else" ":" block]
```

```by
let Point(x, y) := origin
print(x, y)
```

the captures outlive the statement, the way `match` captures and walrus bindings
do

## a `let` must match

a binder binds its captures unconditionally, so a pattern that does not match
would leave them unbound — a `NameError` at the first use. the checker rejects
one that may not match:

```by
def f(v: int | str):
    let int(n) := v  # error: this pattern may not match `int | str`
```

handle the failure with an `else` block:

```by
def f(v: int | str) -> int:
    let int(n) := v else:
        return 0
    return n
```

the block has to diverge — `return`, `raise`, `break`, `continue`, or a call that
never returns. control falling out of it would reach the same unbound captures,
so that is an error too

a pattern that matches every value of the subject's type needs no `else` at all:

```by
def f(p: Point):
    let Point(x, y) := p  # `Point(x, y)` matches every `Point`
```

a subject nobody typed cannot be shown to match or not to match, so gradual code
is left alone:

```by
def f(points):  # unannotated: the element type is `Unknown`
    let Point(x, y) := points[0]  # no error
```

## any pattern destructures

every `match` pattern is accepted, in every position:

```by
let (number, text) := pair
let [first, *rest] := items
let {"size": size} := config
let Circle(radius) := shape else:
    raise TypeError
let int(n) | float(n) := scalar else:
    raise TypeError
```

## `and` patterns

`P and Q` matches a value both `P` and `Q` match, and binds the captures of both.
it is the counterpart of `|`, and binds tighter than it — `A() and B() | C()` is
`(A() and B()) | C()`:

```by
match shape:
    case Circle(r) and Named(name):
        print(name, r)
    case _:
        pass
```

each conjunct sees what the ones before it narrowed, and a name may be bound by
only one of them — `Point(x) and Point(x)` binds `x` twice, which is an error
just as `Point(x, x)` is

an `and` cannot be written inside an alternative of a `|`: every alternative of a
`|` binds the same names, which the lowering of a conjunction cannot preserve

## binding positions

a `for` target, a `with` item and a parameter are only read as patterns when what
is written there cannot be assigned to. `for x in xs`, `for a, b in pairs` and
`with cm as handle` bind targets exactly as python does; `for Point(x, y) in ps`
has nothing it could assign to, so it destructures

a destructuring parameter needs an annotation — there is nothing else to say what
it destructures — and is positional-only, since the name it is bound to is
machinery no call site can write:

```by
def area(Rect(w, h): Rect) -> int:  # `area(rect)`, never `area(rect=…)`
    return w * h
```

the [`init(...)` shorthand](init-method.md) does not take one: every parameter it
declares becomes a field of the same name, and a pattern has no name to make one
of

## why `:=` and not `=`

`let NAME = value` is already the [immutable
declaration](modifiers.md) — `let x = 1` makes `x` `Final` — so `=` was not
available: `let x = 1` would otherwise be ambiguous between declaring a constant
and destructuring with the capture pattern `x`

it also keeps the `if let` clause honest. `=` is not an expression in python, on
purpose: `if a = b:` is a syntax error, and `:=` exists precisely to spell
"binds, here where a value is expected". writing `=` in a clause header would
bring back the shape python rejected

rust spells this `if let P = v`, so `=` is the first thing a reader coming from
there types — it is reported as such:

```by
let Point(x, y) = p  # error: a destructuring `let` binds with `:=`, not `=`
```

## `let` is not a keyword

`let` only introduces a destructuring when a whole pattern followed by `:=` comes
after it. `let = 5`, `let(x)` and the `let x: int = 1` declaration are all
untouched

## a `let` needs a line of its own

its lowering is a block, and a block cannot share a line — so a `let` that shares
one with a neighbour is reported rather than mislowered:

```by
if q: let int(n) := p            # error
print("before"); let int(n) := p  # error
let int(n) := p; print("after")   # error
```

every other binder is a compound statement, which python already refuses to write
after a `;` or a one-line block header

## lowering

a destructuring is a one-case `match`, whose captures bind in the enclosing scope
just as the source says:

```py
match origin:
    case Point(x, y):
        pass
print(x, y)
```

an `else` block cannot move into a `case _:` arm without being re-indented, so a
selector records whether the pattern matched and the block keeps its own source:

```py
_by_let_0 = 0
match v:
    case int(n):
        _by_let_0 = 1
if _by_let_0 == 0:
    return 0
del _by_let_0
```

a binding position binds the value to a binder and destructures it at the top of
the body:

```py
for _by_destructure_0 in points:
    match _by_destructure_0:
        case Point(x, y):
            pass
    print(x, y)
```

python has no `and` pattern, so a conjunction becomes a `match` per conjunct,
nested inside the previous one. a conjunction nested inside another pattern is
hoisted out — a binder captures its position and the conjunction is matched
against the binder afterwards, and every temporary the nest made is dropped once
it is done (left behind, one would become a member of the surrounding namespace —
a class attribute, or a bogus variant in an `enum class` body):

```py
_by_and_0 = None
match subject:
    case Point(x=_by_and_0, y=y):
        match _by_and_0:
            case int():
                match _by_and_0:
                    case 0:
                        ...
del _by_and_0
```

a binder is bound up front because a failed match may or may not have reached the
position that binds it — python does not say how far it gets before giving up —
and dropping it has to be safe either way

a nested `match` cannot fall through to the next case, so a `match` statement
that uses a conjunction flattens onto a selector the same way an
[`if let`](if-let.md) chain does — every case is tried only when no earlier one
matched

`match` is python 3.10 syntax, so destructuring needs a target of 3.10 or later

## open questions

- `while let` for loop-and-peel iteration
- a guard on a `let` (`let P := s if cond else: ...`), which `match` cases have
- comprehension and lambda binders, which still take targets only

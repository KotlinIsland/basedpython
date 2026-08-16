# conversions

three dunders let a type describe how a value becomes one of it, and the
transpiler inserts the call wherever the checker asked for that type:

```by
class Celsius:
    init(let degrees: float)

class Fahrenheit:
    init(let degrees: float)

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return cls(value.degrees * 9 / 5 + 32)

def report(temperature: Fahrenheit) -> None: ...

report(Celsius(100.0))          # `report(Fahrenheit.__from__(Celsius(100.0)))`
```

(the `let` is what makes each parameter an attribute — see
[the init shorthand](init-method.md#let-parameter-modifier).)

| dunder     | declared on | converts                          | lowers to       |
| ---------- | ----------- | --------------------------------- | --------------- |
| `__from__` | the target  | a value of the parameter type     | `T.__from__(x)` |
| `__into__` | the source  | itself, into its return type      | `x.__into__()`  |
| `__of__`   | the target  | a *literal* of the parameter type | `T.__of__(x)`   |

`__from__` and `__into__` are the two directions of the same relation, named
after rust's `From` and `Into`. `__of__` is the third case rust has no name for:
constructing a type from a written-out value, so that `1`, `None` and
`[1, 2, foo()]` can stand for something that is not an `int`, `None` or a `list`.

a fourth route needs no dunder at all: a callable reaching a site that asked for
one returning `None` is wrapped in an adapter that
[throws the result away](#discarding-a-return-value).

they are ordinary methods. nothing is registered and nothing is monkeypatched —
`Fahrenheit.__from__(c)` is a plain classmethod call, and the whole feature is
the checker agreeing to insert it for you.

## why not plain assignability

a conversion is a *call*, not a subtype relation. if `Celsius` were assignable to
`Fahrenheit`, then `list[Celsius]` would be a `list[Fahrenheit]`, and reading an
element back would hand out a value no one converted. so the relation stays out
of the type lattice entirely, and lives only at the positions where the
transpiler can materialize the call — the same rule
[conformances](extensions.md#conformance) are built on:

> a conversion site is any expression that the type checker checks against a
> declared type context

this is one rule over machinery the checker already has, and it is what makes the
inserted call always visible in the output.

## `__from__`

a classmethod on the target, taking the value to convert and returning `Self`:

```by
class UserId:
    init(raw: str)

    @classmethod
    def __from__(cls, value: int) -> Self:
        return cls(str(value))
```

it may be overloaded — the target dispatches on its argument, and the lowered
call is `UserId.__from__(x)` either way, so the runtime sees one function:

```by
class Path:
    @overload
    @classmethod
    def __from__(cls, value: str) -> Self: ...
    @overload
    @classmethod
    def __from__(cls, value: bytes) -> Self: ...
```

## `__into__`

an instance method on the source, whose return type is the target:

```by
class Celsius:
    init(degrees: float)

    def __into__(self) -> Kelvin:
        return Kelvin(self.degrees + 273.15)
```

it may **not** be overloaded, and it may not take parameters. the lowered call is
`x.__into__()`, which carries no target — a second overload would have nothing to
dispatch on at runtime. a source that converts into several targets declares
`__from__` on each of them instead, which is the direction that dispatches.

## `__of__`

a classmethod on the target, like `__from__`, but it only applies when the value
is written out as a literal at the conversion site:

```by
class Vec3:
    init(x: float, y: float, z: float)

    @classmethod
    def __of__(cls, value: tuple[float, float, float]) -> Self:
        return cls(*value)

v: Vec3 = (1.0, 2.0, 3.0)       # `Vec3.__of__((1.0, 2.0, 3.0))`
```

the parameter's annotation is what selects which literals it accepts — it is an
ordinary assignability check against the literal's inferred type, so anything the
type system can say is available:

```by
@classmethod
def __of__(cls, value: int) -> Self: ...           # `1`
@classmethod
def __of__(cls, value: None) -> Self: ...          # `None`
@classmethod
def __of__(cls, value: list[*]) -> Self: ...       # any list display
@classmethod
def __of__(cls, value: list[int | str]) -> Self: ...
```

a *literal* is an expression whose outermost form is written-out syntax:

- `None`, `True`, `False`, `...`
- a number, string, bytes or f-string literal
- a list, set, dict or tuple display

the elements need not be literals — `[1, 2, foo()]` is a list display, and that
is the whole point: the brackets are in the source, so wrapping them is honest.
a comprehension is not a literal (see [open questions](#open-questions)), and
neither is a name that happens to hold a literal:

```by
v: Vec3 = (1.0, 2.0, 3.0)       # converts
t = (1.0, 2.0, 3.0)
v2: Vec3 = t                    # error — `t` is not a literal
```

that restriction is what separates `__of__` from `__from__`. a type that wants
both spellings declares both.

## conversions from an extension

a conversion dunder is looked up the way any other member is, so an
[extension](extensions.md) can supply one for a type whose definition is out of
reach:

```by
extension Path:
    class def __of__(cls, value: str) -> Path:
        return Path(value)

p: Path = "/tmp/x"
```

the dunder is not a runtime attribute, so the site lowers to whatever that
extension lowers to — its backing function, receiving the class as its `cls`:

```python
p: Path = _by_ext__Path____of__(Path, "/tmp/x")
```

a type that declares the dunder itself wins, the same way an extension never
shadows a declared member. this is how the builtin frozen containers get theirs;
see [frozen container displays](frozen-displays.md).

## discarding a return value

a callable that returns something reaches a site that asked for one returning
`None`:

```by
def on_click(cb: () -> None): ...

def handler() -> int:
    return 1

on_click(handler)
```

→

```python
on_click(_by_discard(handler))
```

`_by_discard` calls what it wraps and throws the result away, so a callee that
declared `None` really is handed `None`. it forwards every argument, compares
equal to the callable it wraps and answers attributes off it, so a callback
registered through one can still be found again:

```by
observers: list[() -> None] = []

def subscribe(cb: () -> None):
    observers.append(cb)

def unsubscribe(cb: () -> None):
    observers.remove(cb)      # finds the callback `subscribe` added
```

this is the one route that needs no dunder — nothing has to be declared, and it
applies wherever a callable meets a callable type returning `None`.

the site has to promise `None` and nothing else. `-> object` already accepts
every callable and needs no adapter; any other return type still wants the
value:

```by
def wants_object(cb: () -> object): ...
def wants_str(cb: () -> str): ...

wants_object(handler)         # fine, and unwrapped — `object` takes the `int`
wants_str(handler)            # error: `int` is not assignable to `str`
```

only the return type is repaired. the adapter forwards its arguments unchanged,
so a callable that takes the wrong ones is as unassignable as ever:

```by
def needs_argument(a: int) -> int: ...

on_click(needs_argument)      # error
```

### why it is a conversion and not assignability

kotlin has this feature, and it is worth being precise about what it does. `f(::foo)`
passes an `Int`-returning function to a `() -> Unit` parameter, but the function
type itself is not a subtype:

```kotlin
val g: () -> Int = ::foo
f(g)                          // error: type mismatch
```

the same holds here: `() -> int` is not a `() -> None`. the adapter is a
different object from the callable it wraps, so a relation built on it could not
survive being carried inside a generic:

```by
handlers: list[() -> int] = [...]
callbacks: list[() -> None] = handlers   # error
```

there is nowhere inside the list to write an adapter, and an element read back
out would be a callable nobody wrapped. so the rule lives where the other
conversions live — at sites the transpiler can write the adapter into.

## conversion sites

exactly the positions a [conformance](extensions.md#conformance) repairs, for the
same reason — the transpiler has to be able to wrap the
expression where it stands:

```by
f(c)                            # argument to a parameter declared `Fahrenheit`
x: Fahrenheit = c               # annotated assignment
x = c                           # assignment to a name declared elsewhere
self.field = c                  # a declared attribute
def g() -> Fahrenheit: return c # return in a function declared `-> Fahrenheit`
xs: list[Fahrenheit] = [c1, c2] # each element of a literal with a declared target
d: dict[str, Fahrenheit] = {"k": c}
```

the collection cases convert **element-wise**: each element is wrapped where it
stands. this is where `__of__` earns most of its keep:

```by
class Meters:
    init(value: float)

    @classmethod
    def __of__(cls, value: int | float) -> Self:
        return cls(float(value))

lengths: list[Meters] = [1, 2, 3]
```

→

```python
lengths: list[Meters] = [Meters.__of__(1), Meters.__of__(2), Meters.__of__(3)]
```

the whole value is tried first, elements only if that fails — so a target with
its own `__of__` for the collection wins over per-element conversion, and the
choice never depends on ordering.

### what is not a conversion site

anything already inside a constructed generic:

```by
cs: list[Celsius] = [...]
report_all(cs)   # error: `list[Celsius]` is not assignable to `list[Fahrenheit]`
```

converting that would mean an O(n) copy with different identity hidden behind a
call. write it where it happens:

```by
report_all([Fahrenheit.__from__(c) for c in cs])
```

likewise not sites: `*args` / `**kwargs` splats, an unpacked element (`[c, *cs]`),
an unpacking target (`x, y = ...`, which binds an element rather than the value),
a name in `x = y = c` (one value reaching two places that need not agree about
it), an argument to an overloaded or union callee (no single parameter type), and
a typevar solved by inference.

a union works on both sides. a union *target* converts through whichever arm
offers the conversion, so `x: Fahrenheit? = c` is a site like any other. a union
*source* converts only when **every** arm declares `__into__`, since the lowered
`x.__into__()` runs against whichever arm the value actually is.

the repair is **single-step**: if `A` converts to `B` and `B` converts to `C`,
an `A` is not accepted where a `C` is asked for.

a conversion only ever *adds* an assignment that would otherwise fail. a value
that already fits is left alone, so no existing code changes meaning.

## ambiguity

a conversion site resolves to exactly one call. when more than one route applies
— two dunders, or a dunder and an in-scope
[conformance](extensions.md#conformance) — that is `ambiguous-conversion`, not a
precedence rule:

```by
class B:
    def __into__(self) -> A: ...

class A:
    @classmethod
    def __from__(cls, value: B) -> Self: ...

takes_a(B())    # error: `A.__from__` and `B.__into__` both convert here
```

rust does not have this problem because `Into` is derived from `From` rather than
written; here both are hand-written bodies that can disagree, so picking one
silently would mean the output depends on a rule nobody reads. delete one, or
convert explicitly.

two applicable *implementations* of the same pair are the same error, with a
message that names the interface and the type instead of the two dunders.

## lowering

each site wraps the expression in place, parenthesized:

| route      | source          | python                       |
| ---------- | --------------- | ---------------------------- |
| `__from__` | `f(c)`          | `f(Fahrenheit.__from__(c))`  |
| `__of__`   | `v: Vec3 = [1]` | `v: Vec3 = Vec3.__of__([1])` |
| `__into__` | `f(c)`          | `f((c).__into__())`          |

there is no polyfill and no runtime support: the dunders are methods the module
already defines, and the call is the conversion.

a conversion in the same module as its target spells it by name. one that
reaches across modules imports it under a mangled alias, always:

```python
from temperatures import Fahrenheit as _by_conv__Fahrenheit
```

the alias is not decoration. importing the bare name would rebind whatever this
file already means by it — and a same-named class of this file's own would then
shadow the import, sending the call to the wrong object at runtime. a name the
user never writes cannot do either.

for the same reason, a conversion whose target is shadowed by a binding between
it and the module is rejected rather than emitted:

```by
def use(c: Celsius) -> None:
    Fahrenheit = 3
    report(c)   # error: `Fahrenheit` is shadowed here
```

a conversion that runs at import time must appear after the class it converts
through, because `class` binds its name when the statement executes. inside a
function body the name resolves at call time, so order does not constrain it.
this is only reachable through an
[automatic forward reference](forward-references.md), and is reported rather than
emitted.

## round-tripping

reverse transpiling leaves conversion calls alone. `Fahrenheit.__from__(c)` and
`c.__into__()` are valid basedpython that mean exactly what they say, so nothing
is lost by not re-sugaring them — and unwrapping one would be a guess about
whether the site would re-insert it.

## diagnostics

| lint                   | default | fires on                                                                                                                                                                                                                                            |
| ---------------------- | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `invalid-conversion`   | error   | `__from__` / `__of__` that is not a `@classmethod`, does not take exactly one value besides `cls`, or does not return the declaring class; `__into__` that is overloaded, takes parameters besides `self`, or is a `@classmethod` / `@staticmethod` |
| `ambiguous-conversion` | error   | a conversion site where more than one route applies                                                                                                                                                                                                 |

an assignment that a *malformed* dunder would have repaired keeps its ordinary
error — the declaration is reported at its own site, and a conversion built from
a signature the checker could not read would be worse than the plain failure.

## rejected alternatives

**putting the relation in the lattice.** make `Celsius` assignable to
`Fahrenheit` outright. rejected for the reason at the top: it silently promises
`list[Celsius]` is a `list[Fahrenheit]`, and the element read is unconverted. the
conversion-site rule is the same one implementations already run on, so there is
one concept to learn rather than two.

**deriving `__into__` from `__from__`, as rust does.** rust can, because `Into`
is a blanket impl over `From` and dispatch is static. here `x.__into__()` would
have to find the target at runtime with nothing to find it from. so the two are
independent declarations, and declaring both for one pair is an error rather than
a silent preference.

**a single `__convert__` with the literal case folded in.** one dunder, with the
"is it a literal" question answered by the parameter annotation (`Literal[...]`).
rejected because a `Literal[1] | Literal[2] | ...` annotation cannot say "any
list display", and because the two cases want different overload sets: `__of__`
describes syntax you may write, `__from__` describes types you may hold.

**implicit conversion at every assignment, including plain `x = c` against an
earlier `x: Fahrenheit`.** rejected on the same ground the implementations
feature rejects it: the declared type lives in another statement, and the
transpiler cannot recover the same answer, so the checker would accept code the
lowering leaves unconverted.

**a `converts` clause on the class header** instead of dunders. it reads better,
but it needs parser work, a new AST field, formatter support and a round-trip
story, to express something three method names already express — and a method
body is where the conversion has to live either way.

## implementation plan

1. **ty: `types/conversions.rs`** — one `Route` enum over the four ways a value
    can be repaired (witness, `__from__`, `__of__`, `__into__`), with
    `repair_conversion` as the single entry point every site asks. the dunder
    routes resolve by `try_call_dunder`, so overloads, generics and
    descriptor binding all come from the ordinary call machinery
1. **ty: the literal gate** — a shared `is_literal_expression` used by the
    checker and the transpiler, so the two cannot disagree about what `__of__`
    accepts
1. **ty: validation** — `validate_conversion_dunders`, run from the
    post-inference static-class checks beside
    `validate_implementation_declaration`
1. **ty: rewire the four hooks** — argument (`call/bind.rs`), return
    (`infer/builder/function.rs`), attribute assignment, annotated assignment
    (`diagnostic.rs`) — from `repair_with_implementation` to `repair_conversion`
1. **transpiler bridge** — the existing `implementation_*_conversions` become
    `*_conversions` returning a prefix/suffix pair instead of a witness name
1. **transpiler: forward** — `ImplementationConversionPass` becomes
    `ConversionPass`, emitting the same single template edit per site
1. **tests** — `basedpython_conversions.md` for the checker (including the
    cross-module and ambiguity cases), transform unit tests, and a
    `conversion_runtime.rs` integration test proving the emitted python runs

## open questions

- **comprehensions as literals.** `[f(x) for x in xs]` has the brackets in the
    source too, and excluding it means `xs: Vec3 = [...]` works while the
    comprehension spelling does not. included would make `__of__` fire on
    something whose contents come from another collection, which is the line
    element-wise conversion is drawn on
- **conversions declared in `.py` modules.** the registry gate that keeps this
    off the hot path scans imported modules for the three names; whether a plain
    python class should be able to offer a conversion to basedpython callers is
    a policy question, not a technical one
- **`__from__` on a generic target** (`Wrapper[T].__from__(value: T)`), where the
    specialization has to be solved from the argument rather than read off the
    annotation
- whether a failed assignment should carry a subdiagnostic naming a dunder that
    *nearly* applied, the way implementations point at an out-of-reach witness

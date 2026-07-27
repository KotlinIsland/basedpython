# match types

a type alias can choose its value by pattern matching on its type arguments:

```by
type NDTuple[T, *Shape: int] = match *Shape:
    case ():
        T
    case (Dim, *Rest):
        (NDTuple[T, *Rest],) * Dim


class Array[T, *Shape]:
    init(data: NDTuple[T, *Shape])
```

`NDTuple[int, 2, 3]` is `((int, int, int), (int, int, int))` — a nested tuple
whose shape is the alias's own arguments. `Array[int, 2, 3](...)` accepts
exactly that and nothing else:

```by
Array[int, 2, 3](((1, 2, 3), (4, 5, 6)))    # ok
Array[int, 2, 3](((1, 2), (4, 5, 6)))       # error: a tuple of length 2 is not
                                            # assignable to a tuple of length 3
```

## syntax

```text
match_type ::= "type" NAME [type_params] "=" "match" subject ":" NEWLINE
               INDENT case_block+ DEDENT
case_block ::= "case" pattern ":" type_expression
```

the subject is a type expression. writing it as an unpack — `match *Shape:` —
matches over the *elements* of a [type variable tuple](generics.md) rather than
over the pack as a single type; that is the usual form.

each `case` body is a single type expression: the value the alias takes when
that pattern matches. cases are tried in order and the first match wins. a
`case` cannot have an `if` guard — a type-level match decides on the pattern
alone.

## patterns

| pattern                  | matches                                                             |
| ------------------------ | ------------------------------------------------------------------- |
| `()`, `(A, B)`, `[A, B]` | a tuple type of exactly that length                                 |
| `(A, *Rest)`             | a tuple type of at least that length; `Rest` captures the remainder |
| `Name`                   | anything, capturing it                                              |
| `_`                      | anything, capturing nothing                                         |
| `2`, `"s"`, `b"s"`, `-1` | that literal type exactly                                           |
| `None`, `True`, `False`  | those singleton types                                               |
| `A \| B`                 | either alternative                                                  |

a name in a pattern is a *capture*, never a value to compare against — the same
rule python's `match` statement uses. a capture stands for a type, and a starred
capture for a whole pack of types, so `case (Dim, *Rest)` introduces one type
variable `Dim` and one type variable tuple `Rest`.

class patterns (`case Foo(x)`) and mapping patterns (`case {"a": x}`) take apart
a *value*; there is nothing at the type level for them to destructure, so they
are reported as invalid type forms.

a capture is scoped to the case that binds it. referring to another case's
capture is an unresolved reference:

```by
type Bad[*Ts] = match *Ts:
    case (A,):
        A
    case (B, C):
        A       # error: unresolved reference `A`
```

within one pattern a name may be captured only once, and every alternative of an
or-pattern must bind the same names — the two rules python's own `match` enforces:

```by
type Dup[*Ts] = match *Ts:
    case (A, A):        # error: multiple assignments to name `A`
        A

type Uneven[*Ts] = match *Ts:
    case (A,) | (B, C):  # error: alternatives must all bind the same names
        (A, B, C)
```

## recursion

a match type may name itself. `NDTuple[T, *Rest]` inside the second case is what
peels one dimension off the shape, and the `()` case is what terminates it. the
recursion unfolds only as far as it is asked to, so a well-founded definition —
one whose recursive uses shrink the subject — always terminates.

a definition that does *not* shrink its subject would recurse forever. evaluation
gives up once the subject grows past its budget, and the application simply has no
value:

```by
type Grow[*Ts] = match *Ts:
    case ():
        int
    case (A, *R):
        Grow[A, *R, A]     # never shrinks


x: Grow[int, str]          # `Unknown`
```

the budget is measured on the subject, not on how deep the recursion has gone, so
whether an application reduces never depends on which use site asked first. a case
that names itself at an unchanging argument (`case X: Loop[X]`) is an ordinary
definition cycle and is caught as one.

## evaluation

an application is evaluated as soon as its arguments are known. one whose
arguments still mention a type parameter cannot pick a case — `*Shape` might be
empty or not — so it stays symbolic and is evaluated later, when the enclosing
generic is specialized. that is why `init(data: NDTuple[T, *Shape])` inside
`Array` is checkable at all: the parameter's type is decided per construction,
not once at the class definition.

a case is only ever chosen when it can be *decided*. a subject whose shape is not
pinned down — a variable-length pack, or a gradual type — leaves the whole match
unresolved rather than falling through to whatever later case happens to match
everything:

```by
type M[*Ts] = match *Ts:
    case ():
        int
    case _:
        str


x: M[*tuple[int, ...]]   # `Unknown`, not `str`
```

an unresolved application behaves as `Unknown`, so nothing about it is reported
until it can be decided. an application whose arguments *are* known but match no
case is an error:

```by
type OnlyPairs[*Ts] = match *Ts:
    case (A, B):
        (A, B)


x: OnlyPairs[int]   # error: No `case` of match type `OnlyPairs` matches these
                    # type arguments
```

## bounds on a type variable tuple

`*Shape: int` bounds every *element* of the pack, so a shape must be made of
`int`s:

```by
def f(x: NDTuple[int, 2, "three"]): ...
# error: Type `"three"` is not assignable to upper bound `int`
#        of type variable tuple `Shape@NDTuple`
```

CPython rejects a bound on a `TypeVarTuple` outright, so this is `.by`-only
syntax; it is stripped by lowering.

## polyfill

a match type has no runtime meaning — every application is resolved before
anything runs — but the alias is still *named* by annotations, which python
evaluates at runtime below 3.14. so the declaration is kept and its value
becomes `object`:

```by
type NDTuple[T, *Shape: int] = match *Shape:
    case ():
        T
    case (Dim, *Rest):
        (NDTuple[T, *Rest],) * Dim
```

transpiles to:

```python
type NDTuple[T, *Shape] = object
```

below python 3.12 the alias goes through the ordinary
[pep 695 polyfill](polyfills.md) on top of that.

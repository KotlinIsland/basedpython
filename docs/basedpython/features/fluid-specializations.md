# fluid specializations

a binding like `a = [1]` or `a = A(1)` creates a generic instance whose specialization was
inferred rather than declared. while the type checker can see every observer of the value,
later uses of the binding may refine ("widen") the inferred specialization instead of being
checked against it

```python
a = [1]        # list[int]
a[0]           # int — a covariant operation, doesn't lock anything
a.append("x")  # no error — the specialization widens
a[0]           # int | str — the promoted element type accumulates while the binding is fluid
b = a          # the value escapes: lock
a[0]           # int | str
b[0]           # int | str
```

the same applies to constructor calls of user-defined generic classes:

```python
class A[T]:
    def __init__(self, t: T): ...
    def x(self) -> T: ...
    def y(self, t: T): ...

def foo(a: A[object]): ...

a = A(1)       # A[int]
a.x()          # int
foo(a)         # an invariant observer — locks the specialization to A[object]
a.x()          # object
```

a literal type is only widened where the parameter it fills is written through — an invariant or
contravariant one. a covariant parameter (a bivariant one counts as covariant) keeps the literal
it was inferred from, since nothing can read a different type back out of it:

```python
class Covariant[T]:
    def __init__(self, v: T): ...
    def get(self) -> T: ...

c = Covariant(1)  # Covariant[Literal[1]]
i = [1]           # list[int] — list is invariant in its element
```

> **note (performance):** an invariant element type is promoted (`a = [1]` is `list[int]`, not
> `list[Literal[1]]`). retaining literals there is the intended behavior, but literal-parametrized
> generics are currently too expensive in the cross-module constraint solver (~40x, ecosystem
> timeouts), so precision is traded for performance until that cost is addressed.

## motivation

variance fundamentally works on a system of observers. an invariant specialization must be
fixed because any number of observers may rely on it to insert or extract values. but while a
freshly created value has exactly one observer — the binding it was assigned to — the checker
can prove that every observer agrees on the specialization, and is free to refine it from
usage. we can say that `a` above was always an `A[object]` from the start; before the
specialization becomes rigid, the narrower intermediate types can be taken advantage of

this generalizes the inference that type checkers already perform for empty collection
literals (`x = []` followed by `x.append(1)`) to non-empty literals and constructor calls, and
makes it flow-sensitive

## semantics

a binding is a *fluid candidate* when it is a single unannotated name assignment whose value
is a collection literal or a direct constructor call (not an explicitly specialized one like
`A[int](...)`), and the inferred value is a generic instance. each use of the binding is then
classified:

- *reads* — subscript loads, truthiness tests, iteration, read-only method calls — use the
    specialization solved from the events so far, and change nothing
- *widening uses* — method calls and subscript stores whose arguments don't fit the current
    specialization — are not errors. the specialization at later uses becomes the union of the
    previous types and the promoted argument types
    (`a.append("x")` on `list[int]` gives `list[int | str]`)
- *locking uses* — passing the value to a context whose declared type constrains the class
    typevars (a call argument, an annotated assignment, a return) — adopt the declared
    specialization, and the binding stops being fluid: later incompatible uses are errors again
- *escapes* — any use the checker can't analyze (aliasing to another name, storing in a
    container, an un-called bound method, ...) — promote the current specialization and lock it

contexts that are blind to the class typevars (e.g. `print(a)`, `len(a)`, a parameter typed
`object`) place no requirements on the specialization and don't lock the binding. neither do
parametric contexts: a generic function parameter like `list[T]` adapts to whatever the caller
provides, so it can never share a perspective on the specialization — only a concrete declared
type can lock one in

```python
def f1[T](a: list[T]): ...
def f2(a: list[int | str]): ...

a = [1]
f1(a)   # doesn't lock
a[0]    # int
f2(a)   # locks: the parameter is the concrete `list[int | str]`
a[0]    # int | str
```

however, a call whose return type mentions the typevars solved from the argument hands the
caller a new observer of the specialization — the result aliases the value — so it locks, to
exactly the view the observer received:

```python
def f[T](t: list[T]) -> list[T]:
    return t

def g[T](t: list[T]) -> int:
    return len(t)

a = [1]
xi = f(a)  # xi: list[int]; a locks to list[int] — xi is a surviving observer
g(a)       # T does not occur in the return type: no surviving observer, no lock
f(a)       # the result is discarded: the returned observer does not survive, no lock
```

a covariant observer is the exception: it only ever reads from the value, so its perspective
stays valid under any future widening. the binding stays fluid, and the observer's type is
solved against the binding's eventual specialization

```python
from typing import Sequence

def s[T](t: list[T]) -> Sequence[T]:
    return t

a = [1]
b = s(a)      # b: Sequence[int] — whatever a's final type turns out to be
a.append(2)   # still fluid
a.append(3)
c = a         # the lock: a and c are list[int], agreeing with b's view
```

an invariant element type is promoted at creation time (`a = [1]` is `list[int]`) and accumulates
through widening events as promoted types — literal precision is currently traded for performance
there (see the note above); a covariant parameter keeps its literals

the binding's public type — what nested scopes and post-lock uses see — is the solution at
the lock (or at the end of the scope), so the flow-sensitive narrowing is always a refinement
of the public type

## scope

this is a type-checking enhancement only: it changes no syntax and produces no transpiler
output, and it applies to plain python files as well as `.by` files

known limitations:

- uses in nested scopes (closures, comprehensions) are not tracked; the binding's public type
    is what such scopes observe
- a use whose binding is not unique (e.g. conditionally reassigned names) is not fluid
- inside a loop, a widening event is conservatively visible to every use in the loop,
    including uses that appear earlier in the source. a loop event may execute any number of
    times with different values, so its literal types are promoted wherever promotion applies at
    all (this also keeps self-feeding loops like `for n in nums: nums.add(n + 1)` convergent)

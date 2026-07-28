# exception tracking

python tells you nothing about what a call can raise. the information exists —
it is right there in the body — but it stops at the function boundary, so every
caller either guesses, over-catches, or finds out in production. docstrings
carry the contract, and nothing checks them

basedpython tracks it instead. every function has an **exception set**: the
exceptions that can escape a call to it. the set is inferred from the body and
propagates through calls, and a `raises` clause declares it explicitly

```by
def f():  # raises TypeError
    raise TypeError

def g():  # raises TypeError — propagated from f
    f()

def h():  # raises nothing — the call is handled
    try:
        g()
    except TypeError:
        pass

def main():
    g()  # error: `TypeError` can escape `main`, the entry point
```

nothing here changes what the program does. like
[`abstract` and `override`](modifiers.md), the clause is erased in the lowered
python, and everything it promises is checked before a line runs — with an
opt-in [runtime guard](#runtime-guards) in the same spirit as
[soundness checks](soundness.md)

## the `raises` clause

`raises` follows the return annotation and holds an **ordinary type
expression**, so there is no second algebra to learn — the type system already
has one:

```by
def parse(text: str) -> int raises ValueError:
    ...

def read(path: str) -> bytes raises OSError | ValueError:
    ...

def pure(x: int) -> int raises Never:
    ...

def plugin() raises ...:
    ...
```

| clause          | means                                             |
| --------------- | ------------------------------------------------- |
| `raises T`      | at most `T`                                       |
| `raises A \| B` | at most `A` or `B`                                |
| `raises Never`  | cannot raise                                      |
| `raises ...`    | may raise anything — opt out of tracking          |
| no clause       | inferred from the body, and propagated to callers |

`Never` is the empty set of exceptions and `...` is the gradual one, which is
exactly what those types mean everywhere else

the body is checked against the clause, and callers see the clause rather than
the body:

```by
def f() raises TypeError:
    raise ValueError  # error: `f` can raise `ValueError`, which its `raises` clause does not include
```

### negation

`not T` is accepted, and means the negation type it always means. that is
strict here: any two exception classes can be combined by a third that inherits
both, so `ValueError` is not *provably* outside `TypeError` and is reported too

```by
def f() raises not TypeError:
    raise ValueError  # error: `ValueError` is not provably outside `TypeError`
```

the practical way to rule an exception out is to declare what the function does
raise, or `raises Never`

## what is inferred

the analysis reports what it can see in the body:

- `raise X` and `raise X(...)`, and a bare `raise` inside a handler
- `assert`, which raises `AssertionError`
- calls to functions whose body is visible, transitively

everything else contributes nothing. in particular **a call into a stub raises
nothing** — the standard library, any third-party dependency — until that stub
carries a `raises` clause of its own. that is the only workable default:
assuming an unannotated callee raises anything would make every set
`BaseException` and the feature useless

`try` narrows the set. exceptions raised in the `try` body that an `except`
clause catches do not escape, while the handler, `else` and `finally` bodies
contribute their own raises:

```by
def f():  # raises ValueError — TypeError is handled, the handler's own raise is not
    try:
        raise TypeError
    except TypeError:
        raise ValueError
```

recursion is fine, including mutual recursion: the set is a least fixed point,
and a function's own raises are the identity of the union it contributes to

an **overloaded** function contributes the union of what all its overloads and
its implementation may raise. which overload a given call matched is not known
to this analysis, so the set is an upper bound — deliberately, since the safe
direction for an escape check is to name an exception that cannot happen rather
than to miss one that can

## where escapes are reported

an undeclared function simply **propagates** to its callers, so it is never an
error on its own — that is what makes the feature usable on existing code
without annotating anything. an escape is reported at exactly two boundaries:

- a function with a `raises` clause that can raise outside it —
    `undeclared-raise`
- `main`, the [entry point](main-function.md), which has no caller to propagate
    to — `unhandled-exception`

both point at the `raise` or the call that produces the exception, not at the
signature, so the fix is where the diagnostic is. a clause that contains no
exception at all is
`invalid-raises-clause`

`main` may declare a clause of its own, which opts it into the ordinary rule:

```by
def main() raises TypeError:  # fine — main says it may exit this way
    raise TypeError
```

## overrides

a call is checked against the type it can *see*. when a base method cannot
raise, nothing at a call on the base type says an exception can escape — yet a
subclass substituted for it can still raise from that call:

```by
def a() -> A:
    return B()

class A:
    def foo(self):
        pass

class B(A):
    override def foo(self):
        raise TypeError

def main():
    a().foo()  # statically raises nothing; at runtime raises TypeError
```

`override-raise` closes that hole by bounding
every override with the exception set of the method it overrides. it is a
**strictness option and off by default**, because honouring it makes a base
method's set part of its contract — adding a `raise` to a base method is then a
breaking change for every subclass. enable it per project:

```toml
[tool.ty.rules]
override-raise = "error"
```

with it on, `B.foo` above is reported, and the fixes are the ordinary ones:
declare the base as raising (`def foo(self) raises TypeError`), handle the
exception inside the override, or opt that method out with `raises ...`

only the nearest superclass defining the method is blamed, so a violation
introduced part-way up a hierarchy is reported once rather than at every
descendant. constructors are exempt, matching ty's existing policy for override
compatibility

## inlay hints

a function with no clause gets its inferred set as an inlay hint, written where
the clause would go, so accepting it reads as ordinary source:

```by
def leaf()⟨ raises TypeError⟩:
    raise TypeError

def caller()⟨ raises TypeError⟩:
    leaf()
```

## runtime guards

static checking is unconditional. the `runtime_raises_checks` option (`by run --runtime-raises-checks`, off by default) additionally wraps each declared
function in a guard that fails when it raises outside its clause, defending the
contract against callers the checker never saw — untyped or third-party code:

```by
def f() raises ValueError:
    ...
```

lowers to

```py
@_by_raises(ValueError, "f")
def f():
    ...
```

the guard is a decorator, not a `try` around the body, so it never disturbs the
lowering of anything inside the function. it reaches every declared function,
including one defined inside `if` / `try` / `with` / `for`, and it picks its
wrapper shape at decoration time so a coroutine, a generator and an async
generator are each entered before the check rather than after

only a clause with a faithful runtime test is guarded: a gradual `raises ...`
and any set with no runtime spelling are left alone, and `raises Never` becomes
the empty tuple, which nothing is an instance of

a decorated function whose statement another lowering re-renders cannot carry
the guard — the insertion sits inside the range being rebuilt — and that is a
transpile error rather than a silently missing check

## known gaps

deliberate, for now:

- context-manager `__enter__` / `__exit__`, constructor calls, and operators and
    other implicit dunder dispatch contribute nothing
- a `finally` block that swallows an in-flight exception by returning is not
    modelled
- `except*` is treated as catching nothing, since what escapes it is a regrouped
    `ExceptionGroup`
- the clause does not participate in callable assignability, so a raising
    function is still assignable to a `raises Never` callable type
- an overloaded function's set is the union over its overloads rather than the
    one the call actually matched (see above)

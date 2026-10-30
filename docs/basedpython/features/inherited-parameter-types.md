# inherited parameter types

an override accepts every call the method it overrides accepts, so a parameter
it leaves unannotated has the type of the parameter whose arguments it receives:

```by
class A:
    def f(self, a: int): ...

class B(A):
    override def f(self, a):
        reveal_type(a)  # int

B().f("s")  # error: invalid-argument-type
```

the body sees that type, and so does every call. an editor shows it as an inlay
hint where the annotation would go (`ty.inlayHints.inheritedParameterTypes`; see
[editor features](editor.md)). the emitted python is left unannotated, as it was
written

an annotation the override writes always wins

## which methods are overridden

every class in the MRO that defines the name, the same ones the override checks
hold the method to. the parameter has to accept the arguments of each, so it
takes the union of their types:

```by
class A:
    def f(self, a: str): ...

class B:
    def f(self, a: bytes): ...

class C(A, B):
    override def f(self, a): ...  # a: str | bytes
```

where one of them already overrides another, it accepts everything the other
does, so the union is its own type. a base whose members cannot be known, such
as `Any`, declares nothing to hold the method to and is passed over; a class
whose MRO cannot be built has no MRO to search, and inherits nothing. if one of
the classes declares the name as something other than a method, such as an
attribute or a property, what it accepts cannot be lined up with the method's
parameters, and nothing is inherited

which methods are overridden is a fact about the class, so it holds whether or
not the method says `override` — leaving the keyword out is reported on its
own, as `missing-override-decorator`

each method is read as the class sees it, so a base specialized in the class
header passes its type arguments down: `class B(A[int])` reads `A`'s `T` as
`int`, and `class C[U](A[U])` as `U`

a constructor — `__init__`, `__new__`, `__post_init__`, `__init_subclass__` — is
not held to the signature of the one it overrides, since a subclass's
constructor takes what that subclass needs, so it inherits nothing

## parameters correspond the way arguments bind

a parameter receives the arguments a call passes to its counterpart in the
overridden method: a positional-only parameter's by position, a keyword-only
parameter's by name, and a positional-or-keyword parameter's by both

```by
class A:
    def f(self, a: int, /, *, k: str): ...

class B(A):
    override def f(self, x, /, k): ...  # x: int, k: str
```

so a positional-or-keyword parameter the override renames receives the base
parameter's positional arguments but not its keyword ones, and takes nothing.
that override is reported as `invalid-method-override` anyway

`*args` and `**kwargs` each stand for a run of arguments, so each takes the
type of the base's own `*args` or `**kwargs`. an override that gathers what the
base spells out, as `def f(self, *args, **kwargs)` does to forward everything
to `super()`, receives those arguments in the run too, and takes nothing

a receiver the call binds is not an argument: `self` and `cls` correspond to
nothing, and the parameters after them line up whether each side is an instance
method, a `classmethod` or a `staticmethod`

## an overloaded base

an overloaded method accepts the calls of each overload, so the parameter takes
the union of what the overloads declare for it. an overload with no counterpart
for it passes it nothing, and has no say

the implementation of an overloaded function takes its parameter types from its
own overloads by the same rule, before anything it overrides is consulted —
except that it keeps the overloads' own type variables, which a call through one
of them solves

## what is not inherited

a base parameter has to state a type the override can use, or the parameter
takes nothing and is left to the rest of
[type inference](sound-types.md) — in a `.by` file, an anonymous type parameter
bounded by what the body does with it. that is the case when

- the base parameter has no type of its own either. a type it inherited from
    further up counts as its own, so a chain of overrides passes a type all the
    way down
- its type mentions a type variable of the overridden method, `Self` among
    them. those are bound to that method, and copying one across would silently
    rebind it to the override
- it is a `*args` or `**kwargs` whose annotation describes the whole run rather
    than each argument: `*Ts`, `*tuple[int, str]`, `P.args`, `P.kwargs` or
    `Unpack[TD]`
- it is one of several base parameters that hand the parameter their arguments
- any one counterpart is like that, of an overload or of one of several bases:
    a type made of the others would reject arguments that one accepts

## where it applies

everywhere unannotated parameters are inferred: always in a `.by` file, and in
a `.py` file under `infer-unannotated-signatures` or
[`sound-types`](sound-types.md). the [default](inherited-defaults.md) a
parameter inherits is a separate rule, which holds in `.by` files only

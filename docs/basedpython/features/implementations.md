# implementations

an `implementation` declares that an existing type satisfies an existing
interface, without touching either declaration:

```by
abstract class A:
    abstract def f(self)

class B:
    let a: int

implementation A for B:
    override def f(self):
        print(self.a)   # references to `B` members are okay

def f(a: A):
    a.f()

b = B()
f(b)                    # an implementation of `A` is in scope, so this checks
```

neither `A` nor `B` has to be yours. the interface may come from one dependency
and the type from another, and the conformance lives in your module

## why

python offers three ways to say "this type satisfies that interface", and each
one is missing something:

| mechanism                  | retroactive | adapts mismatched names | default method bodies | needs to own the type |
| -------------------------- | ----------- | ----------------------- | --------------------- | --------------------- |
| subclassing (`class B(A)`) | no          | no                      | yes                   | yes                   |
| protocols                  | yes         | no                      | no                    | no                    |
| `A.register(B)`            | yes         | no                      | no                    | no                    |
| implementations            | yes         | yes                     | yes                   | no                    |

subclassing needs the type's declaration. a protocol needs the members to
already line up, name for name. `abc.register` asserts conformance globally and
provides nothing — the members still have to exist on the type, so it cannot
adapt anything. an implementation is the missing option: conformance declared
from the outside, with a body that translates between the two shapes

[extensions](extensions.md) are the sibling feature: an extension adds *inherent*
members to a type, an implementation makes a type *usable as* an interface. an
extension cannot make `B` acceptable where `A` is asked for; an implementation
cannot add members to `B`'s own API

## syntax

```text
implementation <interface> for <type>[ as <name>]:
    <members>
```

`implementation` is a soft keyword, gated the same way `extension` is: a name at
statement start followed immediately by another name cannot begin a valid python
statement, so no real code changes meaning, and the form is rejected outright in
`.py` files

both operands are type expressions. the interface may be specialized
(`implementation Sized[int] for B:`), and the type side follows the extension
rules for generics (below)

an implementation is a module-level declaration. one nested in a function or a
class body is an error: only module-level implementations are applicable, and the
witness class the block lowers to has to exist at module scope for a conversion to
reach it

### naming an implementation

`as <name>` binds the implementation as a normal module-level symbol. calling it
is the explicit conversion:

```by
implementation A for B as BAsA:
    override def f(self): ...

xs: list[A] = [BAsA(x) for x in bs]
```

an anonymous implementation is only reachable through the conversions the
transpiler inserts (below). naming one is the escape hatch for every position
where an implicit conversion is not available, and it is also what makes the
witness object nameable in a signature or a `reveal_type`

### generic and conditional implementations

the type side reuses the implemented class's own type parameters by the names its
declaration bound, exactly as an extension does — no fresh parameters, no
implicit introduction:

```by
implementation Show for list:
    override def show(self) -> str:
        return ", ".join(str(x) for x in self)
```

a bracket bound narrows where the implementation applies:

```by
implementation Show for list[Element: Show]:
    override def show(self) -> str:
        return ", ".join(x.show() for x in self)
```

`list[int]` gets the first implementation, `list[Widget]` (where `Widget`
implements `Show`) gets the second. two implementations of the same pair are an
error, not a silent ordering choice: within one module the second is reported at
its own declaration, and two reached through imports are reported at the
conversion site that sees both

the interface's type arguments may reference those reused parameters:

```by
implementation Container[Element] for list: ...
```

a *blanket* implementation (`implementation Show for T: Display`) is out of
scope — see open questions

## what may be implemented

the interface side must be an `abstract class` or a protocol, and everything it
declares must be something a witness can carry. concrete classes are rejected:
pretending a type is a concrete class means pretending it has that class's fields,
and there is nowhere to put them

concretely, the interface may declare:

- `abstract def` members, with or without a default body
- concrete methods, `class def` / `static def` members, and properties
- class-level constants

and may not define `__init__`: a witness holds the implemented object and never
runs the interface's constructor, so state assigned there would silently never
exist. an annotation with no value (`label: str`) is the same problem one step
removed — nothing but a constructor would assign it — so the implementation must
supply it, as a method or a class-level value. both are reported at the
`implementation` declaration, not at the interface

the type side must be a class. an implementation whose type already satisfies the
interface — by subclassing it, or structurally for a protocol — is an error: no
conversion would ever fire, so the block would be dead code

## what an implementation body may contain

only members that correspond to interface members, each marked `override`:

```by
implementation A for B:
    override def f(self):
        print(self.a)

    @property
    override def name(self) -> str:
        return f"B({self.a})"
```

every abstract member without a default body must be supplied; a member with a
default body may be omitted (it is inherited) or overridden. a missing one is
reported at the header, because an anonymous implementation is never instantiated
in source and so has no call site for the ordinary abstract-instantiation error to
land on

a member that matches nothing on the interface is an error, with the fix being an
[extension](extensions.md) — that division keeps "what does this block promise"
readable at a glance

private helpers therefore live in an extension or a module-level function, not in
the block

## the witness type

`implementation A for B` introduces a type: the witness. it is a subtype of `A`
and of everything `A` is a subtype of. it is **not** a subtype of `B`

```by
implementation A for B as BAsA:
    override def f(self): ...

def g(w: BAsA):
    reveal_type(w)          # BAsA
    takes_a(w)              # ok — a witness is an A
    takes_b(w)              # error: `BAsA` is not assignable to `B`
```

that asymmetry is the whole safety story. a witness is a distinct object at
runtime (below), so letting it flow into a `B` position would hand out something
whose `type()` and `isinstance` answers are wrong. member *access* still reaches
`B`'s members, because that is what the implementation body needs

### `self` inside the body

`self` is the witness. it resolves interface members and `B`'s members alike:

```by
implementation A for B:
    override def f(self):
        print(self.a)       # B's field
        self.g()            # a sibling interface member
        super().f()         # the interface's default body
```

member precedence is: the block's own members, then the interface's, then `B`'s.
so if both `A` and `B` declare `name`, `self.name` is `A`'s — a witness is an
`A`-faced object. `self.__implemented__` is the underlying `B`, for the cases
that need the real object:

```by
implementation A for B:
    override def f(self):
        takes_b(self.__implemented__)
```

## conversion sites

`B` is not a subtype of `A` anywhere in the type lattice. instead, an
implementation *repairs* an assignment that would otherwise fail, at positions
where the transpiler can materialize the witness:

> a conversion site is any expression that the type checker checks against a
> declared type context

that is one rule over machinery ty already has, and it covers:

```by
f(b)                            # argument to a parameter declared `A`
x: A = b                        # annotated assignment
x = b                           # assignment to a name declared elsewhere
self.field = b                  # a declared attribute
def g() -> A: return b          # return in a function declared `-> A`
z: A = b if c else other_b      # a propagated context — the whole value converts
xs: list[A] = [b, other_b]      # each element of a literal with a declared target
ys: list[A] = [x for x in bs]   # a comprehension's element expression
d: dict[str, A] = {"k": b}      # a mapping literal's values
```

the collection cases convert *element-wise*: each element is wrapped where it
stands, which is an honest conversion precisely because the elements are in the
source. that is also the line the variance restriction sits on — see below

the repair is single-step: if `B` implements `A` and `A` implements `C`, `b` is
not convertible to `C`

a conversion that runs at import time must appear after the implementation it
converts through, because the witness class is a class statement like any other —
python binds its name when the statement executes. a conversion inside a function
body resolves the name when the function runs, so order does not constrain it

### what is not a conversion site

everything else — most importantly, anything already inside a constructed
generic:

```by
bs: list[B] = [...]
takes_sequence(bs)   # error: `list[B]` is not assignable to `Sequence[A]`
```

`list[B]` is not a `Sequence[A]`, because making it one would mean wrapping every
element — an O(n) copy with different identity, hidden behind a call. so the
conversion has to be written where it happens:

```by
takes_sequence([BAsA(x) for x in bs])
```

the type error at such a site carries a subdiagnostic pointing at the applicable
implementation and suggesting the explicit form, so the restriction is
discoverable rather than mysterious

`*args` / `**kwargs` splats, an unpacked element (`[b, *bs]`), an unpacking
target (`x, y = ...`, which binds an element rather than the value), a name in
`x = y = b` (one value reaching two places that need not agree about it), and a
typevar solved by inference are likewise not conversion sites

## scope and coherence

an implementation applies where it is visible: in its own module, and in any
module that imports the module declaring it — `import mod` and `from mod import X`
both count, and there is no per-implementation import. importing the interface and
the implemented type by name is the natural way to write it, so that is what
establishes the dependency; requiring a separate `import mod` whose symbols are
never used would leave an import that reads as removable to anyone tidying the
file, silently withdrawing conformance. nothing is registered globally and nothing
is monkeypatched, so two dependencies cannot fight over the same pair: their
implementations are simply not visible to each other

that is what makes it safe to allow an implementation whose interface and type
both come from elsewhere — the feature's main use. it needs no cooperation from
either, because the adaptation reaches no further than the modules that import
it

when two applicable implementations of the same interface-and-type pair are
visible at one conversion site, that is an error (`ambiguous-conversion`).
constrain one with a bracket bound, or drop the import that brings the second
into scope

## identity, mutation, and equality

a witness is a separate object, and this is the honest cost of the feature:

- `f(b)` inside a loop allocates a witness per call. it is a small object with
    `__slots__`, but it is not free
- `A(…) is b` is false, and two conversions of the same `b` produce two
    witnesses that are not `is`-identical
- state is shared: reads and writes through a witness forward to the underlying
    object, so `w.a = 1` is visible as `b.a`
- `==`, `hash`, and `repr` delegate to the underlying object, so a witness and
    its object are interchangeable as dict keys and in sets — but only where the
    interface leaves them to `object`. when the interface defines one itself, the
    witness does not delegate it and the interface's version wins. `__eq__` and
    `__hash__` move together, because python sets `__hash__ = None` on any class
    that defines `__eq__` alone
- `isinstance(b, A)` is false. `b` really is not an `A`; the witness is

## lowering

an implementation lowers to a class, and a conversion lowers to constructing it.
there is no registry and no runtime type inspection — the type checker decides
everything and the output is plain python

### the witness class

```by
implementation A for B:
    override def f(self):
        print(self.a)
```

→

```python
class _by_impl__A__B(_by_Implementation, A):  # basedpython: implementation A for B
    __slots__ = ()

    def f(self):
        print(self.a)
```

a named implementation uses its own name instead of the mangled one. the shared
base is a polyfill, injected like every other basedpython polyfill:

```python
class _by_Implementation:
    __slots__ = ("__implemented__",)

    def __init__(self, implemented):
        object.__setattr__(self, "__implemented__", implemented)

    def __getattr__(self, name):
        if name == "__implemented__":
            raise AttributeError(name)
        return getattr(self.__implemented__, name)

    def __setattr__(self, name, value):
        setattr(self.__implemented__, name, value)

    def __eq__(self, other):
        if isinstance(other, _by_Implementation):
            other = other.__implemented__
        return self.__implemented__ == other

    def __hash__(self):
        return hash(self.__implemented__)

    def __repr__(self):
        return repr(self.__implemented__)
```

three things fall out of subclassing the interface, rather than needing to be
built: the interface's default bodies are inherited, `super()` in a block member
works with no special handling, and `isinstance(witness, A)` is true. `__getattr__`
forwarding is what makes `self.a` reach `B` with no rewriting of the body, and
`__setattr__` forwarding keeps mutation shared. the interface's `__init__` never
runs, which is exactly why an interface with stored state is rejected

### conversions, lowered

each conversion site wraps the expression in place:

```by
f(b)
xs: list[A] = [b1, b2]
```

→

```python
f(_by_impl__A__B(b))
xs: list[A] = [_by_impl__A__B(b1), _by_impl__A__B(b2)]
```

when the implementation lives in another module, the lowering emits the precise
import of the witness class, keyed off the checker's resolution — the same
implicit-import treatment extension members get:

```python
from adapters import _by_impl__A__B
```

## round-tripping

the marker comment on the witness class carries the header, so the reverse
transpiler re-sugars it into an `implementation` block. a call of a same-file
witness class unwraps back to its argument for an anonymous implementation, and
stays a call for a named one (an explicit witness call is valid basedpython
either way, so nothing is lost). a witness-shaped class written by hand without
the marker stays ordinary python

## diagnostics

| lint                     | default | fires on                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| ------------------------ | ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `invalid-implementation` | error   | interface is not an abstract class or protocol; interface has stored instance state; either operand does not resolve to a class; a bracket parameter the type does not declare; an abstract member left unimplemented; a block member matching nothing on the interface; a duplicate implementation of the same pair in one module; a type that already satisfies the interface; a delegated dunder the interface declares and the block does not override |
| `ambiguous-conversion`   | error   | a conversion site where two visible implementations of the same pair apply                                                                                                                                                                                                                                                                                                                                                                                 |

a failed assignment that an out-of-reach implementation would have repaired
keeps its usual error (`invalid-argument-type` and friends) and gains a
subdiagnostic naming the implementation and the explicit form

## rejected alternatives

**monkeypatching plus `register`.** lower to `B.f = …; A.register(B)`, and pass
`b` itself. tempting: identity is preserved and `list[B]` really is a
`Sequence[A]` at runtime. rejected because it mutates `B` for the whole process,
so the lexical scoping that makes conflicting implementations harmless
disappears; it cannot touch builtins or C types; it clobbers a same-named member
on `B`, which is precisely the case adaptation exists for; and `register` gives
no default bodies. a hybrid that monkeypatches when it can and wraps otherwise
would make the runtime semantics depend on invisible conditions

**typing the witness as `B & A`.** an [intersection](intersection.md) already
means "both", needs no new type, and gives `self` the right member set for free.
rejected because the intersection is assignable to `B`, so `return self` from a
block member would type-check and hand out a witness where a real `B` is
required — an unsoundness that the distinct witness type rules out by
construction

**dot access on the implementing type** — `b.f()` resolving through an in-scope
implementation, so an interface member reads like a member of `B` itself.
deferred rather than rejected: it would have to allocate a witness behind a plain
attribute access, since a block member may call sibling interface members that
only exist on the witness, and an implicit allocation behind a `.` is worth
settling separately. adding inherent members is what extensions are for

## implementation plan

1. **parser** — soft-keyword branch beside `extension` in `statement.rs`,
    producing a `ClassDef` whose base is the interface, plus a real AST field for
    the `for <type>` operand and the optional `as <name>`. a real field rather
    than a synthesized marker node: the formatter rebuilds from AST ranges, and
    synthesized surface syntax corrupts on reformat
1. **formatter** — print the `implementation … for … as …:` header from those
    ranges, and sweep the mdtest corpus with the cache disabled to confirm no
    round-trip corruption
1. **semantic index** — mangled binding for an anonymous declaration
    (`<implementation:A:B>`), normal binding for a named one; typevar reuse for
    the type operand reusing extensions' `body_view_class` /
    `extension_body_typevar`
1. **ty: registry** — `types/implementations.rs` mirroring `types/extensions.rs`:
    `implementations_in_module`, `applicable_implementations`, `implemented_class`
    / `implementing_class`, and the pair-conflict query. all salsa-tracked,
    boxed slices
1. **ty: witness type** — the declaration's class *is* the witness type, so
    subtyping, MRO, `override` checking, abstract-member enforcement, and
    `super()` need no new code. the new parts are the member-lookup fallback to
    the implementing class (mirroring `resolve_extension_member`) and the
    `__implemented__` member
1. **ty: repair hook** — consulted where an expression is checked against a
    declared type context, *not* in `relation.rs`: keeping the edge out of the
    lattice is what forbids the `list[B]` → `Sequence[A]` hole
1. **ty: validation** — `validate_implementation_declaration` mirroring
    `validate_extension_declaration`, and the two lints above
1. **transpiler bridge** — an `implementation_conversion` method on `TypeInfo`
    answering "does this expression convert, to what class, imported from where"
1. **transpiler: forward** — `transforms/implementation.rs` emitting the witness
    class, the polyfill, and the conversion wraps. each wrap must be a *single*
    template edit (`Lit` + `Src` + `Lit`) so the claim pass cannot tear a
    prefix/suffix pair in half, and must parenthesize the wrapped expression
1. **transpiler: reverse** — marker-driven re-sugaring of the class and
    unwrapping of anonymous witness calls
1. **tests** — three harnesses, and the cross-module ones are not optional:
    every claim in [scope and coherence](#scope-and-coherence) is invisible in a
    single file, and the witness-import lowering cannot run at all without a
    second module
    - **checker** — `basedpython_implementations.md`, mirroring
        `basedpython_extensions.md`. single-file blocks for conformance,
        conversion sites, the variance restriction, and every
        `invalid-implementation` arm; multi-file blocks (`` `impl.by`: `` then
        `` `main.by`: ``, one `##` header per example) for: `import impl` makes an
        implementation applicable; without the import the conversion does not
        happen and the assignment errors; the same pair implemented in two
        imported modules is `ambiguous-conversion`; an implementation applying
        in one module and not its sibling
    - **transpiler** — the `cross_file` module in `by_transforms/src/lib.rs`
        (`project_db` + `transpile_typed`), beside
        `imported_extension_rewrites_call_and_adds_import`: a conversion site
        whose witness class lives in another module must wrap the expression *and*
        emit `from impl_mod import _by_impl__A__B`, and the anonymous mangled
        name must agree between the two files
    - **runtime** — an `implementation_runtime.rs` beside the other `*_runtime.rs`
        integration tests, for shared mutation through a witness, `==` / `hash`
        interchangeability with the underlying object, `isinstance` answers,
        default-body inheritance, and `super()`. note that `build_case` in those
        files transpiles each module independently through `transpile(&str)`, so a
        cross-module witness import needs the project-db path instead — the
        single-file entry point cannot resolve the other module's implementation

## open questions

- **blanket implementations** (`implementation Show for T: Display`). they need a
    witness class that is generic over the implementing type, which the runtime
    shape already supports — one adapter forwarding to anything. the checker side
    is the work: applicability becomes a bound check against every type reaching a
    conversion site, and overlap between a blanket and a concrete implementation
    needs a specificity rule rather than the flat ambiguity error
- **abstract `class def` / `static def` members.** a witness is per-instance, so
    an associated function has no receiver to carry the implementation choice.
    reachable today only through a named implementation (`BAsA.from_str("x")`),
    which may be reason enough to require a name when the interface declares one
- **witness interning**, so `A(b)` twice is `is`-identical. a
    `WeakValueDictionary` keyed on `id` costs a lookup per conversion and fails
    for objects that are neither weak-referenceable nor hashable, so v1 allocates
    every time
- whether the implementing type may be a union (`implementation A for B | C`),
    which would need one witness class per arm and a conversion that picks by
    static type

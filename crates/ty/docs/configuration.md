<!-- WARNING: This file is auto-generated (cargo dev generate-all). Update the doc comments on the 'Options' struct in 'crates/ty_project/src/metadata/options.rs' if you want to change anything here. -->

# Configuration
## `rules`

Configures the enabled rules and their severity.

The keys are either rule names or `all` to set a default severity for all rules.
See [the rules documentation](https://ty.dev/rules) for a list of all available rules.

Valid severities are:

* `ignore`: Disable the rule.
* `warn`: Enable the rule and create a warning diagnostic.
* `error`: Enable the rule and create an error diagnostic.

By default, ty exits with code 1 if it emits any warning or error diagnostics.
Set `terminal.error-on-warning` to `false` to exit with code 0 if all diagnostics have `warning` severity.

**Default value**: `{...}`

**Type**: `dict[RuleName | "all", "ignore" | "warn" | "error"]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.rules]
    possibly-unresolved-reference = "warn"
    division-by-zero = "ignore"
    ```

=== "ty.toml"

    ```toml
    [rules]
    possibly-unresolved-reference = "warn"
    division-by-zero = "ignore"
    ```

---

## `type-checking-preset`

The defaults that `rules` and `analysis` start from.

A preset decides which diagnostics exist and which of them are enabled, and it supplies
the default for every `analysis` option. Both tables are still read, and both still win
over the preset, so a preset is a starting point rather than a straitjacket.

* `strict`: every diagnostic is enabled, and every analysis option that buys soundness
  is on. This is the default.
* `ty-compatible`: the defaults of [ty](https://github.com/astral-sh/ty), which
  basedpython is built on. basedpython's own diagnostics and analysis options are off,
  so that a project reports what ty itself would report. A diagnostic that doesn't exist
  in ty can't be enabled under this preset, not even with `rules = { all = "error" }`.

Defaults to `strict`.

**Default value**: `strict`

**Type**: `"strict" | "ty-compatible"`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty]
    type-checking-preset = "ty-compatible"
    ```

=== "ty.toml"

    ```toml
    type-checking-preset = "ty-compatible"
    ```

---

## `analysis`

### `allowed-unresolved-imports`

A list of module glob patterns for which `unresolved-import` diagnostics should be suppressed.

Details on supported glob patterns:
- `*` matches zero or more characters except `.`. For example, `foo.*` matches `foo.bar` but
  not `foo.bar.baz`; `foo*` matches `foo` and `foobar` but not `foo.bar` or `barfoo`; and `*foo`
  matches `foo` and `barfoo` but not `foo.bar` or `foobar`.
- `**` matches any number of module components (e.g., `foo.**` matches `foo`, `foo.bar`, etc.)
- Prefix a pattern with `!` to exclude matching modules

When multiple patterns match, later entries take precedence.

Glob patterns can be used in combinations with each other. For example, to suppress errors for
any module where the first component contains the substring `test`, use `*test*.**`.

**Default value**: `[]`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Suppress errors for all `test` modules except `test.foo`
    allowed-unresolved-imports = ["test.**", "!test.foo"]
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Suppress errors for all `test` modules except `test.foo`
    allowed-unresolved-imports = ["test.**", "!test.foo"]
    ```

---

### `bivariant-private-attributes`

Whether a private attribute leaves an inferred type parameter bivariant. This is a
basedpython feature.

A private (single-underscore or name-mangled) member is invisible to external observers, so
it cannot be used to distinguish two specializations of its class, and therefore cannot
constrain the class's variance:

```python
class A[T]:
    _t: T
```

With this option enabled, `T` is inferred bivariant: nothing on `A`'s public surface
mentions `T`, so `A[int]` and `A[object]` are mutually assignable. As soon as a public
member mentions `T`, that member drives the inference as usual.

When set to `false`, a private attribute is instead treated as immutable-but-readable,
which constrains the type parameter to covariance.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Let private attributes constrain inferred variance to covariance
    bivariant-private-attributes = false
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Let private attributes constrain inferred variance to covariance
    bivariant-private-attributes = false
    ```

---

### `block-scoped-declarations`

Whether a `let` or `var` declaration written inside a block binds its name for
that block only. This is a basedpython feature.

Python has no block scopes: a name bound anywhere in a function is a local of
that whole function, and the python a `.by` file lowers to keeps it that way. So
this is a rule the checker enforces rather than something the emitted code does:

```by
if flag:
    let a = 1

print(a)  # error: `a` is not in scope here
```

Only the binding keyword scopes a name to its block. A plain `a = 1` binds for
the whole enclosing function or module, as it does in python.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Let a `let` or `var` in a block be visible for the rest of the scope
    block-scoped-declarations = false
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Let a `let` or `var` in a block be visible for the rest of the scope
    block-scoped-declarations = false
    ```

---

### `dependency-groups`

The requirement groups the matching files may import from.

`project` names `[project].dependencies`, an extra or a PEP 735 dependency group
is named by its own name, and `*` names every group.

When this is unset, a file may import from every group unless it is part of what
the project ships — the modules named by `shipped-modules` — in which case it may
import only `project` and the extras. Nothing the project ships can import a
dependency group, because nothing installs one alongside the project.

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    [[tool.ty.overrides]]
    include = ["tests/**"]

    [tool.ty.overrides.analysis]
    dependency-groups = ["project", "dev", "test"]
    ```

=== "ty.toml"

    ```toml
    [analysis]
    [[overrides]]
    include = ["tests/**"]

    [overrides.analysis]
    dependency-groups = ["project", "dev", "test"]
    ```

---

### `disable-fluid-specializations`

Whether to disable "fluid specializations", a basedpython feature that widens the
inferred generic specialization of an unannotated binding flow-sensitively based on
its later uses in the same scope.

When set to `true`, each unannotated binding keeps the specialization it was inferred
with at its creation site; later uses no longer widen or lock it.

Defaults to `false`, and to `true` under the `ty-compatible` type checking preset.

**Default value**: `false`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Turn off fluid specializations
    disable-fluid-specializations = true
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Turn off fluid specializations
    disable-fluid-specializations = true
    ```

---

### `exported-dependencies`

The dependencies this project hands to its own users.

A library whose interface is partly made of another distribution's types — one that
returns numpy arrays, or takes a pydantic model — can say so, and then a project
that depends on this one may import those distributions without declaring them
itself.

Only what the project already depends on can be exported, and the claim only
travels one link: exporting a distribution does not export whatever *it* depends
on, unless that distribution exports it in turn.

This is written into the `by.typed` marker when the project is built, because that
is what its users have — a `pyproject.toml` is not installed with the package.

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    exported-dependencies = ["numpy"]
    ```

=== "ty.toml"

    ```toml
    [analysis]
    exported-dependencies = ["numpy"]
    ```

---

### `implicit-object-repr-exempt-types`

A list of classes never reported as an
[`implicit-object-repr`](rules.md#implicit-object-repr).

A class deriving from one of these is exempt too, so listing a base opts out a whole
hierarchy.

Entries are qualified class names (`decimal.Decimal`). A class in `builtins` may also be
spelled bare (`int`).

**Default value**: `[]`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Never report a bare `Thread` or `Lock`
    implicit-object-repr-exempt-types = ["threading.Thread", "threading.Lock"]
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Never report a bare `Thread` or `Lock`
    implicit-object-repr-exempt-types = ["threading.Thread", "threading.Lock"]
    ```

---

### `implicit-object-repr-report-types`

A list of classes whose stub is taken at its word when looking for an
[`implicit-object-repr`](rules.md#implicit-object-repr).

A stub normally settles nothing, because it omits `__str__` and `__repr__` whether or not
the runtime class has them — `int` declares neither and still prints as a number. For a
class listed here the omission counts as real, the same way it would for a class written
in source, so a value of that class is reported unless the stub does declare one.

Defaults to the two whose bare repr is seen most often: `types.FunctionType`, which prints
`<function f at 0x...>`, and `builtins.type`, which prints `<class 'C'>`.

Entries are qualified class names (`decimal.Decimal`). A class in `builtins` may also be
spelled bare (`int`).

**Default value**: `["types.FunctionType", "builtins.type"]`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Also report a bare module object
    implicit-object-repr-report-types = ["types.FunctionType", "type", "types.ModuleType"]
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Also report a bare module object
    implicit-object-repr-report-types = ["types.FunctionType", "type", "types.ModuleType"]
    ```

---

### `infer-unannotated-signatures`

Whether a function with no annotations is given the signature its body determines. This is
a basedpython feature.

Python's gradual guarantee makes an unannotated `def` say nothing: its parameters accept
anything and it returns `Unknown`. That is the largest remaining source of `Unknown` in an
otherwise typed project, and it silently swallows real mistakes. With this enabled, the
missing half of the signature is recovered from what the function itself already determines:

- **Each unannotated parameter** opens an anonymous type parameter named after it — the same
  hole `some` spells by hand — bounded by everything the function requires of it: the
  promoted type of its default, the members its body reads and calls, the parameters it is
  forwarded into, and any `assert` at the top of the body. Naming the hole is what keeps
  what a call passes in connected to what it gets back, so `def ident(x): return x` is
  inferred as the identity function.
- **A missing return type** is the union of what the body returns, plus `None` when control
  can also fall off the end. An empty body returns `None`, a body that always raises returns
  `Never`, and a generator returns a generator.

Nothing is invented from a use this analysis cannot read, so such a parameter stays gradual
and its body keeps type-checking exactly as it did. An explicit annotation always wins, and
so does anything an overload group or an overridden base method already supplies.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Leave an unannotated function gradual
    infer-unannotated-signatures = false
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Leave an unannotated function gradual
    infer-unannotated-signatures = false
    ```

---

### `overlapping-condition-assume-truthy-instances`

Whether an instance with no `__bool__` and no `__len__` counts as always truthy when
looking for an [`overlapping-condition`](rules.md#overlapping-condition).

Such an instance is only *ambiguously* truthy — a subclass may define `__bool__` — so by
default it is a falsy member of `if not x` just as `None` is. Enabling this assumes the
class means what it looks like it means, which drops the reports for the very common
`if not x` over an optional instance.

Defaults to `false`.

**Default value**: `false`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # `if not x` over a `Foo | None` only selects `None`
    overlapping-condition-assume-truthy-instances = true
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # `if not x` over a `Foo | None` only selects `None`
    overlapping-condition-assume-truthy-instances = true
    ```

---

### `overlapping-condition-exempt-types`

A list of classes whose values do not count as a distinct member of an
[`overlapping-condition`](rules.md#overlapping-condition).

`if not x` over an `int | None` selects both a falsy `int` and `None`, and is reported
because the branch cannot tell them apart. Listing `int` here says that conflating a falsy
`int` with anything else is fine, so only `None` is left and the condition is accepted.

Entries are qualified class names (`decimal.Decimal`). A class in `builtins` may also be
spelled bare (`int`), and `None` stands for the type of `None`.

**Default value**: `[]`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Accept a falsy `int` or `str` sharing a branch with another member
    overlapping-condition-exempt-types = ["int", "str"]
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Accept a falsy `int` or `str` sharing a branch with another member
    overlapping-condition-exempt-types = ["int", "str"]
    ```

---

### `precise-unsolved-typevars`

Whether a type variable that a call leaves unsolved is solved to `Never`. This is a
basedpython feature.

A call can leave a type variable entirely unsolved, because no argument mentions it:

```python
def f[T]() -> T: ...

a = f()
```

`Never` is the precise answer here: no value ever reaches that position, so nothing the
call returns can be observed at type `T`. When set to `false`, the type variable falls back
to the gradual `Unknown` instead, which silences any error that would follow from the call
site.

This applies where the type variable is an output. Where it is instead written through or
passed back in — the element of an invariant `list[T]`, the parameter of a returned
`Callable[[T], R]` — `Never` would say that nothing can ever be put there, so an invariant
or contravariant occurrence keeps the gradual `Unknown`.

A PEP 696 default (`def f[T = str]()`) always takes priority, and a `ParamSpec`,
`TypeVarTuple` or keyword-variadic pack is unaffected because `Never` is not a valid
solution for one.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Solve an unsolved type variable to `Unknown` rather than `Never`
    precise-unsolved-typevars = false
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Solve an unsolved type variable to `Unknown` rather than `Never`
    precise-unsolved-typevars = false
    ```

---

### `replace-imports-with-any`

A list of module glob patterns whose imports should be replaced with `typing.Any`.

Unlike `allowed-unresolved-imports`, this setting replaces the module's type information
with `typing.Any` even if the module can be resolved. Import diagnostics are
unconditionally suppressed for matching modules.

- Prefix a pattern with `!` to exclude matching modules

When multiple patterns match, later entries take precedence.

Glob patterns can be used in combinations with each other. For example, to suppress errors for
any module where the first component contains the substring `test`, use `*test*.**`.

When multiple patterns match, later entries take precedence.

**Default value**: `[]`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Replace all pandas and numpy imports with Any
    replace-imports-with-any = ["pandas.**", "numpy.**"]
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Replace all pandas and numpy imports with Any
    replace-imports-with-any = ["pandas.**", "numpy.**"]
    ```

---

### `respect-type-ignore-comments`

Whether ty should respect `type: ignore` comments.

When set to `false`, `type: ignore` comments are treated like any other normal
comment and can't be used to suppress ty errors (you have to use `ty: ignore` instead).

Setting this option can be useful when using ty alongside other type checkers or when
you prefer using `ty: ignore` over `type: ignore`.

Defaults to `true`.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Disable support for `type: ignore` comments
    respect-type-ignore-comments = false
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Disable support for `type: ignore` comments
    respect-type-ignore-comments = false
    ```

---

### `shipped-modules`

The top-level modules the project ships.

Defaults to the module named after `[project].name`: a project named `my-lib`
ships `my_lib`. Only a project that ships several unrelated modules, or one whose
module is not named after it, needs to say.

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    shipped-modules = ["foo", "foo_plugins"]
    ```

=== "ty.toml"

    ```toml
    [analysis]
    shipped-modules = ["foo", "foo_plugins"]
    ```

---

### `sound-types`

Whether to infer sound (non-gradual) types wherever a precise type is available. This is a
basedpython feature.

Python's gradual guarantee requires a type checker to fall back to a gradual type whenever
an annotation is missing, even when a precise type could be inferred. In a fully typed
project that is pure boilerplate: it forces an annotation to be written for something the
checker already knows. When set to `true`, this option deliberately breaks the gradual
guarantee and uses the precise type instead. It affects:

- **Unannotated parameters**: each one opens an anonymous type parameter named after it,
  bounded by everything the function requires of it — the promoted type of its default, the
  members its body reads and calls, the parameters it is forwarded into, and any `assert` at
  the top of the body. So `def f(a=1)` rejects a `str` at a call site, and
  `def ident(x): return x` is inferred as the identity function. A lambda parameter with a
  default takes that default's promoted type directly.
- **Unannotated return types**: the union of what the body returns, plus `None` when control
  can fall off the end. An empty body returns `None` and a body that always raises returns
  `Never`; a generator returns a generator.
- **Unannotated methods that override a base method**: the parameter and return types are
  inherited from the overridden method, including from `Protocol` members and
  `abstractmethod` declarations.
- **Bare `ClassVar` annotations**: `x: ClassVar = 1` declares `int` rather than the union of
  `Unknown` and the inferred type.
- **Empty collection literals**: `[]` has element type `Never`, so passing one to a generic
  call solves from it precisely instead of leaking `Unknown`.

An explicit annotation always takes priority over any of the above.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Fall back to a gradual type wherever an annotation is missing
    sound-types = false
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Fall back to a gradual type wherever an annotation is missing
    sound-types = false
    ```

---

### `strict-equality-semantics`

Configure ty's behavior regarding type inference and narrowing of equality
checks.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

With this option disabled, ty makes various assumptions about equality checks that
match the intuitions of most Python programmers, but may not be fully sound in all
situations. Leaving it enabled makes ty conservative about those assumptions, making it
less likely to infer `Literal[True]` or `Literal[False]` as the result of an
equality check. This has various effects on type checking, including fewer type
narrowing opportunities and more conservative assumptions regarding control flow.

One such unsound assumption is narrowing an object `x` of type `str` to `Literal["a"]`
after an `if x == "a"` check. This is unsound because a subclass of `str` with value
`"a"` will (by default) compare equal to `"a"`, but will not be of type `Literal["a"]`:

```pycon
>>> # `Literal["a"]` can only be inhabited by instances of exactly `str`, not
>>> # subclasses, but str subclasses compare equal by default:
>>> class StringSubclass(str): ...
...
>>> StringSubclass("a") == "a"
True
>>>
>>> # This also applies to `StrEnum`s:
>>> from enum import StrEnum
>>> class MyEnum(StrEnum):
...     A = "a"
...
>>> MyEnum.A == "a"
True
```

This option prevents the unsound narrowing of `x` to `Literal["a"]`, and instead keeps
it as `str`:

```python
from typing import Literal

def parse(value: str) -> Literal["a"] | None:
    # with `strict-equality-semantics` enabled, no narrowing will occur here,
    # and an error will be emitted on the `return` statement.
    if value == "a":
        return value
    return None
```

Another assumption ty makes by default is that subclasses will never override `__eq__` or
`__ne__`. This allows ty to narrow the following union based on an equality check, despite
the fact that an instance of a subclass of `Foo` could compare equal to `None`, and it's
perfectly valid to pass an instance of a subclass into the `x` parameter of this function:

```python
def narrow(x: Foo | None, other: Foo) -> None:
    if x == other:
        # with this option enabled, `x` still has type `Foo | None` here,
        # since it is legal to subclass `Foo` and override its `__eq__` method.
        reveal_type(x)
```

Many operations in Python implicitly call `__eq__` under the hood, and this option
impacts those too. For example, it also impacts narrowing from `in` checks, and narrowing
in `match` statements that use value patterns:

```python
def narrow_in(x: Foo | None, other: list[Foo]) -> None:
    if x in other:
        # with this option enabled, `x` still has type `Foo | None` here,
        # since the `in` operator implicitly calls `__eq__` on each element of `other`.
        reveal_type(x)


def narrow_match(x: str) -> None:
    match x:
        case "a":
            # with this option enabled, `x` still has type `str` here,
            # since this `case` branch will be taken by any object that compares
            # equal to `"a"`, including subclasses of `str`.
            reveal_type(x)
```

**Default value**: `false`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Preserve broad builtin types instead of narrowing them to literals
    strict-equality-semantics = true
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Preserve broad builtin types instead of narrowing them to literals
    strict-equality-semantics = true
    ```

---

### `strict-float`

Whether `float` and `complex` annotations mean *only* themselves. This is a
basedpython feature.

The typing spec's special case says an `int` is acceptable wherever a `float` is
asked for, so `x: float` really declares `int | float`. A `.by` file opts out of
that already; this makes the same model available to a `.py` one, per module.

It is not only a checking question. The wider annotation is why a `.py`
`list[float]` cannot be laid out as an unboxed buffer and a `.py` class cannot
have `double` fields, so `by compile` reads this to choose a representation.

Defaults to `false`.

**Default value**: `false`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # `float` means float, so a numeric module compiles to unboxed doubles
    strict-float = true
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # `float` means float, so a numeric module compiles to unboxed doubles
    strict-float = true
    ```

---

### `strict-generic-narrowing`

Whether ty should use strict narrowing for unspecialized generic classes in
`isinstance()` and `issubclass()` checks, as well as `match` class patterns.

When enabled, ty narrows to the top materialization of the class. For example,
`isinstance(value, list)` narrows a value of type `object` to `Top[list[Unknown]]`,
representing the (infinite) union of all possible `list` specializations. Iterating
over the list would yield values of type `object`.

When disabled, ty uses gradual generic narrowing, preserving compatible type
arguments from the original type where possible. For example,
`isinstance(value, list)` narrows a value of type `Sequence[int]` to `list[int]`.
If no specialization is available, the same check narrows a value of type `object`
to `list[Unknown]`; items of any type can then be appended to the list. Class
patterns such as `case list():` follow the same behavior.

Defaults to `false`.

**Default value**: `false`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.analysis]
    # Use the top materialization when narrowing to an unspecialized generic class
    strict-generic-narrowing = true
    ```

=== "ty.toml"

    ```toml
    [analysis]
    # Use the top materialization when narrowing to an unspecialized generic class
    strict-generic-narrowing = true
    ```

---

## `build`

### `exclude`

Files to keep out of the build output.

The syntax is the same as `src.exclude`, and paths are anchored to the
project root. Excluding a `.by` file keeps its transpiled output out of
the build as well.

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.build]
    exclude = [
        "tests",
        "**/*.snapshot",
    ]
    ```

=== "ty.toml"

    ```toml
    [build]
    exclude = [
        "tests",
        "**/*.snapshot",
    ]
    ```

---

### `include`

Files to carry into the build output verbatim, in addition to the ones
that are there by default.

`by build` mirrors the whole module tree: a `.by` file is transpiled, and
every other file — a hand-written `.py`, a `py.typed` marker, a template,
a data file — is copied to the same place in the output. `include` is for
the files that sit *outside* a module root and still belong in the build,
such as a data directory next to `src`.

The syntax is the same as `src.include`, and paths are anchored to the
project root. `exclude` takes precedence over `include`.

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.build]
    include = [
        "assets",
    ]
    ```

=== "ty.toml"

    ```toml
    [build]
    include = [
        "assets",
    ]
    ```

---

### `sources`

Whether the build output carries the `.by` sources alongside the python
they were transpiled into, with a `by.typed` marker naming them as the
authoritative surface.

This is what lets one basedpython project depend on another: a downstream
python project reads the transpiled `.py` and is served perfectly, while a
downstream basedpython project reads the `.by` and keeps the declarations
that have no python spelling — `extension` blocks, `raises` clauses,
read-only `let`, sum types.

Enabled by default. Turn it off to ship python only.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.build]
    sources = false
    ```

=== "ty.toml"

    ```toml
    [build]
    sources = false
    ```

---

### `version-from`

The module to read `__version__` from, when `[project]` declares
`dynamic = ["version"]`.

This is read when a wheel or a source distribution is built, not by the
checker: a version has to be settled before the packaging backend sees the
project, and the place it lives is a `.by` module that backend cannot
read.

The value is a path relative to the project root.

**Default value**: `null`

**Type**: `str`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.build]
    version-from = "src/app/__init__.by"
    ```

=== "ty.toml"

    ```toml
    [build]
    version-from = "src/app/__init__.by"
    ```

---

### `wheel-versions`

The python versions to build a wheel for, one wheel each.

`by build --wheels` builds one wheel per version listed and tags each for
the python it was lowered to, so an installer hands every interpreter the
best wheel it can use. A python with no wheel of its own takes the newest
one below it.

Defaults to every version from the one the project targets up to the
newest this release knows about — which is what `requires-python` already
says the project supports, so most projects need not set this. List them
explicitly to ship fewer.

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.build]
    wheel-versions = ["3.9", "3.13"]
    ```

=== "ty.toml"

    ```toml
    [build]
    wheel-versions = ["3.9", "3.13"]
    ```

---

## `editor`

### `common-aliases`

The modules a name is a common alias of, keyed by the alias.

A file that writes `np.` before importing anything almost always means numpy, because `np`
is what numpy is conventionally imported as. The editor completes such a name as the module
it names, and accepting one of those completions writes the `import numpy as np` that makes
the name real.

This adds aliases of your own to the ones ty already knows; an entry whose alias ty knows
replaces it. An alias for a module the project does not have is never offered, so an entry
for a module nobody installed costs nothing.

Defaults to `{}`, which leaves ty's own aliases as they are.

**Default value**: `{}`

**Type**: `dict[str, str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.editor.common-aliases]
    npt = "numpy.typing"
    ```

=== "ty.toml"

    ```toml
    [editor.common-aliases]
    npt = "numpy.typing"
    ```

---

## `environment`

### `extra-paths`

User-provided paths that should take first priority in module resolution.

This is an advanced option that should usually only be used for first-party or third-party
modules that are not installed into your Python environment in a conventional way.
Use the `python` option to specify the location of your Python environment.

This option is similar to mypy's `MYPYPATH` environment variable and pyright's `stubPath`
configuration setting.

**Default value**: `[]`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.environment]
    extra-paths = ["./shared/my-search-path"]
    ```

=== "ty.toml"

    ```toml
    [environment]
    extra-paths = ["./shared/my-search-path"]
    ```

---

### `python`

Path to your project's Python environment or interpreter.

ty uses the `site-packages` directory of your project's Python environment
to resolve third-party (and, in some cases, first-party) imports in your code.

This can be a path to:

- A Python interpreter, e.g. `.venv/bin/python3`
- A virtual environment directory, e.g. `.venv`
- A system Python [`sys.prefix`] directory, e.g. `/usr`

If you're using a project management tool such as uv, you should not generally need to
specify this option, as commands such as `uv run` will set the `VIRTUAL_ENV` environment
variable to point to your project's virtual environment. ty can also infer the location of
your environment from an activated Conda environment, and will look for a `.venv` directory
in the project root if none of the above apply. Failing that, ty will look for a `python3`
or `python` binary available in `PATH`.

[`sys.prefix`]: https://docs.python.org/3/library/sys.html#sys.prefix

**Default value**: `null`

**Type**: `str`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.environment]
    python = "./custom-venv-location/.venv"
    ```

=== "ty.toml"

    ```toml
    [environment]
    python = "./custom-venv-location/.venv"
    ```

---

### `python-platform`

Specifies the target platform that will be used to analyze the source code.
If specified, ty will understand conditions based on comparisons with `sys.platform`, such
as are commonly found in typeshed to reflect the differing contents of the standard library across platforms.
If `all` is specified, ty will assume that the source code can run on any platform.

If no platform is specified, ty will use the current platform:
- `win32` for Windows
- `darwin` for macOS
- `android` for Android
- `ios` for iOS
- `linux` for everything else

**Default value**: `<current-platform>`

**Type**: `"win32" | "darwin" | "android" | "ios" | "linux" | "all" | str`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.environment]
    # Tailor type stubs and conditionalized type definitions to windows.
    python-platform = "win32"
    ```

=== "ty.toml"

    ```toml
    [environment]
    # Tailor type stubs and conditionalized type definitions to windows.
    python-platform = "win32"
    ```

---

### `python-version`

Specifies the version of Python that will be used to analyze the source code.
The version should be specified as a string in the format `M.m` where `M` is the major version
and `m` is the minor (e.g. `"3.7"` or `"3.12"`).
If a version is provided, ty will generate errors if the source code makes use of language features
that are not supported in that version.

ty officially supports type checking code that targets Python 3.10 and later. Python 3.7
through 3.9 can still be selected, but ty may produce false positives or false negatives for
standard-library APIs because its bundled stubs do not fully describe those versions.

If a version is not specified, ty will try the following techniques in order of preference
to determine a value:
1. Check for the `project.requires-python` setting in a `pyproject.toml` file
   and use the minimum version from the specified range
2. Check for an activated or configured Python environment
   and attempt to infer the Python version of that environment
3. Fall back to the default value (see below)

For some language features, ty can also understand conditionals based on comparisons
with `sys.version_info`. These are commonly found in typeshed, for example,
to reflect the differing contents of the standard library across Python versions.

**Default value**: `"3.14"`

**Type**: `"3.7" | "3.8" | "3.9" | "3.10" | "3.11" | "3.12" | "3.13" | "3.14" | "3.15"`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.environment]
    python-version = "3.12"
    ```

=== "ty.toml"

    ```toml
    [environment]
    python-version = "3.12"
    ```

---

### `root`

The root paths of the project, used for finding first-party modules.

Accepts a list of directory paths searched in priority order (first has highest priority).

If left unspecified, ty will try to detect common project layouts and initialize `root` accordingly.
The project root (`.`) is always included. Additionally, the following directories are included
if they exist and are not packages (i.e. they do not contain `__init__.py` or `__init__.pyi` files):

* `./src`
* `./<project-name>` (if a `./<project-name>/<project-name>` directory exists)
* `./python`

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.environment]
    # Multiple directories (priority order)
    root = ["./src", "./lib", "./vendor"]
    ```

=== "ty.toml"

    ```toml
    [environment]
    # Multiple directories (priority order)
    root = ["./src", "./lib", "./vendor"]
    ```

---

### `typeshed`

Optional path to a "typeshed" directory on disk for us to use for standard-library types.
If this is not provided, we will fallback to our vendored typeshed stubs for the stdlib,
bundled as a zip file in the binary

**Default value**: `null`

**Type**: `str`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.environment]
    typeshed = "/path/to/custom/typeshed"
    ```

=== "ty.toml"

    ```toml
    [environment]
    typeshed = "/path/to/custom/typeshed"
    ```

---

## `overrides`

Configuration override that applies to specific files based on glob patterns.

An override allows you to apply different rule configurations to specific
files or directories. Multiple overrides can match the same file, with
later overrides take precedence. Override rules take precedence over global
rules for matching files.

For example, to relax enforcement of rules in test files:

```toml
[[tool.ty.overrides]]
include = ["tests/**", "**/test_*.py"]

[tool.ty.overrides.rules]
possibly-unresolved-reference = "warn"
```

Or, to ignore a rule in generated files but retain enforcement in an important file:

```toml
[[tool.ty.overrides]]
include = ["generated/**"]
exclude = ["generated/important.py"]

[tool.ty.overrides.rules]
possibly-unresolved-reference = "ignore"
```


### `exclude`

A list of file and directory patterns to exclude from this override.

Patterns follow a syntax similar to `.gitignore`.
Exclude patterns take precedence over include patterns within the same override.

If not specified, defaults to `[]` (excludes no files).

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [[tool.ty.overrides]]
    exclude = [
        "generated",
        "*.proto",
        "tests/fixtures/**",
        "!tests/fixtures/important.py"  # Include this one file
    ]
    ```

=== "ty.toml"

    ```toml
    [[overrides]]
    exclude = [
        "generated",
        "*.proto",
        "tests/fixtures/**",
        "!tests/fixtures/important.py"  # Include this one file
    ]
    ```

---

### `include`

A list of file and directory patterns to include for this override.

The `include` option follows a similar syntax to `.gitignore` but reversed:
Including a file or directory will make it so that it (and its contents)
are affected by this override.

If not specified, defaults to `["**"]` (matches all files).

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [[tool.ty.overrides]]
    include = [
        "src",
        "tests",
    ]
    ```

=== "ty.toml"

    ```toml
    [[overrides]]
    include = [
        "src",
        "tests",
    ]
    ```

---

### `rules`

Rule overrides for files matching the include/exclude patterns.

These rules will be merged with the global rules, with override rules
taking precedence for matching files. You can set rules to different
severity levels or disable them entirely.

**Default value**: `{...}`

**Type**: `dict[RuleName | "all", "ignore" | "warn" | "error"]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [[tool.ty.overrides]]
    include = ["src"]

    [tool.ty.overrides.rules]
    possibly-unresolved-reference = "ignore"
    ```

=== "ty.toml"

    ```toml
    [[overrides]]
    include = ["src"]

    [overrides.rules]
    possibly-unresolved-reference = "ignore"
    ```

---

## `overrides.analysis`

#### `allowed-unresolved-imports`

A list of module glob patterns for which `unresolved-import` diagnostics should be suppressed.

Details on supported glob patterns:
- `*` matches zero or more characters except `.`. For example, `foo.*` matches `foo.bar` but
  not `foo.bar.baz`; `foo*` matches `foo` and `foobar` but not `foo.bar` or `barfoo`; and `*foo`
  matches `foo` and `barfoo` but not `foo.bar` or `foobar`.
- `**` matches any number of module components (e.g., `foo.**` matches `foo`, `foo.bar`, etc.)
- Prefix a pattern with `!` to exclude matching modules

When multiple patterns match, later entries take precedence.

Glob patterns can be used in combinations with each other. For example, to suppress errors for
any module where the first component contains the substring `test`, use `*test*.**`.

**Default value**: `[]`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Suppress errors for all `test` modules except `test.foo`
    allowed-unresolved-imports = ["test.**", "!test.foo"]
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Suppress errors for all `test` modules except `test.foo`
    allowed-unresolved-imports = ["test.**", "!test.foo"]
    ```

---

#### `bivariant-private-attributes`

Whether a private attribute leaves an inferred type parameter bivariant. This is a
basedpython feature.

A private (single-underscore or name-mangled) member is invisible to external observers, so
it cannot be used to distinguish two specializations of its class, and therefore cannot
constrain the class's variance:

```python
class A[T]:
    _t: T
```

With this option enabled, `T` is inferred bivariant: nothing on `A`'s public surface
mentions `T`, so `A[int]` and `A[object]` are mutually assignable. As soon as a public
member mentions `T`, that member drives the inference as usual.

When set to `false`, a private attribute is instead treated as immutable-but-readable,
which constrains the type parameter to covariance.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Let private attributes constrain inferred variance to covariance
    bivariant-private-attributes = false
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Let private attributes constrain inferred variance to covariance
    bivariant-private-attributes = false
    ```

---

#### `block-scoped-declarations`

Whether a `let` or `var` declaration written inside a block binds its name for
that block only. This is a basedpython feature.

Python has no block scopes: a name bound anywhere in a function is a local of
that whole function, and the python a `.by` file lowers to keeps it that way. So
this is a rule the checker enforces rather than something the emitted code does:

```by
if flag:
    let a = 1

print(a)  # error: `a` is not in scope here
```

Only the binding keyword scopes a name to its block. A plain `a = 1` binds for
the whole enclosing function or module, as it does in python.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Let a `let` or `var` in a block be visible for the rest of the scope
    block-scoped-declarations = false
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Let a `let` or `var` in a block be visible for the rest of the scope
    block-scoped-declarations = false
    ```

---

#### `dependency-groups`

The requirement groups the matching files may import from.

`project` names `[project].dependencies`, an extra or a PEP 735 dependency group
is named by its own name, and `*` names every group.

When this is unset, a file may import from every group unless it is part of what
the project ships — the modules named by `shipped-modules` — in which case it may
import only `project` and the extras. Nothing the project ships can import a
dependency group, because nothing installs one alongside the project.

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    [[tool.ty.overrides]]
    include = ["tests/**"]

    [tool.ty.overrides.analysis]
    dependency-groups = ["project", "dev", "test"]
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    [[overrides]]
    include = ["tests/**"]

    [overrides.analysis]
    dependency-groups = ["project", "dev", "test"]
    ```

---

#### `disable-fluid-specializations`

Whether to disable "fluid specializations", a basedpython feature that widens the
inferred generic specialization of an unannotated binding flow-sensitively based on
its later uses in the same scope.

When set to `true`, each unannotated binding keeps the specialization it was inferred
with at its creation site; later uses no longer widen or lock it.

Defaults to `false`, and to `true` under the `ty-compatible` type checking preset.

**Default value**: `false`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Turn off fluid specializations
    disable-fluid-specializations = true
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Turn off fluid specializations
    disable-fluid-specializations = true
    ```

---

#### `exported-dependencies`

The dependencies this project hands to its own users.

A library whose interface is partly made of another distribution's types — one that
returns numpy arrays, or takes a pydantic model — can say so, and then a project
that depends on this one may import those distributions without declaring them
itself.

Only what the project already depends on can be exported, and the claim only
travels one link: exporting a distribution does not export whatever *it* depends
on, unless that distribution exports it in turn.

This is written into the `by.typed` marker when the project is built, because that
is what its users have — a `pyproject.toml` is not installed with the package.

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    exported-dependencies = ["numpy"]
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    exported-dependencies = ["numpy"]
    ```

---

#### `implicit-object-repr-exempt-types`

A list of classes never reported as an
[`implicit-object-repr`](rules.md#implicit-object-repr).

A class deriving from one of these is exempt too, so listing a base opts out a whole
hierarchy.

Entries are qualified class names (`decimal.Decimal`). A class in `builtins` may also be
spelled bare (`int`).

**Default value**: `[]`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Never report a bare `Thread` or `Lock`
    implicit-object-repr-exempt-types = ["threading.Thread", "threading.Lock"]
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Never report a bare `Thread` or `Lock`
    implicit-object-repr-exempt-types = ["threading.Thread", "threading.Lock"]
    ```

---

#### `implicit-object-repr-report-types`

A list of classes whose stub is taken at its word when looking for an
[`implicit-object-repr`](rules.md#implicit-object-repr).

A stub normally settles nothing, because it omits `__str__` and `__repr__` whether or not
the runtime class has them — `int` declares neither and still prints as a number. For a
class listed here the omission counts as real, the same way it would for a class written
in source, so a value of that class is reported unless the stub does declare one.

Defaults to the two whose bare repr is seen most often: `types.FunctionType`, which prints
`<function f at 0x...>`, and `builtins.type`, which prints `<class 'C'>`.

Entries are qualified class names (`decimal.Decimal`). A class in `builtins` may also be
spelled bare (`int`).

**Default value**: `["types.FunctionType", "builtins.type"]`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Also report a bare module object
    implicit-object-repr-report-types = ["types.FunctionType", "type", "types.ModuleType"]
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Also report a bare module object
    implicit-object-repr-report-types = ["types.FunctionType", "type", "types.ModuleType"]
    ```

---

#### `infer-unannotated-signatures`

Whether a function with no annotations is given the signature its body determines. This is
a basedpython feature.

Python's gradual guarantee makes an unannotated `def` say nothing: its parameters accept
anything and it returns `Unknown`. That is the largest remaining source of `Unknown` in an
otherwise typed project, and it silently swallows real mistakes. With this enabled, the
missing half of the signature is recovered from what the function itself already determines:

- **Each unannotated parameter** opens an anonymous type parameter named after it — the same
  hole `some` spells by hand — bounded by everything the function requires of it: the
  promoted type of its default, the members its body reads and calls, the parameters it is
  forwarded into, and any `assert` at the top of the body. Naming the hole is what keeps
  what a call passes in connected to what it gets back, so `def ident(x): return x` is
  inferred as the identity function.
- **A missing return type** is the union of what the body returns, plus `None` when control
  can also fall off the end. An empty body returns `None`, a body that always raises returns
  `Never`, and a generator returns a generator.

Nothing is invented from a use this analysis cannot read, so such a parameter stays gradual
and its body keeps type-checking exactly as it did. An explicit annotation always wins, and
so does anything an overload group or an overridden base method already supplies.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Leave an unannotated function gradual
    infer-unannotated-signatures = false
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Leave an unannotated function gradual
    infer-unannotated-signatures = false
    ```

---

#### `overlapping-condition-assume-truthy-instances`

Whether an instance with no `__bool__` and no `__len__` counts as always truthy when
looking for an [`overlapping-condition`](rules.md#overlapping-condition).

Such an instance is only *ambiguously* truthy — a subclass may define `__bool__` — so by
default it is a falsy member of `if not x` just as `None` is. Enabling this assumes the
class means what it looks like it means, which drops the reports for the very common
`if not x` over an optional instance.

Defaults to `false`.

**Default value**: `false`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # `if not x` over a `Foo | None` only selects `None`
    overlapping-condition-assume-truthy-instances = true
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # `if not x` over a `Foo | None` only selects `None`
    overlapping-condition-assume-truthy-instances = true
    ```

---

#### `overlapping-condition-exempt-types`

A list of classes whose values do not count as a distinct member of an
[`overlapping-condition`](rules.md#overlapping-condition).

`if not x` over an `int | None` selects both a falsy `int` and `None`, and is reported
because the branch cannot tell them apart. Listing `int` here says that conflating a falsy
`int` with anything else is fine, so only `None` is left and the condition is accepted.

Entries are qualified class names (`decimal.Decimal`). A class in `builtins` may also be
spelled bare (`int`), and `None` stands for the type of `None`.

**Default value**: `[]`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Accept a falsy `int` or `str` sharing a branch with another member
    overlapping-condition-exempt-types = ["int", "str"]
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Accept a falsy `int` or `str` sharing a branch with another member
    overlapping-condition-exempt-types = ["int", "str"]
    ```

---

#### `precise-unsolved-typevars`

Whether a type variable that a call leaves unsolved is solved to `Never`. This is a
basedpython feature.

A call can leave a type variable entirely unsolved, because no argument mentions it:

```python
def f[T]() -> T: ...

a = f()
```

`Never` is the precise answer here: no value ever reaches that position, so nothing the
call returns can be observed at type `T`. When set to `false`, the type variable falls back
to the gradual `Unknown` instead, which silences any error that would follow from the call
site.

This applies where the type variable is an output. Where it is instead written through or
passed back in — the element of an invariant `list[T]`, the parameter of a returned
`Callable[[T], R]` — `Never` would say that nothing can ever be put there, so an invariant
or contravariant occurrence keeps the gradual `Unknown`.

A PEP 696 default (`def f[T = str]()`) always takes priority, and a `ParamSpec`,
`TypeVarTuple` or keyword-variadic pack is unaffected because `Never` is not a valid
solution for one.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Solve an unsolved type variable to `Unknown` rather than `Never`
    precise-unsolved-typevars = false
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Solve an unsolved type variable to `Unknown` rather than `Never`
    precise-unsolved-typevars = false
    ```

---

#### `replace-imports-with-any`

A list of module glob patterns whose imports should be replaced with `typing.Any`.

Unlike `allowed-unresolved-imports`, this setting replaces the module's type information
with `typing.Any` even if the module can be resolved. Import diagnostics are
unconditionally suppressed for matching modules.

- Prefix a pattern with `!` to exclude matching modules

When multiple patterns match, later entries take precedence.

Glob patterns can be used in combinations with each other. For example, to suppress errors for
any module where the first component contains the substring `test`, use `*test*.**`.

When multiple patterns match, later entries take precedence.

**Default value**: `[]`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Replace all pandas and numpy imports with Any
    replace-imports-with-any = ["pandas.**", "numpy.**"]
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Replace all pandas and numpy imports with Any
    replace-imports-with-any = ["pandas.**", "numpy.**"]
    ```

---

#### `respect-type-ignore-comments`

Whether ty should respect `type: ignore` comments.

When set to `false`, `type: ignore` comments are treated like any other normal
comment and can't be used to suppress ty errors (you have to use `ty: ignore` instead).

Setting this option can be useful when using ty alongside other type checkers or when
you prefer using `ty: ignore` over `type: ignore`.

Defaults to `true`.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Disable support for `type: ignore` comments
    respect-type-ignore-comments = false
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Disable support for `type: ignore` comments
    respect-type-ignore-comments = false
    ```

---

#### `shipped-modules`

The top-level modules the project ships.

Defaults to the module named after `[project].name`: a project named `my-lib`
ships `my_lib`. Only a project that ships several unrelated modules, or one whose
module is not named after it, needs to say.

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    shipped-modules = ["foo", "foo_plugins"]
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    shipped-modules = ["foo", "foo_plugins"]
    ```

---

#### `sound-types`

Whether to infer sound (non-gradual) types wherever a precise type is available. This is a
basedpython feature.

Python's gradual guarantee requires a type checker to fall back to a gradual type whenever
an annotation is missing, even when a precise type could be inferred. In a fully typed
project that is pure boilerplate: it forces an annotation to be written for something the
checker already knows. When set to `true`, this option deliberately breaks the gradual
guarantee and uses the precise type instead. It affects:

- **Unannotated parameters**: each one opens an anonymous type parameter named after it,
  bounded by everything the function requires of it — the promoted type of its default, the
  members its body reads and calls, the parameters it is forwarded into, and any `assert` at
  the top of the body. So `def f(a=1)` rejects a `str` at a call site, and
  `def ident(x): return x` is inferred as the identity function. A lambda parameter with a
  default takes that default's promoted type directly.
- **Unannotated return types**: the union of what the body returns, plus `None` when control
  can fall off the end. An empty body returns `None` and a body that always raises returns
  `Never`; a generator returns a generator.
- **Unannotated methods that override a base method**: the parameter and return types are
  inherited from the overridden method, including from `Protocol` members and
  `abstractmethod` declarations.
- **Bare `ClassVar` annotations**: `x: ClassVar = 1` declares `int` rather than the union of
  `Unknown` and the inferred type.
- **Empty collection literals**: `[]` has element type `Never`, so passing one to a generic
  call solves from it precisely instead of leaking `Unknown`.

An explicit annotation always takes priority over any of the above.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Fall back to a gradual type wherever an annotation is missing
    sound-types = false
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Fall back to a gradual type wherever an annotation is missing
    sound-types = false
    ```

---

#### `strict-equality-semantics`

Configure ty's behavior regarding type inference and narrowing of equality
checks.

Defaults to `true`, and to `false` under the `ty-compatible` type checking preset.

With this option disabled, ty makes various assumptions about equality checks that
match the intuitions of most Python programmers, but may not be fully sound in all
situations. Leaving it enabled makes ty conservative about those assumptions, making it
less likely to infer `Literal[True]` or `Literal[False]` as the result of an
equality check. This has various effects on type checking, including fewer type
narrowing opportunities and more conservative assumptions regarding control flow.

One such unsound assumption is narrowing an object `x` of type `str` to `Literal["a"]`
after an `if x == "a"` check. This is unsound because a subclass of `str` with value
`"a"` will (by default) compare equal to `"a"`, but will not be of type `Literal["a"]`:

```pycon
>>> # `Literal["a"]` can only be inhabited by instances of exactly `str`, not
>>> # subclasses, but str subclasses compare equal by default:
>>> class StringSubclass(str): ...
...
>>> StringSubclass("a") == "a"
True
>>>
>>> # This also applies to `StrEnum`s:
>>> from enum import StrEnum
>>> class MyEnum(StrEnum):
...     A = "a"
...
>>> MyEnum.A == "a"
True
```

This option prevents the unsound narrowing of `x` to `Literal["a"]`, and instead keeps
it as `str`:

```python
from typing import Literal

def parse(value: str) -> Literal["a"] | None:
    # with `strict-equality-semantics` enabled, no narrowing will occur here,
    # and an error will be emitted on the `return` statement.
    if value == "a":
        return value
    return None
```

Another assumption ty makes by default is that subclasses will never override `__eq__` or
`__ne__`. This allows ty to narrow the following union based on an equality check, despite
the fact that an instance of a subclass of `Foo` could compare equal to `None`, and it's
perfectly valid to pass an instance of a subclass into the `x` parameter of this function:

```python
def narrow(x: Foo | None, other: Foo) -> None:
    if x == other:
        # with this option enabled, `x` still has type `Foo | None` here,
        # since it is legal to subclass `Foo` and override its `__eq__` method.
        reveal_type(x)
```

Many operations in Python implicitly call `__eq__` under the hood, and this option
impacts those too. For example, it also impacts narrowing from `in` checks, and narrowing
in `match` statements that use value patterns:

```python
def narrow_in(x: Foo | None, other: list[Foo]) -> None:
    if x in other:
        # with this option enabled, `x` still has type `Foo | None` here,
        # since the `in` operator implicitly calls `__eq__` on each element of `other`.
        reveal_type(x)


def narrow_match(x: str) -> None:
    match x:
        case "a":
            # with this option enabled, `x` still has type `str` here,
            # since this `case` branch will be taken by any object that compares
            # equal to `"a"`, including subclasses of `str`.
            reveal_type(x)
```

**Default value**: `false`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Preserve broad builtin types instead of narrowing them to literals
    strict-equality-semantics = true
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Preserve broad builtin types instead of narrowing them to literals
    strict-equality-semantics = true
    ```

---

#### `strict-float`

Whether `float` and `complex` annotations mean *only* themselves. This is a
basedpython feature.

The typing spec's special case says an `int` is acceptable wherever a `float` is
asked for, so `x: float` really declares `int | float`. A `.by` file opts out of
that already; this makes the same model available to a `.py` one, per module.

It is not only a checking question. The wider annotation is why a `.py`
`list[float]` cannot be laid out as an unboxed buffer and a `.py` class cannot
have `double` fields, so `by compile` reads this to choose a representation.

Defaults to `false`.

**Default value**: `false`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # `float` means float, so a numeric module compiles to unboxed doubles
    strict-float = true
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # `float` means float, so a numeric module compiles to unboxed doubles
    strict-float = true
    ```

---

#### `strict-generic-narrowing`

Whether ty should use strict narrowing for unspecialized generic classes in
`isinstance()` and `issubclass()` checks, as well as `match` class patterns.

When enabled, ty narrows to the top materialization of the class. For example,
`isinstance(value, list)` narrows a value of type `object` to `Top[list[Unknown]]`,
representing the (infinite) union of all possible `list` specializations. Iterating
over the list would yield values of type `object`.

When disabled, ty uses gradual generic narrowing, preserving compatible type
arguments from the original type where possible. For example,
`isinstance(value, list)` narrows a value of type `Sequence[int]` to `list[int]`.
If no specialization is available, the same check narrows a value of type `object`
to `list[Unknown]`; items of any type can then be appended to the list. Class
patterns such as `case list():` follow the same behavior.

Defaults to `false`.

**Default value**: `false`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.overrides.analysis]
    # Use the top materialization when narrowing to an unspecialized generic class
    strict-generic-narrowing = true
    ```

=== "ty.toml"

    ```toml
    [overrides.analysis]
    # Use the top materialization when narrowing to an unspecialized generic class
    strict-generic-narrowing = true
    ```

---

## `run`

### `main`

The module `by run` executes when no module is given on the command line.

This is the project's entry point: with it set, `by run` alone transpiles the project and
runs `python -m <main>`, exactly as if the module had been named on the command line. A
module named explicitly always wins.

The value is a module path, not a file path — `app.cli`, not `app/cli.by`.

Defaults to `null`, in which case `by run` requires a module argument.

**Default value**: `null`

**Type**: `str`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.run]
    main = "app.cli"
    ```

=== "ty.toml"

    ```toml
    [run]
    main = "app.cli"
    ```

---

## `src`

### `exclude`

A list of file and directory patterns to exclude from type checking.

Patterns follow a syntax similar to `.gitignore`:

- `./src/` matches only a directory
- `./src` matches both files and directories
- `src` matches files or directories named `src`
- `*` matches any (possibly empty) sequence of characters (except `/`).
- `**` matches zero or more path components.
  This sequence **must** form a single path component, so both `**a` and `b**` are invalid and will result in an error.
  A sequence of more than two consecutive `*` characters is also invalid.
- `?` matches any single character except `/`
- `[abc]` matches any character inside the brackets. Character sequences can also specify ranges of characters, as ordered by Unicode,
  so e.g. `[0-9]` specifies any character between `0` and `9` inclusive. An unclosed bracket is invalid.
- `!pattern` negates a pattern (undoes the exclusion of files that would otherwise be excluded)

All paths are anchored relative to the project root (`src` only
matches `<project_root>/src` and not `<project_root>/test/src`).
To exclude any directory or file named `src`, use `**/src` instead.

By default, ty excludes commonly ignored directories:

- `**/.bzr/`
- `**/.direnv/`
- `**/.eggs/`
- `**/.git/`
- `**/.git-rewrite/`
- `**/.hg/`
- `**/.mypy_cache/`
- `**/.nox/`
- `**/.pants.d/`
- `**/.pytype/`
- `**/.ruff_cache/`
- `**/.svn/`
- `**/.tox/`
- `**/.venv/`
- `**/__pypackages__/`
- `**/_build/`
- `**/buck-out/`
- `**/dist/`
- `**/node_modules/`
- `**/venv/`

You can override any default exclude by using a negated pattern. For example,
to re-include `dist` use `exclude = ["!dist"]`, or `exclude = ["!**/dist/"]` to
re-include every `dist` directory rather than only the one at the project root.

A negated pattern can only re-include something that is still walked, so it cannot
reach into a directory that is itself excluded. `exclude = ["!dist/generated.py"]`
re-includes nothing, because the walk stops at `dist`. Re-include the directory
first: `exclude = ["!**/dist/", "**/dist/**", "!**/dist/generated.py"]`

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.src]
    exclude = [
        "generated",
        "*.proto",
        "tests/fixtures/**",
        "!tests/fixtures/important.py"  # Include this one file
    ]
    ```

=== "ty.toml"

    ```toml
    [src]
    exclude = [
        "generated",
        "*.proto",
        "tests/fixtures/**",
        "!tests/fixtures/important.py"  # Include this one file
    ]
    ```

---

### `exclude-scripts`

Whether to exclude files containing PEP 723 inline script metadata unless they are
explicitly passed on the command line.

**Default value**: `false`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.src]
    exclude-scripts = true
    ```

=== "ty.toml"

    ```toml
    [src]
    exclude-scripts = true
    ```

---

### `include`

A list of files and directories to check. The `include` option
follows a similar syntax to `.gitignore` but reversed:
Including a file or directory will make it so that it (and its contents)
are type checked.

- `./src/` matches only a directory
- `./src` matches both files and directories
- `src` matches a file or directory named `src`
- `*` matches any (possibly empty) sequence of characters (except `/`).
- `**` matches zero or more path components.
  This sequence **must** form a single path component, so both `**a` and `b**` are invalid and will result in an error.
  A sequence of more than two consecutive `*` characters is also invalid.
- `?` matches any single character except `/`
- `[abc]` matches any character inside the brackets. Character sequences can also specify ranges of characters, as ordered by Unicode,
  so e.g. `[0-9]` specifies any character between `0` and `9` inclusive. An unclosed bracket is invalid.

All paths are anchored relative to the project root (`src` only
matches `<project_root>/src` and not `<project_root>/test/src`).

`exclude` takes precedence over `include`.

**Default value**: `null`

**Type**: `list[str]`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.src]
    include = [
        "src",
        "tests",
    ]
    ```

=== "ty.toml"

    ```toml
    [src]
    include = [
        "src",
        "tests",
    ]
    ```

---

### `respect-ignore-files`

Whether to automatically exclude files that are ignored by `.ignore`,
`.gitignore`, `.git/info/exclude`, and global `gitignore` files.
Enabled by default.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.src]
    respect-ignore-files = false
    ```

=== "ty.toml"

    ```toml
    [src]
    respect-ignore-files = false
    ```

---

## `terminal`

### `error-on-warning`

Use exit code 1, even if all diagnostics only had `warning` severity.

Defaults to `true`.

**Default value**: `true`

**Type**: `bool`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.terminal]
    # Exit with code 0 if all diagnostics had `warning` severity.
    error-on-warning = false
    ```

=== "ty.toml"

    ```toml
    [terminal]
    # Exit with code 0 if all diagnostics had `warning` severity.
    error-on-warning = false
    ```

---

### `output-format`

The format to use for printing diagnostic messages.

Defaults to `full`.

**Default value**: `full`

**Type**: `full | concise | github | gitlab | junit`

**Example usage**:

=== "pyproject.toml"

    ```toml
    [tool.ty.terminal]
    output-format = "concise"
    ```

=== "ty.toml"

    ```toml
    [terminal]
    output-format = "concise"
    ```

---


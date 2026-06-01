# modifiers and visibility

basedpython promotes commonly-used decorators and `typing` annotations into first-class
keyword modifiers on classes, functions, and assignments. the surface keywords replace
boilerplate decorator/annotation pairs at transpile time

## class modifiers

| basedpython             | Python output                                       |
| ----------------------- | --------------------------------------------------- |
| `final class Foo`       | `@final` + `class Foo`                              |
| `abstract class Foo`    | `class Foo` (keyword stripped, no decorator)        |
| `open class Foo`        | `class Foo` (keyword stripped, no decorator)        |
| `data class Foo`        | `@dataclass(slots=True)` + `class Foo`              |
| `frozen data class Foo` | `@dataclass(frozen=True, slots=True)` + `class Foo` |
| `protocol Foo`          | `class Foo(Protocol)` (base added)                  |
| `sealed class Foo`      | `class Foo` + `Foo.__sealed_members__ = (...)`      |

`abstract` is a marker for the type checker; it has no runtime decorator.
`open` is the inverse of `final` — a marker that the class is intended to be
subclassed. neither emits a runtime artefact

`sealed` declares a closed subclass hierarchy — see
[sealed classes](sealed-classes.md)

`enum class` is not a modifier but its own declaration form — see
[based enums](enums.md)

## function modifiers

| basedpython      | Python output           |
| ---------------- | ----------------------- |
| `final def m`    | `@final def m`          |
| `abstract def m` | `@abstractmethod def m` |
| `override def m` | `@override def m`       |
| `static def m`   | `@staticmethod def m`   |
| `class def m`    | `@classmethod def m`    |

`override` is sourced from `typing` on 3.12+ and `typing_extensions` below.
`abstract def` with no body is filled in with `: raise NotImplementedError`
instead of the usual `: ...`

## let / class-var / newtype

| basedpython            | Python output                          |
| ---------------------- | -------------------------------------- |
| `let MAX = 100`        | `MAX: Final = 100`                     |
| `class count = 0`      | `count: ClassVar = 0` (inside a class) |
| `newtype UserId = int` | `UserId = NewType("UserId", int)`      |

`let` works at module and class scope. inside a class, `class x = ...` is the
class-variable form (distinct from the regular `let x = ...` which is `Final`).
`newtype` introduces a distinct `typing.NewType`-backed type at module scope

## assignment modifiers

`override`, `final override`, and `abstract` may also appear on assignments
and annotated assignments. the modifier keyword is stripped at transpile time:

| basedpython            | Python output |
| ---------------------- | ------------- |
| `override x = 1`       | `x = 1`       |
| `final override x = 1` | `x = 1`       |
| `abstract x: T`        | `x: T`        |

these are compile-time-only markers — they constrain how the symbol is
checked but emit no runtime artefact

a bare `final x = 1` (with no `override`) is not an assignment modifier: it would
strip to a plain `x = 1` and declare nothing final. outside a class body ty
rejects it with `final-on-variable` and points you to `let`, which lowers to
`Final`. inside a class body it is a plain attribute, matching `let` there, and
is not flagged

## export / public / private

basedpython infers `__all__` from explicit visibility keywords:

```by
export def public_api(): ...
public def also_exported(): ...
private def helper(): ...
```

transpiles to:

```python
__all__ = ["public_api", "also_exported"]

def public_api(): ...
def also_exported(): ...
def _helper(): ...
```

- `export` and `public` are aliases. each marked symbol is added to a synthesized
    `__all__` list at module level
- `private` strips the keyword and renames the symbol with a leading underscore
    at the definition site *and* every same-module call site. it is excluded from
    `__all__` even when no `export`/`public` declarations exist
- visibility keywords are module-level only. inside a class they are stripped
    without renaming

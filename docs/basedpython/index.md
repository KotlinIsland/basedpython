# basedpython

<p class="by-tagline" markdown>
a python-like language that transpiles to pure python — sum types, extensions,
optional chaining and a type system that knows what your code means, lowered to
files any python tool can read
</p>

- **a python type checker with [framework support](frameworks/index.md)** —
    pydantic, sqlalchemy, pytest and django are modelled directly, so the magic
    they do at runtime checks like ordinary code
- **a build system** — write code against the latest version of python, and ship wheels that are compatible with old ones, no more waiting for 5 years to use something
- **basedpython, a python-like language that builds into python wheels**
- **compiles into high performance python extension modules**
- **a language server, formatter and linter** — high performance and feature rich tooling

<div class="by-actions" markdown>
[get started](getting-started.md){ .md-button .md-button--primary }
[browse the features](features/index.md){ .md-button }
</div>

```by
enum class Shape:
    case Circle(radius: int)
    case Rect(width: int, height: int)

    def area(self) -> int:
        return match self:
            case Shape.Circle(r):
                3 * r * r
            case Shape.Rect(w, h):
                w * h

extension list[Element: Shape]:
    def first_circle(self) -> Shape.Circle?:
        for shape in self:
            if shape is Shape.Circle:
                return shape
        return None

def stats(shapes: list[Shape]) -> (count: int, total: int):
    return (len(shapes), sum(s.area() for s in shapes))

def main():
    let shapes = [Shape.Circle(1), Shape.Rect(2, 3)]
    let summary = stats(shapes)
    print(f"{summary.count} shapes, {summary.total} total")
    print(shapes.first_circle()?.radius ?? 0)
```

## why

> Wouldn't it be nice if you could say `protocol C:` instead of having to write:
>
> ```python
> from typing import Protocol
>
> class C(Protocol):
> ```
>
> \- Guido van Rossum

Python and it's type system are held back due to an inability to make breaking changes and a
hesitation to introduce new syntax

other languages have indulged in modern features, powerful type systems, and integrated tooling.
we want to close that gap

## what you get

<div class="grid cards" markdown>

- :lucide-file-code-2:{ .lg .middle } **plain python out the other end**

    ______________________________________________________________________

    `by build` writes ordinary `.py` files. pytest, mypy, ruff and everything
    else in your stack keep working, because what they see is python

    [:octicons-arrow-right-24: how transpilation works](development/how-transpilation-works.md)

- :lucide-shapes:{ .lg .middle } **syntax python doesn't have**

    ______________________________________________________________________

    sum types with payloads, extension methods, destructuring, trailing lambda
    blocks, `?.`, `??`, and properties that read like declarations

    [:octicons-arrow-right-24: the language reference](features/index.md)

- :lucide-shield-check:{ .lg .middle } **a type system that keeps up**

    ______________________________________________________________________

    intersections, negations, match types, symbolic arithmetic in type
    parameters, and inference that narrows instead of shrugging

    [:octicons-arrow-right-24: type system features](features/index.md#type-system)

- :lucide-blocks:{ .lg .middle } **frameworks understood, not tolerated**

    ______________________________________________________________________

    pydantic, sqlalchemy, pytest and django are modelled directly, so
    synthesized constructors and injected fixtures check like real code

    [:octicons-arrow-right-24: framework support](frameworks/index.md)

</div>

## where to go

<div class="grid cards" markdown>

- :lucide-rocket:{ .lg .middle } **[getting started](getting-started.md)**

    ______________________________________________________________________

    install, your first `.by` file, project layout, and wiring `by build` into
    CI

- :lucide-book-open:{ .lg .middle } **[features](features/index.md)**

    ______________________________________________________________________

    the full language reference, one page per feature

- :lucide-settings:{ .lg .middle } **[configuration](configuration.md)**

    ______________________________________________________________________

    where settings live and how they resolve

- :lucide-package:{ .lg .middle } **[framework support](frameworks/index.md)**

    ______________________________________________________________________

    what basedpython knows about pydantic, sqlalchemy, pytest and django

- :lucide-triangle-alert:{ .lg .middle } **[differences from python](features/differences-from-python.md)**

    ______________________________________________________________________

    every place the same source means something different in a `.by` file

- :lucide-terminal:{ .lg .middle } **[`by` CLI reference](cli-reference.md)**

    ______________________________________________________________________

    every command and flag the `by` driver adds

</div>

[credits](credits.md) for contributors and the
third-party work basedpython relies on

# basedpython

<p class="by-tagline" markdown>
a python-like language that transpiles to pure python — sum types, extensions,
optional chaining and a type system that knows what your code means, lowered to
files any python tool can read
</p>

- **a python type checker with [framework support](frameworks/index.md)** —
    pydantic, sqlalchemy, pytest and django are modelled directly, so the magic
    they do at runtime checks like ordinary code
- **polyfill and transpilation** — write code against the latest version of python, and ship wheels that are compatible with old ones, no more waiting for 5 years to use new features
- **basedpython-language** — based on Python, powerful and modern. fully backwards compatible
- **compiles into high performance python [extension modules](features/compilation.md)** — or ordinary Python
- **a language server, formatter and linter** — high performance and feature rich tooling

<div class="by-actions" markdown>
[get started](getting-started.md){ .md-button .md-button--primary }
[browse the features](features/index.md){ .md-button }
</div>

=== "shapes.by"

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

=== "shapes.py, lowered for python 3.12"

    ```python
    from __future__ import annotations
    from typing import Final, NamedTuple
    def _soundness_check(_v, _t):
        if not isinstance(_v, _t):
            raise TypeError(
                f"type soundness violation: expected {getattr(_t, '__name__', _t)}, "
                f"got {type(_v).__name__}"
            )
        return _v

    def _soundness_iter(_it, _t):
        for _x in _it:
            yield _soundness_check(_x, _t)

    def _by_no_case_matched():
        # a statement expression's `match` the checker found exhaustive, reached with a value it
        # ruled out. nothing else would say so: the name the value was to be read from is just unbound
        raise TypeError("no case of this `match` matched, though the checker found it exhaustive")

    class _AnonNamedTuple_4d75d31a(NamedTuple):
        count: int
        total: int

    from dataclasses import dataclass
    from typing import final, ClassVar
    def _by_ext__list__first_circle(self):  # basedpython: extension method list[Element: Shape]
            for shape in self:
                if isinstance(shape, _Shape_Circle):
                    return shape
            return None
    class Shape:
        Circle: ClassVar[type[_Shape_Circle]]
        Rect: ClassVar[type[_Shape_Rect]]
        def area(self) -> int:
            match self:
                case Shape.Circle(r):
                    __by_stmt_expr_0__ = 3 * r * r
                case Shape.Rect(w, h):
                    __by_stmt_expr_0__ = w * h
                case _:
                    _by_no_case_matched()
            return __by_stmt_expr_0__

    @final
    @dataclass(frozen=True, slots=True)
    class _Shape_Circle(Shape):
        radius: int
    _Shape_Circle.__name__ = "Circle"
    _Shape_Circle.__qualname__ = Shape.__qualname__ + ".Circle"
    Shape.Circle = _Shape_Circle

    @final
    @dataclass(frozen=True, slots=True)
    class _Shape_Rect(Shape):
        width: int
        height: int
    _Shape_Rect.__name__ = "Rect"
    _Shape_Rect.__qualname__ = Shape.__qualname__ + ".Rect"
    Shape.Rect = _Shape_Rect


    def stats(shapes: list[Shape]) -> _AnonNamedTuple_4d75d31a:
        return _AnonNamedTuple_4d75d31a(len(shapes), _soundness_check(sum(s.area() for s in _soundness_iter(shapes, Shape)), int))

    def main():
        shapes: Final = [Shape.Circle(1), Shape.Rect(2, 3)]
        summary: Final = stats(shapes)
        print(f"{summary.count} shapes, {summary.total} total")
        print(__by_t_0__ if (__by_t_0__ := (None if (__by_t_0__ := _soundness_check(_by_ext__list__first_circle(shapes), (_Shape_Circle, type(None)))) is None else __by_t_0__.radius)) is not None else 0)
    if __name__ == "__main__":
        main()
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

Python and its type system are held back due to an inability to make breaking changes and a
hesitation to introduce new syntax

other languages have indulged in modern features, powerful type systems, and integrated tooling.
we want to close that gap

## what you get

<div class="grid cards" markdown>

- :lucide-file-code-2:{ .lg .middle } **plain python out the other end**

    ______________________________________________________________________

    `by build` writes ordinary `.py` files. pytest, mypy, ruff and everything
    else in your stack keep working, because what they see is python

    [:octicons-arrow-right-24: how to build](cli-reference.md)

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

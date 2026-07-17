# basedpython

a python type checker and a python-like language that transpiles to pure python

```by
enum class Shape:
    case Circle(radius: int)
    case Rect(width: int, height: int)

    def area(self) -> int:
        match self:
            case Shape.Circle(r): return 3 * r * r
            case Shape.Rect(w, h): return w * h

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

## installation

```sh
uv add --dev basedpython
by run main
```

## documentation

[kotlinisland.github.io/basedpython](https://kotlinisland.github.io/basedpython/)

## acknowledgements

basedpython is a fork of [astral-sh/ruff](https://github.com/astral-sh/ruff) —
it reuses ruff's parser, AST, and fix-application machinery, and the type
checker is built on [ty](https://github.com/astral-sh/ty). none of this would
exist without the work of the astral team and the wider ruff community

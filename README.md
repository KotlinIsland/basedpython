# basedpython

a python type checker and a python-like language that transpiles to pure python

- **a python type checker with framework support** — pydantic, sqlalchemy,
    pytest and django are modelled directly, so the magic they do at runtime
    checks like ordinary code
- **basedpython, a python-like language that builds into python wheels**
- **compiles into high performance python extension modules**
- **a language server, formatter and linter** — `by server` drives the editor,
    and `buff` is the basedpython build of ruff

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

## installation

```sh
uv add --dev basedpython
echo 'print("hello")' > main.by
by run main
```

## documentation

[kotlinisland.github.io/basedpython](https://kotlinisland.github.io/basedpython/)

## acknowledgements

basedpython is built on top of [astral-sh/ruff](https://github.com/astral-sh/ruff). none of this would
exist without the work of the astral team and the wider ruff community

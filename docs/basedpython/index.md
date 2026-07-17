# basedpython

a python-like language that transpiles to pure python

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

## contents

- [getting started](getting-started.md) — install, your first file, project layout
- [features](features/index.md) — the full language reference
- [`by` cli reference](cli-reference.md) — commands and flags

## development

- [how transpilation works](development/how-transpilation-works.md)
- [reverse transforms](development/reverse-transforms.md)
- [sourcemaps](development/sourcemaps.md)
- [typeshed patches](development/typeshed-patches.md)

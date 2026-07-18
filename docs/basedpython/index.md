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

type-checking improvements with no new syntax — they work in `.by` and `.py` files alike

- [fluid specializations](features/fluid-specializations.md)

## basedpython language features

- [tuple type literals](features/tuple-types.md)
- [callable arrow syntax](features/callable.md)
- [intersection types](features/intersection.md)
- [`or` / `and` type operators](features/or-and-types.md)
- [negation types (`not T`)](features/not-type.md)
- [`typeof` keyword](features/typeof.md)
- [star projections (`X[*]`)](features/star-projection.md)
- [strict `float` and `complex`](features/no-number-promotions.md)
- [infinity and nan float literals](features/float-literals.md)
- [literal type promotion](features/literal-types.md)
- [grapheme strings (`Character`, `.character_count`)](features/character.md)
- [symbolic operations in types](features/symbolic-type-ops.md)
- [typed dict literals](features/typed-dict-literal.md)
- [anonymous named tuple types](features/anonymous-named-tuple.md)
- [explicit typevar constraints](features/constraints.md)
- [typevar variance keywords](features/variance.md)
- [safe variance](features/safe-variance.md)
- [overlapping](features/overlapping.md)
- [explicit generic call sites](features/generic-calls.md)
- [reified type parameters](features/reified-generics.md)
- [type reification](features/type-reification.md)
- [parametric type tests](features/parametric-type-tests.md)
- [automatic forward references](features/forward-references.md)
- [implicit typing imports](features/implicit-typing.md)
- [typed lambda](features/typed-lambda.md)
- [implicit overload stubs](features/overloads.md)
- [decorator keyword](features/decorator-keyword.md)
- [type narrowing predicates](features/type-is.md)
- [generics](features/generics.md)
- [runtime type-soundness checks](features/soundness.md)

## syntax extensions

- [modifiers and visibility](features/modifiers.md)
- [based enums (`enum class`)](features/enums.md)
- [sealed classes](features/sealed-classes.md)
- [init method shorthand](features/init-method.md)
- [empty declarations](features/empty-declarations.md)
- [main function](features/main-function.md)
- [identity and isinstance (`===` / `!==` / `is`)](features/identity-swap.md)
- [optional chaining (`?.`)](features/optional-chaining.md)
- [none-coalesce operator (`??`)](features/none-coalesce.md)
- [postfix await (`.await`)](features/await-attribute.md)
- [mutable default arguments](features/mutable-defaults.md)
- [dedented triple-quoted strings](features/dedent-strings.md)
- [custom string tags](features/string-tags.md)
- [extensions](features/extensions.md)
- [tuple member access (`expr.N`)](features/tuple-index.md)
- [keyword arguments in subscripts](features/kw-subscript.md)
- [unpack syntax](features/unpack-syntax.md)
- [super keyword](features/super.md)
- [`cast` keyword](features/cast.md)
- [checked & safe casts (`cast` / `cast?`)](features/checked-cast.md)
- [`sentinel` declarations](features/sentinel.md)
- [lazy imports](features/lazy-imports.md)
- [repeated `_` parameters](features/repeated-underscore.md)

## planned

- [destructuring with `if let`](features/if-let.md)

## framework support

- [architecture](frameworks/index.md)
- [pydantic](frameworks/pydantic.md)
- [sqlalchemy](frameworks/sqlalchemy.md)
- [pytest](frameworks/pytest.md)
- [django](frameworks/django.md)

## development

- [how transpilation works](development/how-transpilation-works.md)
- [reverse transforms](development/reverse-transforms.md)
- [sourcemaps](development/sourcemaps.md)
- [typeshed patches](development/typeshed-patches.md)

## acknowledgements

- [third-party work basedpython relies on](acknowledgements.md)

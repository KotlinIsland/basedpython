# no special cases for `float` and `complex`

python's typing spec [special-cases][spec] `float` to mean `int | float` and
`complex` to mean `int | float | complex`. basedpython does not. in a `.by`
file, `float` is just `float` and `complex` is just `complex`

```by
def takes(x: float) -> None: ...

takes(1.0)
takes(1)   # rejected: `float` does not include `int`
```

## scope

the rewrite fires only in type-expression positions:

- variable annotations
- function parameter and return annotations
- type alias right-hand sides
- typevar bound and default expressions
- recursive into generic subscripts (`list[float]`, `dict[str, float]`)
- recursive into the first argument of `Annotated[…]`

the rewrite composes with the other type-position transforms — a `float` arm
inside a union, intersection, negation, or callable type is wrapped just like a
bare one:

```by
a: A & float        # → Intersection[A, JustFloat]
b: float | None     # → JustFloat | None
c: not float        # → Not[JustFloat]
d: (float) -> int   # → Callable[[JustFloat], int]
```

literal-value positions inside `Literal[…]` are not type expressions and are
left alone

value-position uses of `float` / `complex` (calls like `float(x)`,
`isinstance(y, float)`) are left alone — they refer to the class object, not
to the type

## interop with `.py`

a `.py` file imported into a `.by` file keeps python's typing-spec meaning of
`float` / `complex`. the strict basedpython meaning only applies inside `.by`
files; consumers reading the transpiled `.py` output see the strict types too

[spec]: https://typing.readthedocs.io/en/latest/spec/special-types.html#special-cases-for-float-and-complex

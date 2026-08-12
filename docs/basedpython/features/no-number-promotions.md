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

## asking for it in a `.py` file

a `.py` module can opt out of the promotion too, per module:

```toml
[tool.ty.analysis]
strict-float = true
```

it applies to `float` *and* `complex`, and it takes the ordinary `[[overrides]]`
matching, so one numeric package can be strict while the rest of a codebase is not

this is a **checking** change first. `f(3)` where `f` takes a `float` becomes an
error the checker reports, rather than a value silently converted:

```python
def scale(x: float) -> float:
    return x * 2.0

scale(1.5)
scale(1)  # error: [invalid-argument-type]
```

### why a compiler cares

the promotion is a rule about what a position *accepts*, but a compiler has to
choose a **representation**, and it cannot lay out something it can only describe as
`int | float`. a field has to be a `double` or a `PyObject *`; a list element has to
be unboxed or boxed. so `x: float` meaning `int | float` is the difference between a
struct member and a pointer, and between an unboxed buffer and a list of boxed
floats

`by compile` reads the same setting. on the compiler's own benchmarks, turning it on
took an object-heavy loop from 17.7ms to 3.4ms — past mypyc, which reaches similar
speed by converting the `int` silently and diverging from python instead

the setting is also read where a type is *inferred* rather than annotated, so an
attribute assigned from a strict parameter stays strict:

```python
class Vec:
    def __init__(self, x: float, y: float) -> None:
        self.x = x  # `float`, so the field is a double
```

## inlay hints

in a `.py` file, where the promotion does apply, the extra arms are shown as an
inlay hint so the widening is visible at the annotation:

```python
def f(x: float⟨ | int⟩, y: complex⟨ | float | int⟩): ...
```

a `.by` file promotes nothing, so nothing is hinted there

[spec]: https://typing.readthedocs.io/en/latest/spec/special-types.html#special-cases-for-float-and-complex

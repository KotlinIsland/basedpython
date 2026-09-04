# literal type promotion

basedpython promotes bare literal values to `typing.Literal` in type positions,
so the `Literal[...]` boilerplate is unnecessary:

```by
mode: 1 | 2 | 3
status: "ok" | "error"
flag: True
result: 1 | 2 | int
```

transpiles to:

```python
mode: Literal[1, 2, 3]
status: Literal["ok", "error"]
flag: Literal[True]
result: Literal[1, 2] | int
```

## promoted forms

the following literal forms are recognized in type contexts:

- integer literals: `1`, `-5`, `0xff`
- string literals: `"ok"`, `b"bytes"`
- booleans: `True`, `False`
- float and complex literals: `1.5`, `3.14j`

## float and complex literals

python has no `Literal[...]` for a float or a complex: PEP 586 admits only
`None`, `int`, `bool`, `str`, `bytes` and enum members, and every checker
enforces it. So a float literal type is written as the type it is one of, and
the precision is lost at the boundary:

```by
ratio: 1.5
scale: int | 2.5
```

→

```python
ratio: float
scale: int | float
```

a project that would rather keep the precision than keep the output checkable
can ask for the literal instead:

```toml
[tool.ty.lowering]
float-literals = "literal"
```

→

```python
ratio: Literal[1.5]
scale: int | Literal[2.5]
```

`typing` does not check what it is handed, so this runs — but a checker reading
the output reports the argument as invalid

## scope

promotion fires only in syntactic type contexts: annotations, return types,
type aliases, and generic subscript slices whose target is a type. value
expressions are untouched. `Annotated[T, metadata]` does not treat its
metadata slice as a type context, so literal metadata is preserved

## polyfill

`Literal` is imported from `typing` exactly once per module that uses
promoted literal types

## inlay hints

an inlay hint is read as source, so types in a `.by` file are spelled the way
that file is written — a promoted literal shows bare, not wrapped:

```by
a⟨: 1⟩ = 1
```

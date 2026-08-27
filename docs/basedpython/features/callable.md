# callable arrow syntax

basedpython adds an arrow form for callable types so function-typed
annotations read naturally:

```by
on_done: (int, str) -> bool
fetch: () -> bytes
curry: (int) -> (str) -> bool
```

transpiles to:

```python
on_done: Callable[[int, str], bool]
fetch: Callable[[], bytes]
curry: Callable[[int], Callable[[str], bool]]
```

## syntax

- `->` separates parameters from return type
- form nests: right side can itself be callable, producing curried
    `Callable[[A], Callable[[B], C]]`
- bare positional types (`(int, str)`) get implicit positional-only treatment
    when followed by named parameters

## parameter forms

### gradual `(...)`

a single bare ellipsis parameter list is the gradual "any arguments"
callable:

```by
f: (...) -> int
```

→ `Callable[..., int]` (not `Callable[[...], int]`). it accepts any
argument list, and the reverse transpile maps `Callable[..., R]` back to
`(...) -> R`

### bare positional

```by
f: (int, str) -> bool
```

→ `Callable[[int, str], bool]`

### named parameter

```by
f: (a: int, b: str) -> bool
```

named parameters are non-denotable in `Callable[...]`; basedpython emits an equivalent shape behind the scenes

### positional-only marker `/`

```by
f: (int, /, name: str) -> bool
```

explicit `/` marks preceding params positional-only. implicit `/` is
inserted after the last bare positional when followed by a named
parameter

### keyword-only marker `*`

```by
f: (int, *, name: str) -> bool
```

bare `*` marks following params keyword-only

### variadic `*args`

```by
f: (*: int) -> int             # anonymous
g: (*args: int) -> int         # named
```

a variadic's type may itself be an unpack, which types the positional
parameters as the tuple it names rather than typing each one:

```by
type Args = (int, str)

f: (*: *Args) -> None          # anonymous
g: (*args: *Args) -> None      # named
```

### kwargs `**kwargs`

```by
f: (**: str) -> int            # anonymous
g: (**kwargs: str) -> int      # named
```

### full form

```by
f: (int, /, name: str, *args: bool, **kwargs: int) -> None
```

### receiver

a type before the parameter list is an [implicit receiver](implicit-receivers.md)
— `int.() -> str` is a callable that runs against an `int`, which it takes as its
leading positional parameter

### borrowed parameter

```by
f: (local int) -> None
g: (once cb: (int) -> None) -> None
```

a parameter may carry a [`local` / `once`](local-lifetimes.md) modifier, which
constrains whatever *implements* the callable: the value passed in does not
outlive the call, so the body filling it may not let it escape. the modifier is
erased in lowering — both of the above are ordinary `Callable`s at runtime — and
does not affect assignability

## scope

arrow form recognized only in syntactic type positions: parameter
annotations, return-type annotations, `AnnAssign` targets, type aliases,
and subscript slices that are themselves type contexts
(`list[(int) -> int]`). value-context `(int) -> int` remains a parse
error

## nesting

callable nests inside callable, union, intersection, tuple, subscript:

```by
a: (int) -> int | None
b: list[(int) -> int]
c: (int) -> (str) -> bool
d: ((int) -> int) & Hashable
```

## polyfill

- `Callable` imported from `typing` once per module using denotable form
- `Protocol` imported from `typing` once per module using non-denotable
    form

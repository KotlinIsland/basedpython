# explicit generic call sites

PEP 695 lets you declare a generic function `def f[T](x: T) -> T`, but the
call site can only ever specialize the type variable through inference from
the arguments. basedpython adds an explicit specialization syntax for
function calls:

```by
def identity[T](x: T) -> T:
    return x

identity[int](1)
pair[int, str](1, "a")
```

transpiles to:

```python
def identity[T](x: T) -> T:
    return x

identity(1)
pair(1, "a")
```

the type arguments are stripped — they exist purely for the type checker.
ty sees the explicit specialization through the AST and uses it to constrain
inference; the runtime call has no `[...]` syntax (which would be a parse
error in standard Python)

## scope

the call-site `[T]` is stripped only when the subscript target is a function
defined in the local typing context. constructor calls — `Foo[int](...)` —
are *not* stripped, because Python supports them natively as
`__class_getitem__` then `__call__`:

```by
class Box[T]:
    ...

Box[int](42)   # unchanged — runtime parametrized constructor
```

## limitations

only locally-defined function targets are recognized. for cross-module
generic calls, prefer the inference path (`identity(x)`)

## inlay hints

a generic function call with no explicit specialization gets the type arguments
ty inferred as an inlay hint, written where the `[...]` would go:

```by
def identity[T](x: T) -> T: ...

identity⟨[int]⟩(x)
```

constructor calls are not hinted — a generic constructor's specialization is
already the type of the value it produces

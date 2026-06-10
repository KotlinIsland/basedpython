# basedpython: self-references in class bases

basedpython auto-quotes class name self-references that appear inside subscript slices of base
classes (`class A(list[A])` lowers to `class A(list["A"])`). ty mirrors that runtime semantic and
resolves the self-reference as a forward reference, instead of reporting `unresolved-reference`

## subscript-slice self-reference resolves

```by
class A(list[A]):
    pass

a = A()
a.append(a)
reveal_type(a[0])  # revealed: A
```

## self-reference inside a union slice resolves

```by
class A(list[A | None]):
    pass

a = A()
a.append(None)
reveal_type(a[0])  # revealed: A | None
```

## nested subscript self-reference resolves

```by
class A(dict[str, list[A]]):
    pass

a = A()
a["k"] = []
reveal_type(a["k"])  # revealed: list[A]
```

## direct base self-reference is still an error

a bare self-reference as a direct base is a genuine runtime error; the transpiler doesn't auto-quote
it, and ty reports the cyclic inheritance

```by
class A(A):  # error: [cyclic-class-definition]
    pass
```

## unrelated unresolved name is still flagged

```by
class A(list[B]):  # error: [unresolved-reference]
    pass
```

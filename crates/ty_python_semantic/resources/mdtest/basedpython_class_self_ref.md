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

a bare base is resolved where it is written, since the class is built on the object itself, so the
`A` not yet bound there is an unresolved reference, as python's `NameError` for it shows. a type
argument inside a base (`class A(list[A])`) is a forward reference the transpiler quotes

```by
class A(A):  # error: [unresolved-reference]
    pass
```

## unrelated unresolved name is still flagged

```by
class A(list[B]):  # error: [unresolved-reference]
    pass
```

## a cycle across modules is a cyclic definition

each base is bound where it is written, by an import of the other module, so the cycle is between
two classes rather than a name read before it is bound

`cycle_a.by`:

```by
from cycle_b import B

class A(B):  # error: [cyclic-class-definition]
    pass
```

`cycle_b.by`:

```by
from cycle_a import A

class B(A):  # error: [cyclic-class-definition]
    pass
```

## a cycle in a stub is a cyclic definition

a stub is never run, so its bases are read as deferred and may name a class defined further down

`cycle_stub.byi`:

```byi
class C(D): ...  # error: [cyclic-class-definition]
class D(C): ...  # error: [cyclic-class-definition]
```

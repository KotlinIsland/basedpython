# modules checked against their stubs

a module with a stub beside it (`.pyi` or `.byi`, a `foo-stubs` package, or the
standard library's own stubs) is typed by the stub wherever it is imported, so the
stub is what the module promises its users. basedpython holds the module's own
body to that promise, in `.py` and `.by` files alike

## what the stub declares, the body is checked against

a parameter, a return type, a module variable or a class attribute the module
leaves unannotated takes the type the stub declares for it:

```python
# shapes.pyi
class Square:
    side: int
    def scale(self, factor: int) -> "Square": ...

def area(side: int) -> int: ...
```

```python
# shapes.py
def area(side):
    reveal_type(side)  # int
    return str(side * side)  # error: `str` is not `int`

class Square:
    def __init__(self):
        self.side = "1"  # error: `str` is not `int`

    def scale(self, factor):
        reveal_type(factor)  # int
        square = Square()
        square.side = self.side * factor
        return square
```

- a function the stub overloads is the implementation of every overload, as an
    implementation written beside its overloads is: a parameter takes the union of
    what the overloads say, and so does the return type
- of a `sys.version_info` or `sys.platform` branch in the stub, the one that runs
    is read
- an annotation the module writes is what its body is checked against. where it
    disagrees with the stub, that is reported as below
- a type the stub writes in terms only the stub has is not carried over: a class
    the stub defines is not the module's class of the same name, and a type variable
    it binds belongs to the stub's declaration. above, `scale` returns the stub's
    `Square`, so the module's `scale` is left to its own body

importers see the stub as they always did; nothing here changes what they read

the editor's `stubTypes` hints show the stub's annotations where the module leaves
them out, and say exactly what the checker reads there

## where the module and its stub disagree

| rule                          | default | reports                                                                                               |
| ----------------------------- | ------- | ----------------------------------------------------------------------------------------------------- |
| `missing-runtime-member`      | warn    | a member the stub declares and the module (or class) lacks                                            |
| `incompatible-stub-signature` | error   | a call the stub accepts that the function can't take, or an annotation that disagrees with the stub's |
| `incompatible-stub-attribute` | error   | a variable annotated with another type than the stub's                                                |
| `incompatible-stub-bases`     | error   | a class that doesn't inherit from a base the stub lists                                               |
| `missing-stub-member`         | off     | a public member of the module the stub leaves out                                                     |

a member the stub declares only for a type checker is not expected of the module:
private names, anything `@type_check_only`, type variables, type aliases,
`__all__`, and a `__getattr__` that says the stub lists only some attributes. nor is anything a module- or class-level `__getattr__` could supply, or
a member a class inherits. `missing-runtime-member` offers to write the member in
as the stub declares it

`missing-stub-member` is off because a stub often describes only the supported
part of a module; turn it on for a module whose stub is meant to be complete

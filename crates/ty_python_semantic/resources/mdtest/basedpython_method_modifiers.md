# basedpython: method modifiers

`class def`, `static def` and `override def` say how a class dispatches one of its members, or that
the member replaces one it inherits. A `def` that no class body owns is not a member of anything, so
there is nothing for them to say about it.

```toml
[environment]
python-version = "3.12"
```

## a method modifier needs a class body

```by
class def make()  # error: [invalid-syntax] "`class` is only a modifier on a method"

static def helper()  # error: [invalid-syntax] "`static` is only a modifier on a method"

override def replace()  # error: [invalid-syntax] "`override` is only a modifier on a method"
```

## a function nested in a method is not itself a method

the class owns the method, not the functions the method makes

```by
class A:
    def run(self):
        static def inner()  # error: [invalid-syntax] "`static` is only a modifier on a method"
```

## a modifier that reads on a class too is left alone

`abstract` and the visibility keywords modify a `def` wherever it is written, so a module-level
function may carry them

```by
abstract def f()

private def g() -> str:
    return "g"
```

## inside a class body each modifier is what it says

```by
class A:
    class def make(cls) -> A:
        return cls()

    static def helper(x: int) -> int:
        return x

    def described(self) -> str:
        return "A"

class B(A):
    override def described(self) -> str:
        return "B"

reveal_type(A.make())  # revealed: A
reveal_type(A.helper(1))  # revealed: int
```

## a method's own type parameters leave it a method

a generic method opens a scope for its type parameters, which sits between the method and the class
body that owns it

```by
class A:
    class def of[T](cls, x: T) -> T:
        return x

reveal_type(A.of(1))  # revealed: 1
```

## an extension body owns its members too

an `extension` declares members of the type it extends, so it is a class body like any other

```by
extension str:
    static def joined(parts: list[str]) -> str:
        return "".join(parts)

reveal_type(str.joined(["a"]))  # revealed: str
```

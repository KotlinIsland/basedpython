# based enums

basedpython "based enums" are algebraic sum types, declared `enum class E:` with `case` variant
declarations (one `case` line may declare several comma-separated variants). variants are reached
**qualified** through the enum name (`Shape.Circle`, `Color.Red`), the same way Python enum members
are. ty models them as a closed set of variants:

- a payload-bearing variant (`Circle(radius: float)`) is a frozen-dataclass subclass of the enum —
    `Shape.Circle(2.0)` constructs it and field access is typed
- a payload-less variant is a singleton *value*, not a class — `Shape.Empty` is the value itself,
    matched `case Shape.Empty:`
- the enum name denotes the **union** of its variants (`Shape` ≡
    `Shape.Circle | Shape.Square |   Shape.Empty`), so annotations, assignability, `match`
    narrowing, and exhaustiveness all work
- an enum whose variants are *all* payload-less lowers to an idiomatic `Enum` (`Color.Red` is an
    enum literal)

## variants are reached through the enum name and construct

```by
enum class Shape:
    case Circle(radius: float)
    case Square(side: float)
    case Empty

reveal_type(Shape.Circle)  # revealed: <class 'Circle'>
c = Shape.Circle(2.0)
reveal_type(c)  # revealed: Circle
reveal_type(c.radius)  # revealed: float
# a payload-less variant is a value, not a class — reached without parens
reveal_type(Shape.Empty)  # revealed: Shape.Empty
```

## construction is checked against the variant fields

```by
enum class Shape:
    case Circle(radius: float)
    case Empty

# error: [invalid-argument-type]
Shape.Circle("not a float")
# error: [missing-argument]
Shape.Circle()
# a unit variant is a value, so it cannot be called
# error: [call-non-callable]
Shape.Empty()
```

## the enum name is the union of its variants

```by
enum class Shape:
    case Circle(radius: float)
    case Square(side: float)
    case Empty

def describe(s: Shape) -> str:
    # the enum name in annotation position *is* the variant union
    reveal_type(s)  # revealed: Circle | Square | Shape.Empty
    return "shape"

# every variant is assignable to the enum type
s: Shape = Shape.Circle(1.0)
s = Shape.Square(2.0)
s = Shape.Empty
describe(Shape.Circle(1.0))

reveal_type(s)  # revealed: Shape.Empty
```

## `match` narrows and checks exhaustiveness

a `match` covering every variant is exhaustive, so the function need not fall through:

```by
enum class Shape:
    case Circle(radius: float)
    case Square(side: float)
    case Empty

def area(s: Shape) -> float:
    match s:
        case Shape.Circle():
            return 1.0
        case Shape.Square():
            return 2.0
        case Shape.Empty:
            return 0.0
```

a `match` that omits a variant is not exhaustive — the function can implicitly return `None` (same
`Shape` as above):

```by
# error: [invalid-return-type]
def area2(s: Shape) -> float:
    match s:
        case Shape.Circle():
            return 1.0
        case Shape.Square():
            return 2.0
```

## positional subpatterns destructure a variant's payload

a payload variant's declared fields *are* its `__match_args__`, in declaration order, so
`case Shape.Circle(r):` binds `r` to the variant's first field. keyword subpatterns address a field
by name, defaulted fields included:

```by
enum class Shape:
    case Circle(radius: int)
    case Rect(width: int, height: int)
    case Point
    case Poly(sides: int, closed: bool = True)

reveal_type(Shape.Circle.__match_args__)  # revealed: ("radius",)
reveal_type(Shape.Rect.__match_args__)  # revealed: ("width", "height")

def area(s: Shape) -> int:
    match s:
        case Shape.Circle(r):
            reveal_type(r)  # revealed: int
            return 3 * r * r
        case Shape.Rect(w, h):
            reveal_type(w)  # revealed: int
            reveal_type(h)  # revealed: int
            return w * h
        case Shape.Point:
            return 0
        case Shape.Poly(n, closed=c):
            reveal_type(n)  # revealed: int
            reveal_type(c)  # revealed: bool
            return n
```

## destructuring does not depend on the project's python version

`dataclasses` only started deriving `__match_args__` in 3.10, but a variant's match args are part of
the language rather than a runtime dataclass detail, and the lowering targets basedpython's 3.10
floor regardless of what the project advertises. so a project that infers an older version — an
ambient 3.9 interpreter, say — still destructures:

```toml
[environment]
python-version = "3.9"
```

```by
enum class Shape:
    case Circle(radius: int)
    case Point

reveal_type(Shape.Circle.__match_args__)  # revealed: ("radius",)

def area(s: Shape) -> int:
    match s:
        case Shape.Circle(r):
            reveal_type(r)  # revealed: int
            return r
        case Shape.Point:
            return 0
```

## anonymous positional fields destructure by position

anonymous fields (`case Both(int, str)`) take the synthetic names `_0`, `_1`, … and destructure by
position just the same:

```by
enum class Pair:
    case Both(int, str)
    case Neither

def show(p: Pair) -> str:
    match p:
        case Pair.Both(n, s):
            reveal_type(n)  # revealed: int
            reveal_type(s)  # revealed: str
            return s
        case Pair.Neither:
            return ""
```

## more positional subpatterns than the variant has fields

```by
enum class Shape:
    case Circle(radius: int)
    case Point

def bad(s: Shape) -> None:
    match s:
        # error: [invalid-match-pattern]
        case Shape.Circle(r, extra):
            pass
        case _:
            pass
```

## positional subpatterns in a generic enum

a variant's fields are specialised by the enum's type arguments before they are bound, so
destructuring `Tree[int]` yields `int` rather than the bare typevar bound:

```by
enum class Tree[T]:
    case Leaf(value: T)
    case Node(left: Tree[T], right: Tree[T])

def depth(t: Tree[int]) -> int:
    match t:
        case Tree.Leaf(v):
            reveal_type(v)  # revealed: int
            return 1
        case Tree.Node(l, r):
            reveal_type(l)  # revealed: Leaf[int] | Node[int]
            return 1 + max(depth(l), depth(r))
```

## destructuring holds at runtime

the lowered variants are frozen dataclasses, so the `__match_args__` the checker models is the one
python derives — the transpiled `match` really does destructure:

```by
enum class Shape:
    case Circle(radius: int)
    case Rect(width: int, height: int)
    case Point

def area(s: Shape) -> int:
    match s:
        case Shape.Circle(r):
            return 3 * r * r
        case Shape.Rect(w, h):
            return w * h
        case Shape.Point:
            return 0

assert area(Shape.Circle(2)) == 12
assert area(Shape.Rect(3, 4)) == 12
assert area(Shape.Point) == 0

def swap(s: Shape) -> Shape:
    match s:
        case Shape.Rect(w, h):
            return Shape.Rect(h, w)
        case _:
            return s

assert swap(Shape.Rect(3, 4)) == Shape.Rect(4, 3)
```

## defaulted fields

named fields may carry defaults; construction accepts positional or keyword arguments, like any
dataclass:

```by
enum class Shape:
    case Rectangle(width: int, height: int)
    case Polygon(sides: int, closed: bool = True)

r = Shape.Rectangle(3, 4)
reveal_type(r.width)  # revealed: int
p = Shape.Polygon(sides=5)
reveal_type(p.closed)  # revealed: bool
```

## members defined on the enum dispatch on its variants

a variant is a subtype of the enum, so methods, properties, and classmethods declared on the enum
body are inherited by the variants. a `match self` in a method is exhaustive over the variants:

```by
enum class Expr:
    case Lit(value: int)
    case Add(left: Expr, right: Expr)

    def eval(self) -> int:
        match self:
            case Expr.Lit(v):
                return v
            case Expr.Add(l, r):
                return l.eval() + r.eval()

    @property
    def is_leaf(self) -> bool:
        match self:
            case Expr.Lit(_):
                return True
            case Expr.Add(_, _):
                return False

    @classmethod
    def zero(cls) -> Expr:
        return Expr.Lit(0)

e = Expr.Add(Expr.Lit(1), Expr.Lit(2))
reveal_type(e.eval())  # revealed: int
reveal_type(e.is_leaf)  # revealed: bool
reveal_type(Expr.zero())  # revealed: Lit | Add
```

## generic payload enums

a generic `enum class` parametrises its variants by the enum's type parameters; construction infers
them, the enum subscript denotes the specialised variant union, and a recursive `match` is
exhaustive:

```by
enum class Tree[T]:
    case Leaf
    case Node(value: T, left: Tree[T], right: Tree[T])

def size(t: Tree[int]) -> int:
    match t:
        case Tree.Leaf:
            return 0
        case Tree.Node(v, l, r):
            return 1 + size(l) + size(r)

t = Tree.Node(1, Tree.Node(2, Tree.Leaf, Tree.Leaf), Tree.Leaf)
reveal_type(size(t))  # revealed: int
```

a subscripted generic enum keeps its type argument, so a differently-specialised value is rejected
(the variant union carries the enum's typevar, not `Unknown`):

```by
enum class Wrap[T]:
    case W(value: T)
    case E

def takes_int(w: Wrap[int]) -> int:
    match w:
        case Wrap.W(v):
            return v
        case Wrap.E:
            return 0

bad: Wrap[str] = Wrap.W("hi")
reveal_type(bad)  # revealed: W[str]

# error: [invalid-argument-type]
n = takes_int(bad)
```

## a unit variant is still a value through the enum's subscript

`Tree[int].Leaf` is the same singleton as `Tree.Leaf` — subscripting the enum names the same class,
so a member lookup on it must not fall back to the variant's class object:

```by
enum class Tree[T]:
    case Leaf
    case Node(value: T, left: Tree[T], right: Tree[T])

reveal_type(Tree.Leaf)  # revealed: Tree.Leaf
reveal_type(Tree[int].Leaf)  # revealed: Tree.Leaf

x: Tree[int] = Tree[int].Leaf
```

## the same variant name may appear in different enums

variants are qualified, so there is no collision:

```by
enum class A:
    case Same(int)
    case X

enum class B:
    case Same(str)
    case Y

reveal_type(A.Same(1)._0)  # revealed: int
reveal_type(B.Same("h")._0)  # revealed: str
```

## all-unit enums

an `enum class` whose variants are all payload-less lowers to an idiomatic Python `Enum` with
`auto()` members. members are reached as `Color.Red` (typed as the enum literal), and `match` over
it narrows and is exhaustiveness-checked.

```by
enum class Color:
    case Red, Green
    case Blue

reveal_type(Color.Red)  # revealed: Color.Red
c: Color = Color.Red
reveal_type(c)  # revealed: Color.Red

def name(c: Color) -> str:
    match c:
        case Color.Red:
            return "red"
        case Color.Green:
            return "green"
        case Color.Blue:
            return "blue"
```

a non-existent member is an error, and an inexhaustive `match` is caught (same `Color` as above):

```by
# error: [unresolved-attribute]
x = Color.Purple

# error: [invalid-return-type]
def partial(c: Color) -> str:
    match c:
        case Color.Red:
            return "red"
```

since the lowering is a real `enum.Enum`, the class carries the `Enum` interface: members expose
`name` and `value`, and the model matches the runtime:

```by
enum class Fruit:
    case Apple, Pear

reveal_type(Fruit.Apple.name)  # revealed: "Apple"
print(Fruit.Apple.name)
print(Fruit.Pear.value)
```

the `Enum` base is injected rather than written, so it is also the class's *metaclass* — the class
iterates, sizes and looks members up like any other enum:

```by
enum class Berry:
    case Straw, Rasp

for berry in Berry:
    reveal_type(berry)  # revealed: Berry

reveal_type(len(Berry))  # revealed: int
reveal_type(Berry.__members__)  # revealed: MappingProxyType[str, Berry]
reveal_type(Berry["Straw"])  # revealed: Berry
```

## constants in an enum body stay constants

an assignment member disqualifies the idiomatic-`Enum` lowering (python's `Enum` would turn the
constant into a *member*), so the enum takes the sealed-hierarchy form where `MAX` is a plain class
attribute — the checker and the runtime agree:

```by
enum class WithConst:
    case A, B
    MAX = 10

reveal_type(WithConst.MAX)  # revealed: int
n: int = WithConst.MAX + 5

def f(e: WithConst) -> str:
    match e:
        case WithConst.A:
            return "a"
        case WithConst.B:
            return "b"
```

## visibility modifiers compose with `enum class`

a `private` or `export` modifier may precede `enum class`. `export` registers the enum in the
module's `__all__`; `private` renames the enum — and every reference the lowering synthesizes (the
variant subclasses, their attachments) — to an underscore-prefixed module-private name, so the
public surface stays consistent. the variants are still reached qualified through the declared name.

```by
export enum class Color:
    case Red, Green

private enum class Shape:
    case Circle(radius: float)
    case Square(side: float)

reveal_type(Color.Red)  # revealed: Color.Red
c = Shape.Circle(2.0)
reveal_type(c.radius)  # revealed: float
reveal_type(Shape.Square(1.0))  # revealed: Square
```

## variants require `case`

a bare name in an `enum class` body is a no-op statement, almost certainly a variant missing its
`case` — the parser says so:

```by
enum class Bad:
    case Ok
    # error: [invalid-syntax] "enum variants must be declared with `case`, e.g. `case Red, Green`"
    # error: [unresolved-reference]
    Oops
```

variant fields are declared in parentheses; a brace payload is rejected with the fix spelled out:

```by
enum class Bad2:
    case A { x: int }  # error: [invalid-syntax] "variant fields are declared in parentheses, e.g. `case A(x: int)`"
```

## a variant is usable bare in type position

a payload-less variant is an enum literal; basedpython accepts it bare in a type expression
(`a: E.A` is `a: Literal[E.A]`), the same as any other bare literal.

```by
enum class E:
    case A
    case B
    case C

a: E.A = E.A
reveal_type(a)  # revealed: E.A

# only the named variant is assignable
b: E.A = E.B  # error: [invalid-assignment]

# unions of variants work as annotations
def f(x: E.A | E.B) -> None:
    reveal_type(x)  # revealed: E.A | E.B
```

This also works for an idiomatic `enum.Enum`:

```by
from enum import Enum

class Color(Enum):
    RED = 1
    GREEN = 2

c: Color.RED = Color.RED
reveal_type(c)  # revealed: Color.RED
```

## `is` / `is not` between members keeps identity at runtime

a payload-less variant is a singleton *instance*, not a class, so the `is`/`is not` keyword pair
keeps python identity semantics for it — the `isinstance` lowering only fires when the rhs resolves
to a variant *class*. this block is checker-clean, so the divergence harness executes it and pins
the runtime contract

```by
enum class Genre:
    case A, B

assert Genre.A is Genre.A
assert Genre.A is not Genre.B

g: Genre = Genre.A
assert g is Genre.A
assert g is not Genre.B

enum class Shape:
    case Circle(radius: float)
    case Point

assert Shape.Point is Shape.Point
p = Shape.Point
assert p is Shape.Point

# a payload variant is a class, so the rhs of `is` lowers to `isinstance`
c = Shape.Circle(1.0)
assert c is Shape.Circle
assert c is not Shape.Point
```

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
reveal_type(Shape.Empty)  # revealed: Shape
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
    return "shape"

# every variant is assignable to the enum type
s: Shape = Shape.Circle(1.0)
s = Shape.Square(2.0)
s = Shape.Empty
describe(Shape.Circle(1.0))

reveal_type(s)  # revealed: Shape
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

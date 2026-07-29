# destructuring with `if let`

shorthand for a one-variant peel of an enum or optional value:

```by
if let Shape.Circle(r) := shape:
    print(r * 2)
else:
    print("not a circle")
```

the pattern left of `:=` is any `match` pattern, the clause is taken when the
pattern matches the subject on the right, and the pattern's captures are bound
for the body

```by
def describe(v: int?) -> str:
    if let int(n) := v:
        return f"got {n}"
    return "nothing"
```

`elif let` works the same way, and mixes freely with ordinary conditions. a
subject is only evaluated once every earlier clause has failed, exactly as a
condition in that position would be

```by
if shape is Square:
    print(shape.side)
elif let Shape.Circle(r) := shape:
    print(r * 2)
else:
    print("something else")
```

## narrowing

a clause narrows like the `match` case it stands for — the captures inside the
body, and the subject in every branch:

```by
def describe(v: int | str) -> str:
    if let int(n) := v:
        reveal_type(n)  # int
        reveal_type(v)  # int
        return f"int {n}"
    else:
        reveal_type(v)  # str
        return v
```

## captures leak

a capture stays bound after the statement, possibly unbound when the pattern did
not match — the same rule walrus bindings and `match` captures follow

```by
if let int(n) := v:
    pass
print(n)  # possibly unbound
```

## `let` is not a keyword here

`let` only introduces a pattern when the clause really is
`let <pattern> := <subject>`; everywhere else it stays an ordinary name, so
`if let := f():` is still a walrus test on a variable called `let`

## `Some` is not a pattern

`Some(x)` builds an optional, it is not a class, so it cannot appear in pattern
position — `if let Some(x) := opt:` is rejected. peel an optional by matching
the type it wraps (`if let int(n) := opt:`), which narrows away the `None` just
the same

## lowering

there is no python spelling that keeps a `match` inside an `if` header, so the
whole chain flattens onto a selector variable: each clause header becomes a
guard that records which clause was taken, followed by a plain `if` that runs
the original body

```py
_by_if_let_0 = 0
match shape:
    case Shape.Circle(r):
        _by_if_let_0 = 1
if _by_if_let_0 == 1:
    print(r * 2)
if _by_if_let_0 == 0:
    print("not a circle")
del _by_if_let_0
```

only the headers are rewritten, so every body keeps its own source. `match` is
python 3.10 syntax, so `if let` needs a target of 3.10 or later

a clause takes any pattern [destructuring](destructuring.md) does, including an
`and` pattern, and binds with `:=` for the reasons set out there — writing `=`
is reported

## open questions

- `while let` for loop-and-peel iteration
- a guard clause (`if let P := s if cond:`), which `match` cases have

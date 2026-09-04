# basedpython: block-scoped `let` and `var`

A `let` or `var` declaration written inside a block — the body of an `if`, a loop, a `with`, a
`try`, a `match` case — binds its name for that block only. Python has no block scopes, so this is a
rule the checker enforces rather than something the emitted python does; a plain assignment still
binds for the whole enclosing function or module, as python does.

```toml
[environment]
python-version = "3.12"
```

## a `let` in a block is gone after the block

```by
def f(flag: bool):
    if flag:
        let a = 1
        reveal_type(a)  # revealed: 1

    print(a)  # error: [unresolved-reference]
```

## the error says which declaration went out of scope

```by
def f(flag: bool):
    if flag:
        let a = 1

    print(a)  # snapshot
```

```snapshot
error[unresolved-reference]: Name `a` used when not defined
 --> src/mdtest_snippet.by:5:11
  |
3 |         let a = 1
  |         --- `a` is declared here
4 |
5 |     print(a)  # snapshot
  |           ^
info: `a` is declared with `let`, so it is in scope only inside the block that declares it
```

## a `var` in a block is gone after the block

```by
def f(flag: bool):
    if flag:
        var a = 1
        reveal_type(a)  # revealed: 1

    print(a)  # error: [unresolved-reference]
```

## a plain assignment in a block is not block-scoped

Only the binding keyword makes a name block-scoped; `a = 1` binds for the whole scope, exactly as it
does in python.

```by
def f(flag: bool):
    if flag:
        a = 1
    else:
        a = 2

    reveal_type(a)  # revealed: 1 | 2
```

## the block a declaration is scoped to is the clause it is written in

An `if` and its `else` are separate blocks, so a declaration in one is out of scope in the other.

```by
def f(flag: bool):
    if flag:
        let a = 1
    else:
        print(a)  # error: [unresolved-reference]
```

## declaring in every branch does not carry the name out

```by
def f(flag: bool) -> int:
    if flag:
        let a = 1
    else:
        let a = 2
    return a  # error: [unresolved-reference]
```

## a nested block is its own block

```by
def f(flag: bool, other: bool):
    if flag:
        while other:
            let a = 1
            reveal_type(a)  # revealed: 1

        print(a)  # error: [unresolved-reference]
```

## a `break` carries nothing out of the loop

The jump leaves the block from inside it, which takes the block's names out of scope just as running
off its end does.

```by
def f() -> int:
    while True:
        let a = 1
        break

    return a  # error: [unresolved-reference]
```

## a `continue` carries nothing back to the loop header

```by
def f(values: list[int]):
    for value in values:
        print(carried)  # error: [unresolved-reference]
        let carried = value
        continue
```

## a function declared in a block still sees the block's declarations

The name is out of scope after the block, not after the statement, so anything written inside the
block reads it.

```by
def f(flag: bool):
    if flag:
        let a = 1

        def g() -> int:
            return a
```

## a declaration in a function body is not in a block

A scope's own body is not a block, so a `let` at the top of a function is visible for the rest of
it.

```by
def f(flag: bool) -> int:
    let a = 1
    if flag:
        reveal_type(a)  # revealed: 1
    return a
```

## a loop body's declaration does not escape the loop

```by
def f(values: list[int]) -> int:
    for value in values:
        var total = value

    return total  # error: [unresolved-reference]
```

## a loop body's declaration is a new one on each iteration

The body is a block, so its declaration goes out of scope where the body ends and the next iteration
declares the name again. Assigning it once per iteration assigns each of those declarations once.

```by
def f():
    while True:
        let a
        a = 1
        print(a)
```

## assigning twice in one iteration is still a reassignment

```by
def f():
    while True:
        let a
        a = 1
        a = 2  # error: [invalid-assignment] "read-only symbol `a` cannot be reassigned"
```

## a declaration outside the loop is assigned once per iteration

Nothing takes it out of scope between iterations, so the assignment reaches itself around the loop.

```by
def f():
    let a
    while True:
        a = 1  # error: [invalid-assignment] "read-only symbol `a` cannot be reassigned"
```

## a loop body that runs assigns what it declares

```by
def f():
    for _ in range(3):
        let a
        a = 1
        print(a)
```

## a `with` body's declaration does not escape it

```by
class Resource:
    def __enter__(self) -> int:
        return 1

    def __exit__(self, *args: object) -> None:
        return None

def f():
    with Resource():
        let opened = 1

    print(opened)  # error: [unresolved-reference]
```

## a `try` clause's declarations do not escape it

Each clause of a `try` statement is its own block. An `except` clause is entered from the middle of
the `try` block, so it takes the names that block declared out of scope on that edge too.

```by
def risky() -> int:
    raise ValueError

def f():
    try:
        let attempted = risky()
    except ValueError:
        print(attempted)  # error: [unresolved-reference]
    else:
        let succeeded = 1
    finally:
        print(succeeded)  # error: [unresolved-reference]
```

The `try` body has to be able to raise for any of this to matter: a handler for a body that cannot
is never entered, and nothing in it is analysed at all.

## an exception raised inside a nested block still leaves it

```by
def f(flag: bool):
    try:
        if flag:
            let attempted = 1
            raise ValueError
    except ValueError:
        print(attempted)  # error: [unresolved-reference]
```

## a `match` case's declarations do not escape it

```by
def f(value: int | str):
    match value:
        case int():
            let matched = 1
        case _:
            print(matched)  # error: [unresolved-reference]
```

## `block-scoped-declarations` turns it off

With the option off, a declaration binds for the whole enclosing scope, as a plain assignment does.

```toml
[environment]
python-version = "3.12"

[analysis]
block-scoped-declarations = false
```

```by
def f(flag: bool):
    if flag:
        let a = 1

    print(a)  # error: [possibly-unresolved-reference]

def g(flag: bool) -> int:
    if flag:
        var b = 1
    else:
        var b = 2
    return b
```

## a class body in a block keeps its own members

A class body is a scope, not a block, so its declarations are members of the class either way.

```by
def f(flag: bool):
    if flag:
        class C:
            let tag = "c"

        reveal_type(C.tag)  # revealed: "c"
```

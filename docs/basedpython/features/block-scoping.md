# block scoping

a variable declared with `let` or `var` inside a block belongs to that block. after the
block ends, the name is gone:

```by
if flag:
    let a = 1
    print(a)

print(a)  # error: `a` is not in scope here
```

python has no block scopes — a name bound anywhere in a function is a local of the
whole function — and the python this lowers to keeps it that way. so this is a rule the
checker enforces, not a change to what the emitted code does. the transpiled output is
the same either way:

```python
if flag:
    a: Final = 1
    print(a)

print(a)
```

## a block is a clause, not a statement

each clause of a compound statement is its own block: an `if` body and its `else`, a
loop body and its `else`, every `except` and `finally`, every `match` case

```by
def f(flag: bool):
    if flag:
        let a = 1
    else:
        print(a)  # error: `a` is not in scope here
```

that holds even when every branch declares the name — two declarations of `a` are two
variables, and neither outlives its own branch:

```by
def f(flag: bool) -> int:
    if flag:
        let a = 1
    else:
        let a = 2
    return a  # error: `a` is not in scope here
```

to carry a value out of a branch, declare it where it is used:

```by
def f(flag: bool) -> int:
    let a = 1 if flag else 2
    return a
```

a scope's own body is not a block, so a declaration at the top of a function or a module
is visible for the rest of it, and a class body declares members as it always did

## only the binding keyword scopes a name

a plain assignment binds for the whole enclosing function or module, exactly as it does
in python:

```by
def f(flag: bool) -> int:
    if flag:
        a = 1
    else:
        a = 2
    return a
```

## leaving a block early leaves it

a `break`, a `continue` or a raised exception leaves the block from the middle, and
takes its declarations with it:

```by
def f() -> int:
    while True:
        let a = 1
        break

    return a  # error: `a` is not in scope here
```

## a loop body declares again on each iteration

the body is a block, so what it declares is gone before the next iteration starts. a
`let` assigned once in the body is assigned once per declaration, which is what a
read-only variable allows:

```by
def f(lines: list[str]):
    for line in lines:
        let width
        width = len(line)
        print(width)
```

a declaration written outside the loop is a single one, so the same assignment reaches
itself on the next iteration and is reported as `invalid-assignment`:

```by
def f(lines: list[str]):
    let width
    for line in lines:
        width = len(line)  # error: read-only symbol `width` cannot be reassigned
```

## turning it off

block scoping is on by default. `analysis.block-scoped-declarations` turns it off, and a
declaration then binds for the whole enclosing scope, as a plain assignment does:

```toml
[tool.ty.analysis]
block-scoped-declarations = false
```

## requiring a declaration

the `implicit-declaration` rule asks that every variable a scope binds be declared once —
`let` for a binding that never changes, `var` for one that does. it is off by default:

```toml
[tool.ty.rules]
implicit-declaration = "error"
```

```by
count = 0  # error: [implicit-declaration]
count = count + 1  # error: [implicit-declaration]
```

declaring it once answers both:

```by
var count = 0
count = count + 1
```

a class body is left alone: `x: int` there declares a field, which is how a
[data class](modifiers.md) or a protocol is written. so is an assignment to anything but
a plain name — an attribute, a subscript, an item of an unpacking — and so is a `.byi`
stub, which says what a module has rather than what it does

see [modifiers and visibility](modifiers.md) for what `let` and `var` declare

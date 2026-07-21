# basedpython: a trailing-lambda block must not capture a loop variable unless confined

A trailing-lambda block inside a loop lowers to a closure that captures the loop variable by
reference. If the callee is a borrow (`local` / `once`), it runs the block synchronously — the
variable still holds this iteration's value. A non-borrow callee may keep the block and call it
after the loop advances, when the variable holds its final value (the classic late-binding trap).
This is the type-aware complement to ruff's `B023`, which cannot resolve the callee's marker.

```toml
[environment]
python-version = "3.12"
```

## a `once` callee is confined, so capturing a loop variable is fine

```by
def run(once fn: () -> None):
    fn()

for x in [1, 2, 3]:
    run:
        print(x)
```

## a `local` callee is confined too

```by
def run(local fn: () -> None):
    fn()

for x in [1, 2, 3]:
    run:
        print(x)
```

## a non-borrow callee may defer the block, so a captured loop variable is flagged

Even a callee that happens to call the block synchronously is flagged when its parameter is not
declared `local` / `once` — the marker is how the callee promises to confine the block.

```by
def run(fn: () -> None):
    fn()

for x in [1, 2, 3]:
    run:
        print(x)  # error: [escaping-loop-variable] "trailing-lambda block captures loop variable `x`: its callee is not `local` / `once`, so it may run the block after the loop advances, when `x` holds its final value"
```

## a block that does not capture the loop variable is fine

```by
def run(fn: () -> None):
    fn()

y = 0
for x in [1, 2, 3]:
    run:
        print(y)
```

## a block outside any loop is fine

```by
def run(fn: () -> None):
    fn()

x = 1
run:
    print(x)
```

## a block that rebinds the name locally is not capturing it

```by
def run(fn: () -> None):
    fn()

for x in [1, 2, 3]:
    run:
        x = 99
        print(x)
```

## an imported non-borrow callee is flagged too (cross-file, unlike `B023`)

`callee.by`:

```by
def run(fn: () -> None):
    fn()
```

`main.by`:

```by
from callee import run

for x in [1, 2, 3]:
    run:
        print(x)  # error: [escaping-loop-variable]
```

## an imported `once` callee is confined (cross-file)

`callee.by`:

```by
def run(once fn: () -> None):
    fn()
```

`main.by`:

```by
from callee import run

for x in [1, 2, 3]:
    run:
        print(x)
```

## an opaque callee is left alone

When the callee cannot be resolved, its marker cannot be inspected, so the block is not flagged.

```by
# error: [unresolved-import]
from nowhere import run

for x in [1, 2, 3]:
    run:
        print(x)
```

## a nested loop variable is caught

```by
def run(fn: () -> None):
    fn()

for i in [1, 2]:
    for j in [3, 4]:
        run:
            print(i)  # error: [escaping-loop-variable]
```

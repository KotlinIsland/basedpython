## What it does

Checks for a `return` inside a `once` content block that is itself written inside another
trailing-lambda block.

## Why is this bad?

A `once` block runs exactly once, inline, so a `return` inside it is allowed to leave the enclosing
scope — but only one level: the language propagates a block's `return` to the scope the block is
written in. When that scope is itself a block, the `return` leaves the inner block and stops there;
the enclosing function keeps running, and the returned value is silently discarded.

(A `break` or `continue` inside any block is already rejected as `break` outside loop: a block is
its own function.)

## Examples

```by
def Column(once content: () -> None):
    content()

def Row(once content: () -> None):
    content()

def App(done: bool) -> int:
    Column:
        Row:
            if done:
                return 1  # error: [content-block-control-flow]
        return 2  # ok: one level, leaves `App`
    return 0
```

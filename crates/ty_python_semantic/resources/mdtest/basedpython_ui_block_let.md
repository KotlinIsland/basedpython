# basedpython-ui: a declaration inside a trailing-lambda block is the block's own

A `once` block runs inline, so a plain assignment in it writes through to an enclosing binding of
the same name. A *declaration* — a `let`, a `var`, an annotated assignment — is different: it
introduces the block's own local, exactly as python treats an annotated name inside a nested
function (which it refuses to make `nonlocal`). The enclosing binding is untouched after the block.

## a `let` inside a `once` block shadows, and does not write through

```by
def run(once block: () -> None):
    block()

def show(user: str) -> None:
    run:
        let user = 1
        reveal_type(user)  # revealed: 1
    reveal_type(user)  # revealed: str

```

## the enclosing binding may be a `match` capture

```by
async def load(name: str) -> str:
    return name.upper()

async def scope(once block: () -> Awaitable[None]):
    await block()

async def main() -> None:
    match "morgan":
        case str() as user:
            await scope():
                let user = await load("nested")
                reveal_type(user)  # revealed: str
            reveal_type(user)  # revealed: "morgan"

```

## an annotated assignment is a declaration too

```by
def run(once block: () -> None):
    block()

def main() -> None:
    total: int = 1
    run:
        total: int = 2
    reveal_type(total)  # revealed: 1

```

## a plain assignment still writes through

```by
def run(once block: () -> None):
    block()

def main() -> None:
    total: int = 1
    run:
        total = 2
    reveal_type(total)  # revealed: 2

```

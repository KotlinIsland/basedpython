# basedpython: postfix `.await`

`expr.await` is the postfix spelling of a prefix `await expr` — same type, same async-context
requirement. ty type-checks it through the standard `Await` node.

```toml
[environment]
python-version = "3.12"
```

## infers the awaited type

```by
class C:
    async def m(self) -> int:
        return 1

async def g() -> C:
    return C()

async def f() -> None:
    reveal_type(g().await)  # revealed: C
```

## chains like attribute access

```by
class C:
    async def m(self) -> int:
        return 1

async def g() -> C:
    return C()

async def f() -> None:
    reveal_type(g().await.m().await)  # revealed: int
```

## same type as the prefix form

```by
async def g() -> int:
    return 1

async def f() -> None:
    postfix = g().await
    prefix = await g()
    reveal_type(postfix)  # revealed: int
    reveal_type(prefix)  # revealed: int
```

## non-awaitable operand is still an error

```by
async def f() -> None:
    x = 1
    x.await  # error: [invalid-await]
```

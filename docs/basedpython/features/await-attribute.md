# postfix await (`.await`)

`expr.await` awaits `expr`, like rust's postfix `.await`. it binds as tightly
as attribute access, so it chains left-to-right without parentheses:

```by
async def f():
    g().await.bar().await
```

transpiles to:

```python
async def f():
    await (await g()).bar()
```

## semantics

`expr.await` is exactly a prefix `await expr` — same runtime behaviour, same
type. the only difference is surface syntax and how it reads in a chain. it is
valid only inside an `async def`, like any other `await`

## precedence

because `.await` binds at the postfix level (alongside `.attr`, `(...)`,
`[...]`), each `.await` applies to everything to its left:

```by
g().await.bar().await
```

reads as "await `g()`, call `.bar()` on the result, then await that". the
transpiler inserts parentheses only where prefix-`await` precedence requires
them — when the awaited value is the spine of a following attribute, call, or
subscript:

| basedpython             | python                  |
| ----------------------- | ----------------------- |
| `g().await`             | `await g()`             |
| `g().await.bar`         | `(await g()).bar`       |
| `g().await[0]`          | `(await g())[0]`        |
| `g().await + h().await` | `await g() + await h()` |

## prefix `await` still works

the postfix form is additive — a prefix `await expr` is left untouched. mix
them freely; both lower to the same thing

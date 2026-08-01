# optional chaining

`a?.b` short-circuits to `None` when `a is None`, otherwise evaluates `a.b`:

```by
city = user?.address.city
```

transpiles to:

```python
city = (None if user is None else user.address.city)
```

the expansion is always parenthesized. a conditional expression binds looser
than every operator, so without them the chain would swallow whatever followed
it — `not user?.name` would evaluate to `user.name` rather than to
`not user.name`

## chains

each `?.` introduces a new short-circuit guard. multi-step chains share a
temp variable so each prefix is evaluated only once:

```by
country = user?.address?.country
```

transpiles to:

```python
country = (None if user is None else None if (__by_t_0__ := user.address) is None else __by_t_0__.country)
```

mixed chains — `?.` followed by regular `.` — only guard at the explicit
optional steps:

```by
zip = user?.address.zip
# → (None if user is None else user.address.zip)
```

`a.b?.c` works in the obvious way: only the part after `a.b` is short-circuited
through a temp variable

## the chain runs to the end of the trailers

a `?.` opens a chain that runs out through every trailer that follows it —
`.attr`, `(...)` and `[...]`. an absent receiver skips the whole rest of the
chain, so the `None` belongs to the chain's last link rather than to each link:

```by
name = user?.profile.display_name()
# → (None if user is None else user.profile.display_name())
```

method calls, subscripts and further attribute access all chain, in any
combination:

```by
first = user?.orders[0].total()
# → (None if user is None else user.orders[0].total())
```

the type of a chain is the type of its last link unioned with `None`, so
`user?.profile.display_name()` is `str | None`. `?.` only contributes a `None`
when its receiver can actually be absent — a chain over a non-optional receiver
stays non-optional

## `?.` guards its own receiver, not the rest of the chain

each `?.` guards the value immediately to its left. an attribute that is
optional in its *own* right is still reported:

```by
class User:
    cb: Callable[[], int] | None
    address: Address | None

user?.cb()          # error: `cb` may be None
user?.address.city  # error: `address` may be None
```

add a `?.` at the step that needs guarding — `user?.address?.city` is
`str | None`

## scope

`?.` is recognized in attribute-access expressions only. there is no optional
call (`a?.()`) or optional subscript (`a?.[k]`) — those would guard the
*callable* or the *container* rather than a receiver, and are a syntax error.
where you need them, fall back to a guard expression

a chain covers the trailers that follow it, and stops there — it does not
extend over the enclosing expression. `user?.age + 1` is rejected, because the
chain ends at `user?.age` and `int | None` has no `+`. write
`(user?.age ?? 0) + 1`

## interaction with `??`

see [none-coalesce operator](none-coalesce.md). `?.` composes with `??`
without re-evaluating the chained prefix, including when the chain ends in a
call:

```by
name = user?.profile.display_name() ?? "anonymous"
```

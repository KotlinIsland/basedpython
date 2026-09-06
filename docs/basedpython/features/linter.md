# linting

`buff` lints and formats `.by` source. it is ruff, so it reads ruff's
configuration out of `pyproject.toml` and every rule ruff ships is available:

```toml
[tool.ruff.lint]
select = ["E", "F", "BY"]
```

`BY` is basedpython's own rule prefix. most of those rules look for a python
spelling of something basedpython has syntax for:

| code    | name                       | what it finds                                                              |
| ------- | -------------------------- | -------------------------------------------------------------------------- |
| `BY001` | `manual-none-coalesce`     | a conditional expression that is a [`??`](none-coalesce.md)                |
| `BY002` | `manual-optional-chain`    | a conditional expression that is a [`?.`](optional-chaining.md)            |
| `BY003` | `manual-isinstance`        | an `isinstance` call that is an [`is`](identity-swap.md)                   |
| `BY004` | `manual-super-call`        | `super()` where [`super`](super.md) will do                                |
| `BY007` | `manual-any-annotation`    | `Any` where [`dynamic`](dynamic.md) will do                                |
| `BY009` | `manual-unpack-annotation` | `Unpack[…]` where [`*`](unpack-syntax.md) will do                          |
| `BY010` | `manual-typeof-annotation` | `TypeOf[…]` where [`typeof`](typeof.md) will do                            |
| `BY011` | `manual-re-export`         | `import name as name`, which is [`export`](export-imports.md)              |
| `BY012` | `redundant-typing-import`  | an import of a member [already implicit](implicit-typing.md)               |
| `BY017` | `unnecessary-stub-body`    | a `: ...` body an [empty declaration](empty-declarations.md) needs no more |
| `BY019` | `manual-sentinel`          | a `Sentinel(…)` assignment, which is [`sentinel`](sentinel.md)             |
| `BY020` | `manual-cast-call`         | a `typing.cast` call, which is the [`cast`](cast.md) keyword               |
| `BY021` | `manual-property`          | a `@property`, which is a [declaration with accessors](properties.md)      |
| `BY022` | `manual-modifier`          | a decorator that is a [modifier keyword](modifiers.md)                     |
| `BY023` | `manual-tuple-annotation`  | a `tuple[…]` annotation, which is a [tuple type](tuple-types.md)           |
| `BY101` | `redundant-none-coalesce`  | a `??` whose fallback cannot change the result                             |

every one of them is fixable. `BY020`'s fix is the only one that is always
unsafe — the `cast!` it writes is [checked](checked-cast.md) where `typing.cast`
is a no-op, so the rewrite adds a way for the program to fail.

`BY021` is the only one that sometimes has no fix to offer. an accessor body is
re-rendered when it is lowered and does not keep a comment, so a property with a
comment in it is reported and left for you to move. a tuple type is re-rendered
the same way, so `BY023`'s fix is unsafe on an annotation with a comment in it.

they also compose with the upstream rules that produce their input. `SIM108`
turns an `if` / `else` block into a conditional expression, and `BY001` takes it
the rest of the way, so one `--fix` run rewrites

```by
if a is None:
    result = b
else:
    result = a
```

into

```by
result = a ?? b
```

## what the linter does not check

the linter reads one file at a time and has no types. anything that takes a type
to decide is `by check`'s job instead, and reporting it in both places would
mean two answers that can disagree. that split is why a `once` callback the
callee never calls is `once-not-called` in the checker and has no `BY` rule
here, and why `t[0]` is not reported as a
[tuple member access](tuple-index.md) — whether `t` is a tuple is a type
question

## upstream rules on `.by` source

ruff's own rules are written for python source, and two things follow from that.

a rule that resolves a name through its import does not see a name basedpython
resolves for you. `UP045` rewrites `Optional[int]` to `int | None` when
`Optional` was imported, and says nothing when it was left to
[implicit typing](implicit-typing.md) — the same annotation, reported or not
depending on a line that basedpython does not need

a rule that suggests a replacement suggests the python one. `SIM108` above is
the case where that composes; where it does not, the suggestion is still valid
`.by`, just not the shortest way to write it

nothing in ruff's rule set is known to report a construct that is correct
basedpython. `F821` used to: an unqualified builder inside a
[trailing-lambda](trailing-lambdas.md) block resolves against the block's
[implicit receiver](implicit-receivers.md), and the linter cannot see receiver
types, so it now defers every unresolved name inside a block to `by check`. the
same deferral covers `self` and an
[enum variant](context-sensitive-resolution.md) written bare

# custom string tags

a custom string tag is an identifier placed directly before a string
literal, with no intervening whitespace. it lowers to a call that receives
the string as a [PEP 750](https://peps.python.org/pep-0750/) `Template`:

```by
query = sql"select * from {table} where id = {id}"
```

transpiles to:

```python
query = sql(t"select * from {table} where id = {id}")
```

the tag is any in-scope callable. its signature is uniform — `(Template) -> T`
— and the result type is whatever the call returns, so the type checker
infers it with no special handling

## why basedpython has these and python does not

PEP 750 considered arbitrary string-literal prefixes (the javascript
`` tag`...` `` model) and rejected them: arbitrary callable tags were "too
complex to build in full generality" at the interpreter level, and a general
prefix mechanism would foreclose adding future builtin prefixes

neither concern applies to a transpiler. a tag is sugar that desugars to a
call — no bytecode, no runtime machinery, no reserved-grammar anxiety. the
builtin prefixes stay reserved as lexer prefixes; custom tags are a disjoint
mechanism resolved as ordinary identifiers

## desugaring

the rewrite is uniform regardless of whether the literal interpolates:

```by
a = greet"hello"
b = greet"hello {name}"
```

→

```python
a = greet(t"hello")
b = greet(t"hello {name}")
```

a tag always receives a `Template`, never a bare `str` — the same way a
javascript tag always receives the structured object. this keeps every tag
to a single signature and keeps the interpolation structured rather than
eagerly string-joined, which is the whole point of the structural tags PEP
750 was built for (`sql`, `html`, `re`, `path`, …)

## the lexer rule

`tag"..."` is currently a syntax error in python — a `NAME` juxtaposed with a
`STRING` has no operator between them — so the slot is free

an identifier abutting a quote is read as a tag **unless** it is exactly a
valid builtin-prefix combination, which stays reserved:

| token                   | meaning                    |
| ----------------------- | -------------------------- |
| `f"x"`, `rb"x"`, `t"x"` | builtin string — unchanged |
| `ff"x"`, `sql"x"`       | custom tag                 |

the rule is decidable: a valid builtin-prefix set is a builtin string,
anything else is a tag. the cost is that a one-letter function named `f`
cannot be tag-called as `f"x"` (that is an f-string); write `f(t"x")`
explicitly

## boundaries

custom tags do not combine with builtin prefixes by juxtaposition.
`sqlr"..."` lexes the tag greedily as `sqlr`, not `sql` + raw. raw and bytes
handling happen at lex time, so a tag cannot retroactively change the escape
processing of its content — the underlying `t"..."` is processed normally.
this is the same generality boundary PEP 750 drew, drawn in the same place

## where a tag resolves

the tag is an ordinary name, resolved where it is written. inside a
[trailing lambda](trailing-lambdas.md) block whose callback declares an
[implicit receiver](implicit-receivers.md), that includes the receiver's
members — so `text"…"` reaches the same method `text(t"…")` does:

```by
root.div:
    text"hello {who}"
```

both spellings lower to the same call on the receiver

## runtime

`Template` is a python 3.14 type. on earlier targets the tag receives a
`Template` [polyfill](polyfills.md) with the same `strings` / `interpolations`
shape, so tagged strings work before 3.14

## round-tripping

the reverse transpiler re-sugars `tag(t"...")` back to `tag"..."` when the
call carries `TaggedString` provenance in the `LoweringMap`. calls written by
hand as `tag(t"...")` without that provenance are left as-is

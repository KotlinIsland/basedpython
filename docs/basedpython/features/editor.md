# editor support

the `by` language server backs the editor experience: completions, inlay
hints, an outline, and the rest of the language-server protocol. this page
covers the parts that are basedpython's own — the ones you would not find by
guessing from python

## postfix templates

writing `.` after an expression offers a set of templates that rewrite the
whole expression rather than reading as attribute access. accepting `.print`
on `xs` replaces `xs.print` with `print(xs)`:

```by
xs.print        # → print(xs)
```

| template      | rewrites `x` into |
| ------------- | ----------------- |
| `.print`      | `print(x)`        |
| `.not`        | `not x`           |
| `.type`       | `type(x)`         |
| `.repr`       | `repr(x)`         |
| `.list`       | `list(x)`         |
| `.par`        | `(x)`             |
| `.return`     | `return x`        |
| `.raise`      | `raise x`         |
| `.if`         | an `if` statement |
| `.for`        | a `for` loop      |
| `.match`      | a `match`         |
| `.let` `.var` | a binding         |

the templates from `.return` down are statements, so they are only offered
where the expression is the whole statement — `f(xs.print)` offers `.print`
but not `.for`. the ones that open a suite or need a name to bind place the
cursor for you, when your editor supports snippets

`.let` and `.var` spell basedpython bindings; the rest are valid python and are
offered in `.py` files too

### postfix `await`

[`.await`](await-attribute.md) is not a template — it is real syntax, so it
completes as an ordinary word. it appears inside an `async def`, on an
expression that can actually be awaited

## completions

### the entry point

at module level, `main` completes to the whole [entry point](main-function.md)
definition, with `async main` beside it. once the module defines one, the name
completes to it like any other

### keywords written as two words

a construct spelled with more than one keyword is offered whole, wherever it is
valid: `async def`, `data class`, `frozen data class`, `enum class`,
`override def`, `static var`, and the rest of the
[modifiers](modifiers.md). the method modifiers only appear inside a class
body, and `async for` / `async with` only inside an `async def`

they are offered at the start of a statement only — after `async` the plain
`def` keyword completes the rest

### overriding

in a class body, every superclass member the class does not define is offered
as the whole header an override would be written with:

```by
class B(A):
    override def greet(self, name: str) -> str:
```

`object`'s members are left out

### types

a type position offers the words basedpython spells types with — `literal`,
`final`, `dynamic`, `some`, `protocol`, `typeof` — and drops the statement
keywords, which never read there. a [type parameter](generics.md) offers its
modifiers before the name: `in`, `out`, `in out`, `overlapping`, `reified`

### common aliases

a name that is conventionally an alias of a module completes as that module,
in a file that has not imported it yet. `np.` offers what numpy has, and
accepting one of those completions writes the import that binds the name:

```py
import numpy as np

np.arange
```

the alias itself completes the same way — typing `n` offers `np`, and taking it
writes `import numpy as np`

only a module the project actually has is offered, and only where the name is
free: a file that binds `np` to something of its own means that, and gets
nothing from numpy

the aliases are the ones the python ecosystem already writes — `np`, `pd`,
`plt`, `dt`, and the rest. a project spells aliases of its own in its
configuration, keyed by the alias:

```toml
[tool.ty.editor.common-aliases]
npt = "numpy.typing"
```

an alias configured under a name ty already knows replaces it

a name left unimported reports as unresolved, and the quick fix on that
diagnostic writes the same import

these are auto-imports, so turning off `ty.completions.autoImport` turns them
off too

### enum members and extensions

a bare [enum member](enums.md) is offered where the expected type admits one —
the value of a declared assignment, and a `case` pattern:

```by
a: Color = Red
```

attribute completions include the members any [`extension`](extensions.md)
block in scope declares on the receiver, alongside the type's own

## inlay hints

each kind of hint can be turned off on its own through the
`ty.inlayHints.<name>` setting your editor passes to the server. all default to
on

| setting                   | shows                                                       |
| ------------------------- | ----------------------------------------------------------- |
| `variableTypes`           | the type of a variable the source does not annotate         |
| `callArgumentNames`       | the parameter each positional argument fills                |
| `inferredRaises`          | the [exception set](exceptions.md) of an undeclared `def`   |
| `inferredVariance`        | the [variance](variance.md) inferred for a type parameter   |
| `inferredReification`     | `reified` on a parameter the body reifies                   |
| `inferredOverride`        | `override` on a method that overrides without saying so     |
| `callTypeArguments`       | the type arguments inferred for a generic call              |
| `typeArgumentNames`       | the parameter a positional type argument fills              |
| `numericPromotions`       | the arms numeric promotion adds to `float` and `complex`    |
| `revealedTypes`           | what a `reveal_type` call reveals                           |
| `implicitParameters`      | a [trailing lambda](trailing-lambdas.md)'s `it`             |
| `implicitSelf`            | the `self` an [`init(...)`](init-method.md) binds           |
| `lambdaParameterTypes`    | the type of an unannotated `lambda` parameter               |
| `inheritedParameterTypes` | the type a parameter takes from the method it overrides     |
| `inferredReturnTypes`     | the return type of a `def` that leaves it out               |
| `implicitArguments`       | the [context arguments](context-parameters.md) a call fills |
| `enumValues`              | the value an [enum](enums.md) member takes implicitly       |
| `templateBindingTypes`    | a django template `{% for %}` binding's element type        |
| `resolvedTemplates`       | the file a django `{% extends %}` name resolves to          |

## outline

a [property](properties.md) is one member in the source, so it is one entry in
the outline — not the getter, backing field and setter it lowers into. enum
variants and an extension's methods appear under their declarations

## debugger facts

while a program is stopped, an editor knows something no checker does: what the
names in the current frame actually hold. the server takes those readings and
answers what the code below the stop line will do, rather than what it could do

given a debugger stopped on line 5 holding `limit = 5`:

```by
def compute() -> int: ...

limit = compute()
# stopped here
if limit > 100:     # = false
    over = 1        # will not run
```

nothing in the source decides that branch — `compute()` returns an `int` and any
`int` is possible. the reading of `limit` is what settles it

this is the checker's ordinary reachability analysis, reading the file under one
extra assumption. it does not change the diagnostics you already see: a seeded
reading and an unseeded one are separate questions, and the ordinary one is what
the squiggles come from

the editor asks with a custom request, `by/dataFlowAt`:

```json
{
  "textDocument": { "uri": "file:///src/main.by" },
  "line": 5,
  "observations": [{ "name": "limit", "observed": "isInt", "text": "5" }]
}
```

`line` is one-based and is the line the program is stopped on. that line is
answered along with everything below it, because nothing on it has run yet.
a name may be a dotted path — `self.limit` — spelled as the source spells it

| `observed`     | carries                        | means                    |
| -------------- | ------------------------------ | ------------------------ |
| `isNone`       |                                | the value is `None`      |
| `isBool`       | `value`                        | exactly this `bool`      |
| `isInt`        | `text`, decimal                | exactly this integer     |
| `isStr`        | `text`                         | exactly this string      |
| `isBytes`      | `bytes`, an array of numbers   | exactly these bytes      |
| `isExactly`    | `module`, `qualname`           | `type(value)` is this    |
| `isEnumMember` | `module`, `qualname`, `member` | this member of this enum |

the answer is a list of findings, each with a `range`, a `kind` of `condition`
or `unreachable`, a `taken` for a condition, and a `label` to draw

an empty answer is the ordinary case. only readings the server can express as a
type produce anything, and only where nothing can have changed the name in
between:

- a binding at or below the stop line is the program's own assignment, and wins
    over a reading taken before it
- a name a loop around the stop line rebinds is refused, because the reading is
    true of this iteration and not of the next
- an observation applies only to the scope the program is actually stopped in.
    one frame's `limit` is not another's, and a name that scope does not itself
    bind — a global it only reads, an attribute it never assigns — is a value
    nothing in that scope can vouch for

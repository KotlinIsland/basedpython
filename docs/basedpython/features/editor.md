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

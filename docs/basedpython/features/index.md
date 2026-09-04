# features

the basedpython language reference — one page per feature, each with the
surface syntax, what it checks, and the python it lowers to

!!! tip "new here?"

    [getting started](../getting-started.md) installs `by` and walks a `.by`
    file through to running python. this page is the reference you come back to

## python compatibility

`.by` is not a superset of `.py`

<div class="by-index" markdown>

- [differences from python](differences-from-python.md) — every place the same
    source reads differently

</div>

## runtime compatibility

what the transpiled python does at runtime, beyond what you wrote

<div class="by-index" markdown>

- [polyfills](polyfills.md) — write modern python, run it on older interpreters
- [runtime type-soundness checks](soundness.md)

</div>

## project-level

features that apply to a project rather than to a file

<div class="by-index" markdown>

- [api lockfile (`api.lock`)](api-lock.md)
- [declared dependencies](dependencies.md) — imports checked against
    `pyproject.toml`, and a quick fix that declares them
- [editor support](editor.md) — postfix templates, completions, inlay hints
    and the outline
- [language injection](language-injection.md) — a string that says what
    language is written inside it
- [linting](linter.md) — the `BY` rules, and how ruff's own rules read `.by`
    source

</div>

## standard library

what basedpython's vendored typeshed says that upstream's does not

<div class="by-index" markdown>

- [typeshed improvements](typeshed.md) — covariant mapping keys, honest `re`
    groups, precise `functools.cache`, and the rest

</div>

## enhancements that also apply to python

type-checking improvements with no new syntax — they work in `.by` and `.py` files alike

<div class="by-index" markdown>

- [fluid specializations](fluid-specializations.md)
- [sound types](sound-types.md) — infer precise types instead of gradual ones
- [precise unsolved type variables](precise-unsolved-typevars.md) — an unsolved type variable is
    `Never`, not `Unknown`
- [regex group types](regex-groups.md) — type a match from the pattern it came from
- [boolean conditions](conditions.md) — catch a test that conflates two members, or asks nothing
- [refutable unpacking](refutable-unpacking.md) — catch `a, b = ...` over a value of unknown length
- [unused return value](unused-return-value.md) — catch a call whose answer is thrown away
- [string formatting](string-formatting.md) — check the format spec, and the value that has no
    rendering of its own

</div>

## type system

what a type is allowed to say

<div class="by-index" markdown>

- [tuple type literals](tuple-types.md)
- [callable arrow syntax](callable.md)
- [implicit receivers (`int.() -> str`)](implicit-receivers.md)
- [intersection types](intersection.md)
- [`or` / `and` type operators](or-and-types.md)
- [negation types (`not T`)](not-type.md)
- [unsafe unions](unsafe-union.md)
- [`dynamic` type](dynamic.md)
- [`typeof` keyword](typeof.md)
- [star projections (`X[*]`)](star-projection.md)
- [strict `float` and `complex`](no-number-promotions.md)
- [infinity and nan float literals](float-literals.md)
- [literal type promotion](literal-types.md)
- [template literal types](template-literal-types.md)
- [use-site type modifiers (`literal T`, `final T`)](type-modifiers.md)
- [symbolic operations in types](symbolic-type-ops.md)
- [match types](match-types.md)
- [`type def` type functions](type-def.md)
- [typed dict literals](typed-dict-literal.md)
- [anonymous named tuple types](anonymous-named-tuple.md)
- [inline protocol types](inline-protocol.md)
- [wrapped optional and result types](wrapped-results.md)
- [automatic forward references](forward-references.md)
- [implicit typing imports](implicit-typing.md)
- [typed lambda](typed-lambda.md)
- [implicit overload stubs](overloads.md)
- [type narrowing predicates](type-is.md)

</div>

## generics

type parameters — their bounds, their variance, and what survives to runtime

<div class="by-index" markdown>

- [generics](generics.md)
- [type mappings](type-mappings.md)
- [type parameter bound ranges](bound-ranges.md)
- [bounds on a variadic pack](pack-bounds.md)
- [attribute types (`T.a`)](attribute-types.md)
- [`TypedDict` and `Self` in type parameters](typeddict-self-bounds.md)
- [keyword-variadic packs](keyword-variadic.md)
- [type parameter separators](type-param-separators.md)
- [typevar variance keywords](variance.md)
- [safe variance](safe-variance.md)
- [overlapping](overlapping.md)
- [explicit generic call sites](generic-calls.md)
- [reified type parameters](reified-generics.md)
- [reified class type parameters](reified-class-generics.md)
- [type reification](type-reification.md)
- [parametric type tests](parametric-type-tests.md)

</div>

## declarations

the forms a class, function or binding can take

<div class="by-index" markdown>

- [modifiers and visibility](modifiers.md)
- [block scoping](block-scoping.md)
- [based enums (`enum class`)](enums.md)
- [sealed classes](sealed-classes.md)
- [module api enforcement (`implements`)](module-api.md)
- [init method shorthand](init-method.md)
- [properties](properties.md)
- [empty declarations](empty-declarations.md)
- [main function](main-function.md)
- [build stamps](build-stamps.md)
- [`sentinel` declarations](sentinel.md)
- [decorator keyword](decorator-keyword.md)
- [decorated function parameters](decorated-parameters.md)
- [decorating anything](decorate-anything.md)
- [inherited default values](inherited-defaults.md)

</div>

## expressions and statements

syntax inside a function body

<div class="by-index" markdown>

- [context-sensitive resolution](context-sensitive-resolution.md) — `a: Color = Red`
- [identity and isinstance (`===` / `!==` / `is`)](identity-swap.md)
- [optional chaining (`?.`)](optional-chaining.md)
- [none-coalesce operator (`??`)](none-coalesce.md)
- [postfix await (`.await`)](await-attribute.md)
- [`cast` keyword](cast.md)
- [checked & safe casts (`cast!` / `cast?`)](checked-cast.md)
- [`super` keyword](super.md)
- [tuple member access (`expr.N`)](tuple-index.md)
- [keyword arguments in subscripts](kw-subscript.md)
- [flexible keyword argument names](flexible-keyword-names.md) — `f(a.b=1, "x-y"=2)`
- [destructuring](destructuring.md) — `let`, and patterns in every binding position
- [destructuring with `if let`](if-let.md)
- [starred wildcards in class patterns](class-pattern-star.md) — `case A(x, *_, y)`
- [statement expressions](statement-expressions.md)
- [trailing lambda blocks](trailing-lambdas.md)
- [unpack syntax](unpack-syntax.md)
- [mutable default arguments](mutable-defaults.md)
- [unique loop bindings](unique-loop-bindings.md) — a closure captures its own iteration
- [dedented triple-quoted strings](dedent-strings.md)
- [custom string tags](string-tags.md)
- [strings and characters](character.md) — grapheme-aware `str` api
- [repeated `_` parameters](repeated-underscore.md)
- [lazy imports](lazy-imports.md)
- [export imports](export-imports.md) — `from x export y`
- [static resources](static-resources.md) — `import "data/config.yaml" as config`
- [extensions](extensions.md) — add members to an existing type, and declare that
    it conforms to an existing protocol
- [conversions (`__from__` / `__into__` / `__of__`)](conversions.md) — and
    discarding a callable's return where the site asked for `None`
- [frozen container displays](frozen-displays.md) — `{1}` for a `frozenset`
- [context parameters](context-parameters.md)
- [local lifetimes (`local` / `once`)](local-lifetimes.md)
- [exception tracking (`raises`)](exceptions.md)

</div>

## formatting

how the formatter lays basedpython out

<div class="by-index" markdown>

- [assignment alignment](assignment-alignment.md) — line up the `=` of consecutive assignments

</div>

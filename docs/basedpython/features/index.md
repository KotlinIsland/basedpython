# features

the basedpython language reference

## runtime compatibility

- [polyfills](polyfills.md) — write modern python, run it on older interpreters
- [runtime type-soundness checks](soundness.md)

## project-level

- [api lockfile (`api.lock`)](api-lock.md)

## enhancements that also apply to python

type-checking improvements with no new syntax — they work in `.by` and `.py` files alike

- [fluid specializations](fluid-specializations.md)
- [sound types](sound-types.md) — infer precise types instead of gradual ones
- [regex group types](regex-groups.md) — type a match from the pattern it came from
- [boolean conditions](conditions.md) — catch a test that conflates two members, or asks nothing

## type system

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

## generics

- [generics](generics.md)
- [explicit typevar constraints](constraints.md)
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
- [type reification](type-reification.md)
- [parametric type tests](parametric-type-tests.md)

## declarations

- [modifiers and visibility](modifiers.md)
- [based enums (`enum class`)](enums.md)
- [sealed classes](sealed-classes.md)
- [init method shorthand](init-method.md)
- [properties](properties.md)
- [empty declarations](empty-declarations.md)
- [main function](main-function.md)
- [`sentinel` declarations](sentinel.md)
- [decorator keyword](decorator-keyword.md)

## expressions and statements

- [context-sensitive resolution](context-sensitive-resolution.md) — `a: Color = Red`
- [identity and isinstance (`===` / `!==` / `is`)](identity-swap.md)
- [optional chaining (`?.`)](optional-chaining.md)
- [none-coalesce operator (`??`)](none-coalesce.md)
- [postfix await (`.await`)](await-attribute.md)
- [`cast` keyword](cast.md)
- [checked & safe casts (`cast` / `cast?`)](checked-cast.md)
- [`super` keyword](super.md)
- [tuple member access (`expr.N`)](tuple-index.md)
- [keyword arguments in subscripts](kw-subscript.md)
- [statement expressions](statement-expressions.md)
- [trailing lambda blocks](trailing-lambdas.md)
- [unpack syntax](unpack-syntax.md)
- [mutable default arguments](mutable-defaults.md)
- [dedented triple-quoted strings](dedent-strings.md)
- [custom string tags](string-tags.md)
- [strings and characters](character.md) — grapheme-aware `str` api
- [repeated `_` parameters](repeated-underscore.md)
- [lazy imports](lazy-imports.md)
- [export imports](export-imports.md) — `from x export y`
- [extensions](extensions.md)
- [conversions (`__from__` / `__into__` / `__of__`)](conversions.md)
- [context parameters](context-parameters.md)
- [local lifetimes (`local` / `once`)](local-lifetimes.md)
- [exception tracking (`raises`)](exceptions.md)

## formatting

- [assignment alignment](assignment-alignment.md) — line up the `=` of consecutive assignments

## planned

- [destructuring with `if let`](if-let.md)
- [implementations (`implementation A for B`)](implementations.md)

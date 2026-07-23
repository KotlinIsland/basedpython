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

## type system

- [tuple type literals](tuple-types.md)
- [callable arrow syntax](callable.md)
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
- [symbolic operations in types](symbolic-type-ops.md)
- [typed dict literals](typed-dict-literal.md)
- [anonymous named tuple types](anonymous-named-tuple.md)
- [wrapped optional and result types](wrapped-results.md)
- [automatic forward references](forward-references.md)
- [implicit typing imports](implicit-typing.md)
- [typed lambda](typed-lambda.md)
- [implicit overload stubs](overloads.md)
- [type narrowing predicates](type-is.md)

## generics

- [generics](generics.md)
- [explicit typevar constraints](constraints.md)
- [keyword-variadic packs](keyword-variadic.md)
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

- [identity and isinstance (`===` / `!==` / `is`)](identity-swap.md)
- [optional chaining (`?.`)](optional-chaining.md)
- [none-coalesce operator (`??`)](none-coalesce.md)
- [postfix await (`.await`)](await-attribute.md)
- [`cast` keyword](cast.md)
- [checked & safe casts (`cast` / `cast?`)](checked-cast.md)
- [`super` keyword](super.md)
- [tuple member access (`expr.N`)](tuple-index.md)
- [keyword arguments in subscripts](kw-subscript.md)
- [unpack syntax](unpack-syntax.md)
- [mutable default arguments](mutable-defaults.md)
- [dedented triple-quoted strings](dedent-strings.md)
- [custom string tags](string-tags.md)
- [repeated `_` parameters](repeated-underscore.md)
- [lazy imports](lazy-imports.md)
- [extensions](extensions.md)
- [local lifetimes (`local` / `once`)](local-lifetimes.md)

## planned

- [destructuring with `if let`](if-let.md)

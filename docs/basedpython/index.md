# basedpython language reference

basedpython is a Python-like language that transpiles to pure Python

## getting started

- [getting started](getting-started.md)

## tools

`by` and `buff` are available when the `basedpython` package is installed

- [`by` cli reference](cli-reference.md)

## runtime compatibility

- [polyfills](features/polyfills.md)

## project-level features

- [api lockfile (`api.lock`)](features/api-lock.md)

## basedpython language features

- [tuple type literals](features/tuple-types.md)
- [callable arrow syntax](features/callable.md)
- [intersection types](features/intersection.md)
- [negation types (`not T`)](features/not-type.md)
- [`typeof` keyword](features/typeof.md)
- [star projections (`X[*]`)](features/star-projection.md)
- [strict `float` and `complex`](features/no-number-promotions.md)
- [infinity and nan float literals](features/float-literals.md)
- [literal type promotion](features/literal-types.md)
- [symbolic operations in types](features/symbolic-type-ops.md)
- [typed dict literals](features/typed-dict-literal.md)
- [anonymous named tuple types](features/anonymous-named-tuple.md)
- [explicit typevar constraints](features/constraints.md)
- [typevar variance keywords](features/variance.md)
- [explicit generic call sites](features/generic-calls.md)
- [automatic forward references](features/forward-references.md)
- [implicit typing imports](features/implicit-typing.md)
- [typed lambda](features/typed-lambda.md)
- [implicit overload stubs](features/overloads.md)
- [decorator keyword](features/decorator-keyword.md)
- [type narrowing predicates](features/type-is.md)
- [generics](features/generics.md)

## syntax extensions

- [modifiers and visibility](features/modifiers.md)
- [based enums (`enum class`)](features/enums.md)
- [sealed classes](features/sealed-classes.md)
- [init method shorthand](features/init-method.md)
- [empty declarations](features/empty-declarations.md)
- [main function](features/main-function.md)
- [identity and isinstance (`===` / `!==` / `is`)](features/identity-swap.md)
- [optional chaining (`?.`)](features/optional-chaining.md)
- [none-coalesce operator (`??`)](features/none-coalesce.md)
- [postfix await (`.await`)](features/await-attribute.md)
- [mutable default arguments](features/mutable-defaults.md)
- [dedented triple-quoted strings](features/dedent-strings.md)
- [tuple member access (`expr.N`)](features/tuple-index.md)
- [keyword arguments in subscripts](features/kw-subscript.md)
- [unpack syntax](features/unpack-syntax.md)
- [super keyword](features/super.md)
- [`cast` keyword](features/cast.md)
- [`sentinel` declarations](features/sentinel.md)
- [lazy imports](features/lazy-imports.md)
- [repeated `_` parameters](features/repeated-underscore.md)

## planned

- [destructuring with `if let`](features/if-let.md)

## development

- [how transpilation works](development/how-transpilation-works.md)
- [reverse transforms](development/reverse-transforms.md)

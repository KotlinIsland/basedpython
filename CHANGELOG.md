# Changelog

ruff's changelog, from before basedpython forked it, is in [`changelogs/`](https://github.com/KotlinIsland/basedpython/tree/main/changelogs)

## 0.0.1

the first stable release of basedpython: a python-like language with sum types, extensions,
optional chaining and a type system that knows what your code means, transpiled to plain python
any python tool can read. it ships as the `basedpython` package, with two executables: `by`, the
type checker, transpiler and project driver, and `buff`, the linter and formatter

```sh
uv add --dev "basedpython>=0.0.1"
```

[getting started](https://kotlinisland.github.io/basedpython/getting-started/) takes about five minutes. the
[feature reference](https://kotlinisland.github.io/basedpython/features/) has a page for everything below

### why try it

- **plain python out the other end**: `by build` writes ordinary `.py` files, so pytest, mypy,
    ruff and the rest of your stack keep working
- **write for the newest python, ship for the oldest**: [polyfills](https://kotlinisland.github.io/basedpython/features/polyfills/)
    lower new syntax to run on the interpreters your users have
- **a type checker that is useful on plain python too**: built on ty, with
    [sound types](https://kotlinisland.github.io/basedpython/features/sound-types/) in place of gradual ones and
    [framework support](https://kotlinisland.github.io/basedpython/frameworks/) for pydantic, sqlalchemy, pytest and
    django
- **native speed when you want it**: [`by compile`](https://kotlinisland.github.io/basedpython/features/compilation/) builds
    the same source into CPython extension modules
- **tooling from day one**: a language server, a formatter, a linter and an
    [intellij plugin](https://kotlinisland.github.io/basedpython/features/intellij-plugin/)

### the language

`.by` keeps almost all of python's meaning; [differences from python](https://kotlinisland.github.io/basedpython/features/differences-from-python/)
lists every place it does not

- **data and variants**: [`enum class`](https://kotlinisland.github.io/basedpython/features/enums/) sum types with payloads,
    [sealed classes](https://kotlinisland.github.io/basedpython/features/sealed-classes/) with exhaustive `match`, and
    [`sentinel`](https://kotlinisland.github.io/basedpython/features/sentinel/) declarations
- **expressions**: [optional chaining](https://kotlinisland.github.io/basedpython/features/optional-chaining/) (`?.`),
    [none-coalesce](https://kotlinisland.github.io/basedpython/features/none-coalesce/) (`??`),
    [statement expressions](https://kotlinisland.github.io/basedpython/features/statement-expressions/)
    (`x = match y:`), [destructuring](https://kotlinisland.github.io/basedpython/features/destructuring/) and
    [`if let`](https://kotlinisland.github.io/basedpython/features/if-let/),
    [trailing lambda blocks](https://kotlinisland.github.io/basedpython/features/trailing-lambdas/), and
    [checked casts](https://kotlinisland.github.io/basedpython/features/checked-cast/)
- **declarations**: [modifiers and visibility](https://kotlinisland.github.io/basedpython/features/modifiers/),
    [block scoping](https://kotlinisland.github.io/basedpython/features/block-scoping/),
    [properties](https://kotlinisland.github.io/basedpython/features/properties/) that read like declarations, an
    [init method shorthand](https://kotlinisland.github.io/basedpython/features/init-method/), and a
    [`main` function](https://kotlinisland.github.io/basedpython/features/main-function/) whose parameters are its command
    line
- **extending what exists**: [extensions](https://kotlinisland.github.io/basedpython/features/extensions/) add members to
    an existing type and declare its conformance to a protocol, and
    [conversions](https://kotlinisland.github.io/basedpython/features/conversions/) (`__from__`, `__into__`, `__of__`) say
    how one type becomes another
- **effects**: [exception tracking](https://kotlinisland.github.io/basedpython/features/exceptions/) (`raises`),
    [local lifetimes](https://kotlinisland.github.io/basedpython/features/local-lifetimes/) (`local`, `once`) and
    [context parameters](https://kotlinisland.github.io/basedpython/features/context-parameters/)
- **imports and resources**: [lazy imports](https://kotlinisland.github.io/basedpython/features/lazy-imports/),
    [`from x export y`](https://kotlinisland.github.io/basedpython/features/export-imports/) and
    [static resources](https://kotlinisland.github.io/basedpython/features/static-resources/)
    (`import "config.yaml" as config`)

### the type system

- **types that say more**: [intersections](https://kotlinisland.github.io/basedpython/features/intersection/),
    [negations](https://kotlinisland.github.io/basedpython/features/not-type/), [tuple](https://kotlinisland.github.io/basedpython/features/tuple-types/)
    and [callable](https://kotlinisland.github.io/basedpython/features/callable/) type literals,
    [typed dict literals](https://kotlinisland.github.io/basedpython/features/typed-dict-literal/),
    [template literal types](https://kotlinisland.github.io/basedpython/features/template-literal-types/) and
    [inline protocols](https://kotlinisland.github.io/basedpython/features/inline-protocol/)
- **computation in types**: [match types](https://kotlinisland.github.io/basedpython/features/match-types/),
    [`type def` type functions](https://kotlinisland.github.io/basedpython/features/type-def/) and
    [symbolic arithmetic](https://kotlinisland.github.io/basedpython/features/symbolic-type-ops/) in type parameters
- **generics**: [bound ranges](https://kotlinisland.github.io/basedpython/features/bound-ranges/),
    [type mappings](https://kotlinisland.github.io/basedpython/features/type-mappings/),
    [safe variance](https://kotlinisland.github.io/basedpython/features/safe-variance/), and type parameters that survive
    to runtime through [reified generics](https://kotlinisland.github.io/basedpython/features/reified-generics/)
- **inference that narrows instead of shrugging**: [sound types](https://kotlinisland.github.io/basedpython/features/sound-types/),
    [precise unsolved type variables](https://kotlinisland.github.io/basedpython/features/precise-unsolved-typevars/),
    [inherited parameter types](https://kotlinisland.github.io/basedpython/features/inherited-parameter-types/) and
    [narrowing predicates](https://kotlinisland.github.io/basedpython/features/type-is/)
- **checks that work on `.py` as well**: [regex group types](https://kotlinisland.github.io/basedpython/features/regex-groups/),
    [boolean conditions](https://kotlinisland.github.io/basedpython/features/conditions/),
    [refutable unpacking](https://kotlinisland.github.io/basedpython/features/refutable-unpacking/),
    [unused return values](https://kotlinisland.github.io/basedpython/features/unused-return-value/) and
    [string formatting](https://kotlinisland.github.io/basedpython/features/string-formatting/)
- **a more honest standard library**: [typeshed improvements](https://kotlinisland.github.io/basedpython/features/typeshed/)
    such as covariant mapping keys and precise `functools.cache`
- **two presets**: `strict` and `ty-compatible`, chosen with
    [`type-checking-preset`](https://kotlinisland.github.io/basedpython/configuration/#the-preset)

### transpiling and running

- [`by build`](https://kotlinisland.github.io/basedpython/cli-reference/#by-build) transpiles a project to python, and
    [`by run`](https://kotlinisland.github.io/basedpython/cli-reference/#by-run) transpiles and runs a module in one step
- [runtime soundness checks](https://kotlinisland.github.io/basedpython/features/soundness/) guard each place an unchecked
    value enters a typed slot
- [`by repl`](https://kotlinisland.github.io/basedpython/features/repl/) is an interactive session in basedpython, and
    [debugging](https://kotlinisland.github.io/basedpython/features/debugger/) stops and steps at `.by` lines
- [`by transpile --reverse`](https://kotlinisland.github.io/basedpython/getting-started/#converting-python-to-basedpython)
    rewrites existing python into basedpython

### native compilation

[`by compile`](https://kotlinisland.github.io/basedpython/features/compilation/) builds modules into native CPython
extension modules for python 3.11 to 3.14, including the free-threaded builds. a function it cannot
compile keeps its interpreted definition, and `--verbose` says which and why

### editors

- the [language server](https://kotlinisland.github.io/basedpython/features/editor/) gives completions, inlay hints, hover,
    an outline and [refactorings](https://kotlinisland.github.io/basedpython/features/editor/#refactorings) such as moving a
    definition, extracting a superclass and changing a signature
- the [intellij plugin](https://kotlinisland.github.io/basedpython/features/intellij-plugin/) makes `.by` a first-class
    language in PyCharm and IntelliJ IDEA, with `by`, `buff` and the debugger bundled
- [`by check`](https://kotlinisland.github.io/basedpython/features/project-server/) answers from a running language server's
    warm state when there is one
- [`buff`](https://kotlinisland.github.io/basedpython/features/linter/) lints and formats `.by` with ruff's rules plus the
    `BY` rules, and [aligns consecutive assignments](https://kotlinisland.github.io/basedpython/features/assignment-alignment/)

### frameworks

[pydantic](https://kotlinisland.github.io/basedpython/frameworks/pydantic/), [sqlalchemy](https://kotlinisland.github.io/basedpython/frameworks/sqlalchemy/),
[pytest](https://kotlinisland.github.io/basedpython/frameworks/pytest/) and [django](https://kotlinisland.github.io/basedpython/frameworks/django/)
are modelled directly, so synthesized constructors, mapped columns and injected fixtures check like
ordinary code. the language server also understands
[django templates](https://kotlinisland.github.io/basedpython/frameworks/django-templates/), and
[basedpython-ui](https://kotlinisland.github.io/basedpython/frameworks/basedpython-ui/) is a compose-style ui library the
type checker knows

### packaging

- a project [builds into an ordinary wheel](https://kotlinisland.github.io/basedpython/packaging/) with `basedpython` as
    its build backend, or into [one wheel for each python](https://kotlinisland.github.io/basedpython/packaging/#a-wheel-for-each-python)
    it supports
- [`api.lock`](https://kotlinisland.github.io/basedpython/features/api-lock/) records a project's public api as a reviewable
    diff, and [declared dependencies](https://kotlinisland.github.io/basedpython/features/dependencies/) checks imports against
    `pyproject.toml`
- [`by init`](https://kotlinisland.github.io/basedpython/cli-reference/#by-init) starts an application or library, and sets
    up [git merges](https://kotlinisland.github.io/basedpython/cli-reference/#by-merge-driver) of the files `by` generates

### installing without python

each release on github carries a `basedpython-<target>` archive of `by` and `buff` for 17 targets,
with shell and powershell installers

### since 0.0.1b2

- `raise` can be [written as an expression](https://kotlinisland.github.io/basedpython/features/statement-expressions/#the-diverging-forms)
    anywhere a value can, as in `table.get(key) ?? raise KeyError(key)`
- a `match` over a sealed class's subclasses is exhaustive, closed over the
    [workspace](https://kotlinisland.github.io/basedpython/features/sealed-classes/#workspace-boundary)
- an unannotated parameter of an override takes its type from every method it
    [overrides](https://kotlinisland.github.io/basedpython/features/inherited-parameter-types/)
- a call to a function that asserts a variable of an enclosing scope narrows that variable
- reified type parameters are tested by the same subtype relation the checker uses, at every depth,
    and a type parameter reads as a type form, so `T()` without an `isinstance(T, type)` is an error
- an annotation written as a literal, `x: 1`, is the same type as `Literal[1]`
- the editor reports what stops a module that checks clean from transpiling, and offers an
    "invert boolean" refactoring and renames of the overrides, parameters and modules tied to a
    renamed symbol
- `by run` and `by repl` pass the signals that end a process on to the program
- a PEP 723 script's own `type-checking-preset` applies when a project override names the script
- `api.lock` is the same whatever order modules are found in
- windows paths in tracebacks and sourcemaps read `C:\...` rather than `\\?\C:\...`
- `by --version` reports basedpython's version
- the standalone archives and installers are published on the github release page

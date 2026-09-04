# module api enforcement

## summary

a module is already a structural value: ty models it as `Type::ModuleLiteral`,
and a module literal already satisfies a protocol through its public surface, so

```by
backend: Backend = postgres
```

type-checks today when `postgres` has the members `Backend` asks for. what is
missing is a way to **attach that obligation to a module permanently**, so a
break is reported in the file that broke it rather than in whichever consumer
happened to assign it — or nowhere at all, when no consumer does

one statement attaches one, and it has two forms:

```by
implements Backend                  # this module answers `Backend`
implements Backend for ".*"         # every submodule of this package does
```

the second form is the one that matters for a plugin directory. written in
`backends/__init__.by`, it holds for every module in `backends`, including one
added tomorrow by someone who never read the interface, and it cannot be dropped
by deleting a line in the module that fails it

## what already works

- a module literal answers protocol members through
    `ModuleLiteralType::static_member` in `types.rs`
- a module-level function satisfies an instance or static method member —
    `implementation_access` in `types/protocol_class.rs` deliberately skips the
    class-side check for `Type::ModuleLiteral`, and `protocols.md` has the test
    (*module objects with static-method protocol members*)
- `crates/ty_python_semantic/src/api_lockfile.rs` already owns the definition of
    a module's *public* surface: `is_public_module_symbol`, `__all__` via
    `dunder_all_names`, the public-by-convention dunder allowlist
- `ty_module_resolver::ModuleGlobSet` already owns module-name globs — `*` within
    a component, `**` across components, `!` to exclude, last match wins. it is
    what `allowed-unresolved-imports` matches with, and it is documented
- `Module::all_submodules` and `ruff_db::files::directory_listing` already
    enumerate a package's contents as a tracked input, so "which modules does this
    rule reach" is an ordinary incremental query rather than a filesystem walk
    bolted onto the checker

so the checking engine and the glob language both exist. what this feature adds
is a declaration, a way to find it from the module it governs, and a direction of
blame

### where it falls short today

- **no obligation.** a plugin module that stops matching its interface is only
    caught if something assigns it to a protocol-typed place. a plugin loaded by
    name never is
- **blame points the wrong way.** the error lands on the consumer's assignment,
    which is often in a different package from the mistake
- **nothing is imposable.** an interface's author cannot state a requirement that
    holds over a directory. every module has to volunteer

## writing the interface

no new syntax is required. a module's members are unbound, so the interface
spells them `static`, which basedpython already has:

```by
protocol Backend:
    name: str
    static def connect(url: str) -> Connection
```

a plain `def connect(self, url: str) -> Connection` also works — instance access
strips `self`, so a module-level function matches the bound signature — but it
reads as a lie about a module, and it makes the protocol usable only in that
direction

### optional sugar: `module protocol`

`module protocol P:` is a protocol every one of whose members is `static`:

```by
module protocol Backend:
    name: str
    def connect(url: str) -> Connection
```

it is *only* that desugaring. in particular it does not restrict what may
satisfy it: a class object satisfies a static-membered protocol just as a module
does, which is what lets a test substitute a fake:

```by
class FakeBackend:
    name = "fake"
    static def connect(url: str) -> Connection: ...

def run(backend: Backend): ...

run(FakeBackend)    # ok
```

the sugar is cheap (a flag on the existing `protocol_class` path) but it is not
load-bearing — it can come last, or never

## the statement

```by
# backends/__init__.by
from .api import Backend

implements Backend for ".*", "!.base"
```

- **without `for`**, the statement obliges the module it is written in
- **with `for`**, it obliges the modules its patterns name, and says nothing
    about the file it is written in

a rule may name several protocols, and several rules may reach one module:
obligations accumulate, because two rules naming two protocols is the normal way
to say a module must answer both

### the patterns

they are `ModuleGlobSet` patterns — the existing matcher, the existing semantics,
the existing documentation — with a leading `.` marking them relative to the
declaring package, exactly as a relative import does. so in `app/__init__.by`:

| pattern         | reaches                                                                  |
| --------------- | ------------------------------------------------------------------------ |
| `".*"`          | every direct submodule — `app.home`, not `app.blog.home`                 |
| `".**"`         | the whole subtree                                                        |
| `".pages.*"`    | the direct submodules of `app.pages`                                     |
| `".**.pages.*"` | a `pages` package at any depth — `app.pages.home`, `app.blog.pages.home` |
| `".handler_*"`  | one component, matched by name                                           |
| `"!.base"`      | carves `app.base` back out                                               |

`*` matches within one component, `**` matches zero or more whole components and
must stand alone as one, and `!` excludes — all of that is the existing matcher's
behaviour and none of it is new here. `!` goes before the dot, because it negates
the pattern rather than being part of the path

a pattern may not climb: `"..sibling"` is `invalid-module-api`. a rule that
reached outside its own package would break the ownership boundary, and it would
also be unfindable — a module looks for rules in its ancestors, and a rule
imposing sideways is not in one

an absolute pattern (no leading dot) is rejected for the same reason. the
spelling is reserved rather than repurposed, so that a project-wide rule table,
if one is ever wanted, can use it

### where a rule may live, and why

**in a package's `__init__.by`, governing that package's subtree.** an absolute
pattern, or a rule in a file that is not a package `__init__`, is
`invalid-module-api`

this is the whole design, so it is worth saying why it is not "anywhere". the
obligations of a module have to be discoverable **from that module**, or the
error will not appear in the file the author is editing (see *where the error
lands*). so something has to index rules by the module they govern, and the index
has to be cheap:

- **anywhere in the project** means a project-wide scan, and then every file's
    check depends on every file's declaration set. one edit anywhere
    reinvalidates everything
- **through the import graph**, the way `extension` and conformance visibility
    work, is the wrong relation here: the whole point is imposing on a module that
    does not cooperate, and a module that does not cooperate does not import you
- **through containment** costs a bounded walk. the obligations of `a.b.c` are
    found in `a/__init__.by` and `a/b/__init__.by`, two files the module resolver
    already touches to resolve `a.b.c` at all

containment also draws the ownership line in the right place: you may impose on
modules inside your own package, and you may not impose on someone else's. that
is the same boundary `sealed` already uses, and it means a dependency cannot
reach into your tree and add requirements to it

### the private default

`".*"` does not match a submodule whose name starts with `_`. a leading underscore
already means "not part of the surface" everywhere else in this project, and a
private helper module sitting next to the plugins is the common case, not the
exception. naming one exactly (`for "._special"`) still reaches it

### rules that match nothing

a typo'd pattern is a rule that silently enforces nothing, which is the worst
failure mode a checker can have. a rule that matches no module in its package is
reported at the rule, using `Module::all_submodules` on the declaring package —
a listing of one directory, not a project walk

## what is checked

the candidate member set is the module's public surface as `api_lockfile.rs`
already defines it, so the two features cannot drift:

- a symbol whose simple name starts with `_` does not count, unless it is one of
    the public-by-convention dunders
- `__all__`, when present, is the surface
- a submodule counts as a member only where an ordinary attribute access would
    resolve it — the existing `available_submodule_attributes` rule, unchanged
- a re-export (`from .impl export Widget`) counts; a plain private import in a
    stub does not. this is the existing distinction, not a new one

nothing is checked about members the interface does not mention. a module may
expose whatever else it likes; `_`-prefixing and `__all__` are how a module says
a name is not part of its surface, and they are enough

### `__getattr__` defeats it

a module defining `__getattr__` answers every name, so every requirement would be
vacuously met. that is a silent pass, and a silent pass is worse than no check:
a module with a module-level `__getattr__` that carries an obligation is
`invalid-module-api`, however the obligation was attached

### stubs

when a module has a `.byi` stub, the stub is what importers see, so the stub is
what is checked. an obligation attached in the stub is checked against the stub;
one attached in the implementation is checked against the implementation. a rule
whose patterns reach both checks both — which is the only thing in the system
that relates a stub to its implementation, and is worth having for that alone

## where the error lands

in the module that failed, always — an obligation imposed from a package is still
the failing module's problem to fix. the anchor depends on what is available:

- a member with the wrong type — on that member's own definition
- a missing member — there is nothing to point at, so a file-level primary
    annotation (`Span::from(file)`, as `fixes.rs` already does) at the top of the
    file
- a bare `implements` in the file itself — on the statement

every such diagnostic carries a secondary annotation on the **rule that imposed
it**, in whichever `__init__.by` it was written. an error saying a module must
have `connect` is useless without saying who says so, and unlike a config file
the rule is a real declaration a reader can jump to

## the demanding side

three cases, in increasing order of how much new machinery they need

**1. a protocol-typed parameter.** works today, no new work:

```by
def migrate(backend: Backend): ...
migrate(postgres)
```

**2. requiring an actual module.** `module T` as a third use-site type modifier
alongside `literal T` and `final T` (`types/restricted.rs`, `TypeModifier` in
`ruff_python_ast::helpers`), accepting only a `Type::ModuleLiteral` assignable to
`T`. this exists for the reflective cases — a registry keyed by `__name__`, a
reloader — and is not needed for ordinary use

**3. loading a plugin by name.** the case the whole feature is for, since a
dynamically chosen module is the one thing no assignment can check:

```by
def import_module[P](name: str, api: type[P]) -> P
```

- when `name` is a literal, resolve the module and check it statically, and the
    call costs nothing at runtime beyond the import
- otherwise, the transpiler emits a witness check at the call — the module's
    members are verified against `P`'s requirements once, at load — reusing the
    existing `_soundness_check` boundary machinery rather than inventing a second
    runtime check

a package rule and a dynamic loader cover the two halves of the same problem: the
rule checks the plugins that are in the project, the loader checks the ones that
arrive from outside it

## lowering

the statement erases in both forms. the emitted python keeps the import that
named the protocol — it may be needed by annotations, and dropping it would
change the emitted module's own surface — so a package whose `__init__` carries
rules pays a real import at runtime for a purely static declaration. a
[lazy import](../features/lazy-imports.md) of the interface is the answer where
that matters, and the feature doc should say so rather than leaving it to be
discovered

phase 3 must not see either form survive

the reverse direction produces nothing: python has no idiom that means "this
module implements this interface", so `reverse_transforms` has no rule to add. a
heuristic — a module whose names happen to match some protocol — would invent an
obligation the author never wrote, which is exactly the kind of guess this
project does not make

## implementation map

| layer          | work                                                                                                                                                                                                                                                                  |
| -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| parser         | `implements` as a soft keyword at statement start, recognised only when followed by a name or dotted name, gated on `PySourceType::BasedPython` — the `extension` / `protocol` dispatch in `parser/statement.rs` is the model. the `for` clause takes string literals |
| ast            | a real `StmtImplements { interfaces, targets, range }` node via `ast.toml` + `generate.py`, `targets` empty for the bare form. see *alternatives* for why not a marker-decorated `ClassDef`                                                                           |
| semantic index | each interface named is a **load** of that name, so an import used only by the declaration is not unused (`F401`) and an undefined one is `F821`                                                                                                                      |
| ty             | `package_rules(db, file)` — syntactic, the rules an `__init__` declares — and `module_obligations(db, file)` — the ancestor walk plus the file's own bare statements — then the check over them                                                                       |
| formatter      | a printer for the node and a `.by` fixture. the formatter rebuilds from the ast, so a source-only form corrupts on reformat                                                                                                                                           |
| linter         | nothing, unless a name the declaration resolves becomes invisible to ruff's binder                                                                                                                                                                                    |
| lsp            | semantic tokens for the keyword; the secondary annotation gives go-to-definition on the rule for free                                                                                                                                                                 |
| api lockfile   | one record per obligation, `<module>:I=<protocol>`, so an api contract is visible in the reviewed diff whichever way it was attached                                                                                                                                  |
| docs           | `docs/basedpython/features/module-api.md`, plus `features/index.md` and the `zensical.toml` nav (all three, `scripts/check_docs_nav.py` enforces it)                                                                                                                  |

## salsa and cost

- **the fast negative is syntactic.** `package_rules` reads the semantic index of
    one `__init__` and needs no inference, so a package that declares nothing
    answers immediately and the ancestor walk is a handful of interned lookups.
    only a module that actually carries an obligation pays for resolving a
    protocol
- **the check is a cycle.** resolving the protocol a rule names infers the
    `__init__`'s module-level code, and an `__init__` routinely imports the very
    submodules its rules govern. `module_obligations` takes
    `cycle_initial = no obligations`, the same recovery the conformance registry
    uses for the same reason
- **no lookup rule changes.** nothing here touches module member lookup, so no
    cost lands on programs that use none of this

## diagnostics

| id                   | when                                                                                                                                                                                               |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `unmet-module-api`   | an attached interface has a requirement the module does not answer, or answers with the wrong type. error, default on                                                                              |
| `invalid-module-api` | the declaration itself is wrong: the name is not a protocol, an abstract class, an absolute pattern, a rule outside a package `__init__`, a module with `__getattr__`, a rule that matches nothing |

the `unmet-module-api` message should say which member, what shape was required,
and what the module has instead — the three-part form `types/conformance.rs`
already uses for a conformance that fails to answer a requirement

## staging

1. the bare `implements` — parser, node, formatter, index, the check,
    `unmet-module-api`, `invalid-module-api`, mdtests, feature doc
1. the `for` clause — pattern parsing, the ancestor walk, the imposed-rule
    diagnostics with their secondary annotation, the matches-nothing report. this
    is the half that makes a plugin directory enforceable
1. the demanding side — `module T`, then the typed `import_module`
1. `module protocol` sugar, if the `static`-per-member spelling proves noisy

## alternatives considered

- **rules in `pyproject.toml`**, `[[tool.ty.module-api]]` with a `protocol =   "backends.api.Backend"` path and a module glob list. the config machinery is
    all there — array-of-tables like `[[tool.ty.overrides]]`, module globs like
    `allowed-unresolved-imports`, `RangedValue` for anchoring a diagnostic on the
    line — and it can impose across package boundaries, which the source form
    deliberately cannot. it was the earlier draft of this document and it is
    rejected because the protocol becomes a **string**: no go-to-definition, no
    rename, no import edge, no way for the checker to tell a typo from a module it
    cannot see. an api contract belongs in the language, next to the interface it
    names. left available as a later escape hatch if imposing across packages
    turns out to be needed
- **`api P`, a lid on the module.** an earlier draft had a second statement that
    both obliged the module and narrowed what importers could see, hiding public
    members outside the protocol. it is not worth its weight: `_`-prefixing and
    `__all__` already say "not part of my surface", a `.byi` stub already narrows
    what importers see, and the lid would have put a new arm in module member
    lookup — a hot path — to buy the difference between a name list and a type
- **checking from the rule site.** the rule enumerates its modules and reports
    there, which needs no index at all. rejected because the squiggle then appears
    in `backends/__init__.by` while the author is editing `backends/postgres.by`,
    and in the language server it does not appear until the `__init__` is checked
    again
- **a marker-decorated `ClassDef`**, the trick `extension` and `protocol` use. it
    costs no `ast.toml` change, but a declaration with no body is not a class in
    any sense, and every consumer — the formatter, the semantic index, the ide —
    would have to un-tell the lie
- **an assignment instead of a statement**, `_: Backend = sys.modules[__name__]`.
    it works today, which is the honest baseline. it also requires a runtime
    import of `sys`, blames a line nobody reads as a declaration, cannot be
    imposed from outside, and reads as dead code to every tool that does not know
    better
- **a `.byi` stub as the contract.** a stub already narrows what importers see,
    but it replaces the module's types wholesale and nothing checks the
    implementation against it. that is a different tool: a stub describes, an
    obligation checks
- **status quo, use-site checking only.** covered above under *where it falls
    short*

## open questions

- **namespace packages have no `__init__`**, so they cannot carry rules. giving
    one an `__init__.by` is the answer, and is what a package that wants to impose
    requirements on its contents should have anyway — but it is a real gap for a
    plugin directory assembled across distributions, which is exactly where
    namespace packages are used
- **is `".*"` the right default reach**, or should a bare `implements Backend   for` with no patterns mean "every direct submodule"? the pattern list is one
    token of ceremony against being explicit about a rule that imposes on files
    its author may not have written
- **should a rule be able to demand a module exist?** a pattern matching nothing
    is currently a diagnostic about the rule. for a plugin a package genuinely
    requires, an error saying the module is missing might be what is wanted, which
    is a different feature wearing the same syntax
- **write access.** a protocol data member is read-write, so a module satisfying
    `name: str` must have a rebindable `name`. whether an immutable module-level
    binding can answer a member declared read-only needs the same answer protocols
    already give for `ReadOnly`, and should not get a second one
- **third-party modules.** a rule cannot reach outside its own package, and the
    project does not check installed code anyway. the dynamic loader's runtime
    witness is the only enforcement available for a plugin that arrives installed,
    and that asymmetry should be stated in the feature doc rather than discovered

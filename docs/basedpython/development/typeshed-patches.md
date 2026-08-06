# typeshed patches

basedpython vendors typeshed as `.byi` stubs, regenerated from upstream ruff's
`.pyi` on every sync (see the upstream-sync workflow). reverse-transpile
recovers most of what basedpython needs, but the stubs still arrive in the
legacy `TypeVar(...)` + `Generic[...]` form, and a few carry semantics no
mechanical step can reconstruct. the `by_typeshed_patch` crate fixes both,
deterministically, so a fresh sync always reproduces the committed tree

it does two things:

- **semantic patches** — small rust modules under
    `crates/by_typeshed_patch/src/patches/`, registered in `all_patches()`,
    each restoring one deliberate basedpython choice (see
    `mapping-key-covariance` below)
- **pep 695 conversion** — `crates/by_typeshed_patch/src/pep695.rs` rewrites
    every legacy generic class into a pep 695 header with explicit variance
    keywords (`out`/`in`/`in out`) and nice type-parameter names
    (`_KT_co` → `Key`, `_T_co` → `Element`, ...). this is the bulk of the diff

## where patches sit in the sync

`scripts/sync_typeshed_by.sh` runs two phases:

1. **reverse-transpile** — every upstream `.pyi` becomes a `.byi`
1. **`by_typeshed_patch`** — for each `.byi`: apply the registered semantic
    patches, re-parse, then run the pep 695 conversion

basedpython owns the pep 695 migration itself, which is what
lets it emit the explicit variance and nice names

the two passes run in order with a re-parse between them, because a semantic
patch may rewrite a typevar reference (e.g. covariance) that the conversion then
renames. **semantic patches see the legacy form**: typevars declared with
`TypeVar(...)` and referenced as plain names in class bases and method
signatures, with no pep 695 type-parameter lists and no variance keywords yet.
write them against that form

## the pep 695 conversion

`pep695::convert_module` reads every module-level `TypeVar`/`TypeVarTuple`/
`ParamSpec` declaration (recording variance, bound, constraints, default) and
rewrites each generic class header. variance maps covariant → `out`,
contravariant → `in`, invariant → `in out` (basedpython has no bivariant
spelling — `in out` *is* explicit invariance). names come from a curated table
for the core containers and a mechanical fallback (strip the leading underscore
and the `_co`/`_contra` suffix) for everything else; within one class colliding
names get a numeric suffix. a candidate is also rejected when the module already
binds it — an import, a class/function definition, an assignment — so a type
parameter never shadows a name the stub's own annotations refer to
(`xml.etree.ElementPath` imports `Element`, so its `_T` becomes `T`). the
curated table is keyed by kind as well as name, so `_P` is `Parameters` only
when it really is a `ParamSpec`

a `ParamSpec` becomes a type variable bound by the *top parameters* form —
`_P = ParamSpec("_P")` gives `[Parameters: (*: *, **: *)]`, not `[**Parameters]`,
which in basedpython declares a keyword-variadic pack instead

it is deliberately conservative. a class is only rewritten when every type
parameter resolves to a known module-level typevar — anything it can't fully
characterise (an imported typevar, an unusual base) is left in legacy form
rather than risking a broken stub. generic functions and type aliases are left
alone too. a typevar declaration is removed only once every reference to it has
been consumed by a conversion, and only when it is private (`_`-prefixed):
public typevars like `AnyStr` may be re-exported and imported by other modules,
so they always survive

## the `Patch` trait

```rust
pub trait Patch {
    fn name(&self) -> &'static str;
    fn target_symbols(&self) -> &'static [&'static str];
    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit>;
}
```

- `name` — stable id for logs and drift alerts
- `target_symbols` — qualified symbols the patch depends on, e.g.
    `["typing.Mapping"]`. used for drift detection: if an upstream sync changes one
    of these symbols, the patch is flagged for review
- `rewrite` — return `Edit`s (disjoint byte spans + replacement text) for the
    module at `module_path`, relative to the typeshed `stdlib/` root. an empty vec
    is a no-op for this file

prefer driving edits off the parsed AST rather than raw text scanning — locate
the node, then emit an edit over its range. that keeps a patch precise (exact
identifier matches, correct scoping) and idempotent (re-running over
already-patched source produces no edits)

## worked example: `mapping-key-covariance`

upstream typeshed declares `Mapping` with an invariant key typevar:

```python
class Mapping(Collection[_KT], Generic[_KT, _VT_co]):
    def __getitem__(self, key: _KT, /) -> _VT_co: ...
```

basedpython treats mapping keys as covariant. the covariant typevar `_KT_co` is
already declared in `typing`, so the patch rewrites every `_KT` reference inside
the `Mapping` class to `_KT_co`:

```python
class Mapping(Collection[_KT_co], Generic[_KT_co, _VT_co]):
    def __getitem__(self, key: _KT_co, /) -> _VT_co: ...
```

variance inference can't recover this on its own — `_KT` sits in parameter
position in `__getitem__`, `get`, and friends, which would force an invariant
(or contravariant) reading. the covariance is a deliberate basedpython choice,
so it has to be applied explicitly

the patch walks the module, enters only the `Mapping` class (tracking depth so a
future `sys.version_info` guard wouldn't hide it), and collects the spans of
every `_KT` name within. `MutableMapping`, which needs an invariant key for
`__setitem__`, is a separate class and is left untouched.
`collections.abc.Mapping` and `_collections_abc.Mapping` both re-export
`typing.Mapping`, so the single rewrite covers every surface path

the pep 695 conversion then runs over the patched source and, seeing two
covariant key/value typevars, produces the final header:

```by
class Mapping[out Key, out Value](Collection[Key]):
    def __getitem__(self, key: Key, /) -> Value: ...
```

`MutableMapping`, whose `_KT`/`_VT` stayed invariant, becomes
`class MutableMapping[in out Key, in out Value](Mapping[Key, Value])`

## adding a new patch

1. create `crates/by_typeshed_patch/src/patches/<name>.rs` implementing `Patch`,
    and declare it in `src/patches/mod.rs`
1. register it in `all_patches()` in `crates/by_typeshed_patch/src/lib.rs`
1. write unit tests exercising input → expected output on a minimal snippet,
    plus an idempotency case and a scoping case (what the patch must *not* touch)

after wiring it up, run the patch binary over the real stub to confirm it
reproduces the committed form:

```sh
cargo run --bin by_typeshed_patch -- crates/ty_vendored/vendor/typeshed/stdlib
git diff -- crates/ty_vendored/vendor/typeshed/
```

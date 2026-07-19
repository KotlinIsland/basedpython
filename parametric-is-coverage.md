# parametric type tests (`x is C[args]`): type coverage report

what the feature handles, and where the tests actually are. every "works /
broken" claim below was run against `target/debug/by`, not read off the source.

## the gate — when `is` becomes a type test

`lhs is rhs` is only a parametric type test when **`rhs` evaluates to a
`Type::GenericAlias`** — a subscripted generic *class*
([`semantic_model.rs:129`](crates/ty_python_semantic/src/semantic_model.rs:129)).
everything else stays an ordinary `isinstance` lowering:

| rhs shape                             | example                         | treated as              |
| ------------------------------------- | ------------------------------- | ----------------------- |
| subscripted generic class             | `x is list[int]`, `x is A[int]` | parametric test         |
| bare class                            | `x is int`                      | plain `isinstance`      |
| subscript of a non-class              | `x is candidates[0]`            | plain `isinstance`      |
| union / `type[...]` / other type-form | `x is (int \| str)`             | not a subscript → plain |

so the target is *always* a single subscripted class. union targets, `type[]`
targets, and bare-name targets never reach the classifier by construction —
that's a design boundary, not a gap.

## the four resolution strategies

[`classify_value`](crates/ty_python_semantic/src/types/reified_infer.rs:407)
picks one:

- **`Fold(bool)`** — value's static type settles it. `is_subtype_of` → `True`,
    `is_disjoint_from` → `False`. uses ty's own subtyping, so declared variance is
    respected for free.
- **`TokenEq`** — value is (or is built from) a reified type parameter; compares
    runtime cells (`T == int`).
- **`Probe`** — undecidable statically, target is a user generic; reads
    `__orig_class__` at runtime, matching each arg by effective variance.
- **`ErasedTarget`** — undecidable and target is a builtin collection; no sound
    runtime check exists → `erased-type-check` error.

## coverage by target-type category

| target category                         | resolves to                | test?         | where                                                                                    |
| --------------------------------------- | -------------------------- | ------------- | ---------------------------------------------------------------------------------------- |
| builtin collection, decidable           | Fold                       | ✅            | mdtest "statically decided"; unit `concrete_*_folds`                                     |
| builtin collection, undecidable         | ErasedTarget (error)       | ✅            | mdtest "erased…"; unit `builtin_union_is_an_error`, `erased_builtin_probe_becomes_false` |
| user generic, single param              | Fold / Probe               | ✅            | mdtest "user-defined generic target is valid"; unit `*_probes_orig_class`                |
| user generic, **multiple params**       | Probe with N variances     | ❌            | —                                                                                        |
| **tuple[...]** (fixed)                  | TokenEq via `Tuple::Fixed` | ❌            | — (path verified working)                                                                |
| declared covariant `out T`              | Fold/Probe code 1          | ✅            | mdtest "variance is respected"; unit `covariant_*`                                       |
| declared contravariant `in T`           | Fold/Probe code 2          | ⚠️ thin       | only use-site `in` tested, not a declared `in T` class                                   |
| **declared bivariant** (unused typevar) | Probe code 3               | ❌            | — (reachable; see gaps)                                                                  |
| use-site variance `A[out int]` etc.     | projected                  | ✅            | mdtest "use-site variance"; unit `use_site_*` (the fix this branch shipped)              |
| **generic Protocol** `P[int]`           | Probe                      | ❌ **broken** | — (runtime `TypeError`; see gaps)                                                        |

## coverage by value-type category

| value category                     | resolves to           | test? | where                                      |
| ---------------------------------- | --------------------- | ----- | ------------------------------------------ |
| concrete instance `list[int]`      | Fold                  | ✅    | unit `concrete_match/mismatch`             |
| reified bare typevar `x: T`        | TokenEq               | ✅    | unit `bare_typevar_compares_reified_cell`  |
| reified single-level `x: list[T]`  | TokenEq               | ✅    | unit `structural_typevar_unifies`          |
| reified **nested** `x: A[list[T]]` | TokenEq via recursion | ❌    | — (path verified working)                  |
| dynamic `object` / gradual         | Probe / ErasedTarget  | ✅    | unit `dynamic_value_against_*`             |
| union of user generics             | Probe per arm         | ✅    | unit/mdtest `user_generic_union`           |
| union excluding target             | Fold(false)           | ✅    | unit `union_excluding_target_folds_false`  |
| non-generic union `int \| str`     | Fold(false)           | ⚠️    | verified disjoint→False, no dedicated test |
| literal (promoted) `Literal[5]`    | via `promote`         | ⚠️    | promote runs but no test pins it           |
| intersection value                 | Fold/Probe            | ❌    | —                                          |
| `type[...]` value                  | Fold/Probe            | ❌    | —                                          |
| side-effecting lhs                 | preserved             | ✅    | unit `effectful_lhs_preserved_in_fold`     |

plus narrowing (positive branch, mdtest "positive narrowing") and `===`
untouched (unit `identity_operator_untouched`).

## gaps found (grounded)

ordered by severity.

1. **generic Protocol target → runtime `TypeError` (blocker, untested).**
    `x is P[int]` for a `Protocol P` is classified as a `Probe` — a Protocol
    isn't a builtin collection, so `target_carries_orig_class` returns true
    ([`reified_infer.rs:361`](crates/ty_python_semantic/src/types/reified_infer.rs:361)).
    the probe runs `isinstance(value, P)`, which raises
    `TypeError: Instance and class checks can only be used with @runtime_checkable protocols` unless `P` is decorated. verified — the transpiled program
    crashes. the classifier should either reject a non-`@runtime_checkable`
    Protocol target (like it rejects builtins) or the probe should degrade. no
    test exists.

1. **multi-parameter probe, untested.** every probe test uses a one-param
    generic. `x is Pair[int, str]` on an `object` emits
    `_parametric_is(x, Pair[int, str], (0, 0))` — correct, but the two-entry
    variance tuple and the polyfill's per-arg loop are only exercised by the
    one-element case in tests. the trailing-comma one-tuple special-case *is*
    tested; the plural case is not.

1. **`ArgVariance::Bivariant` (code 3) untested.** reachable: a typevar unused
    in the class body infers bivariant, so `class Pair[K, V]` with an empty body
    probes with `(3, 3)`. the polyfill's `if v == 3: continue` branch and the
    `Bivariant` arm of `target_variances` have no test. (arguably sound — a truly
    bivariant param makes all specializations mutually assignable — but it's an
    untested runtime path with a subtle justification.)

1. **`tuple[...]` target, untested — but works.** `x: tuple[T, U] is tuple[int, str]` lowers to `(T == int and U == str)`. exercises the entire
    `Tuple::Fixed` branch of `unify_specializations`
    ([`reified_infer.rs:533`](crates/ty_python_semantic/src/types/reified_infer.rs:533)),
    which nothing else reaches. verified correct; zero coverage.

1. **nested-generic reified value, untested — but works.** `x: A[list[T]] is A[list[int]]` lowers to `(T == int)`, driving the recursive
    `unify_argument` → `unify_specializations` descent
    ([`reified_infer.rs:596`](crates/ty_python_semantic/src/types/reified_infer.rs:596)).
    verified correct; zero coverage.

1. **thin declared-contravariant coverage.** `covariant_subtype_folds_true`
    pins a declared `out T` fold, but no test pins a declared `in T` *class*
    fold/probe — only use-site `in` projection is tested. the declared-`in`
    variance-code path is inferred-correct but unverified.

1. **minor untested value categories:** intersection values, `type[...]`
    values, non-generic-union values, and literal-promotion all flow through
    `Fold`/`promote` without a dedicated test. lower risk (they bottom out in
    ty's own subtyping) but undocumented.

## recommendation

- fix #1 before it ships — a Protocol target is an easy footgun and currently
    compiles to a crash. simplest sound fix: treat a non-`@runtime_checkable`
    Protocol target like a builtin (extend the `ErasedTarget` reasoning), with an
    mdtest.
- add mdtest cases for the tuple target and nested reified value (#4, #5) —
    they cover whole unify branches and already pass, so they're cheap and lock
    in behavior.
- add a two-param probe + a bivariant-typevar probe (#2, #3) to exercise the
    plural variance tuple and code-3 branch.
- the rest (#6, #7) are worth a small mdtest section but aren't urgent.

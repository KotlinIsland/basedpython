pub use ruff_python_ast::PythonVersion;

#[derive(Debug, Clone)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent transpile toggle, not a state machine"
)]
pub struct Config {
    pub min_version: PythonVersion,
    /// when true, source is plain python — no basedpython transforms are applied
    pub is_python: bool,
    /// when true, source is a stub file (`.pyi` / `.byi`) — disables transforms
    /// that don't make sense for stubs (e.g. rewriting `typing_extensions`
    /// imports to `typing`, since stubs use `typing_extensions` intentionally)
    pub is_stub: bool,
    /// when true (the default), every `import` / `from import` is lowered
    /// to a lazy form: PEP 810 `lazy` keyword for `min_version >= 3.15`,
    /// otherwise a runtime polyfill that wraps `importlib.util.LazyLoader`.
    /// Tests that compare exact transpile output should set this to `false`
    /// to keep their expected strings free of the lazy preamble
    pub lazy_imports: bool,
    /// when true, every transpiled file is prefixed with
    /// `from __future__ import annotations`, deferring all annotation
    /// evaluation. off by default: forward references are handled surgically
    /// by quoting (see `auto_quote`), and the polyfilled type names
    /// (`Intersection`, `Not`, lazy-imported names) are already runtime-safe
    /// on their own. left as an opt-in for users who specifically want
    /// PEP 563 semantics across every annotation
    pub inject_future_annotations: bool,
    /// when true (the default), `reverse_transpile` strips imports whose
    /// bindings became unused after the reverse rewrites ran (e.g.
    /// `from typing import Callable` after `Callable[...]` was rewritten to
    /// the arrow form). Tests that compare verbatim preservation should set
    /// this to `false`
    pub prune_unused_imports_after_reverse: bool,
    /// which runtime `_soundness_check` insertions are enabled (all on by
    /// default). each field guards one syntactic position where ty's
    /// inference rests on an unverifiable assumption; see [`SoundnessPositions`].
    /// Tests that compare exact transpile output should leave this
    /// [`SoundnessPositions::none`] unless they exercise the checks themselves
    pub soundness: SoundnessPositions,
    /// when true, a function with a `raises` clause is wrapped in a runtime
    /// guard that fails when it raises something the clause does not include.
    /// off by default: static checking is unconditional, so this is only for
    /// builds that want the contract enforced against callers the checker never
    /// saw. a clause with no faithful runtime test — `raises ...`, or a set with
    /// no runtime spelling such as a negation — is never guarded
    pub runtime_raises_checks: bool,
    /// when true (the default), the `<value> cast? <type>` checked-cast
    /// operator is lowered to a runtime `isinstance` test that yields the
    /// value or `None`. when false, using `cast?` is a transpile error — the
    /// feature is simply unavailable
    pub checked_cast: bool,
}

/// Per-position toggles for the runtime type-soundness checks. Each field
/// enables validation at one syntactic position where ty accepts a value on
/// an annotation-level claim it can't verify at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent per-position toggles, not a state machine"
)]
pub struct SoundnessPositions {
    /// results of calls whose type is typevar-derived — a generic function's
    /// return (`t[T]() -> T`) or a method bound to a specialized generic
    /// instance (`dict[str, int].get`)
    pub generic_calls: bool,
    /// element reads out of a specialized container (`a[0]` on `list[str]`)
    pub projections: bool,
    /// loop / comprehension elements drawn from a specialized iterable
    pub iterations: bool,
    /// explicit `Any` (or a context-solved generic result) flowing into an
    /// annotated assignment target (`a: str = any_val`)
    pub assignments: bool,
    /// a returned value validated against the enclosing function's declared
    /// return type (`def g() -> str: return any_val`)
    pub returns: bool,
    /// a call argument validated against its matched parameter's annotation
    /// (`takes(any_val)` where `takes(s: str)`)
    pub arguments: bool,
    /// a function's own parameters validated against their annotations at
    /// entry, defending its contract against callers the checker never saw
    /// (untyped or third-party code). off in the default set — it runs on
    /// every call, so it's opt-in — but included in [`Self::all`]
    pub parameters: bool,
}

impl SoundnessPositions {
    /// every position enabled, including the opt-in defensive `parameters`
    /// entry checks (the CLI `--soundness all`)
    pub fn all() -> Self {
        Self {
            parameters: true,
            ..Self::defaults()
        }
    }

    /// the positions enabled without an explicit request: every inference-gap
    /// check, but *not* the defensive `parameters` entry checks (those add a
    /// check on every call, so they stay opt-in)
    pub fn defaults() -> Self {
        Self {
            generic_calls: true,
            projections: true,
            iterations: true,
            assignments: true,
            returns: true,
            arguments: true,
            parameters: false,
        }
    }

    /// no position enabled — the pass is a no-op
    pub fn none() -> Self {
        Self {
            generic_calls: false,
            projections: false,
            iterations: false,
            assignments: false,
            returns: false,
            arguments: false,
            parameters: false,
        }
    }

    /// whether any position is enabled
    pub fn any(self) -> bool {
        self.generic_calls
            || self.projections
            || self.iterations
            || self.assignments
            || self.returns
            || self.arguments
            || self.parameters
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            min_version: PythonVersion::PY310,
            is_python: false,
            is_stub: false,
            lazy_imports: true,
            inject_future_annotations: false,
            prune_unused_imports_after_reverse: true,
            soundness: SoundnessPositions::defaults(),
            runtime_raises_checks: false,
            checked_cast: true,
        }
    }
}

impl Config {
    /// Config used by the in-tree transform unit tests. Identical to
    /// [`Config::default`] but with `lazy_imports` and
    /// `prune_unused_imports_after_reverse` disabled so test expected
    /// strings don't need to include the lazy preamble or worry about pruning,
    /// and all soundness checks off so plain output isn't peppered with them
    pub fn test_default() -> Self {
        Self {
            lazy_imports: false,
            prune_unused_imports_after_reverse: false,
            soundness: SoundnessPositions::none(),
            ..Self::default()
        }
    }
}

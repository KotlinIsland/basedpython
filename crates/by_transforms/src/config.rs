use std::collections::BTreeMap;

pub use ty_python_semantic::types::soundness_sites::SoundnessPositions;

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
    /// when true (the default), the polyfill defers a `from` import below 3.15 by
    /// binding nothing where it is written and reading the name through the
    /// module's `_LazyFrom` on first use. off, a `from` import stays an ordinary
    /// import, and only module imports are deferred. the interpreted fallback a
    /// natively compiled module embeds turns it off: the compiled functions read
    /// the module's globals by name, where a deferred name is not until something
    /// has read it
    pub defer_from_imports: bool,
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
    /// the values a `build:` block's stamps take, supplied by whatever drove
    /// this transpile — `by build` and `by run` settle them once, ahead of the
    /// pipeline.
    ///
    /// they arrive as config rather than being read here because transpile
    /// output has to stay a function of the source plus this config. a pipeline
    /// that asked git for the commit itself would give a different answer to a
    /// re-stage than it gave the build that re-stage is patching, and the two
    /// trees would disagree about what they are
    ///
    /// a stamp with no entry here falls back to the default written in the
    /// block; one with neither is a transpile error, which is the whole point of
    /// declaring it
    pub stamps: BTreeMap<String, String>,
    /// when true (the default), a closure created inside a loop captures the
    /// loop's bindings *by value*, so each iteration's closure sees that
    /// iteration's element instead of every closure sharing the one cell
    /// python leaves behind. Tests that compare exact transpile output should
    /// set this to `false` unless they exercise the capture itself
    pub unique_loop_bindings: bool,
    /// how a float or complex literal written in a type position is spelled in
    /// the emitted python. see [`FloatLiteralLowering`]
    pub float_literals: FloatLiteralLowering,
    /// the module the emitted code imports its runtime helpers from, or `None`
    /// to paste the definitions in. a build names the package-qualified copy it
    /// writes into its tree; a transpile with nowhere to write one leaves it
    /// `None`
    pub runtime_module: Option<String>,
    /// when true, the module is a `by repl` session: the inputs typed so far, transpiled again
    /// with each new one, of which only the statements written for the new input run. off by
    /// default. two things follow. the module never runs as a script, so a top-level `main`
    /// gets no `__main__` guard: it is a function like any other, and nothing calls it. and an
    /// earlier input's statements have already run, so an import the emitted code needs is
    /// written on a line of its own rather than joined to one of the module's own imports,
    /// which would change a statement that cannot run again
    pub session: bool,
}

/// What a float or complex literal type becomes in the emitted python.
///
/// basedpython reads `a: 1.5` as a literal type, and python has no spelling for
/// one: PEP 586 admits only `None`, `int`, `bool`, `str`, `bytes` and enum
/// members into `Literal[...]`, so `Literal[1.5]` — while perfectly happy at
/// runtime, since `typing` does not check what it is handed — is rejected by
/// every type checker that reads the output, ty included.
///
/// Leaving the literal bare is not an option: `a: int | 3.5` would be a
/// `TypeError` the moment the annotation is evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FloatLiteralLowering {
    /// the nominal type the literal is one of — `a: 1.5` becomes `a: float`,
    /// `a: 2j` becomes `a: complex`. precision is lost, and the output is
    /// something every checker accepts
    #[default]
    Nominal,
    /// the literal kept inside `Literal[...]` — `a: 1.5` becomes
    /// `a: Literal[1.5]`. the precision survives and the output still runs, but
    /// a checker reading it reports the argument as invalid
    Literal,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            min_version: PythonVersion::PY310,
            is_python: false,
            is_stub: false,
            lazy_imports: true,
            defer_from_imports: true,
            inject_future_annotations: false,
            prune_unused_imports_after_reverse: true,
            soundness: SoundnessPositions::defaults(),
            runtime_raises_checks: false,
            unique_loop_bindings: true,
            float_literals: FloatLiteralLowering::Nominal,
            runtime_module: None,
            stamps: BTreeMap::new(),
            session: false,
        }
    }
}

impl Config {
    /// Config used by the in-tree transform unit tests. Identical to
    /// [`Config::default`] but with `lazy_imports` and
    /// `prune_unused_imports_after_reverse` disabled so test expected
    /// strings don't need to include the lazy preamble or worry about pruning,
    /// and all soundness checks off so plain output isn't peppered with them
    #[cfg(test)]
    pub(crate) fn test_default() -> Self {
        Self {
            lazy_imports: false,
            prune_unused_imports_after_reverse: false,
            soundness: SoundnessPositions::none(),
            ..Self::default()
        }
    }
}

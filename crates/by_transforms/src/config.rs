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
    /// when true (the default), insert runtime `_soundness_check` validations
    /// at expressions whose inferred type rests on an assumption the checker
    /// cannot verify: typevar solutions at generic call sites, element
    /// projections out of annotated containers, and explicit `Any` flowing
    /// into a declared binding. Tests that compare exact transpile output
    /// should set this to `false` unless they exercise the checks themselves
    pub soundness_checks: bool,
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
            soundness_checks: true,
        }
    }
}

impl Config {
    /// Config used by the in-tree transform unit tests. Identical to
    /// [`Config::default`] but with `lazy_imports` and
    /// `prune_unused_imports_after_reverse` disabled so test expected
    /// strings don't need to include the lazy preamble or worry about pruning
    pub fn test_default() -> Self {
        Self {
            lazy_imports: false,
            prune_unused_imports_after_reverse: false,
            soundness_checks: false,
            ..Self::default()
        }
    }
}

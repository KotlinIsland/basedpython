//! `typing.Mapping` key-type covariance
//!
//! upstream typeshed declares `Mapping` with an invariant key typevar (`_KT`).
//! basedpython treats mapping keys as covariant, so within the `Mapping` class
//! the key references the covariant `_KT_co` typevar (already declared in
//! `typing`) instead. `MutableMapping`, which needs an invariant key for
//! `__setitem__`, keeps `_KT` — only the `Mapping` class itself is rewritten
//!
//! the patch runs after reverse-transpile but before the pep 695 ruff-fix, so
//! it sees legacy `TypeVar` + `Generic[...]` form: the key typevar appears as
//! plain `_KT` name references in the class bases and method signatures.
//! `collections.abc.Mapping` and `_collections_abc.Mapping` both re-export
//! `typing.Mapping`, so rewriting the one definition covers every surface path

use std::path::Path;

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ModModule, Stmt};
use ruff_python_parser::Parsed;

use crate::{Edit, Patch};

/// module that owns the canonical `Mapping` definition
const MODULE: &str = "typing";
/// class whose key typevar is made covariant
const CLASS: &str = "Mapping";
/// invariant key typevar upstream uses
const INVARIANT_KEY: &str = "_KT";
/// covariant key typevar basedpython uses
const COVARIANT_KEY: &str = "_KT_co";

pub struct MappingKeyCovariance;

impl Patch for MappingKeyCovariance {
    fn name(&self) -> &'static str {
        "mapping-key-covariance"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &["typing.Mapping"]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, _source: &str) -> Vec<Edit> {
        if crate::module_qualname(module_path).as_deref() != Some(MODULE) {
            return Vec::new();
        }

        let mut collector = MappingKeyReferences::default();
        for stmt in &parsed.syntax().body {
            collector.visit_stmt(stmt);
        }

        collector
            .spans
            .into_iter()
            .map(|(start, end)| Edit {
                start,
                end,
                replacement: COVARIANT_KEY.to_string(),
            })
            .collect()
    }
}

/// collects the byte spans of every `_KT` reference that lexically sits inside a
/// `Mapping` class body or header. tracking depth (rather than only scanning the
/// top level) keeps the patch correct if upstream ever wraps the definition in a
/// `sys.version_info` guard, while still leaving `MutableMapping` untouched
#[derive(Default)]
struct MappingKeyReferences {
    depth: u32,
    spans: Vec<(usize, usize)>,
}

impl<'a> SourceOrderVisitor<'a> for MappingKeyReferences {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if let Stmt::ClassDef(class) = stmt
            && class.name.as_str() == CLASS
        {
            self.depth += 1;
            walk_stmt(self, stmt);
            self.depth -= 1;
        } else {
            walk_stmt(self, stmt);
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if self.depth > 0
            && let Expr::Name(name) = expr
            && name.id.as_str() == INVARIANT_KEY
        {
            self.spans
                .push((name.range.start().to_usize(), name.range.end().to_usize()));
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(path: &str, src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = MappingKeyCovariance.rewrite(Path::new(path), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn rewrites_every_key_reference_in_mapping() {
        let src = "\
class Mapping(Collection[_KT], Generic[_KT, _VT_co]):
    def __getitem__(self, key: _KT) -> _VT_co: ...
    def get(self, key: _KT, default: _T) -> _VT_co | _T: ...
    def keys(self) -> KeysView[_KT]: ...
    def __contains__(self, key: object) -> bool: ...
";
        let expected = "\
class Mapping(Collection[_KT_co], Generic[_KT_co, _VT_co]):
    def __getitem__(self, key: _KT_co) -> _VT_co: ...
    def get(self, key: _KT_co, default: _T) -> _VT_co | _T: ...
    def keys(self) -> KeysView[_KT_co]: ...
    def __contains__(self, key: object) -> bool: ...
";
        assert_eq!(run("typing.byi", src), expected);
    }

    #[test]
    fn idempotent_when_already_covariant() {
        let src = "class Mapping(Collection[_KT_co], Generic[_KT_co, _VT_co]): ...\n";
        assert_eq!(run("typing.byi", src), src);
    }

    #[test]
    fn leaves_mutable_mapping_invariant() {
        let src = "\
class Mapping(Generic[_KT, _VT_co]):
    def __getitem__(self, key: _KT) -> _VT_co: ...
class MutableMapping(Mapping[_KT, _VT]):
    def __setitem__(self, key: _KT, value: _VT) -> None: ...
";
        let expected = "\
class Mapping(Generic[_KT_co, _VT_co]):
    def __getitem__(self, key: _KT_co) -> _VT_co: ...
class MutableMapping(Mapping[_KT, _VT]):
    def __setitem__(self, key: _KT, value: _VT) -> None: ...
";
        assert_eq!(run("typing.byi", src), expected);
    }

    #[test]
    fn skips_modules_that_do_not_own_mapping() {
        let src = "class Mapping(Generic[_KT, _VT_co]): ...\n";
        assert_eq!(run("builtins.byi", src), src);
    }

    #[test]
    fn module_qualname_handles_packages_and_init() {
        assert_eq!(
            crate::module_qualname(Path::new("typing.byi")).as_deref(),
            Some("typing")
        );
        assert_eq!(
            crate::module_qualname(Path::new("os/path.byi")).as_deref(),
            Some("os.path")
        );
        assert_eq!(
            crate::module_qualname(Path::new("asyncio/__init__.byi")).as_deref(),
            Some("asyncio")
        );
    }
}

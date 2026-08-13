//! basedpython: which module-level symbols a file declares `private`.
//!
//! `private` is a transpile-time marker everywhere else in ty — the lowering
//! renames the symbol with a `_` prefix and drops it from `__all__`, with no
//! type-level effect. What it *does* mean semantically is a module boundary:
//! the symbol is part of the module's implementation, so another module must
//! not import it. [`private_symbols`] collects the marked names so
//! `infer_import_from_definition` can report [`PRIVATE_IMPORT`].
//!
//! Only module-level declarations are collected. A `private` member of a class
//! is name-mangled rather than renamed, and is unreachable through an import
//! anyway.
//!
//! [`PRIVATE_IMPORT`]: super::diagnostic::PRIVATE_IMPORT

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, Stmt};
use ruff_text_size::Ranged;
use rustc_hash::FxHashSet;

use crate::Db;

/// The module-level names `file` declares `private`.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub fn private_symbols(db: &dyn Db, file: File) -> FxHashSet<Name> {
    let _span = tracing::trace_span!("private_symbols", file=?file.path(db)).entered();

    let parsed = parsed_module(db, db.program_file(file).python_file(db)).load(db);
    let source = source_text(db, file);

    let mut names = FxHashSet::default();
    for stmt in parsed.suite() {
        match stmt {
            Stmt::TypeAlias(alias) if alias.is_private => {
                if let ast::Expr::Name(name) = alias.name.as_ref() {
                    names.insert(name.id.clone());
                }
            }
            Stmt::FunctionDef(function) => {
                if has_private_marker(&source, &function.decorator_list) {
                    names.insert(Name::new(function.name.as_str()));
                }
            }
            Stmt::ClassDef(class) => {
                if has_private_marker(&source, &class.decorator_list) {
                    names.insert(Name::new(class.name.as_str()));
                }
            }
            _ => {}
        }
    }
    names.shrink_to_fit();
    names
}

/// Whether a decorator list carries the synthetic `private` modifier.
///
/// The parser models a modifier keyword as a decorator whose source range does
/// not start with `@`; a real `@private` decorator is an ordinary decorator and
/// must not be mistaken for the modifier.
fn has_private_marker(source: &str, decorators: &[ast::Decorator]) -> bool {
    decorators.iter().any(|decorator| {
        matches!(&decorator.expression, ast::Expr::Name(name) if name.id.as_str() == "private")
            && source
                .as_bytes()
                .get(usize::from(decorator.range().start()))
                .copied()
                != Some(b'@')
    })
}

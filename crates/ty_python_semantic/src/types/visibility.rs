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

/// basedpython: the name a `private` method is reached by in the emitted
/// python, for an attribute whose inferred type is `member_type`.
///
/// `None` when the attribute is not a private method.
pub(crate) fn private_method_name<'db>(
    db: &'db dyn Db,
    env: &crate::types::ProgramEnvironment<'db>,
    receiver: crate::types::Type<'db>,
    member_type: crate::types::Type<'db>,
    member: &str,
) -> Option<String> {
    // a method reaches the access site as the bound method it produced, so its
    // own type answers. a property hands back whatever its getter returns
    // instead — an `int` says nothing about the declaration — so the class is
    // asked for the member it actually holds
    let function = declared_function(db, member_type)
        .or_else(|| declared_function(db, class_member(db, env, receiver, member)?))?;
    if !function.has_known_decorator(db, super::function::FunctionDecorators::PRIVATE) {
        return None;
    }
    let scope = function.definition(db).scope(db);
    let index = crate::semantic_index(db, scope.program_file(db));
    let class = super::infer::nearest_enclosing_class(db, index, scope)?;
    Some(mangled_private_name(class.name(db).as_str(), member))
}

/// The function a member's type stands for — the member itself, or the getter
/// of the property wrapping it.
fn declared_function<'db>(
    db: &'db dyn Db,
    member_type: crate::types::Type<'db>,
) -> Option<super::function::FunctionType<'db>> {
    match member_type {
        crate::types::Type::FunctionLiteral(function) => Some(function),
        crate::types::Type::BoundMethod(method) => Some(method.function(db)),
        crate::types::Type::PropertyInstance(property) => {
            declared_function(db, property.getter(db)?)
        }
        _ => None,
    }
}

/// The member a class holds under `name`, read off the class rather than
/// through an instance, so a descriptor is not resolved on the way.
fn class_member<'db>(
    db: &'db dyn Db,
    env: &crate::types::ProgramEnvironment<'db>,
    receiver: crate::types::Type<'db>,
    name: &str,
) -> Option<crate::types::Type<'db>> {
    receiver
        .erase_restriction(db)
        .nominal_class(db, env)?
        .class_member(db, env, name, super::MemberLookupPolicy::default())
        .place
        .ignore_possibly_undefined()
}

/// basedpython: the attribute name a `private` class member is reached by in
/// the emitted python — python's own name mangling, written out in full.
///
/// A member lowered to `__helper` is stored under `_A__helper` when `A`'s body
/// is executed. Python applies that rewrite lexically, to every `__name` it
/// reads inside a class body, so a reference from anywhere else — a subclass, a
/// module-level function — would land on a different attribute or none at all.
/// Spelling the mangled name out reaches the member from all of them.
///
/// The rule is python's: leading underscores are stripped from the class name,
/// and a class named only with underscores mangles nothing.
pub(crate) fn mangled_private_name(class: &str, member: &str) -> String {
    let class = class.trim_start_matches('_');
    if class.is_empty() {
        return format!("__{member}");
    }
    format!("_{class}__{member}")
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

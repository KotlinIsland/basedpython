//! basedpython: which module-level symbols a file declares `private`.
//!
//! `private` is a transpile-time marker everywhere else in ty — the lowering
//! renames the symbol with a `_` prefix and drops it from `__all__`, with no
//! type-level effect. What it *does* mean semantically is a module boundary:
//! the symbol is part of the module's implementation, so another module must
//! not import it. [`private_symbols`] collects the marked names so
//! `infer_import_from_definition` can report [`PRIVATE_IMPORT`].
//!
//! Only module-level declarations are collected — functions, classes, type
//! aliases and variables. A `private` member of a class is name-mangled rather
//! than renamed, and is unreachable through an import anyway. The same set also
//! answers a private symbol reached as an attribute of its module (`m.x`), and
//! the lowering renames every reference to one from it.
//!
//! A dunder is the exception, and the rest of this module is about it. Python
//! mangles only a name with at most one trailing underscore, so a `private`
//! dunder keeps the name it was written with. For every dunder but one that
//! makes `private` a no-op, which the parser reports. The one it does not is
//! `__init__`: [`restricted_constructor`] answers which class declared a private
//! one, so that construction can be refused wherever the declaring class's own
//! body does not reach — see [`PRIVATE_CONSTRUCTOR`].
//!
//! [`PRIVATE_IMPORT`]: super::diagnostic::PRIVATE_IMPORT
//! [`PRIVATE_CONSTRUCTOR`]: super::diagnostic::PRIVATE_CONSTRUCTOR

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::helpers::MemberVisibility;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, Stmt};
use ruff_text_size::Ranged;
use rustc_hash::FxHashSet;

use ruff_python_ast::statement_visitor::{StatementVisitor, walk_stmt};
use ty_python_core::ProgramFile;
use ty_python_core::ast_ids::HasScopedUseId;
use ty_python_core::definition::DefinitionState;
use ty_python_core::scope::{ScopeId, ScopeKind};

use crate::Db;

/// The module-level names `file` declares `private`.
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub fn private_symbols(db: &dyn Db, file: File) -> FxHashSet<Name> {
    let _span = tracing::trace_span!("private_symbols", file=?file.path(db)).entered();

    let parsed = parsed_module(db, db.program_file(file).python_file(db)).load(db);
    let source = source_text(db, file);

    let mut collector = PrivateSymbolCollector {
        source: &source,
        names: FxHashSet::default(),
    };
    collector.visit_body(parsed.suite());
    let mut names = collector.names;
    names.shrink_to_fit();
    names
}

/// basedpython: collects [`private_symbols`] — the module-level declarations,
/// including those written inside a module-level `if` or `try`, but none from a
/// function's or a class's body
struct PrivateSymbolCollector<'src> {
    source: &'src str,
    names: FxHashSet<Name>,
}

impl<'a> StatementVisitor<'a> for PrivateSymbolCollector<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::TypeAlias(alias) if alias.is_private => {
                if let ast::Expr::Name(name) = alias.name.as_ref() {
                    self.names.insert(name.id.clone());
                }
            }
            Stmt::FunctionDef(function) => {
                if has_private_marker(self.source, &function.decorator_list) {
                    self.names.insert(Name::new(function.name.as_str()));
                }
            }
            Stmt::ClassDef(class) => {
                if has_private_marker(self.source, &class.decorator_list) {
                    self.names.insert(Name::new(class.name.as_str()));
                }
            }
            // `private count: int = 0`, `private count = 0`, `private let count = 0`
            // — a variable's visibility rides in its declaration marker
            Stmt::AnnAssign(assign)
                if ruff_python_ast::helpers::declaration_marker_visibility(&assign.annotation)
                    == MemberVisibility::Private =>
            {
                if let ast::Expr::Name(name) = assign.target.as_ref() {
                    self.names.insert(name.id.clone());
                }
            }
            _ => walk_stmt(self, stmt),
        }
    }
}

/// basedpython: reports each `private` symbol the module lists in `__all__` —
/// in every idiom `__all__` is built with: assigned, annotated, `+=`,
/// `.append(...)`, `.extend([...])`
pub(crate) fn check_private_exports(context: &super::context::InferContext<'_, '_>, body: &[Stmt]) {
    let db = context.db();
    let private = private_symbols(db, context.file());
    if private.is_empty() {
        return;
    }
    let mut strings = DunderAllStrings::default();
    strings.visit_body(body);
    for string in strings.found {
        let value = string.value.to_str();
        if !private.contains(&Name::new(value)) {
            continue;
        }
        let Some(builder) = context.report_lint(&super::diagnostic::PRIVATE_EXPORT, string) else {
            continue;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{value}` is private, so it cannot be in `__all__`"
        ));
        diagnostic.info(
            "the lowering renames it with a leading underscore, so `from ... import *` would not find it",
        );
    }
}

/// basedpython: the string literals a module writes into `__all__` at module level
#[derive(Default)]
struct DunderAllStrings<'a> {
    found: Vec<&'a ast::ExprStringLiteral>,
}

impl<'a> DunderAllStrings<'a> {
    fn collect(&mut self, value: &'a ast::Expr) {
        let elements = match value {
            ast::Expr::List(list) => &list.elts,
            ast::Expr::Tuple(tuple) => &tuple.elts,
            ast::Expr::StringLiteral(string) => {
                self.found.push(string);
                return;
            }
            _ => return,
        };
        for element in elements {
            if let ast::Expr::StringLiteral(string) = element {
                self.found.push(string);
            }
        }
    }
}

fn is_dunder_all(expr: &ast::Expr) -> bool {
    matches!(expr, ast::Expr::Name(name) if name.id.as_str() == "__all__")
}

impl<'a> StatementVisitor<'a> for DunderAllStrings<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::Assign(assign) if assign.targets.iter().any(is_dunder_all) => {
                self.collect(&assign.value);
            }
            Stmt::AnnAssign(assign) if is_dunder_all(&assign.target) => {
                if let Some(value) = &assign.value {
                    self.collect(value);
                }
            }
            Stmt::AugAssign(assign) if is_dunder_all(&assign.target) => self.collect(&assign.value),
            Stmt::Expr(expr) => {
                if let ast::Expr::Call(call) = expr.value.as_ref()
                    && let ast::Expr::Attribute(attribute) = call.func.as_ref()
                    && is_dunder_all(&attribute.value)
                    && matches!(attribute.attr.as_str(), "append" | "extend")
                {
                    for argument in &call.arguments.args {
                        self.collect(argument);
                    }
                }
            }
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            _ => walk_stmt(self, stmt),
        }
    }
}

/// basedpython: a class body read of a module-level `private` name that the body
/// binds on some paths to the read and not on others. python reads the class's
/// own binding where it exists and the module's otherwise, and the lowering
/// renames the module's, so no one emitted name reads both
pub(crate) fn check_private_class_read<'db>(
    context: &super::context::InferContext<'db, '_>,
    index: &ty_python_core::SemanticIndex<'db>,
    scope: ScopeId<'db>,
    name: &ast::ExprName,
) {
    let db = context.db();
    if index.scope(scope.file_scope_id(db)).kind() != ScopeKind::Class
        || !private_symbols(db, context.file()).contains(&name.id)
        || class_scope_binding(db, scope, name) != ClassBinding::Maybe
    {
        return;
    }
    let Some(builder) = context.report_lint(&super::diagnostic::INVALID_VISIBILITY, name) else {
        return;
    };
    let mut diagnostic = builder.into_diagnostic(format_args!(
        "`{name}` may read this class body's own `{name}` or the module's private one",
        name = name.id,
    ));
    diagnostic.info(
        "the lowering renames the module's with a leading underscore, so no one name reads both: \
         bind it on every path through the class body, or on none",
    );
}

/// basedpython: whether a class body has bound a name by the point a read of it
/// is written. python reads a class's own binding only once the body has made
/// it; before that the read resolves outward, exactly as it would from a
/// function
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClassBinding {
    Bound,
    Unbound,
    /// bound on some paths to the read and not on others
    Maybe,
}

/// basedpython: see [`ClassBinding`]. `reference` must be a read in `scope`
fn class_scope_binding<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    reference: &ast::ExprName,
) -> ClassBinding {
    let use_id = reference.scoped_use_id(db, scope.program_file(db));
    let (mut defined, mut undefined) = (false, false);
    for binding in ty_python_core::use_def_map(db, scope).bindings_at_use(use_id) {
        match binding.binding {
            DefinitionState::Defined(_) => defined = true,
            DefinitionState::Undefined | DefinitionState::Deleted => undefined = true,
        }
    }
    match (defined, undefined) {
        (true, false) => ClassBinding::Bound,
        (false, _) => ClassBinding::Unbound,
        (true, true) => ClassBinding::Maybe,
    }
}

/// basedpython: how a bare name read or written in a class body is emitted, when
/// it names a member the class declares with a visibility keyword — `y = x + 1`
/// after `private x = 1`, `alias = helper`, `@size.setter` over a protected
/// property. `None` for any other name
///
/// the class body spells the member as it is declared (`__x`, `_x`): python
/// mangles a class body's names lexically, so `__x` there reaches the same
/// attribute the declaration made
pub(crate) fn class_body_member_name<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    reference: &ast::ExprName,
) -> Option<String> {
    let index = crate::semantic_index(db, file);
    let file_scope = index.try_expression_scope_id(&ast::ExprRef::from(reference))?;
    let class_node = index.scope(file_scope).node().as_class()?;
    if reference.ctx.is_load()
        && class_scope_binding(db, file_scope.to_scope_id(db, file), reference)
            == ClassBinding::Unbound
    {
        return None;
    }
    let definition = index.expect_single_definition(class_node);
    let class = super::infer::original_class_type(db, definition)?.as_static()?;
    let visibility = *class.member_visibilities(db).get(reference.id.as_str())?;
    ruff_python_stdlib::basedpython::visibility_rename(
        reference.id.as_str(),
        visibility.name_prefix(),
    )
}

/// basedpython: whether the name `reference` reads or writes resolves to the
/// module scope — no scope between it and the module binds it, or one declares
/// it `global`. `None` when ty did not index the reference
///
/// a class body is flow-sensitive here as python is: a read the body has not yet
/// bound resolves outward. a read that is bound on some paths and not on others
/// answers `false`, and the checker reports it where the name is private
pub(crate) fn resolves_to_module_scope<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    reference: &ast::ExprName,
) -> Option<bool> {
    let index = crate::semantic_index(db, file);
    let reference_scope = index.try_expression_scope_id(&ast::ExprRef::from(reference))?;
    for (ancestor_id, scope) in index.ancestor_scopes(reference_scope) {
        let is_own_scope = ancestor_id == reference_scope;
        // a class body is skipped by name resolution from a scope nested in it
        if scope.kind() == ScopeKind::Class && !is_own_scope {
            continue;
        }
        let scope_id = ancestor_id.to_scope_id(db, file);
        let table = ty_python_core::place_table(db, scope_id);
        let Some(symbol) = table.symbol_by_name(&reference.id) else {
            continue;
        };
        // `global count` hands the name to the module, whatever this scope does
        if symbol.is_global() {
            return Some(true);
        }
        // a binding, or a bare annotation, makes the name local to its scope
        if !(symbol.is_bound() || symbol.is_declared()) {
            continue;
        }
        if scope.kind() == ScopeKind::Class && reference.ctx.is_load() {
            match class_scope_binding(db, scope_id, reference) {
                ClassBinding::Unbound => continue,
                ClassBinding::Bound | ClassBinding::Maybe => return Some(false),
            }
        }
        return Some(scope.kind() == ScopeKind::Module);
    }
    Some(false)
}

/// basedpython: how a member reached through a receiver is restricted on one
/// class the receiver can be
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberAccess<'db> {
    /// declared with no visibility keyword, or by a class whose body cannot be read
    Unrestricted,
    /// declared with `visibility` by `owner`
    Restricted {
        visibility: MemberVisibility,
        owner: super::class::StaticClassLiteral<'db>,
    },
}

impl MemberAccess<'_> {
    /// the name an access is emitted under, `None` when it keeps the written one
    fn emitted_name(self, db: &dyn Db, member: &str) -> Option<String> {
        match self {
            MemberAccess::Unrestricted => None,
            MemberAccess::Restricted { visibility, owner } => {
                emitted_member_name(db, visibility, owner, member)
            }
        }
    }
}

/// basedpython: the classes a receiver can be emit a member under different
/// names — one restricts it and another does not, or two restrict it to names
/// of their own — so no single access reaches all of them
#[derive(Debug)]
pub(crate) struct AmbiguousAccess;

/// basedpython: how `member` is restricted on each class `receiver` can be that
/// defines it — the one answer both the access check and the lowering's rename
/// read, so the two cannot disagree about a shape. every access returned is
/// emitted under the same name, and each has to be reachable from the access.
/// a class that has no such member at all is left to the checker's own
/// unresolved-attribute diagnostic, so `A | None` is not ambiguous
pub(crate) fn member_access<'db>(
    db: &'db dyn Db,
    env: &crate::types::ProgramEnvironment<'db>,
    receiver: crate::types::Type<'db>,
    member: &str,
) -> Result<smallvec::SmallVec<[MemberAccess<'db>; 2]>, AmbiguousAccess> {
    let mut sources = smallvec::SmallVec::<[MroSource<'db>; 2]>::new();
    receiver_sources(db, env, receiver, &mut sources);
    let mut accesses = smallvec::SmallVec::<[MemberAccess<'db>; 2]>::new();
    // the common case, answered without a member lookup: nothing the receiver can
    // be declares any member with a visibility keyword
    if !sources.iter().any(|source| source.declares_any(db, env)) {
        return Ok(accesses);
    }
    for source in sources {
        let Some(access) = source.access(db, env, member) else {
            continue;
        };
        if let Some(first) = accesses.first()
            && first.emitted_name(db, member) != access.emitted_name(db, member)
        {
            return Err(AmbiguousAccess);
        }
        if !accesses.contains(&access) {
            accesses.push(access);
        }
    }
    Ok(accesses)
}

/// basedpython: the name a class member declared with a visibility keyword is
/// reached by in the emitted python. `None` when the access is not renamed
pub(crate) fn restricted_member_name<'db>(
    db: &'db dyn Db,
    env: &crate::types::ProgramEnvironment<'db>,
    receiver: crate::types::Type<'db>,
    member: &str,
) -> Option<String> {
    // every access emits the same name, so the first one speaks for all of them
    member_access(db, env, receiver, member)
        .ok()?
        .first()?
        .emitted_name(db, member)
}

/// basedpython: how an access to `owner`'s `member`, declared with `visibility`,
/// is spelled in the emitted python. `None` for a name the keyword cannot rename
pub(crate) fn emitted_member_name(
    db: &dyn Db,
    visibility: MemberVisibility,
    owner: super::class::StaticClassLiteral<'_>,
    member: &str,
) -> Option<String> {
    let renamed =
        ruff_python_stdlib::basedpython::visibility_rename(member, visibility.name_prefix())?;
    Some(match visibility {
        // `__name` is what python mangles, and it mangles lexically: written in a
        // subclass's body it would name `_Subclass__name`, and outside a class body
        // nothing at all. the mangled name is spelled out so the same attribute is
        // reached from all of them
        MemberVisibility::Private => mangled_private_name(owner.name(db).as_str(), member),
        _ => renamed,
    })
}

/// basedpython: how `class`'s own member `name`, declared with a visibility
/// keyword, is spelled — in the class body, where python mangles `__name`
/// lexically, and anywhere else. `None` for a member no keyword renames
pub(crate) fn class_member_spellings<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    class: &ast::StmtClassDef,
    name: &str,
) -> Option<(String, String)> {
    let index = crate::semantic_index(db, file);
    let definition = index.expect_single_definition(class);
    let literal = super::infer::original_class_type(db, definition)?.as_static()?;
    let visibility = *literal.member_visibilities(db).get(name)?;
    let in_body =
        ruff_python_stdlib::basedpython::visibility_rename(name, visibility.name_prefix())?;
    let anywhere = emitted_member_name(db, visibility, literal, name)?;
    Some((in_body, anywhere))
}

/// basedpython: where a member reached through a receiver is looked up
#[derive(Clone, Copy)]
enum MroSource<'db> {
    /// a class's MRO, from the class itself
    Class(crate::types::ClassType<'db>),
    /// what `super()` reads: the owner's MRO after the pivot
    AfterPivot(super::bound_super::BoundSuperType<'db>),
}

impl<'db> MroSource<'db> {
    fn declares_any(self, db: &'db dyn Db, env: &crate::types::ProgramEnvironment<'db>) -> bool {
        match self {
            MroSource::Class(class) => declares_any(db, class.iter_mro(db)),
            MroSource::AfterPivot(bound) => bound
                .lookup_mro_after_pivot(db, env)
                .is_some_and(|mro| declares_any(db, mro)),
        }
    }

    fn access(
        self,
        db: &'db dyn Db,
        env: &crate::types::ProgramEnvironment<'db>,
        member: &str,
    ) -> Option<MemberAccess<'db>> {
        match self {
            MroSource::Class(class) => access_in(db, env, class.iter_mro(db), member),
            MroSource::AfterPivot(bound) => bound
                .lookup_mro_after_pivot(db, env)
                .and_then(|mro| access_in(db, env, mro, member)),
        }
    }
}

/// basedpython: every class a receiver can be, as the MRO an attribute on it is
/// looked up in. a class object is asked about its instances: `cls.made` in a
/// classmethod and `Registry.made` reach the class variable `self.made` does
fn receiver_sources<'db>(
    db: &'db dyn Db,
    env: &crate::types::ProgramEnvironment<'db>,
    receiver: crate::types::Type<'db>,
    sources: &mut smallvec::SmallVec<[MroSource<'db>; 2]>,
) {
    use crate::types::Type;
    match receiver.erase_restriction(db) {
        Type::Union(union) => {
            for element in union.elements(db) {
                receiver_sources(db, env, *element, sources);
            }
        }
        Type::Intersection(intersection) => {
            for element in intersection.positive(db) {
                receiver_sources(db, env, *element, sources);
            }
        }
        Type::BoundSuper(bound) => sources.push(MroSource::AfterPivot(bound)),
        receiver @ (Type::ClassLiteral(_) | Type::GenericAlias(_)) => {
            if let Some(class) = receiver.to_class_type(db) {
                sources.push(MroSource::Class(class));
            }
        }
        Type::SubclassOf(subclass_of) => {
            receiver_sources(db, env, subclass_of.to_instance(db, env), sources);
        }
        receiver => {
            if let Some(class) = receiver.nominal_class(db, env) {
                sources.push(MroSource::Class(class));
            }
        }
    }
}

/// basedpython: whether any class in `mro` declares a member with a visibility
/// keyword. a class whose body cannot be read is assumed to
fn declares_any<'db>(db: &'db dyn Db, mro: impl Iterator<Item = super::ClassBase<'db>>) -> bool {
    mro.filter_map(super::ClassBase::into_class).any(|base| {
        base.class_literal(db)
            .as_static()
            .is_none_or(|literal| !literal.member_visibilities(db).is_empty())
    })
}

/// basedpython: the access to `member` the first class in `mro` that defines it
/// declares. `None` when no class in it defines the member
fn access_in<'db>(
    db: &'db dyn Db,
    env: &crate::types::ProgramEnvironment<'db>,
    mro: impl Iterator<Item = super::ClassBase<'db>>,
    member: &str,
) -> Option<MemberAccess<'db>> {
    for base in mro.filter_map(super::ClassBase::into_class) {
        if base.own_instance_member(db, env, member).is_undefined()
            && base.own_class_member(db, env, None, member).is_undefined()
        {
            continue;
        }
        let Some(literal) = base.class_literal(db).as_static() else {
            return Some(MemberAccess::Unrestricted);
        };
        return Some(match literal.member_visibilities(db).get(member) {
            Some(&visibility) if visibility != MemberVisibility::Public => {
                MemberAccess::Restricted {
                    visibility,
                    owner: literal,
                }
            }
            _ => MemberAccess::Unrestricted,
        });
    }
    None
}

/// basedpython: the class that declares `class`'s `private` or `protected`
/// constructor — the class whose body may construct it. `None` when the
/// constructor carries no visibility keyword.
///
/// The declaring class answers rather than `class` itself, so a subclass that
/// inherits a private `__init__` is reported at its own construction sites: the
/// constructor is the base's implementation detail, and a subclass is outside
/// the base's body like any other caller.
pub(crate) fn restricted_constructor<'db>(
    db: &'db dyn Db,
    class: crate::types::ClassType<'db>,
) -> Option<RestrictedConstructor<'db>> {
    // the specialization says nothing about which `__init__` is found or how it
    // was declared, so the question is asked of the class itself and the answer
    // is shared by every specialization of it
    *restricted_constructor_of(db, class.class_literal(db).as_static()?)
}

/// Tracked for two reasons. It is asked at every construction site in the
/// program, and answering it walks an MRO. More importantly, the answer is read
/// off the *declaring* module — its `__init__`, and the semantic index that
/// says which class encloses it — so without a query boundary here, checking
/// one module would depend on the index of every module it constructs
/// something from.
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
fn restricted_constructor_of<'db>(
    db: &'db dyn Db,
    class: super::class::StaticClassLiteral<'db>,
) -> Option<RestrictedConstructor<'db>> {
    let env = &crate::types::ProgramEnvironment::from_file(class.program_file(db));
    let init = class
        .identity_specialization(db)
        .class_member(db, env, "__init__", super::MemberLookupPolicy::default())
        .place
        .ignore_possibly_undefined()?;
    let function = declared_function(db, init)?;
    let visibility = crate::types::declared_visibility(
        db,
        crate::types::TypeQualifiers::empty(),
        crate::types::Type::FunctionLiteral(function),
    )?;
    let scope = function.definition(db).scope(db);
    let index = crate::semantic_index(db, scope.program_file(db));
    let owner = super::infer::nearest_enclosing_class(db, index, scope)?;
    Some(RestrictedConstructor {
        owner,
        function,
        visibility,
    })
}

/// A `private` constructor, and the class that declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct RestrictedConstructor<'db> {
    pub(crate) owner: super::class::StaticClassLiteral<'db>,
    pub(crate) function: super::function::FunctionType<'db>,
    /// `Private` when only `owner`'s own body may construct it, `Protected` when
    /// a subclass's body may too.
    pub(crate) visibility: MemberVisibility,
}

/// Whether `scope` lies within the body of `class` — the test for "this code is
/// the class's own", which a nested function or a nested class passes too.
pub(crate) fn scope_is_within_class<'db>(
    db: &'db dyn Db,
    index: &ty_python_core::SemanticIndex<'db>,
    scope: ty_python_core::scope::ScopeId<'db>,
    class: super::class::StaticClassLiteral<'db>,
) -> bool {
    index
        .ancestor_scopes(scope.file_scope_id(db))
        .filter_map(|(_, ancestor)| ancestor.node().as_class())
        .any(|ancestor| {
            let definition = index.expect_single_definition(ancestor);
            super::infer::original_class_type(db, definition)
                .and_then(super::class::ClassLiteral::as_static)
                == Some(class)
        })
}

/// basedpython: whether `scope` sits inside `class`'s body or inside the body of
/// a subclass of it — the code a `protected` member is declared for.
pub(crate) fn scope_is_within_subclass_of<'db>(
    db: &'db dyn Db,
    index: &ty_python_core::SemanticIndex<'db>,
    scope: ty_python_core::scope::ScopeId<'db>,
    class: super::class::StaticClassLiteral<'db>,
) -> bool {
    index
        .ancestor_scopes(scope.file_scope_id(db))
        .filter_map(|(_, ancestor)| ancestor.node().as_class())
        .any(|ancestor| {
            let definition = index.expect_single_definition(ancestor);
            let Some(literal) = super::infer::original_class_type(db, definition) else {
                return false;
            };
            literal
                .identity_specialization(db)
                .iter_mro(db)
                .filter_map(super::ClassBase::into_class)
                .any(|base| base.class_literal(db).as_static() == Some(class))
        })
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
fn mangled_private_name(class: &str, member: &str) -> String {
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

//! basedpython conversions (`__from__`, `__into__`, `__of__`, and the adapter a
//! callable gets where the site asked for one returning `None`)
//!
//! A conversion is a *call*, not a subtype relation: `Celsius` is never
//! assignable to `Fahrenheit`, or `list[Celsius]` would be a `list[Fahrenheit]`
//! and reading an element back would hand out a value no one converted. So the
//! relation stays out of the lattice and lives only at the positions where the
//! transpiler can materialize the call.
//!
//! This module owns that rule for all five routes a value can be repaired by,
//! so every site asks one question and gets one answer:
//!
//! - `T.__from__(x)` — a classmethod on the target taking the source
//! - `x.__into__()` — a method on the source returning the target
//! - `T.__of__(x)` — like `__from__`, but only when `x` is written out as a
//!   literal at the site
//! - a conformance extension (`extension str(A):`), which
//!   [`super::conformance`] resolves and this module only routes to. Nothing is
//!   emitted for one — the value already *is* what the protocol asks for at
//!   runtime — but the route still lives here so that a site served by two
//!   routes at once is reported rather than silently picked between
//! - `_by_discard(f)` — the one route no type declares. a callable reaching a
//!   site that asked for one returning `None` is wrapped in an adapter that
//!   calls it and throws the result away. See [`discards_return`]
//!
//! More than one applicable route is an error rather than a precedence rule:
//! `__from__` and `__into__` are hand-written bodies that can disagree, and
//! picking one silently would make the output depend on a rule nobody reads.

use ruff_db::files::File;
use ruff_python_ast as ast;
use ruff_text_size::{Ranged, TextRange};
use ty_module_resolver::{ModuleName, resolve_module};
use ty_python_core::semantic_index;

use crate::Db;
use crate::place::builtins_symbol;
use crate::types::ProgramEnvironment;
use crate::types::call::CallArguments;
use crate::types::class::{ClassLiteral, ClassType, KnownClass, StaticClassLiteral};
use crate::types::context::InferContext;
use crate::types::diagnostic::{AMBIGUOUS_CONVERSION, INVALID_CONVERSION};
use crate::types::extensions::{self, ExtensionMemberKind, ExtensionMemberResolution};
use crate::types::function::FunctionType;
use crate::types::signatures::Parameters;
use crate::types::{MemberLookupPolicy, Type, TypeContext};
use ty_module_resolver::ImportingFile;

/// the classmethod on a target that converts a value of some other type
pub(crate) const FROM: &str = "__from__";
/// the method on a source that converts it into the type it returns
pub(crate) const INTO: &str = "__into__";
/// the classmethod on a target that converts a *literal*
pub(crate) const OF: &str = "__of__";

/// every conversion dunder, for the declaration-site validation
pub(crate) const CONVERSION_DUNDERS: [&str; 3] = [FROM, INTO, OF];

/// where a target's `__from__` / `__of__` was declared.
///
/// An `extension` may supply a conversion for a type it does not own, which is
/// how the builtin frozen containers get one — but an extension member is not a
/// runtime attribute, so the lowering cannot spell it `T.__of__(x)`. The route
/// carries its origin so `conversion_info` can ask the extension what it lowers
/// to instead
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DunderSource<'db> {
    /// the target class declares the dunder itself
    Declared,
    /// an applicable `extension` declares it for the target
    Extension(StaticClassLiteral<'db>),
}

/// one way a value can be made to satisfy a declared type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route<'db> {
    /// a visible conformance extension makes the source's class conform to the
    /// protocol the target asks for. nothing is emitted: the value is already
    /// the object the protocol dispatches on
    Conformance(ClassType<'db>),
    /// `T.__from__(value)`, where `T` is the target class
    From(ClassType<'db>, DunderSource<'db>),
    /// `T.__of__(value)`, where `T` is the target class
    Of(ClassType<'db>, DunderSource<'db>),
    /// `value.__into__()`. carries the *source* type, for diagnostics only —
    /// the lowered call names nothing
    Into(Type<'db>),
    /// the value is a callable that returns something, and the site declared a
    /// callable that returns `None`. it is wrapped in an adapter that calls it
    /// and throws the result away
    DiscardReturn,
}

impl<'db> Route<'db> {
    /// how the route reads in a diagnostic
    fn describe(self, db: &'db dyn Db, env: &ProgramEnvironment<'db>) -> String {
        let dunder = |class: ClassType<'db>, source: DunderSource<'db>, dunder: &str| match source {
            DunderSource::Declared => format!("{}.{dunder}", class.name(db)),
            // two extensions supplying the same dunder read alike without this,
            // and telling them apart is the whole point of the report
            DunderSource::Extension(extension) => format!(
                "{}.{dunder}, from the extension in `{}`",
                class.name(db),
                extension.file(db).path(db)
            ),
        };
        match self {
            Route::Conformance(protocol) => format!("conformance to `{}`", protocol.name(db)),
            Route::From(class, source) => dunder(class, source, FROM),
            Route::Of(class, source) => dunder(class, source, OF),
            Route::Into(source) => format!("{}.{INTO}", source.display(db, env)),
            Route::DiscardReturn => "discarding the return value".to_owned(),
        }
    }
}

/// a conversion the checker found for an assignment that would otherwise fail
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConversionRepair<'db> {
    pub(crate) route: Route<'db>,
    /// every *other* applicable route — an ambiguity the checker reports at the
    /// site. all of them, not just the runner-up: a site served by three
    /// conversions should not have to be fixed one report at a time
    pub(crate) ambiguous_with: Vec<Route<'db>>,
}

/// would an in-scope conversion make `source` assignable to `target`?
///
/// `value` is the expression standing at the site, which only `__of__` reads —
/// it applies to a written-out literal and nothing else. Passing `None` declines
/// that route, so a site that cannot identify its own expression must not accept
/// one either.
///
/// This is the whole of conversion in the type system. Nothing nested inside a
/// generic can ask, which is why `list[Celsius]` is not a `list[Fahrenheit]`.
pub(crate) fn repair_conversion<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    source: Type<'db>,
    target: Type<'db>,
    value: Option<&ast::Expr>,
) -> Option<ConversionRepair<'db>> {
    if !file.source_type(db).is_basedpython() {
        return None;
    }
    // a conversion only ever *adds* an assignment that fails without it, so no
    // code that checks today changes meaning
    if source.is_assignable_to(db, env, target) {
        return None;
    }
    // the value being converted is an ordinary value of the type it was
    // restricted from — `final Celsius` converts exactly as `Celsius` does
    let source = source.erase_restriction(db);

    let mut routes: Vec<Route<'db>> = Vec::new();
    if let Some(protocol) =
        super::conformance::repair_with_conformance(db, env, file, source, target)
    {
        routes.push(Route::Conformance(protocol));
    }
    dunder_routes(db, env, file, source, target, value, &mut routes);
    if discards_return(db, env, source, target) {
        routes.push(Route::DiscardReturn);
    }

    let mut routes = routes.into_iter();
    let route = routes.next()?;
    Some(ConversionRepair {
        route,
        ambiguous_with: routes.collect(),
    })
}

/// the dunder routes applicable to `source` → `target`, appended in a fixed
/// order so that an ambiguity reads the same way whichever site asks
fn dunder_routes<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    source: Type<'db>,
    target: Type<'db>,
    value: Option<&ast::Expr>,
    routes: &mut Vec<Route<'db>>,
) {
    let literal = value.is_some_and(is_literal_expression);

    for arm in union_arms(db, target) {
        let Some(class) = arm.nominal_class(db, env) else {
            continue;
        };
        for dunder in [FROM, OF] {
            if dunder == OF && !literal {
                continue;
            }
            // `__of__` reads the syntax, so an empty display is typed exactly
            // here rather than at the widening ordinary inference gives it
            let sources = std::iter::once(source).chain(
                value
                    .filter(|_| dunder == OF)
                    .into_iter()
                    .flat_map(|value| empty_display_types(db, env, value)),
            );
            let route = |dunder_source| {
                if dunder == FROM {
                    Route::From(class, dunder_source)
                } else {
                    Route::Of(class, dunder_source)
                }
            };

            // the lowered call is `T.__from__(x)`, which binds `x` to `cls`
            // unless the member really is a classmethod. resolving the route the
            // same way the declaration is validated keeps a malformed dunder
            // from converting anything
            if conversion_classmethod(db, env, class, dunder).is_some() {
                if sources.clone().any(|source| {
                    converts(
                        db,
                        env,
                        arm,
                        dunder,
                        CallArguments::positional([source]),
                        target,
                    )
                }) {
                    routes.push(route(DunderSource::Declared));
                }
                continue;
            }
            // a type that declares no conversion of its own may still be given
            // one from outside. `try_call_dunder` cannot see an extension
            // member, so it is resolved and called directly
            for member in extension_classmethods(db, env, file, class, dunder) {
                if sources
                    .clone()
                    .any(|source| calls_to(db, env, member.ty, source, target))
                {
                    routes.push(route(DunderSource::Extension(member.extension)));
                }
            }
        }
    }

    if source_declares_into(db, env, source)
        && converts(db, env, source, INTO, CallArguments::none(), target)
    {
        routes.push(Route::Into(source));
    }
}

/// would `source` fit `target` if its return value were thrown away?
///
/// This is kotlin's coercion to `Unit`, and it is a conversion here for the same
/// reason it is not subtyping there: the adapter is a *different callable*, so
/// `list[() -> int]` has to stay unrelated to `list[() -> None]`. Only a site the
/// transpiler can wrap gets to ask, which is what keeps the relation out of the
/// lattice — and out of the reach of the native backend, which picks a value
/// representation from the declared type and would be picking it from a lie.
///
/// Nothing about the parameters is restated here: both halves below are ordinary
/// assignability questions, so overloads, generics and parameter contravariance
/// come from the relation rather than from a second implementation of it.
///
/// Two questions rather than one, because neither is sufficient alone:
///
/// 1. is the *return type* the only thing wrong? Asked by widening the target's
///    return to `object`, which every type satisfies, and checking `source`
///    against that — `source` itself, never a callable rebuilt from it.
///    Upcasting to a callable is a lossy view: a reified generic is a two-step
///    `f[...]()` that a plain callable has no slot for, and rebuilding from the
///    view would quietly drop that and call the result repaired
/// 2. does the adapter actually satisfy the target? Asked of `source` rebuilt
///    with a `None` return, which is what the adapter's type is. A target that
///    asks for more than a callable — a protocol with a `__call__` *and* other
///    members — is not satisfied by an adapter, and only this half notices
fn discards_return<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    source: Type<'db>,
    target: Type<'db>,
) -> bool {
    if !target_discards_return(db, env, target) {
        return false;
    }
    let Some(target_callables) = target.try_upcast_to_callable(db, env) else {
        return false;
    };
    let anything = KnownClass::Object.to_instance(db, env);
    let ignores_return = target_callables
        .map(|callable| callable.with_return_type(db, anything))
        .into_type(db, env);
    if !source.is_assignable_to(db, env, ignores_return) {
        return false;
    }

    let Some(source_callables) = source.try_upcast_to_callable(db, env) else {
        return false;
    };
    let none = Type::none(db, env);
    source_callables
        .map(|callable| callable.with_return_type(db, none))
        .into_type(db, env)
        .is_assignable_to(db, env, target)
}

/// does `target` ask for a callable that returns exactly `None`?
///
/// Anything wider is a caller that may still read the value the adapter would
/// have dropped: `-> object` already accepts every callable without one, and
/// `-> int | None` would hand back a `None` where an `int` was promised
pub(crate) fn target_discards_return<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    target: Type<'db>,
) -> bool {
    target
        .try_upcast_to_callable(db, env)
        .is_some_and(|callables| {
            callables
                .iter()
                .all(|callable| callable.returns_only_none(db))
        })
}

/// does calling `dunder` on `receiver` with `arguments` produce something the
/// target accepts? the ordinary call machinery answers, so overloads, generics
/// and descriptor binding all come from it rather than being re-derived here
fn converts<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    receiver: Type<'db>,
    dunder: &str,
    arguments: CallArguments<'_, 'db>,
    target: Type<'db>,
) -> bool {
    receiver
        .try_call_dunder(db, env, dunder, arguments, TypeContext::default())
        .is_ok_and(|bindings| {
            bindings
                .return_type(db, env)
                .is_assignable_to(db, env, target)
        })
}

/// the same question for an already-bound member: does calling it with `source`
/// produce something the target accepts? An extension member does not live on
/// the receiver's meta-type, so it is resolved first and called here
fn calls_to<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    member: Type<'db>,
    source: Type<'db>,
    target: Type<'db>,
) -> bool {
    member
        .try_call(db, env, &CallArguments::positional([source]))
        .is_ok_and(|bindings| {
            bindings
                .return_type(db, env)
                .is_assignable_to(db, env, target)
        })
}

/// the `__from__` / `__of__` an `extension` supplies for `class`, bound to the
/// class object the lowered call would name.
///
/// More than one applicable extension is not resolved here: both are returned so
/// the site reports the ambiguity rather than silently picking the first
fn extension_classmethods<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    class: ClassType<'db>,
    dunder: &str,
) -> Vec<ExtensionMemberResolution<'db>> {
    extensions::resolve_extension_members(db, env, file, Type::from(class), dunder)
        .into_iter()
        .filter(|resolution| resolution.kind == ExtensionMemberKind::ClassMethod)
        .collect()
}

/// both types an empty display can present at a conversion site.
///
/// A dunder may be asking for either. One taking `dict[Never, Never]` is asking
/// for the empty display and nothing else; one taking `dict[str, int]` takes `{}`
/// the way any dict-shaped parameter does, because there is nothing in it to
/// disagree. Ordinary inference only ever produces one of the two — the exact
/// type under [`sound-types`](crate::AnalysisSettings::sound_types), the widened
/// `dict[Unknown, Unknown]` otherwise — so offering both here is what keeps the
/// route from turning on that setting. `__of__` can, because it has the syntax in
/// hand and an empty display is every element type at once.
fn empty_display_types<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    value: &ast::Expr,
) -> Vec<Type<'db>> {
    match value {
        ast::Expr::Dict(_) if is_empty_display(value) => vec![
            KnownClass::Dict.to_specialized_instance(db, env, &[Type::Never, Type::Never]),
            KnownClass::Dict.to_specialized_instance(db, env, &[Type::unknown(), Type::unknown()]),
        ],
        ast::Expr::List(_) if is_empty_display(value) => vec![
            KnownClass::List.to_specialized_instance(db, env, &[Type::Never]),
            KnownClass::List.to_specialized_instance(db, env, &[Type::unknown()]),
        ],
        _ => Vec::new(),
    }
}

/// is `value` a display written with nothing in it?
///
/// Asked by the type above and by the lowering, which must agree: the lowering
/// drops the value only where the checker typed it as the empty display, and it
/// can only drop it safely because there is nothing inside to drop
fn is_empty_display(value: &ast::Expr) -> bool {
    match value {
        ast::Expr::Dict(dict) => dict.items.is_empty(),
        ast::Expr::List(list) => list.elts.is_empty(),
        _ => false,
    }
}

/// the arms of a union, or the type itself. a target may offer `__from__` on any
/// arm — `x: Fahrenheit? = c` is as much a conversion site as the bare
/// annotation is — and two arms that both convert is an ambiguity like any other
fn union_arms<'db>(db: &'db dyn Db, ty: Type<'db>) -> Vec<Type<'db>> {
    match ty {
        Type::Union(union) => union.elements(db).to_vec(),
        _ => vec![ty],
    }
}

/// does every arm of `source` declare a usable `__into__`?
///
/// The lowered `x.__into__()` runs against whichever arm the value actually is,
/// so one arm without it would be an `AttributeError` at runtime. Requiring all
/// of them is what lets a union source convert at all
fn source_declares_into<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    source: Type<'db>,
) -> bool {
    let arms = union_arms(db, source);
    !arms.is_empty()
        && arms.iter().all(|arm| {
            arm.nominal_class(db, env)
                .is_some_and(|class| conversion_method(db, env, class).is_some())
        })
}

/// the `__from__` / `__of__` declared on `class`, when it is the classmethod the
/// lowered call needs
pub(crate) fn conversion_classmethod<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: ClassType<'db>,
    dunder: &str,
) -> Option<FunctionType<'db>> {
    match class
        .class_member(db, env, dunder, MemberLookupPolicy::default())
        .place
        .ignore_possibly_undefined()?
    {
        Type::FunctionLiteral(function) if function.is_classmethod(db) => Some(function),
        _ => None,
    }
}

/// the `__into__` declared on `class`, when it is the plain instance method the
/// lowered `x.__into__()` needs. an overloaded one is rejected: the call carries
/// no target, so there would be nothing to dispatch on at runtime
pub(crate) fn conversion_method<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: ClassType<'db>,
) -> Option<FunctionType<'db>> {
    match class
        .class_member(db, env, INTO, MemberLookupPolicy::default())
        .place
        .ignore_possibly_undefined()?
    {
        Type::FunctionLiteral(function)
            if !function.is_classmethod(db)
                && !function.is_staticmethod(db)
                && function.signature(db).iter().len() == 1 =>
        {
            Some(function)
        }
        _ => None,
    }
}

/// does `class` declare any conversion dunder?
///
/// Tracked because the call gate asks it of every parameter type of every call,
/// and the answer for `int` / `str` / `float` is the same "no" every time —
/// unmemoized, walking their MROs three times per parameter costs more than the
/// binding the gate exists to avoid
#[salsa::tracked(heap_size = ruff_memory_usage::heap_size)]
fn class_declares_conversion<'db>(db: &'db dyn Db, class: StaticClassLiteral<'db>) -> bool {
    let env = &ProgramEnvironment::from_file(class.program_file(db));
    let class = class.identity_specialization(db);
    CONVERSION_DUNDERS.iter().any(|dunder| {
        !class
            .class_member(db, env, dunder, MemberLookupPolicy::default())
            .place
            .is_undefined()
    })
}

/// might `ty` be one end of a conversion? the call gate's question, deliberately
/// over-approximate in both directions: a `true` only costs the full check that
/// would have run anyway, and anything this cannot classify answers `true`
pub(crate) fn may_convert<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    ty: Type<'db>,
) -> bool {
    union_arms(db, ty).iter().any(|arm| {
        match arm
            .nominal_class(db, env)
            .map(|class| class.class_literal(db))
        {
            Some(ClassLiteral::Static(literal)) => {
                *class_declares_conversion(db, literal)
                    || extensions::extension_converts_class(db, file, literal)
            }
            // a synthesized class, or a type with no class at all: not worth
            // classifying cheaply, so let the full check decide
            Some(_) => true,
            None => !arm.is_never(),
        }
    })
}

/// is `expr` a literal — an expression whose outermost form is written-out
/// syntax? that is what `__of__` converts, and the reason it can: the brackets
/// are in the source, so the wrap goes exactly where the value was written.
///
/// the elements need not be literals (`[1, 2, foo()]` is a list display). a
/// comprehension is not one: its contents come from another collection, which is
/// the line element-wise conversion is drawn on
pub(crate) fn is_literal_expression(expr: &ast::Expr) -> bool {
    matches!(
        expr,
        ast::Expr::NoneLiteral(_)
            | ast::Expr::BooleanLiteral(_)
            | ast::Expr::NumberLiteral(_)
            | ast::Expr::StringLiteral(_)
            | ast::Expr::BytesLiteral(_)
            | ast::Expr::FString(_)
            | ast::Expr::EllipsisLiteral(_)
            | ast::Expr::List(_)
            | ast::Expr::Set(_)
            | ast::Expr::Dict(_)
            | ast::Expr::Tuple(_)
    )
}

/// the `return` value expression covering `range` in `function`'s body.
///
/// The return sites the checker collects carry a type and a range but no node,
/// and the literal gate needs the expression itself. A range is unique in a
/// file, so this cannot match a different `return` — including one in a nested
/// function, which is why the walk does not have to stop at scope boundaries
pub(crate) fn returned_value_at(
    function: &ast::StmtFunctionDef,
    range: TextRange,
) -> Option<&ast::Expr> {
    struct FindReturn<'ast> {
        range: TextRange,
        found: Option<&'ast ast::Expr>,
    }

    impl<'ast> ast::visitor::Visitor<'ast> for FindReturn<'ast> {
        fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
            if let ast::Stmt::Return(ret) = stmt
                && let Some(value) = ret.value.as_deref()
                && value.range() == self.range
            {
                self.found = Some(value);
                return;
            }
            ast::visitor::walk_stmt(self, stmt);
        }
    }

    let mut visitor = FindReturn { range, found: None };
    for stmt in &function.body {
        ast::visitor::Visitor::visit_stmt(&mut visitor, stmt);
        if visitor.found.is_some() {
            break;
        }
    }
    visitor.found
}

/// the search state for [`imported_module_spelling`]: the first import statement
/// resolving to `target` wins
struct ImportSpelling<'a> {
    db: &'a dyn Db,
    from_file: File,
    target: File,
    found: Option<String>,
}

impl ImportSpelling<'_> {
    fn resolves(&self, name: &ModuleName) -> bool {
        let db = self.db;
        resolve_module(
            self.db,
            ImportingFile::File(
                self.from_file,
                db.program_file(self.from_file).resolver_environment(db),
            ),
            name,
        )
        .and_then(|module| module.file(self.db))
            == Some(self.target)
    }
}

impl<'ast> ast::visitor::Visitor<'ast> for ImportSpelling<'_> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        let db = self.db;
        if self.found.is_some() {
            return;
        }
        match stmt {
            ast::Stmt::Import(import) => {
                for alias in &import.names {
                    if let Some(name) = ModuleName::new(&alias.name)
                        && self.resolves(&name)
                    {
                        self.found = Some(alias.name.to_string());
                        return;
                    }
                }
            }
            ast::Stmt::ImportFrom(import) => {
                if let Ok(name) = ModuleName::from_import_statement(
                    self.db,
                    ImportingFile::File(
                        self.from_file,
                        db.program_file(self.from_file).resolver_environment(db),
                    ),
                    import,
                ) && self.resolves(&name)
                {
                    // keep the leading dots: a relative import is how this file
                    // addresses the module, and the absolute name may not resolve
                    let mut spelling = ".".repeat(import.level as usize);
                    if let Some(module) = &import.module {
                        spelling.push_str(module);
                    }
                    self.found = Some(spelling);
                    return;
                }
            }
            _ => {}
        }
        ast::visitor::walk_stmt(self, stmt);
    }
}

/// how `from_file` spells the module that `target` is, in its own imports.
///
/// A synthesized import has to address the module the way the importing file
/// already does. ty's absolute module name is not usable for that: a file under a
/// directory that is not an importable package still resolves for the checker
/// (`target/mod.by` → `target.mod`), while the interpreter running the output only
/// sees `mod` — and a relative import has no absolute spelling at all.
pub(crate) fn imported_module_spelling(
    db: &dyn Db,
    from_file: File,
    target: File,
) -> Option<String> {
    let module =
        ruff_db::parsed::parsed_module(db, db.program_file(from_file).python_file(db)).load(db);
    let mut spelling = ImportSpelling {
        db,
        from_file,
        target,
        found: None,
    };
    for stmt in &module.syntax().body {
        ast::visitor::Visitor::visit_stmt(&mut spelling, stmt);
        if spelling.found.is_some() {
            break;
        }
    }
    spelling.found
}

/// every module named by a `from <module> import ...` statement anywhere in
/// `file`, relative imports resolved.
///
/// [`SemanticIndex::imported_modules`] deliberately records only `import mod`, so
/// anything that wants both forms has to collect these itself.
///
/// [`SemanticIndex::imported_modules`]: ty_python_core::SemanticIndex::imported_modules
#[salsa::tracked(returns(deref), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn from_imported_modules(db: &dyn Db, file: File) -> Box<[ModuleName]> {
    struct Collector<'a> {
        db: &'a dyn Db,
        file: File,
        modules: Vec<ModuleName>,
    }
    impl<'ast> ast::visitor::Visitor<'ast> for Collector<'_> {
        fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
            if let ast::Stmt::ImportFrom(import) = stmt
                && let Ok(name) = ModuleName::from_import_statement(
                    self.db,
                    ImportingFile::File(
                        self.file,
                        self.db
                            .program_file(self.file)
                            .resolver_environment(self.db),
                    ),
                    import,
                )
                && !self.modules.contains(&name)
            {
                self.modules.push(name);
            }
            ast::visitor::walk_stmt(self, stmt);
        }
    }

    let module = ruff_db::parsed::parsed_module(db, db.program_file(file).python_file(db)).load(db);
    let mut collector = Collector {
        db,
        file,
        modules: Vec::new(),
    };
    for stmt in &module.syntax().body {
        // `visit_stmt`, not `walk_stmt`: the latter descends into the statement's
        // children and would skip the statement itself
        ast::visitor::Visitor::visit_stmt(&mut collector, stmt);
    }
    collector.modules.into_boxed_slice()
}

/// the declared return type of `function`, as the conversion machinery sees it.
///
/// Both sides go through this: the transpiler to find what a `return` value must
/// convert to, and the checker to confirm the type it is enforcing is the same one
/// — a `return` is only a conversion site when the two agree, so the lowering can
/// never be handed a target the checker did not use.
pub(crate) fn function_declared_return_type<'db>(
    db: &'db dyn Db,
    file: File,
    function: &ast::StmtFunctionDef,
) -> Option<Type<'db>> {
    let index = semantic_index(db, db.program_file(file));
    let definition = index.expect_single_definition(function);
    let Type::FunctionLiteral(literal) = crate::types::binding_type(db, definition) else {
        return None;
    };
    // one signature only: an overloaded function has no single declared return
    let [overload] = literal.signature(db).overloads.as_slice() else {
        return None;
    };
    Some(overload.return_ty)
}

/// the element expressions of a collection literal, when each one is a whole
/// expression the transpiler can wrap.
///
/// `None` for anything else — including a literal containing an unpack
/// (`[*bs]`, `{**d}`), whose elements come from another collection and so have no
/// expression of their own at this site
pub(crate) fn addressable_elements(value: &ast::Expr) -> Option<Vec<&ast::Expr>> {
    fn plain(elements: &[ast::Expr]) -> Option<Vec<&ast::Expr>> {
        elements
            .iter()
            .map(|element| (!element.is_starred_expr()).then_some(element))
            .collect()
    }
    match value {
        ast::Expr::List(list) => plain(&list.elts),
        ast::Expr::Set(set) => plain(&set.elts),
        ast::Expr::Tuple(tuple) => plain(&tuple.elts),
        // only the values convert; a key's own type is checked against the key type
        ast::Expr::Dict(dict) => dict
            .items
            .iter()
            .map(|item| item.key.as_ref().map(|_| &item.value))
            .collect(),
        ast::Expr::ListComp(comp) => Some(vec![&comp.elt]),
        ast::Expr::SetComp(comp) => Some(vec![&comp.elt]),
        ast::Expr::Generator(comp) => Some(vec![&comp.elt]),
        ast::Expr::DictComp(comp) => Some(vec![&comp.value]),
        _ => None,
    }
}

/// the type a declared collection's elements must satisfy: a mapping's *value*
/// type, else what iterating the declared type yields
pub(crate) fn declared_element_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    declared: Type<'db>,
) -> Option<Type<'db>> {
    // a mapping is keyed, and only its values sit at the literal's element
    // positions (`{"k": b}`); iterating one would give the *key* type
    if let Some((_, value)) = declared.unpack_keys_and_items(db, env) {
        return Some(value);
    }
    let element = declared.iterate(db, env).homogeneous_element_type(db, env);
    (!element.is_unknown() && !element.is_never()).then_some(element)
}

/// the annotated parameter type each of `call`'s source-order arguments was
/// matched to, binding the call outside inference.
///
/// `None` when the callee is a union or overloaded — where no single parameter
/// type per argument is well-defined. The checker declines to repair in exactly
/// that case too, so the two answers cannot drift apart
pub(crate) fn call_parameter_types<'db>(
    model: &crate::semantic_model::SemanticModel<'db>,
    callable_ty: Type<'db>,
    call: &ast::ExprCall,
) -> Option<Vec<Option<Type<'db>>>> {
    use crate::types::constraints::ConstraintSetBuilder;
    let env = &model.program_environment();

    let db = model.db();
    let arguments = CallArguments::from_arguments_typed(&call.arguments, |splatted_value| {
        crate::HasType::inferred_type(splatted_value, model).unwrap_or_else(Type::unknown)
    });
    let constraints = ConstraintSetBuilder::new();
    // a conversion site's binding *does* carry an argument error — the checker
    // suppresses its diagnostic rather than making the argument assignable — so
    // the parameter types have to be read out of either outcome
    let bindings = match callable_ty
        .bindings(db, env)
        .match_parameters(db, env, &arguments)
        .check_types(
            db,
            env,
            &constraints,
            &arguments,
            TypeContext::default(),
            &[],
        ) {
        Ok(bindings) => bindings,
        Err(error) => *error.into_bindings(),
    };
    bindings.plain_callee_parameter_types(arguments.len())
}

/// report a conversion site served by more than one route. the site is otherwise
/// accepted — any of the conversions would work — but which one runs must not
/// depend on ordering
pub(crate) fn report_ambiguous_conversion<'db>(
    context: &InferContext<'db, '_>,
    node: impl Ranged,
    repair: &ConversionRepair<'db>,
) {
    let env = context.program_environment();
    let db = context.db();
    if repair.ambiguous_with.is_empty() {
        return;
    }
    let Some(builder) = context.report_lint(&AMBIGUOUS_CONVERSION, node.range()) else {
        return;
    };
    let mut diagnostic = builder.into_diagnostic("More than one conversion applies here");
    let names: Vec<String> = std::iter::once(repair.route)
        .chain(repair.ambiguous_with.iter().copied())
        .map(|route| format!("`{}`", route.describe(db, env)))
        .collect();
    diagnostic.info(format_args!("{} all convert this value", names.join(", ")));
    diagnostic.help("Remove all but one of them, or write the conversion you want explicitly");
}

/// the conversions the value expression `value` needs so that it satisfies
/// `declared`, as `(range to wrap, conversion)` in source order.
///
/// This is the single answer both sides use at every statement conversion site:
/// the checker asks whether it is non-empty (and suppresses the assignment error
/// if so), and the transpiler emits exactly these wraps. Two shapes are
/// possible:
///
/// - the value itself converts — one wrap around the whole expression
/// - the value is a collection *literal* (or comprehension) whose elements
///   convert — one wrap per element. The whole value is tried first, so a target
///   with its own conversion for the collection wins and the choice never
///   depends on ordering
pub(crate) fn value_conversions<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    model: &crate::SemanticModel<'db>,
    value: &ast::Expr,
    declared: Type<'db>,
) -> Vec<(TextRange, ConversionRepair<'db>)> {
    let Some(value_ty) = crate::HasType::inferred_type(value, model) else {
        return Vec::new();
    };
    if let Some(repair) = repair_conversion(db, env, file, value_ty, declared, Some(value)) {
        return vec![(value.range(), repair)];
    }
    element_conversions(db, env, file, model, value, declared)
}

/// the per-element conversions a collection literal needs to satisfy `declared`.
///
/// Empty unless *every* element either already fits or converts: a partial answer
/// would leave the value unassignable, and the ordinary error is the right report
fn element_conversions<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    model: &crate::SemanticModel<'db>,
    value: &ast::Expr,
    declared: Type<'db>,
) -> Vec<(TextRange, ConversionRepair<'db>)> {
    let Some(elements) = addressable_elements(value) else {
        return Vec::new();
    };
    let Some(element_target) = declared_element_type(db, env, declared) else {
        return Vec::new();
    };
    if !display_kind_fits(db, env, model, value, declared) {
        return Vec::new();
    }
    let mut conversions = Vec::new();
    for element in elements {
        let Some(element_ty) = crate::HasType::inferred_type(element, model) else {
            return Vec::new();
        };
        if element_ty.is_assignable_to(db, env, element_target) {
            continue;
        }
        match repair_conversion(db, env, file, element_ty, element_target, Some(element)) {
            Some(repair) => conversions.push((element.range(), repair)),
            // one element that neither fits nor converts sinks the whole value
            None => return Vec::new(),
        }
    }
    conversions
}

/// does the display's own kind satisfy `declared`, setting aside the element
/// types the per-element conversions repair?
///
/// Element-wise conversion replaces the elements, never the display: `{1, 2}` is
/// still a `set` and `[1, 2]` still a `list` however their elements are wrapped.
/// So a declared type the display's own class does not satisfy — a `frozenset`,
/// a `tuple` — is not repairable this way, and accepting it would emit a value
/// of the wrong kind with nothing to report it. Both element types are erased to
/// `Unknown` because only the *kind* is in question here
fn display_kind_fits<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    model: &crate::SemanticModel<'db>,
    value: &ast::Expr,
    declared: Type<'db>,
) -> bool {
    let erased = |ty: Type<'db>| {
        ty.nominal_class(db, env).map(|class| {
            Type::instance(db, env, class.class_literal(db).unknown_specialization(db))
        })
    };
    let Some(value_ty) = crate::HasType::inferred_type(value, model).and_then(erased) else {
        return true;
    };
    // a target with no nominal class of its own — a union, a structural protocol
    // — cannot be compared this way, so it is left to the ordinary check
    let Some(declared) = erased(declared) else {
        return true;
    };
    value_ty.is_assignable_to(db, env, declared)
}

/// the sub-expression of `value` covering exactly `range`.
///
/// An element-wise conversion wraps an element rather than the whole literal,
/// and the name it emits has to resolve at *that* expression — which is the same
/// scope in every case a comprehension is not involved, and a different one when
/// it is
pub(crate) fn expression_at(value: &ast::Expr, range: TextRange) -> Option<&ast::Expr> {
    addressable_elements(value)?
        .into_iter()
        .find(|element| element.range() == range)
}

/// the import a cross-file conversion needs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionImport {
    /// spelled the way this file already imports the module — an absolute module
    /// name the interpreter cannot resolve would break at runtime
    pub module: String,
    /// the class's own name in that module
    pub name: String,
    /// the name to bind it to here, which is what `prefix` spells
    pub alias: String,
}

/// the name of the adapter [`ConversionRuntime::DiscardReturn`] defines, which
/// is what the emitted `prefix` spells. The definition itself lives with the
/// transpiler's other injected helpers, and is built from this
pub const DISCARD_ADAPTER: &str = "_by_discard";

/// a definition a conversion's emitted call needs that no module supplies.
///
/// The python text lives in the transpiler beside the other injected helpers;
/// this only says which one the site needs, so that the lowering pass still
/// never has to know which *route* it is emitting
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionRuntime {
    /// the adapter that calls a callable and throws its result away
    DiscardReturn,
}

/// how the transpiler materializes one conversion
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionInfo {
    /// emit `prefix`, the value, then `suffix`
    Call {
        prefix: String,
        suffix: String,
        /// whether the value is *dropped* rather than kept between the two.
        ///
        /// Only ever set where the value has no sub-expression a sibling pass
        /// could be rewriting — an empty display — because the source span
        /// carries any edit made inside it, and dropping the span drops those
        /// too. `{}` reaching a `frozenset` is the case: the emitted
        /// `frozenset()` says what `frozenset({})` says without building the
        /// throwaway dict first
        replaces_value: bool,
        /// the module-level name `prefix` spells, which python binds when its own
        /// statement runs — so a conversion at import time may not precede it.
        /// `None` for a route that names nothing (`__into__`)
        referenced_name: Option<String>,
        /// every name `prefix` spells that this file does not already bind
        imports: Vec<ConversionImport>,
        /// a definition `prefix` spells that no module can supply, injected once
        /// at the top of the file. `None` for a route that only names code that
        /// already exists somewhere
        runtime: Option<ConversionRuntime>,
    },
    /// the checker accepted the site, but the conversion cannot be spelled here.
    /// The transpiler reports this rather than skipping it: emitting nothing
    /// would leave python that type-checks and never converts
    Rejected(String),
}

/// build the transpiler's view of a repair. `anchor` is the expression being
/// wrapped, which decides what the emitted names have to resolve to
pub(crate) fn conversion_info<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    from_file: File,
    model: &crate::SemanticModel<'db>,
    anchor: &ast::Expr,
    repair: &ConversionRepair<'db>,
) -> ConversionInfo {
    if !repair.ambiguous_with.is_empty() {
        return ConversionInfo::Rejected(
            "more than one conversion applies to this value; remove all but one, or write \
             the conversion you want explicitly"
                .to_owned(),
        );
    }
    match repair.route {
        // conformance is not a conversion at runtime: the value already answers
        // every member the protocol asks for, through the witness table its
        // conformance registered. the site emits nothing at all
        Route::Conformance(_) => ConversionInfo::Call {
            prefix: String::new(),
            suffix: String::new(),
            replaces_value: false,
            referenced_name: None,
            imports: Vec::new(),
            runtime: None,
        },
        Route::From(class, source) => {
            dunder_call_info(db, env, from_file, model, anchor, class, FROM, source)
        }
        Route::Of(class, source) => {
            dunder_call_info(db, env, from_file, model, anchor, class, OF, source)
        }
        // the receiver is the value itself, so nothing has to be named or
        // imported. the parentheses are what make it safe to wrap an operand of
        // any precedence
        Route::Into(_) => ConversionInfo::Call {
            prefix: "(".to_owned(),
            suffix: format!(").{INTO}()"),
            replaces_value: false,
            referenced_name: None,
            imports: Vec::new(),
            runtime: None,
        },
        // the adapter is injected above every statement in the file, so unlike a
        // class this file declares there is no order for the site to fall foul
        // of — which is why it names nothing for the import-time check to read
        Route::DiscardReturn => ConversionInfo::Call {
            prefix: format!("{DISCARD_ADAPTER}("),
            suffix: ")".to_owned(),
            replaces_value: false,
            referenced_name: None,
            imports: Vec::new(),
            runtime: Some(ConversionRuntime::DiscardReturn),
        },
    }
}

/// `T.__from__(` / `T.__of__(`, with `T` spelled so that it resolves to the
/// target class *at the conversion site*.
///
/// A dunder an `extension` supplies is not a runtime attribute, so it lowers to
/// whatever that extension lowers to: the target's own constructor for a prelude
/// declaration (`{1}` in a `frozenset[int]` context is `frozenset({1})`), and
/// the backing function for one a module declares
#[expect(clippy::too_many_arguments)]
fn dunder_call_info<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    from_file: File,
    model: &crate::SemanticModel<'db>,
    anchor: &ast::Expr,
    class: ClassType<'db>,
    dunder: &str,
    source: DunderSource<'db>,
) -> ConversionInfo {
    if let DunderSource::Extension(extension) = source
        && !extensions::is_prelude_extension(db, from_file, extension)
    {
        return backing_call_info(db, env, from_file, model, anchor, class, extension, dunder);
    }
    // a prelude conversion means construction, so the emitted call is the class
    // itself — spelled, and shadow-checked, exactly as the dunder call would be
    let constructs = matches!(source, DunderSource::Extension(_));
    let spelling = |name: &str| {
        if constructs {
            format!("{name}(")
        } else {
            format!("{name}.{dunder}(")
        }
    };
    // constructing from an empty display needs no argument at all: `frozenset()`
    // is what `frozenset({})` means, without the throwaway dict. safe only
    // because construction *is* the conversion here — a real `T.__of__(x)` needs
    // its argument, and an empty display holds nothing another pass could edit
    let replaces_value = constructs && is_empty_display(anchor);
    match class_reference(db, env, from_file, model, anchor, class) {
        Ok((name, import)) => ConversionInfo::Call {
            prefix: spelling(&name),
            suffix: ")".to_owned(),
            replaces_value,
            referenced_name: Some(name),
            imports: import.into_iter().collect(),
            runtime: None,
        },
        Err(reason) => ConversionInfo::Rejected(reason),
    }
}

/// the call an extension-supplied dunder lowers to: the extension's own backing
/// function, imported when the extension is declared elsewhere.
///
/// A conversion dunder is a `class def`, so the first argument is the class
/// object — the same thing `Widget.kind` passes when an ordinary `static let` is
/// read off the class. The class is what the ordering check watches, because a
/// `class` statement binds its name late while the backing function is hoisted
/// above the module
#[expect(clippy::too_many_arguments)]
fn backing_call_info<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    from_file: File,
    model: &crate::SemanticModel<'db>,
    anchor: &ast::Expr,
    class: ClassType<'db>,
    extension: StaticClassLiteral<'db>,
    dunder: &str,
) -> ConversionInfo {
    let function = extensions::backing_function_name(db, extension, dunder);
    let extension_file = extension.file(db);
    let mut imports = Vec::new();
    if extension_file != from_file {
        let Some(module) = imported_module_spelling(db, from_file, extension_file) else {
            return ConversionInfo::Rejected(
                "the conversion this value needs comes from an `extension` in a module this \
                 file does not import; import it, or convert the value explicitly"
                    .to_owned(),
            );
        };
        imports.push(ConversionImport {
            module,
            name: function.clone(),
            alias: function.clone(),
        });
    }
    let (receiver, class_import) = match class_reference(db, env, from_file, model, anchor, class) {
        Ok(spelling) => spelling,
        Err(reason) => return ConversionInfo::Rejected(reason),
    };
    imports.extend(class_import);
    ConversionInfo::Call {
        prefix: format!("{function}({receiver}, "),
        suffix: ")".to_owned(),
        // the backing function takes the value as a parameter, so it stays
        replaces_value: false,
        referenced_name: Some(receiver),
        imports,
        runtime: None,
    }
}

/// the alias a cross-file conversion imports its target under.
///
/// Always aliased, never the class's own name: this file may already bind that
/// name to something else, and an import that silently rebinds it — or that the
/// file's own class then shadows — turns the conversion into an `AttributeError`
/// at runtime.
///
/// Reusing a name the file already binds to the *same* class looks tempting and
/// is not safe: the binding may be conditional (`if TYPE_CHECKING:`) or come
/// after the site, and the end-of-scope type a symbol lookup reports says
/// nothing about either. One leading underscore, not two, so a reference inside
/// a class body is not python name-mangled
fn conversion_alias(name: &str) -> String {
    format!("_by_conv__{name}")
}

/// how the conversion site spells `class`: its own name when this file declares
/// it and nothing between the site and the module shadows that name, otherwise
/// an aliased import. `Err` when neither is possible
pub(crate) fn class_reference<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    from_file: File,
    model: &crate::SemanticModel<'db>,
    anchor: &ast::Expr,
    class: ClassType<'db>,
) -> Result<(String, Option<ConversionImport>), String> {
    let ClassLiteral::Static(literal) = class.class_literal(db) else {
        return Err("this value converts through a type that has no class statement".to_owned());
    };
    let name = literal.name(db).to_string();
    // a builtin needs no import — the name is already there — but it can still
    // be shadowed by a local, which would send the emitted call elsewhere
    if literal.file(db) != from_file && is_builtin_class(db, env, literal) {
        return if name_is_shadowed_at(db, from_file, model, anchor, &name) {
            Err(format!(
                "the conversion this value needs goes through the builtin `{name}`, which is \
                 shadowed by a binding in an enclosing scope; rename that binding, or convert \
                 the value explicitly"
            ))
        } else {
            Ok((name, None))
        };
    }
    if literal.file(db) == from_file {
        // the class is right here, so the bare name is the spelling — unless a
        // scope between the site and the module binds it to something else
        if name_is_shadowed_at(db, from_file, model, anchor, &name) {
            return Err(format!(
                "the conversion this value needs goes through `{name}`, which is shadowed by \
                 a binding in an enclosing scope; rename that binding, or convert the value \
                 explicitly"
            ));
        }
        return Ok((name, None));
    }
    let module = imported_module_spelling(db, from_file, literal.file(db)).ok_or_else(|| {
        format!(
            "the conversion this value needs goes through `{name}`, which is declared in a \
             module this file does not import; import it, or convert the value explicitly"
        )
    })?;
    let alias = conversion_alias(&name);
    Ok((
        alias.clone(),
        Some(ConversionImport {
            module,
            name,
            alias,
        }),
    ))
}

/// is `class` the builtin of its own name?
///
/// A builtin is in scope everywhere, so the emitted call names it directly
/// rather than importing it under an alias. Asked by identity, not by module
/// path: a class that merely *lives* in `builtins` but is shadowed there by
/// something else is not what the bare name would reach
fn is_builtin_class<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class: StaticClassLiteral<'db>,
) -> bool {
    builtins_symbol(db, env, class.name(db))
        .place
        .ignore_possibly_undefined()
        .and_then(Type::as_class_literal)
        == Some(ClassLiteral::Static(class))
}

/// does a scope between `anchor` and the module bind `name`?
///
/// A conversion emits a bare class name, which python resolves in the scope the
/// call runs in — so a local of the same name would take the call somewhere
/// else. Over-reporting is the safe direction: a binding anywhere in the scope
/// counts, whether or not control reaches it before the site
fn name_is_shadowed_at<'db>(
    db: &'db dyn Db,
    file: File,
    model: &crate::SemanticModel<'db>,
    anchor: &ast::Expr,
    name: &str,
) -> bool {
    let Some(scope) = model.scope(ast::AnyNodeRef::from(anchor)) else {
        return false;
    };
    let index = semantic_index(db, db.program_file(file));
    index.ancestor_scopes(scope).any(|(id, _)| {
        // a scope's place table holds every name the scope *mentions*, so
        // merely naming the class — which the annotation of the very assignment
        // being converted does — is not shadowing. only a binding or a
        // declaration takes the name over
        !id.is_global()
            && index
                .place_table(id)
                .symbol_by_name(name)
                .is_some_and(|symbol| symbol.is_bound() || symbol.is_declared())
    })
}

/// declaration-site validation, run from the post-inference static-class checks.
///
/// The route resolution above declines a dunder whose shape the lowered call
/// cannot use, so this is what keeps that from being silent
pub(crate) fn validate_conversion_dunders<'db>(
    context: &InferContext<'db, '_>,
    class: StaticClassLiteral<'db>,
    class_node: &ast::StmtClassDef,
) {
    let env = context.program_environment();
    let db = context.db();
    // only basedpython has conversions. in a `.py` file `__from__` and friends
    // are ordinary method names that mean nothing to anyone, and inventing an
    // error for them would be a false positive on valid python
    if !context.file().source_type(db).is_basedpython() {
        return;
    }
    let class_type = class.identity_specialization(db);
    let mut reported: Vec<&str> = Vec::new();
    for stmt in &class_node.body {
        let ast::Stmt::FunctionDef(function_node) = stmt else {
            continue;
        };
        let name = function_node.name.as_str();
        if !CONVERSION_DUNDERS.contains(&name) {
            continue;
        }
        // the member type is the whole overload set, so validating it once per
        // definition would report an overloaded dunder once per `@overload`
        if reported.contains(&name) {
            continue;
        }
        reported.push(name);
        let member = class_type
            .class_member(db, env, name, MemberLookupPolicy::default())
            .place
            .ignore_possibly_undefined();
        let Some(Type::FunctionLiteral(function)) = member else {
            continue;
        };
        if name == INTO {
            validate_into(context, function, function_node);
        } else {
            validate_from_or_of(context, class_type, function, function_node);
        }
    }
}

/// how many of `parameters` a caller must supply, and whether any of them can be
/// filled positionally. The receiver (`self` / `cls`) is skipped: it is bound by
/// the attribute access the lowered call makes, not passed
fn arity_after_receiver(parameters: &Parameters<'_>) -> (usize, bool) {
    let rest = || parameters.iter().skip(1);
    let required = rest()
        .filter(|parameter| {
            parameter.default_type().is_none()
                && !parameter.is_variadic()
                && !parameter.is_keyword_variadic()
        })
        .count();
    let takes_positional =
        rest().any(|parameter| parameter.is_positional() || parameter.is_variadic());
    (required, takes_positional)
}

/// `__from__` / `__of__`: a classmethod on the target, taking one value and
/// returning the target
fn validate_from_or_of<'db>(
    context: &InferContext<'db, '_>,
    class: ClassType<'db>,
    function: FunctionType<'db>,
    function_node: &ast::StmtFunctionDef,
) {
    let env = context.program_environment();
    let db = context.db();
    let name = function_node.name.as_str();
    if !function.is_classmethod(db) {
        if let Some(builder) = context.report_lint(&INVALID_CONVERSION, &function_node.name) {
            let mut diagnostic =
                builder.into_diagnostic(format_args!("`{name}` must be a `class def`"));
            diagnostic.info(format_args!(
                "a conversion lowers to `{}.{name}(value)`, which would bind the value to \
                 the first parameter",
                class.name(db),
            ));
        }
        return;
    }
    let instance = Type::instance(db, env, class);
    for signature in function.signature(db) {
        let (required, takes_positional) = arity_after_receiver(signature.parameters());
        if required > 1 || !takes_positional {
            if let Some(builder) = context.report_lint(&INVALID_CONVERSION, &function_node.name) {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`{name}` must take exactly one value besides `cls`"
                ));
                diagnostic.info(format_args!(
                    "a conversion lowers to `{}.{name}(value)`, so a signature that call does \
                     not fit converts nothing",
                    class.name(db),
                ));
            }
            return;
        }
        if !signature.return_ty.is_assignable_to(db, env, instance) {
            if let Some(builder) = context.report_lint(&INVALID_CONVERSION, &function_node.name) {
                let mut diagnostic = builder
                    .into_diagnostic(format_args!("`{name}` must return `{}`", class.name(db)));
                diagnostic.info(format_args!(
                    "it returns `{}`, so no conversion site would ever accept it",
                    signature.return_ty.display(db, env),
                ));
            }
            return;
        }
    }
}

/// `__into__`: a plain instance method taking nothing, and only one of them
fn validate_into<'db>(
    context: &InferContext<'db, '_>,
    function: FunctionType<'db>,
    function_node: &ast::StmtFunctionDef,
) {
    let db = context.db();
    if function.is_classmethod(db) || function.is_staticmethod(db) {
        if let Some(builder) = context.report_lint(&INVALID_CONVERSION, &function_node.name) {
            let mut diagnostic =
                builder.into_diagnostic(format_args!("`{INTO}` must be an instance method"));
            diagnostic.info(format_args!(
                "a conversion lowers to `value.{INTO}()`, which calls it on the value itself",
            ));
        }
        return;
    }
    if function.signature(db).iter().len() > 1 {
        if let Some(builder) = context.report_lint(&INVALID_CONVERSION, &function_node.name) {
            let mut diagnostic =
                builder.into_diagnostic(format_args!("`{INTO}` may not be overloaded"));
            diagnostic.info(format_args!(
                "`value.{INTO}()` carries no target, so there is nothing to dispatch on",
            ));
            diagnostic.help(
                "Declare `__from__` on each target instead — that is the direction that dispatches",
            );
        }
        return;
    }
    for signature in function.signature(db) {
        let (required, _) = arity_after_receiver(signature.parameters());
        if required > 0 {
            if let Some(builder) = context.report_lint(&INVALID_CONVERSION, &function_node.name) {
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`{INTO}` may not take parameters besides `self`"
                ));
                diagnostic.info(format_args!(
                    "a conversion lowers to `value.{INTO}()`, which passes none",
                ));
            }
            return;
        }
    }
}

//! basedpython conversion dunders (`__from__`, `__into__`, `__of__`)
//!
//! A conversion is a *call*, not a subtype relation: `Celsius` is never
//! assignable to `Fahrenheit`, or `list[Celsius]` would be a `list[Fahrenheit]`
//! and reading an element back would hand out a value no one converted. So the
//! relation stays out of the lattice and lives only at the positions where the
//! transpiler can materialize the call — the same conversion-site rule
//! [implementations] are built on.
//!
//! This module owns that rule for all four routes a value can be repaired by,
//! so every site asks one question and gets one answer:
//!
//! - `T.__from__(x)` — a classmethod on the target taking the source
//! - `x.__into__()` — a method on the source returning the target
//! - `T.__of__(x)` — like `__from__`, but only when `x` is written out as a
//!   literal at the site
//! - an `implementation A for B:` witness, which [`super::implementations`]
//!   resolves and this module only routes to
//!
//! More than one applicable route is an error rather than a precedence rule:
//! `__from__` and `__into__` are hand-written bodies that can disagree, and
//! picking one silently would make the output depend on a rule nobody reads.

use ruff_db::files::File;
use ruff_python_ast as ast;
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::semantic_index;

use crate::Db;
use crate::types::call::CallArguments;
use crate::types::class::{ClassLiteral, ClassType, StaticClassLiteral};
use crate::types::context::InferContext;
use crate::types::diagnostic::{AMBIGUOUS_CONVERSION, INVALID_CONVERSION};
use crate::types::function::FunctionType;
use crate::types::implementations::{
    self, ImplementationRepair, imported_module_spelling, report_ambiguous_implementation,
};
use crate::types::signatures::Parameters;
use crate::types::{MemberLookupPolicy, Type, TypeContext};

/// the classmethod on a target that converts a value of some other type
pub(crate) const FROM: &str = "__from__";
/// the method on a source that converts it into the type it returns
pub(crate) const INTO: &str = "__into__";
/// the classmethod on a target that converts a *literal*
pub(crate) const OF: &str = "__of__";

/// every conversion dunder, for the declaration-site validation
pub(crate) const CONVERSION_DUNDERS: [&str; 3] = [FROM, INTO, OF];

/// one way a value can be made to satisfy a declared type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route<'db> {
    /// wrap the value in an `implementation A for B:` witness
    Witness(ImplementationRepair<'db>),
    /// `T.__from__(value)`, where `T` is the target class
    From(ClassType<'db>),
    /// `T.__of__(value)`, where `T` is the target class
    Of(ClassType<'db>),
    /// `value.__into__()`. carries the *source* type, for diagnostics only —
    /// the lowered call names nothing
    Into(Type<'db>),
}

impl<'db> Route<'db> {
    /// how the route reads in a diagnostic
    fn describe(self, db: &'db dyn Db) -> String {
        match self {
            Route::Witness(repair) => repair.witness.name(db).to_string(),
            Route::From(class) => format!("{}.{FROM}", class.name(db)),
            Route::Of(class) => format!("{}.{OF}", class.name(db)),
            Route::Into(source) => format!("{}.{INTO}", source.display(db)),
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
    if source.is_assignable_to(db, target) {
        return None;
    }
    // the value being converted is an ordinary value of the type it was
    // restricted from — `final Celsius` converts exactly as `Celsius` does
    let source = source.erase_restriction(db);

    let mut routes: Vec<Route<'db>> = Vec::new();
    if let Some(repair) = implementations::repair_with_implementation(db, file, source, target) {
        routes.push(Route::Witness(repair));
    }
    dunder_routes(db, source, target, value, &mut routes);

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
    source: Type<'db>,
    target: Type<'db>,
    value: Option<&ast::Expr>,
    routes: &mut Vec<Route<'db>>,
) {
    let literal = value.is_some_and(is_literal_expression);

    for arm in union_arms(db, target) {
        let Some(class) = arm.nominal_class(db) else {
            continue;
        };
        for dunder in [FROM, OF] {
            if dunder == OF && !literal {
                continue;
            }
            // the lowered call is `T.__from__(x)`, which binds `x` to `cls`
            // unless the member really is a classmethod. resolving the route the
            // same way the declaration is validated keeps a malformed dunder
            // from converting anything
            if conversion_classmethod(db, class, dunder).is_none() {
                continue;
            }
            if converts(db, arm, dunder, CallArguments::positional([source]), target) {
                routes.push(if dunder == FROM {
                    Route::From(class)
                } else {
                    Route::Of(class)
                });
            }
        }
    }

    if source_declares_into(db, source) && converts(db, source, INTO, CallArguments::none(), target)
    {
        routes.push(Route::Into(source));
    }
}

/// does calling `dunder` on `receiver` with `arguments` produce something the
/// target accepts? the ordinary call machinery answers, so overloads, generics
/// and descriptor binding all come from it rather than being re-derived here
fn converts<'db>(
    db: &'db dyn Db,
    receiver: Type<'db>,
    dunder: &str,
    arguments: CallArguments<'_, 'db>,
    target: Type<'db>,
) -> bool {
    receiver
        .try_call_dunder(db, dunder, arguments, TypeContext::default())
        .is_ok_and(|bindings| bindings.return_type(db).is_assignable_to(db, target))
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
fn source_declares_into<'db>(db: &'db dyn Db, source: Type<'db>) -> bool {
    let arms = union_arms(db, source);
    !arms.is_empty()
        && arms.iter().all(|arm| {
            arm.nominal_class(db)
                .is_some_and(|class| conversion_method(db, class).is_some())
        })
}

/// the `__from__` / `__of__` declared on `class`, when it is the classmethod the
/// lowered call needs
pub(crate) fn conversion_classmethod<'db>(
    db: &'db dyn Db,
    class: ClassType<'db>,
    dunder: &str,
) -> Option<FunctionType<'db>> {
    match class
        .class_member(db, dunder, MemberLookupPolicy::default())
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
    class: ClassType<'db>,
) -> Option<FunctionType<'db>> {
    match class
        .class_member(db, INTO, MemberLookupPolicy::default())
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
    let class = class.identity_specialization(db);
    CONVERSION_DUNDERS.iter().any(|dunder| {
        !class
            .class_member(db, dunder, MemberLookupPolicy::default())
            .place
            .is_undefined()
    })
}

/// might `ty` be one end of a conversion? the call gate's question, deliberately
/// over-approximate in both directions: a `true` only costs the full check that
/// would have run anyway, and anything this cannot classify answers `true`
pub(crate) fn may_convert<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    union_arms(db, ty).iter().any(|arm| {
        match arm.nominal_class(db).map(|class| class.class_literal(db)) {
            Some(ClassLiteral::Static(literal)) => *class_declares_conversion(db, literal),
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

/// report a conversion site served by more than one route. the site is otherwise
/// accepted — any of the conversions would work — but which one runs must not
/// depend on ordering
pub(crate) fn report_ambiguous_conversion<'db>(
    context: &InferContext<'db, '_>,
    node: impl Ranged,
    repair: &ConversionRepair<'db>,
) {
    let db = context.db();
    if repair.ambiguous_with.is_empty() {
        // one route, which may still be two implementations of the same pair —
        // that has its own message, and it names the interface and the type
        if let Route::Witness(witness) = repair.route {
            report_ambiguous_implementation(context, node, &witness);
        }
        return;
    }
    let Some(builder) = context.report_lint(&AMBIGUOUS_CONVERSION, node.range()) else {
        return;
    };
    let mut diagnostic = builder.into_diagnostic("More than one conversion applies here");
    let names: Vec<String> = std::iter::once(repair.route)
        .chain(repair.ambiguous_with.iter().copied())
        .map(|route| format!("`{}`", route.describe(db)))
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
    file: File,
    model: &crate::SemanticModel<'db>,
    value: &ast::Expr,
    declared: Type<'db>,
) -> Vec<(TextRange, ConversionRepair<'db>)> {
    let Some(value_ty) = crate::HasType::inferred_type(value, model) else {
        return Vec::new();
    };
    if let Some(repair) = repair_conversion(db, file, value_ty, declared, Some(value)) {
        return vec![(value.range(), repair)];
    }
    element_conversions(db, file, model, value, declared)
}

/// the per-element conversions a collection literal needs to satisfy `declared`.
///
/// Empty unless *every* element either already fits or converts: a partial answer
/// would leave the value unassignable, and the ordinary error is the right report
fn element_conversions<'db>(
    db: &'db dyn Db,
    file: File,
    model: &crate::SemanticModel<'db>,
    value: &ast::Expr,
    declared: Type<'db>,
) -> Vec<(TextRange, ConversionRepair<'db>)> {
    let Some(elements) = implementations::addressable_elements(value) else {
        return Vec::new();
    };
    let Some(element_target) = implementations::declared_element_type(db, declared) else {
        return Vec::new();
    };
    let mut conversions = Vec::new();
    for element in elements {
        let Some(element_ty) = crate::HasType::inferred_type(element, model) else {
            return Vec::new();
        };
        if element_ty.is_assignable_to(db, element_target) {
            continue;
        }
        match repair_conversion(db, file, element_ty, element_target, Some(element)) {
            Some(repair) => conversions.push((element.range(), repair)),
            // one element that neither fits nor converts sinks the whole value
            None => return Vec::new(),
        }
    }
    conversions
}

/// the sub-expression of `value` covering exactly `range`.
///
/// An element-wise conversion wraps an element rather than the whole literal,
/// and the name it emits has to resolve at *that* expression — which is the same
/// scope in every case a comprehension is not involved, and a different one when
/// it is
pub(crate) fn expression_at(value: &ast::Expr, range: TextRange) -> Option<&ast::Expr> {
    implementations::addressable_elements(value)?
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

/// how the transpiler materializes one conversion
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionInfo {
    /// emit `prefix`, the value, then `suffix`
    Call {
        prefix: String,
        suffix: String,
        /// the module-level name `prefix` spells, which python binds when its own
        /// statement runs — so a conversion at import time may not precede it.
        /// `None` for a route that names nothing (`__into__`)
        referenced_name: Option<String>,
        import: Option<ConversionImport>,
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
        Route::Witness(witness) => {
            match implementations::conversion_info(db, from_file, &witness) {
                // a witness name is generated and already collision-proof, so unlike
                // a user's class name below it needs no alias
                Some(info) => ConversionInfo::Call {
                    prefix: format!("{}(", info.witness),
                    suffix: ")".to_owned(),
                    referenced_name: Some(info.witness.clone()),
                    import: info.import_from.map(|module| ConversionImport {
                        module,
                        name: info.witness.clone(),
                        alias: info.witness,
                    }),
                },
                None => ConversionInfo::Rejected(
                    "the `implementation` this value converts through cannot be named here"
                        .to_owned(),
                ),
            }
        }
        Route::From(class) => dunder_call_info(db, from_file, model, anchor, class, FROM),
        Route::Of(class) => dunder_call_info(db, from_file, model, anchor, class, OF),
        // the receiver is the value itself, so nothing has to be named or
        // imported. the parentheses are what make it safe to wrap an operand of
        // any precedence
        Route::Into(_) => ConversionInfo::Call {
            prefix: "(".to_owned(),
            suffix: format!(").{INTO}()"),
            referenced_name: None,
            import: None,
        },
    }
}

/// `T.__from__(` / `T.__of__(`, with `T` spelled so that it resolves to the
/// target class *at the conversion site*
fn dunder_call_info<'db>(
    db: &'db dyn Db,
    from_file: File,
    model: &crate::SemanticModel<'db>,
    anchor: &ast::Expr,
    class: ClassType<'db>,
    dunder: &str,
) -> ConversionInfo {
    match class_reference(db, from_file, model, anchor, class) {
        Ok((name, import)) => ConversionInfo::Call {
            prefix: format!("{name}.{dunder}("),
            suffix: ")".to_owned(),
            referenced_name: Some(name),
            import,
        },
        Err(reason) => ConversionInfo::Rejected(reason),
    }
}

/// the alias a cross-file conversion imports its target under.
///
/// Always aliased, never the class's own name: this file may already bind that
/// name to something else, and an import that silently rebinds it — or that the
/// file's own class then shadows — turns the conversion into an `AttributeError`
/// at runtime. One leading underscore, not two, so a reference inside a class
/// body is not python name-mangled
fn conversion_alias(name: &str) -> String {
    format!("_by_conv__{name}")
}

/// how the conversion site spells `class`: its own name when this file declares
/// it and nothing between the site and the module shadows that name, otherwise
/// an aliased import. `Err` when neither is possible
fn class_reference<'db>(
    db: &'db dyn Db,
    from_file: File,
    model: &crate::SemanticModel<'db>,
    anchor: &ast::Expr,
    class: ClassType<'db>,
) -> Result<(String, Option<ConversionImport>), String> {
    let ClassLiteral::Static(literal) = class.class_literal(db) else {
        return Err("this value converts through a type that has no class statement".to_owned());
    };
    let name = literal.name(db).to_string();
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
    let index = semantic_index(db, file);
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
            .class_member(db, name, MemberLookupPolicy::default())
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
    let instance = Type::instance(db, class);
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
        if !signature.return_ty.is_assignable_to(db, instance) {
            if let Some(builder) = context.report_lint(&INVALID_CONVERSION, &function_node.name) {
                let mut diagnostic = builder
                    .into_diagnostic(format_args!("`{name}` must return `{}`", class.name(db)));
                diagnostic.info(format_args!(
                    "it returns `{}`, so no conversion site would ever accept it",
                    signature.return_ty.display(db),
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

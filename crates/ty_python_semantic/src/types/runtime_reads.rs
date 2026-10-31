//! basedpython: whether each module-level name a lowering writes into code that runs holds
//! there the binding the checker resolved
//!
//! a type test, a checked cast and a bare call or construction of a reified generic are
//! lowered to code that names classes — the test's target, the specialization the call is
//! given — and the program has to find there the class the checker resolved. a name bound
//! only under `if TYPE_CHECKING:`, only on some paths, or only further on does not hold it
//! ([`runtime_binding`]). which one it is depends on every
//! binding the module makes of the name and on every condition deciding whether each is
//! made, the conditions after the site included, so it is asked once the scope holding the
//! site, and every scope around it, is inferred ([`InferredInCheckOrder`])
//!
//! inference only records where to ask ([`RuntimeReadCheck`]). asked from inside the
//! module's own inference, the question would infer every condition below the site out of
//! order, each one inside the inference of the next

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, AnyRootNodeRef, CastKind, NodeIndex};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::FxHashSet;
use ty_python_core::definition::Definition;
use ty_python_core::scope::{FileScopeId, NodeWithScopeKind, ScopeId};
use ty_python_core::{ProgramFile, semantic_index};

use crate::place::{RuntimeBinding, RuntimeRead, runtime_binding};
use crate::reified::{method_receiver, reified_class_reads};
use crate::semantic_model::{HasType, SemanticModel};
use crate::types::check_order::{InferredInCheckOrder, infer_scope_in_check_order};
use crate::types::class::{ClassLiteral, ClassType};
use crate::types::class_base::ClassBase;
use crate::types::context::InferContext;
use crate::types::diagnostic::{
    ERASED_TYPE_CHECK, NOT_SUBSCRIPTABLE, TypeCheckDiagnostics, UNRESOLVED_REFERENCE,
    report_erased_type_check, report_invalid_reified_override, report_unspecialized_reified_call,
    report_unspecialized_reified_classmethod, report_unspecialized_reified_construction,
    report_untestable_type_argument,
};
use crate::types::function::FunctionType;
use crate::types::generics::GenericContext;
use crate::types::infer::{infer_scope_types, original_class_type};
use crate::types::overrides::{OverrideStep, overridden_declarations};
use crate::types::reified_infer::{
    BareGenericCall, ParametricIsPlan, ReifiedOverrideError, TypeParamCells, WrittenTarget,
    constructor_specialization, forwarded_override_error, inferred_call_type_arguments,
    solved_type_arguments, type_test_plan, untestable_type_argument, written_unbound_read,
};
use crate::types::runtime_spelling::RuntimeSite;
use crate::types::soundness::{CastCheck, cast_check_plan, cast_is_redundant};
use crate::types::subscript::defers_type_arguments;
use crate::types::typevar::{BindingContext, BoundTypeVarInstance};
use crate::types::{ProgramEnvironment, Type, TypeContext, binding_type};
use crate::{Db, FxIndexMap, FxIndexSet};

/// a site inference found that writes module-level names into code that runs, and whether
/// the inference that found it reports diagnostics there
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct RuntimeReadCheck {
    pub(crate) site: RuntimeReadSite,
    pub(crate) reports: bool,
}

/// what writes the names, by the nodes it is lowered from
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(crate) enum RuntimeReadSite {
    /// `value is target`, a test inference found checkable wherever the names hold
    TypeTest { value: NodeIndex, target: NodeIndex },
    /// `value cast! target` or `value cast? target`
    CheckedCast { value: NodeIndex, target: NodeIndex },
    /// a bare call of a generic function or a construction of a reified class, which is
    /// checked here because whether a type parameter of the function around it has a runtime
    /// value depends on the inference of that function's body, which holds this very site
    Reified {
        call: NodeIndex,
        generic: ReifiedGeneric,
    },
    /// a method with type parameters of its own, whose override compatibility depends on what
    /// its body forwards, and on what the body of the method it overrides does
    GenericMethod { function: NodeIndex },
    /// a specialization written as a value, `f[int]` or `A[int]`, whose type arguments the
    /// generic may go on to test a value against, and whose names are read where it runs
    Specialization { subscript: NodeIndex },
    /// a classmethod called on a generic class no specialization was made of, which may read a
    /// type argument the class has none of
    BareClassmethod { call: NodeIndex },
}

/// the reified generic a bare call specializes
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(crate) enum ReifiedGeneric {
    /// the function called, by name
    Function(Name),
    /// the class constructed, by name
    Class(Name),
}

/// what [`runtime_read_checks`] found for one scope
#[derive(Debug, Default, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct RuntimeReadChecks {
    /// a site whose names do not hold the bindings the checker resolved
    pub(crate) diagnostics: TypeCheckDiagnostics,
    /// the module-level names the type tests and casts are written with, which the source
    /// itself need not read
    pub(crate) module_reads: Box<[Name]>,
}

/// [`runtime_read_checks`] of every scope of a checked `file` whose inference recorded a site,
/// which most scopes have none of
pub(crate) fn file_runtime_read_checks<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> impl Iterator<Item = &'db RuntimeReadChecks> {
    semantic_index(db, file)
        .scope_ids()
        .filter(move |scope| {
            !scope.accepts_type_context(db)
                && infer_scope_types(db, *scope, TypeContext::default())
                    .diagnostics()
                    .is_some_and(|inferred| !inferred.runtime_read_checks().is_empty())
        })
        .map(move |scope| runtime_read_checks(db, scope))
}

/// checks every site the inference of `scope` recorded ([`RuntimeReadCheck`]), once `scope`
/// and every scope around it are inferred
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
pub(crate) fn runtime_read_checks<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> RuntimeReadChecks {
    let (inferred, in_check_order) = infer_scope_in_check_order(db, scope);
    let Some(inferred) = inferred.diagnostics() else {
        return RuntimeReadChecks::default();
    };
    // a site inferred more than once is checked once, and reported if any inference of it
    // would have reported there
    let mut sites: FxIndexMap<&RuntimeReadSite, bool> = FxIndexMap::default();
    for check in inferred.runtime_read_checks() {
        *sites.entry(&check.site).or_default() |= check.reports;
    }
    if sites.is_empty() {
        return RuntimeReadChecks::default();
    }

    let file = scope.program_file(db);
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let env = ProgramEnvironment::from_file(file);
    let model = SemanticModel::new(db, file);
    let mut checker = RuntimeReadChecker {
        db,
        env: &env,
        file: file.file(db),
        parsed: &parsed,
        model: &model,
        in_check_order,
        contexts: FxIndexMap::default(),
        module_reads: FxIndexSet::default(),
    };
    for (site, reports) in sites {
        checker.check(site, reports);
    }
    let mut diagnostics = TypeCheckDiagnostics::default();
    for context in checker.contexts.into_values() {
        diagnostics.extend(&context.finish());
    }
    diagnostics.shrink_to_fit();
    RuntimeReadChecks {
        diagnostics,
        module_reads: checker.module_reads.into_iter().collect(),
    }
}

/// checks the sites one scope's inference recorded
struct RuntimeReadChecker<'a, 'db> {
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    file: File,
    parsed: &'a ParsedModuleRef,
    model: &'a SemanticModel<'db>,
    in_check_order: InferredInCheckOrder<'db>,
    /// a context per scope a site is in, which decides whether a diagnostic there is reported
    contexts: FxIndexMap<FileScopeId, InferContext<'db, 'a>>,
    module_reads: FxIndexSet<Name>,
}

impl<'a, 'db> RuntimeReadChecker<'a, 'db> {
    fn expression(&self, index: NodeIndex) -> Option<&'a ast::Expr> {
        match self.parsed.get_by_index(index) {
            AnyRootNodeRef::Expr(expr) => Some(expr),
            _ => None,
        }
    }

    fn context(&mut self, node: &ast::Expr) -> Option<&InferContext<'db, 'a>> {
        let file = self.model.program_file();
        let scope = semantic_index(self.db, file).try_expression_scope_id(node)?;
        let (db, env, source_file, parsed) = (self.db, self.env, self.file, self.parsed);
        Some(self.contexts.entry(scope).or_insert_with(|| {
            InferContext::new(
                db,
                env,
                scope.to_scope_id(db, file),
                source_file,
                file,
                parsed,
            )
        }))
    }

    fn site(&self, node: &ast::Expr) -> RuntimeSite<'db> {
        RuntimeSite::at(node, self.in_check_order)
    }

    fn check(&mut self, site: &RuntimeReadSite, reports: bool) {
        match site {
            RuntimeReadSite::TypeTest { value, target } => {
                if let (Some(value), Some(target)) =
                    (self.expression(*value), self.expression(*target))
                {
                    self.type_test(value, target, reports);
                }
            }
            RuntimeReadSite::CheckedCast { value, target } => {
                if let (Some(value), Some(target)) =
                    (self.expression(*value), self.expression(*target))
                {
                    self.checked_cast(value, target, reports);
                }
            }
            RuntimeReadSite::Reified { call, generic } => {
                if reports && let Some(ast::Expr::Call(call)) = self.expression(*call) {
                    self.reified(call, generic);
                }
            }
            RuntimeReadSite::GenericMethod { function } => {
                if reports
                    && let AnyRootNodeRef::Stmt(ast::Stmt::FunctionDef(function)) =
                        self.parsed.get_by_index(*function)
                {
                    self.generic_method(function);
                }
            }
            RuntimeReadSite::Specialization { subscript } => {
                if reports && let Some(subscript) = self.expression(*subscript) {
                    self.specialization(subscript);
                }
            }
            RuntimeReadSite::BareClassmethod { call } => {
                if reports && let Some(ast::Expr::Call(call)) = self.expression(*call) {
                    self.bare_classmethod(call);
                }
            }
        }
    }

    /// a specialization written as a value is evaluated where it stands, and what it specializes
    /// then holds its type arguments
    fn specialization(&mut self, subscript_expression: &'a ast::Expr) {
        let (db, model) = (self.db, self.model);
        let ast::Expr::Subscript(subscript) = subscript_expression else {
            return;
        };
        let Some(value_ty) = subscript.value.inferred_type(model) else {
            return;
        };
        let Some(generic) = GenericUse::of(db, value_ty) else {
            return;
        };
        let source_type = self.file.source_type(db);
        if source_type.is_stub() {
            return;
        }
        if !source_type.is_basedpython() {
            self.python_subscript(subscript, value_ty, &generic);
            return;
        }
        let reads_type_arguments = match generic.function {
            Some(function) => function_takes_type_arguments(db, function),
            None => !defers_type_arguments(db, value_ty),
        };
        if reads_type_arguments {
            self.report_unbound_reads(subscript_expression);
        } else if generic.function.is_none() {
            self.report_unbound_reads(&subscript.value);
        }
        let tested = tested_type_params(db, generic.scope);
        if tested.is_empty() {
            return;
        }
        for (typevar, argument, node) in
            written_type_arguments(db, model, subscript, generic.context)
        {
            let parameter = typevar.name(db);
            if tested.contains(parameter)
                && let Some(reason) =
                    untestable_type_argument(db, self.env, self.file, argument, node)
            {
                let source = source_text(db, self.file);
                let Some(context) = self.context(&subscript.value) else {
                    return;
                };
                report_untestable_type_argument(
                    context,
                    node.range(),
                    &source[node.range()],
                    reason,
                    generic.name.as_str(),
                    parameter,
                );
            }
        }
    }

    /// a `.py` file runs as written, so its subscript of a function reaches the function, and
    /// only a `.by` function that takes type arguments is lowered to one that accepts them
    fn python_subscript(
        &mut self,
        subscript: &'a ast::ExprSubscript,
        value_ty: Type<'db>,
        generic: &GenericUse<'db>,
    ) {
        let db = self.db;
        let Some(function) = generic.function else {
            return;
        };
        if function.file(db).source_type(db).is_basedpython()
            && function_takes_type_arguments(db, function)
        {
            return;
        }
        let env = self.env;
        let Some(builder) = self
            .context(&subscript.value)
            .and_then(|context| context.report_lint(&NOT_SUBSCRIPTABLE, &*subscript.value))
        else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "Cannot subscript object of type `{}`",
            value_ty.display(db, env)
        ));
        diagnostic.info("a python function has no `__getitem__`, and a `.py` file runs as written");
    }

    /// reports each name `node` reads that does not hold, where it runs, the binding the checker
    /// resolved. one the checker could not resolve either has been reported as it was inferred
    fn report_unbound_reads(&mut self, node: &ast::Expr) {
        struct Reads<'a, 'db> {
            db: &'db dyn Db,
            env: &'a ProgramEnvironment<'db>,
            model: &'a SemanticModel<'db>,
            inferred: InferredInCheckOrder<'db>,
            found: Vec<(TextRange, Name, RuntimeBinding)>,
        }
        impl<'ast> Visitor<'ast> for Reads<'_, '_> {
            fn visit_expr(&mut self, expr: &'ast ast::Expr) {
                if let ast::Expr::Name(name) = expr {
                    if name.ctx.is_load()
                        && !expr
                            .inferred_type(self.model)
                            .is_some_and(|ty| ty.is_dynamic())
                    {
                        let binding = runtime_binding(
                            self.db,
                            self.env,
                            RuntimeRead::Written(ast::ExprRef::from(expr)),
                            self.inferred,
                        );
                        if binding != RuntimeBinding::Bound {
                            self.found.push((name.range(), name.id.clone(), binding));
                        }
                    }
                    return;
                }
                walk_expr(self, expr);
            }
        }
        let mut reads = Reads {
            db: self.db,
            env: self.env,
            model: self.model,
            inferred: self.in_check_order,
            found: Vec::new(),
        };
        reads.visit_expr(node);
        for (range, name, binding) in reads.found {
            let Some(builder) = self
                .context(node)
                .and_then(|context| context.report_lint(&UNRESOLVED_REFERENCE, range))
            else {
                continue;
            };
            let mut diagnostic = builder.into_diagnostic(match binding {
                RuntimeBinding::NotTheCheckers => format!(
                    "the `{name}` bound where this runs need not be the class the checker resolved"
                ),
                RuntimeBinding::Bound | RuntimeBinding::Unbound => {
                    format!("Name `{name}` is not bound where this runs")
                }
            });
            diagnostic.info(binding.describe(&name));
        }
    }

    /// a classmethod reached through a class no specialization was made of reads its type
    /// argument from a class that has none, unless the class has it from a base or a default
    fn bare_classmethod(&mut self, call: &'a ast::ExprCall) {
        let (db, env) = (self.db, self.env);
        let Some(Type::BoundMethod(method)) = call.func.inferred_type(self.model) else {
            return;
        };
        let (Some(function), Type::ClassLiteral(ClassLiteral::Static(receiver))) =
            (method.function(db), method.self_instance(db))
        else {
            return;
        };
        let reads = classmethod_type_argument_reads(db, function_body_scope(db, function));
        let unfilled = reads.iter().find(|typevar| {
            let argument = receiver
                .identity_specialization(db)
                .iter_mro(db)
                .find_map(|base| {
                    let ClassBase::Class(ClassType::Generic(alias)) = base else {
                        return None;
                    };
                    let position = alias
                        .origin(db)
                        .generic_context(db)?
                        .variables(db)
                        .position(|variable| variable == **typevar)?;
                    alias.specialization(db).types(db).get(position).copied()
                });
            match argument {
                None => false,
                Some(Type::TypeVar(argument)) => {
                    argument.typevar(db).default_type(db, env).is_none()
                }
                Some(argument) => argument.is_dynamic(),
            }
        });
        if let Some(typevar) = unfilled
            && let Some(context) = self.context(&call.func)
        {
            report_unspecialized_reified_classmethod(
                context,
                call.range(),
                function.name(db),
                receiver.name(db),
                typevar.name(db),
            );
        }
    }

    /// a method's forwarded type parameters are part of its runtime interface, as a declared
    /// `reified` list is: a call through the base reaches this method
    fn generic_method(&mut self, function: &'a ast::StmtFunctionDef) {
        let (db, env) = (self.db, self.env);
        let file = self.model.program_file();
        let index = semantic_index(db, file);
        let definition = index.expect_single_definition(function);
        let Type::FunctionLiteral(sub) = binding_type(db, definition) else {
            return;
        };
        let Some(class_node) = index.scope(definition.file_scope(db)).node().as_class() else {
            return;
        };
        let Some(class) = original_class_type(db, index.expect_single_definition(class_node))
        else {
            return;
        };
        let class = class.identity_specialization(db);
        for step in overridden_declarations(db, env, class, &function.name.id) {
            let OverrideStep::Overrides(declaration) = step else {
                continue;
            };
            let base = match declaration.ty {
                Type::FunctionLiteral(base) => Some(base),
                Type::BoundMethod(method) => method.function(db),
                _ => None,
            };
            if let Some(base) = base
                && let Some(error) = forwarded_override_error(db, env, base, sub)
                    .or_else(|| tests_more_than_base(db, base, sub))
            {
                let (source_file, parsed) = (self.file, self.parsed);
                let scope = definition.file_scope(db);
                let context = self.contexts.entry(scope).or_insert_with(|| {
                    InferContext::new(db, env, definition.scope(db), source_file, file, parsed)
                });
                report_invalid_reified_override(
                    context,
                    function.name.id.as_str(),
                    definition,
                    sub,
                    declaration.superclass,
                    error,
                );
            }
            // the nearest declaration is the one a more distant one is already checked against
            return;
        }
    }

    /// a bare call or construction that has to name a specialization, where no spelling of the
    /// solved one holds where the call runs
    fn reified(&mut self, call: &'a ast::ExprCall, generic: &ReifiedGeneric) {
        let (db, env, file, model) = (self.db, self.env, self.file, self.model);
        let site = RuntimeSite::at(call, self.in_check_order);
        match generic {
            ReifiedGeneric::Function(callee) => {
                let Some(bare_call) = BareGenericCall::of(db, model, call) else {
                    return;
                };
                let function = bare_call.function;
                if !function.is_unspecialized_reified(db)
                    && function.forwarded_type_params(db).required.is_empty()
                {
                    return;
                }
                // a reason of the call's own comes ahead of a name the spelling reads not being
                // bound, whichever argument each is about
                let inference = if bare_call.arguments.is_some() {
                    let arguments = |site| {
                        inferred_call_type_arguments(
                            db,
                            env,
                            file,
                            &bare_call,
                            site,
                            TypeParamCells::Lowered(None),
                        )
                    };
                    arguments(None)
                        .and_then(|_| arguments(Some(site)))
                        .map(|_| ())
                        .map_err(Some)
                } else {
                    Err(None)
                };
                if let Err(failure) = inference
                    && let Some(context) = self.context(&call.func)
                {
                    report_unspecialized_reified_call(context, call.range(), callee, failure);
                }
            }
            ReifiedGeneric::Class(class) => {
                let (Some(callee), Some(constructed)) =
                    (call.func.inferred_type(model), call.inferred_type(model))
                else {
                    return;
                };
                let Some(class_literal) = callee.as_class_literal() else {
                    return;
                };
                let specialization = |site| {
                    constructor_specialization(
                        db,
                        env,
                        file,
                        class_literal,
                        constructed,
                        site,
                        TypeParamCells::Lowered(None),
                    )
                };
                if let Err(failure) = specialization(None).and_then(|_| specialization(Some(site)))
                    && let Some(context) = self.context(&call.func)
                {
                    report_unspecialized_reified_construction(
                        context,
                        call.range(),
                        class,
                        failure,
                    );
                }
            }
        }
    }

    fn type_test(&mut self, value: &ast::Expr, target: &'a ast::Expr, reports: bool) {
        let (Some(value_ty), Some(target_ty)) = (
            value.inferred_type(self.model),
            target.inferred_type(self.model),
        ) else {
            return;
        };
        let written = WrittenTarget::checked(target, self.in_check_order);
        let plan = type_test_plan(
            self.db,
            self.env,
            self.file,
            value_ty,
            target_ty,
            Some(written),
        );
        self.read_spellings(&plan);
        if reports && let ParametricIsPlan::ErasedTarget(reason) = plan {
            let source = source_text(self.db, self.file);
            let primary = TextRange::new(value.start(), target.end());
            if let Some(context) = self.context(target) {
                report_erased_type_check(context, primary, &source[target.range()], reason);
            }
        }
    }

    /// a checked cast's runtime check is the type test the cast shares with `is`, falling
    /// back to the shallow target the soundness checks use, so both are read as spelled
    fn checked_cast(&mut self, value: &ast::Expr, target: &'a ast::Expr, reports: bool) {
        let (Some(value_ty), Some(target_ty)) = (
            value.inferred_type(self.model),
            target.inferred_type(self.model),
        ) else {
            return;
        };
        let (db, env, file) = (self.db, self.env, self.file);
        let written = WrittenTarget::checked(target, self.in_check_order);
        let plan = type_test_plan(db, env, file, value_ty, target_ty, Some(written));
        self.read_spellings(&plan);
        let check = cast_check_plan(db, env, file, target_ty, Some(self.site(target)));
        if let Some(spelling) = check.as_ref().and_then(CastCheck::spelling) {
            self.module_reads.extend(spelling.global_reads().cloned());
        }
        // a cast with no runtime check the checker can spell tests its value against the
        // target as written, so a target reading a name that does not hold there the binding
        // the checker resolved — a class imported only under `if TYPE_CHECKING:` — would
        // raise `NameError`, or test against another class. a cast that needs no check (a
        // widening) is a plain `typing.cast`, whose target is written as a string
        if !reports || check.is_some() || cast_is_redundant(db, env, value_ty, target_ty) {
            return;
        }
        let Some((name, binding)) = written_unbound_read(db, env, target, self.in_check_order)
        else {
            return;
        };
        let Some(builder) = self
            .context(target)
            .and_then(|context| context.report_lint(&ERASED_TYPE_CHECK, target))
        else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "`{}` cannot be checked at runtime: {}",
            target_ty.display(db, env),
            binding.why_not_checkable(&name)
        ));
        diagnostic.info(binding.describe(&name));
    }

    fn read_spellings(&mut self, plan: &ParametricIsPlan) {
        for spelling in plan.spellings() {
            self.module_reads.extend(spelling.global_reads().cloned());
        }
    }
}

/// whether the override `sub` tests a value against a type parameter whose position in the base
/// tests nothing, when a specialization through the base reaches the override
fn tests_more_than_base<'db>(
    db: &'db dyn Db,
    base: FunctionType<'db>,
    sub: FunctionType<'db>,
) -> Option<ReifiedOverrideError<'db>> {
    let positions = |function: FunctionType<'db>| -> Vec<(usize, Name)> {
        let tested = tested_type_params(db, function_body_scope(db, function));
        function
            .signature(db)
            .overloads
            .first()
            .and_then(|overload| overload.generic_context)
            .into_iter()
            .flat_map(|context| context.variables(db))
            .enumerate()
            .map(|(position, variable)| (position, variable.name(db).clone()))
            .filter(|(_, name)| tested.contains(name))
            .collect()
    };
    if !function_takes_type_arguments(db, base) {
        return None;
    }
    let base_tested = positions(base);
    let untested: Box<[Name]> = positions(sub)
        .into_iter()
        .filter(|(position, _)| !base_tested.iter().any(|(tested, _)| tested == position))
        .map(|(_, name)| name)
        .collect();
    (!untested.is_empty()).then_some(ReifiedOverrideError::TestsUntested(untested))
}

/// a function or class a specialization is being made of
struct GenericUse<'db> {
    /// the body that decides what the type arguments are used for
    scope: ScopeId<'db>,
    context: GenericContext<'db>,
    name: Name,
    /// the function, when it isn't a class
    function: Option<FunctionType<'db>>,
}

impl<'db> GenericUse<'db> {
    fn of(db: &'db dyn Db, ty: Type<'db>) -> Option<Self> {
        let function = match ty {
            Type::ClassLiteral(class) => {
                let class = class.as_static()?;
                return Some(Self {
                    scope: class.body_scope(db),
                    context: class.generic_context(db)?,
                    name: class.name(db).clone(),
                    function: None,
                });
            }
            Type::FunctionLiteral(function) => function,
            // a method that erases its type arguments is reported as not subscriptable where it
            // is inferred, and a classmethod hides the function a specialization would be given
            Type::BoundMethod(method) if !method.binds_classmethod(db) => method
                .function(db)
                .filter(|function| function.is_reified(db))?,
            _ => return None,
        };
        Some(Self {
            scope: function_body_scope(db, function),
            context: function.signature(db).overloads.first()?.generic_context?,
            name: function.name(db).clone(),
            function: Some(function),
        })
    }
}

fn function_body_scope<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> ScopeId<'db> {
    function.literal(db).last_definition.body_scope(db)
}

/// whether the python written for `function` takes the subscript that specializes it. this reads
/// what its body forwards, so it is asked once the file is inferred
fn function_takes_type_arguments<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> bool {
    function.is_reified(db) || !function.forwarded_type_params(db).names.is_empty()
}

/// each plain type parameter a specialization gives an argument to, with the argument and what
/// wrote it. a run a `*Ts` absorbs and the fields of a pack hold a tuple and a mapping, which a
/// test does not read as one type
fn written_type_arguments<'a, 'db>(
    db: &'db dyn Db,
    model: &SemanticModel<'db>,
    subscript: &'a ast::ExprSubscript,
    context: GenericContext<'db>,
) -> Vec<(BoundTypeVarInstance<'db>, Type<'db>, &'a ast::Expr)> {
    let elements = match subscript.slice.as_ref() {
        ast::Expr::Tuple(tuple) if !tuple.parenthesized => tuple.elts.as_slice(),
        slice => std::slice::from_ref(slice),
    };
    let keyword = |element: &'a ast::Expr| match element {
        ast::Expr::Named(named) => match named.target.as_ref() {
            ast::Expr::Name(name) if name.ctx.is_invalid() => Some((&name.id, &*named.value)),
            _ => None,
        },
        _ => None,
    };
    let keywords: Vec<(&Name, &'a ast::Expr)> = elements.iter().filter_map(keyword).collect();
    let positional: Vec<&'a ast::Expr> = elements
        .iter()
        .filter(|element| keyword(element).is_none())
        .collect();
    // a pack takes no positional argument, and a `*Ts` leaves those after it to the end of the list
    let slots: Vec<BoundTypeVarInstance<'db>> = context
        .variables(db)
        .filter(|variable| !variable.is_keyword_variadic(db))
        .collect();
    let variadic = slots
        .iter()
        .position(|variable| variable.is_typevartuple(db));
    let mut found = Vec::new();
    for (index, variable) in slots.iter().enumerate() {
        if variable.is_typevartuple(db) {
            continue;
        }
        let name = variable.name(db);
        let node = keywords
            .iter()
            .find(|(keyword, _)| *keyword == name)
            .map(|(_, value)| *value)
            .or_else(|| match variadic {
                Some(variadic) if index > variadic => positional
                    .len()
                    .checked_sub(slots.len() - index)
                    .and_then(|position| positional.get(position).copied()),
                _ => positional.get(index).copied(),
            });
        if let Some(node) = node
            && let Some(argument) = node.inferred_type(model)
        {
            found.push((*variable, argument, node));
        }
    }
    found
}

/// each type parameter's argument in the specialization a construction built
fn constructed_type_arguments<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    constructed: Type<'db>,
) -> Vec<(Name, Type<'db>)> {
    let Type::NominalInstance(instance) = constructed.promote_keeping_literals_in(db, env, file)
    else {
        return Vec::new();
    };
    let ClassType::Generic(alias) = instance.class(db, env) else {
        return Vec::new();
    };
    let Some(context) = alias.origin(db).generic_context(db) else {
        return Vec::new();
    };
    context
        .variables(db)
        .zip(alias.specialization(db).types(db))
        .map(|(variable, argument)| (variable.name(db).clone(), *argument))
        .collect()
}

/// the type parameters of the function or class with this body whose value reaches a runtime test
/// of a value: `x is T`, a checked cast to `T`, or handing `T` on to a type parameter that does.
/// a type argument the runtime cannot test against is refused where one is given to these
///
/// ```by
/// def chk[T](x: object) -> bool:
///     return x is T
///
/// def hands_on[U](x: object) -> bool:
///     return chk[U](x)     # `U` is tested, so `hands_on[UserId]` crashes as `chk[UserId]` does
/// ```
///
/// this reads the inference of the body, so nothing inference asks may depend on it. it is a fixed
/// point over the call graph, starting from nothing: a parameter is only added by what a callee
/// already tests
#[salsa::tracked(
    returns(deref),
    cycle_initial=|_, _, _| Box::default(),
    heap_size=ruff_memory_usage::heap_size
)]
fn tested_type_params<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> Box<[Name]> {
    let source_type = scope.file(db).source_type(db);
    if !source_type.is_basedpython() || source_type.is_stub() {
        return Box::default();
    }
    let module = parsed_module(db, scope.python_file(db)).load(db);
    let file = scope.program_file(db);
    let index = semantic_index(db, file);
    let (definition, body, parameters) = match scope.node(db) {
        NodeWithScopeKind::Function(function) => {
            let function = function.node(&module);
            (
                index.expect_single_definition(function),
                &function.body,
                function.type_params.as_deref(),
            )
        }
        NodeWithScopeKind::Class(class) => {
            let class = class.node(&module);
            (
                index.expect_single_definition(class),
                &class.body,
                class.type_params.as_deref(),
            )
        }
        _ => return Box::default(),
    };
    let Some(parameters) = parameters else {
        return Box::default();
    };
    let model = SemanticModel::new(db, file);
    let env = ProgramEnvironment::from_file(file);
    let mut cells = TestedCells {
        db,
        env: &env,
        file: scope.file(db),
        model: &model,
        definition,
        tested: FxHashSet::default(),
    };
    cells.visit_body(body);
    if let Type::ClassLiteral(ClassLiteral::Static(class)) = binding_type(db, definition) {
        cells.class_bases(class.explicit_bases(db));
    }
    parameters
        .iter()
        .map(|parameter| parameter.name().id.clone())
        .filter(|name| cells.tested.contains(name))
        .collect()
}

/// walks a body for what it does with the value of the type parameters `definition` binds
struct TestedCells<'a, 'db> {
    db: &'db dyn Db,
    env: &'a ProgramEnvironment<'db>,
    file: File,
    model: &'a SemanticModel<'db>,
    definition: Definition<'db>,
    tested: FxHashSet<Name>,
}

impl<'db> TestedCells<'_, 'db> {
    /// every type parameter of `definition` a test against `ty` reads the value of: a union is
    /// tested arm by arm, and an argument of a class in it is compared rather than tested against
    fn mark(&mut self, ty: Type<'db>, open: &mut Vec<Type<'db>>) {
        let db = self.db;
        match ty {
            Type::TypeVar(bound_typevar) => {
                if matches!(
                    bound_typevar.binding_context(db),
                    BindingContext::Definition(definition) if definition == self.definition
                ) {
                    self.tested.insert(bound_typevar.name(db).clone());
                }
            }
            Type::Union(union) => {
                for element in union.elements(db) {
                    self.mark(*element, open);
                }
            }
            Type::TypeAlias(alias) => {
                if !open.contains(&ty) {
                    open.push(ty);
                    self.mark(alias.value_type(db), open);
                    open.pop();
                }
            }
            Type::Restricted(restricted) => self.mark(restricted.value_type(db), open),
            _ => {}
        }
    }

    fn type_test(&mut self, value: &ast::Expr, target: &ast::Expr) {
        let (Some(value_ty), Some(target_ty)) = (
            value.inferred_type(self.model),
            target.inferred_type(self.model),
        ) else {
            return;
        };
        let plan = type_test_plan(
            self.db,
            self.env,
            self.file,
            value_ty,
            target_ty,
            Some(WrittenTarget::unchecked(target)),
        );
        if plan.tests_cell() {
            self.mark(target_ty, &mut Vec::new());
        }
    }

    fn checked_cast(&mut self, target: &ast::Expr, value: &ast::Expr) {
        let (Some(value_ty), Some(target_ty)) = (
            value.inferred_type(self.model),
            target.inferred_type(self.model),
        ) else {
            return;
        };
        if !cast_is_redundant(self.db, self.env, value_ty, target_ty) {
            self.mark(target_ty, &mut Vec::new());
        }
    }

    fn handed_on(&mut self, tested: &[Name], arguments: Vec<(Name, Type<'db>)>) {
        for (parameter, argument) in arguments {
            if tested.contains(&parameter) {
                self.mark(argument, &mut Vec::new());
            }
        }
    }

    fn specialization(&mut self, subscript: &ast::ExprSubscript) {
        let (db, model) = (self.db, self.model);
        let Some(generic) = subscript
            .value
            .inferred_type(model)
            .and_then(|value_ty| GenericUse::of(db, value_ty))
        else {
            return;
        };
        let tested = tested_type_params(db, generic.scope);
        if tested.is_empty() {
            return;
        }
        let arguments = written_type_arguments(db, model, subscript, generic.context)
            .into_iter()
            .map(|(typevar, argument, _)| (typevar.name(db).clone(), argument))
            .collect();
        self.handed_on(tested, arguments);
    }

    fn call(&mut self, call: &ast::ExprCall) {
        let (db, env, file, model) = (self.db, self.env, self.file, self.model);
        let Some(callee) = call.func.inferred_type(model) else {
            return;
        };
        if let Some(class) = callee.as_class_literal().and_then(ClassLiteral::as_static) {
            let tested = tested_type_params(db, class.body_scope(db));
            if !tested.is_empty()
                && let Some(constructed) = call.inferred_type(model)
            {
                self.handed_on(
                    tested,
                    constructed_type_arguments(db, env, file, constructed),
                );
            }
        } else if let Some(bare_call) = BareGenericCall::of(db, model, call) {
            let tested = tested_type_params(db, function_body_scope(db, bare_call.function));
            if !tested.is_empty() {
                let reads = std::cell::RefCell::new(Vec::new());
                let arguments = solved_type_arguments(
                    db,
                    env,
                    file,
                    &bare_call,
                    TypeParamCells::Assumed(&reads),
                );
                self.handed_on(tested, arguments);
            }
        }
    }

    /// a class's type argument is handed on by its base list, as much as by its methods
    fn class_bases(&mut self, bases: &[Type<'db>]) {
        let db = self.db;
        for base in bases {
            let Type::GenericAlias(alias) = base else {
                continue;
            };
            let Some(context) = alias.origin(db).generic_context(db) else {
                continue;
            };
            let tested = tested_type_params(db, alias.origin(db).body_scope(db));
            let arguments = context
                .variables(db)
                .zip(alias.specialization(db).types(db))
                .map(|(variable, argument)| (variable.name(db).clone(), *argument))
                .collect();
            self.handed_on(tested, arguments);
        }
    }
}

impl<'ast> Visitor<'ast> for TestedCells<'_, '_> {
    fn visit_annotation(&mut self, _expr: &'ast ast::Expr) {}
    fn visit_type_params(&mut self, _type_params: &'ast ast::TypeParams) {}

    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        if !matches!(stmt, ast::Stmt::TypeAlias(_)) {
            walk_stmt(self, stmt);
        }
    }

    // the target of a test or a cast names a type rather than running as a value, so a class it
    // names is not given the type arguments it is written with
    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        match expr {
            ast::Expr::Compare(compare) => {
                if let Some(first) = compare.operands.first() {
                    self.visit_expr(first);
                }
                for (index, (value, _, target)) in compare.iter().enumerate() {
                    if compare.is_type_test(index, self.file.source_type(self.db)) {
                        self.type_test(value, target);
                    } else {
                        self.visit_expr(target);
                    }
                }
                return;
            }
            ast::Expr::Call(call) => match (call.cast_kind, &*call.arguments.args) {
                (Some(kind), [target, value]) => {
                    if kind != CastKind::Static {
                        self.checked_cast(target, value);
                    }
                    self.visit_expr(value);
                    return;
                }
                _ => {
                    if !matches!(call.func.as_ref(), ast::Expr::Subscript(_)) {
                        self.call(call);
                    }
                }
            },
            ast::Expr::Subscript(subscript) => self.specialization(subscript),
            _ => {}
        }
        walk_expr(self, expr);
    }
}

/// the type parameters of the classes around a classmethod that the method with this body reads
/// through its receiver: itself, by calling another classmethod on it, or by constructing it,
/// which a bare `A()` is refused for
#[salsa::tracked(
    returns(deref),
    cycle_initial=|_, _, _| Box::default(),
    heap_size=ruff_memory_usage::heap_size
)]
fn classmethod_type_argument_reads<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
) -> Box<[BoundTypeVarInstance<'db>]> {
    let source_type = scope.file(db).source_type(db);
    if !source_type.is_basedpython() {
        return Box::default();
    }
    let NodeWithScopeKind::Function(function) = scope.node(db) else {
        return Box::default();
    };
    let module = parsed_module(db, scope.python_file(db)).load(db);
    let function = function.node(&module);
    let file = scope.program_file(db);
    let index = semantic_index(db, file);
    let definition = index.expect_single_definition(function);
    let Some(class_node) = index.scope(definition.file_scope(db)).node().as_class() else {
        return Box::default();
    };
    let class_node = class_node.node(&module);
    let Some(receiver) = method_receiver(function) else {
        return Box::default();
    };
    let class_definition = index.expect_single_definition(class_node);
    let Some(context) =
        original_class_type(db, class_definition).and_then(|class| class.generic_context(db))
    else {
        return Box::default();
    };
    let reads = reified_class_reads(source_type, class_node);
    let own: Vec<&Name> = reads
        .methods
        .iter()
        .find(|method| std::ptr::eq(method.function, function))
        .map(|method| method.names.iter().collect())
        .unwrap_or_default();
    let mut found: Vec<BoundTypeVarInstance<'db>> = context
        .variables(db)
        .filter(|variable| own.contains(&variable.name(db)))
        .collect();
    let model = SemanticModel::new(db, file);
    let mut through = ThroughReceiver {
        db,
        model: &model,
        receiver,
        context,
        found: &mut found,
    };
    through.visit_body(&function.body);
    found.into_boxed_slice()
}

/// walks a classmethod's body for what it reaches through its receiver
struct ThroughReceiver<'a, 'db> {
    db: &'db dyn Db,
    model: &'a SemanticModel<'db>,
    receiver: &'a str,
    context: GenericContext<'db>,
    found: &'a mut Vec<BoundTypeVarInstance<'db>>,
}

impl<'db> ThroughReceiver<'_, 'db> {
    fn add(&mut self, typevars: impl IntoIterator<Item = BoundTypeVarInstance<'db>>) {
        for typevar in typevars {
            if !self.found.contains(&typevar) {
                self.found.push(typevar);
            }
        }
    }

    fn is_receiver(&self, expr: &ast::Expr) -> bool {
        matches!(expr, ast::Expr::Name(name) if name.id.as_str() == self.receiver)
    }
}

impl<'ast> Visitor<'ast> for ThroughReceiver<'_, '_> {
    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        if let ast::Expr::Call(call) = expr {
            let db = self.db;
            match call.func.as_ref() {
                func if self.is_receiver(func) => {
                    self.add(self.context.variables(db).collect::<Vec<_>>());
                }
                ast::Expr::Attribute(attribute) if self.is_receiver(&attribute.value) => {
                    if let Some(Type::BoundMethod(method)) = call.func.inferred_type(self.model)
                        && method.binds_classmethod(db)
                        && let Some(function) = method.function(db)
                    {
                        let reads =
                            classmethod_type_argument_reads(db, function_body_scope(db, function));
                        self.add(reads.iter().copied());
                    }
                }
                _ => {}
            }
        }
        walk_expr(self, expr);
    }
}

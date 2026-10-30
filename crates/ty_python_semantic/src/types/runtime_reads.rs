//! basedpython: whether each module-level name a lowering writes into code that runs holds
//! there the binding the checker resolved
//!
//! a type test, a checked cast and a bare call or construction of a reified generic are
//! lowered to code that names classes — the test's target, the specialization the call is
//! given — and the program has to find there the class the checker resolved. a name bound
//! only under `if TYPE_CHECKING:`, only on some paths, or only further on does not hold it
//! ([`runtime_binding`](crate::place::runtime_binding)). which one it is depends on every
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
use ruff_python_ast::{self as ast, AnyRootNodeRef, NodeIndex};
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::scope::{FileScopeId, ScopeId};
use ty_python_core::{ProgramFile, semantic_index};

use crate::semantic_model::{HasType, SemanticModel};
use crate::types::check_order::{InferredInCheckOrder, infer_scope_in_check_order};
use crate::types::context::InferContext;
use crate::types::diagnostic::{
    ERASED_TYPE_CHECK, TypeCheckDiagnostics, report_erased_type_check,
    report_invalid_reified_override, report_unspecialized_reified_call,
    report_unspecialized_reified_construction,
};
use crate::types::infer::{infer_scope_types, original_class_type};
use crate::types::overrides::{OverrideStep, overridden_declarations};
use crate::types::reified_infer::{
    BareGenericCall, ParametricIsPlan, TypeParamCells, WrittenTarget, constructor_specialization,
    forwarded_override_error, inferred_call_type_arguments, type_test_plan, written_unbound_read,
};
use crate::types::runtime_spelling::RuntimeSite;
use crate::types::soundness::{CastCheck, cast_check_plan, cast_is_redundant};
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

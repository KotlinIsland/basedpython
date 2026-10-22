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
    report_unspecialized_reified_call, report_unspecialized_reified_construction,
};
use crate::types::infer::infer_scope_types;
use crate::types::reified_infer::{
    ParametricIsPlan, ReifiedInferenceError, WrittenTarget, type_test_plan, written_unbound_read,
};
use crate::types::runtime_spelling::{RuntimeSite, first_unbound_read};
use crate::types::soundness::{CastCheck, cast_check_plan, cast_is_redundant};
use crate::types::{ProgramEnvironment, TypeContext};
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
    /// a bare call or construction of a reified generic, with the module-level names the
    /// specialization the lowering injects reads
    Reified {
        call: NodeIndex,
        generic: ReifiedGeneric,
        reads: Box<[Name]>,
    },
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
            RuntimeReadSite::Reified {
                call,
                generic,
                reads,
            } => {
                if reports && let Some(call) = self.expression(*call) {
                    self.reified(call, generic, reads);
                }
            }
        }
    }

    /// a bare reified call or construction whose specialization reads a name that does not
    /// hold, where the call runs, the binding the checker resolved
    fn reified(&mut self, call: &ast::Expr, generic: &ReifiedGeneric, reads: &[Name]) {
        let Some((name, binding)) = first_unbound_read(self.db, self.env, reads, self.site(call))
        else {
            return;
        };
        let failure = ReifiedInferenceError::NotAtRuntime(name, binding);
        let Some(context) = self.context(call) else {
            return;
        };
        match generic {
            ReifiedGeneric::Function(callee) => {
                report_unspecialized_reified_call(context, call.range(), callee, Some(failure));
            }
            ReifiedGeneric::Class(class) => {
                report_unspecialized_reified_construction(context, call.range(), class, failure);
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

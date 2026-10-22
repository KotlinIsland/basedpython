//! basedpython: the module-level names the transpiled program reads through what the
//! checker spells for it
//!
//! a lowering writes some types out as the checker spells them — a soundness check's target,
//! a reified call's or construction's specialization, the arms of a union parameter given a
//! reified type parameter — and a spelling names each class by the binding the module's
//! global scope reaches it through. the source spells none of those reads, so the import
//! binding a class read only there would look unused. these are the same queries the
//! lowerings ask, asked once the file is checked: each needs the inference of the scope the
//! node is in to be finished
//!
//! the transpiler asks them of the source after its pre-passes (an erased union given a type
//! parameter, a context-sensitive name qualified, an enum lowered), and these ask them of the
//! source as written, which is the one the checker has. a site only the rewritten source has
//! is not seen here

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};

use crate::semantic_model::LoweringModel;
use crate::types::runtime_spelling::RuntimeSpelling;
use crate::types::soundness_sites::{SoundnessPositions, soundness_sites};

/// every module-level name a spelling the transpiled program writes for `suite` reads. every
/// soundness position counts, because which ones a build enables is the build's choice and
/// the answer has to hold for any build of the file
pub(crate) fn spelled_module_reads(model: &LoweringModel<'_>, suite: &[Stmt]) -> Vec<Name> {
    let mut spellings = Spellings {
        model,
        reads: Vec::new(),
    };
    for site in soundness_sites(model, suite, SoundnessPositions::all()) {
        for plan in site.plans() {
            spellings.read(&plan.spelling());
        }
    }
    spellings.visit_body(suite);
    spellings.reads
}

struct Spellings<'a, 'db> {
    model: &'a LoweringModel<'db>,
    reads: Vec<Name>,
}

impl Spellings<'_, '_> {
    fn read(&mut self, spelling: &RuntimeSpelling) {
        self.reads.extend(spelling.global_reads().cloned());
    }
}

impl<'ast> Visitor<'ast> for Spellings<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt {
            for parameter in &function.parameters {
                let Some(annotation) = parameter.annotation() else {
                    continue;
                };
                if let Some(union) = self.model.erased_union(annotation) {
                    self.read(&union.origin);
                    for arm in &union.arms {
                        self.read(arm);
                    }
                    for (_, fixed) in &union.fixed {
                        self.read(fixed);
                    }
                }
            }
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.call(call);
        }
        walk_expr(self, expr);
    }
}

impl Spellings<'_, '_> {
    fn call(&mut self, call: &ast::ExprCall) {
        if let Some(specialization) = self.model.reified_call_specialization(call) {
            self.read(&specialization);
        }
        if let Some(arguments) = self.model.reified_constructor_type_arguments(call) {
            self.read(&arguments);
        }
    }
}

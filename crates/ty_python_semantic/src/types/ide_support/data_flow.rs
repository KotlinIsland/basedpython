//! what the branches below a line will do, given what a debugger saw at it
//!
//! [`unreachable_code`](super::unreachable_code) answers which code the *source alone* proves
//! cannot run. this answers the other half of the same question: which of the branches that are
//! reachable on paper will actually be taken, given the state a program was really in
//!
//! both are the same machinery. the difference is entirely in the program the file is read
//! under — a seeded one pins some names to what was observed, and every reachability constraint
//! that depends on them then evaluates to something definite instead of to
//! [`Truthiness::Ambiguous`]. see [`crate::assumed`]
//!
//! ## only what the seed added
//!
//! a verdict the unseeded analysis already reaches is not reported here. the editor is already
//! drawing those — an `if False:` is greyed out whether or not anything is being debugged — and
//! reporting them again would either double-draw them or, worse, attribute an ordinary static
//! finding to the debugger. what this returns is the difference the runtime state made

use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor};
use ruff_python_ast::{self as ast};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_core::{ProgramFile, Truthiness};

use crate::Db;
use crate::semantic_model::{HasType, SemanticModel};
use crate::types::context::ProgramEnvironment;

use super::unreachable_code::{UnreachableRange, unreachable_ranges};

/// what one condition will do when the program reaches it
#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub struct ConditionVerdict {
    /// the condition expression itself, so an editor can draw against it
    pub range: TextRange,
    /// which way it goes. never [`Truthiness::Ambiguous`] — an ambiguous condition is not a
    /// verdict and is left out rather than reported as a shrug
    pub verdict: Truthiness,
}

/// what the runtime state settles that the source alone does not
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFlow {
    /// the conditions below the stop line whose value is now decided
    pub conditions: Box<[ConditionVerdict]>,
    /// the code below the stop line that is now known not to run
    pub unreachable: Box<[UnreachableRange]>,
}

/// what a seeded reading of `file` decides that the unseeded reading of `unseeded` does not
///
/// both arguments are the same physical file. they differ only in their program: one carries the
/// debugger's observations and one does not, which is what makes them separate semantic
/// identities — see [`ty_python_core::assumptions`]
pub fn data_flow<'db>(
    db: &'db dyn Db,
    seeded: ProgramFile<'db>,
    unseeded: ProgramFile<'db>,
    below: TextSize,
) -> DataFlow {
    let without = verdicts(db, unseeded);

    let conditions = verdicts(db, seeded)
        .iter()
        .copied()
        .filter(|decided| decided.range.start() >= below)
        // a condition the source alone already decides is not this feature's to report. the
        // verdict is compared too, not just the range: if the two readings ever disagreed about
        // one condition that would be a bug in the seeding, and swallowing it here would be the
        // one place it could have shown
        .filter(|decided| !without.contains(decided))
        .collect();

    let already_dead = unreachable_ranges(db, unseeded);
    let unreachable = unreachable_ranges(db, seeded)
        .iter()
        .filter(|range| range.range.start() >= below)
        .filter(|range| !already_dead.iter().any(|dead| dead.range == range.range))
        .copied()
        .collect();

    DataFlow {
        conditions,
        unreachable,
    }
}

/// every condition in the file whose value this reading settles
///
/// the whole file rather than the part below the stop line, because the stop line moves on every
/// step and the reading of the *unseeded* file does not. keyed on the file alone, the unseeded
/// half of the comparison above is computed once and then answered from salsa for the rest of the
/// debug session
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn verdicts<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Box<[ConditionVerdict]> {
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let mut collector = Conditions { found: Vec::new() };
    source_order::walk_body(&mut collector, parsed.suite());

    let model = SemanticModel::new(db, file);
    let env = ProgramEnvironment::from_file(file);

    collector
        .found
        .into_iter()
        .filter_map(|condition| {
            let ty = condition.inferred_type(&model)?;
            let verdict = ty.bool(db, &env);
            // an ambiguous condition is what an unseeded reading says about nearly everything.
            // reporting it would be filling the editor with hints that say nothing
            (!verdict.is_ambiguous()).then(|| ConditionVerdict {
                range: condition.range(),
                verdict,
            })
        })
        .collect()
}

/// collects the expressions that decide which way control flows
///
/// not every expression, and not every `bool` — the point is what a reader would draw a `=true`
/// beside. a condition is somewhere the program takes one path or another because of a value
struct Conditions<'ast> {
    found: Vec<&'ast ast::Expr>,
}

impl<'ast> SourceOrderVisitor<'ast> for Conditions<'ast> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        match stmt {
            ast::Stmt::If(node) => {
                self.found.push(&node.test);
                for clause in &node.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.found.push(test);
                    }
                }
            }
            ast::Stmt::While(node) => self.found.push(&node.test),
            ast::Stmt::Assert(node) => self.found.push(&node.test),
            _ => {}
        }
        source_order::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        if let ast::Expr::If(node) = expr {
            self.found.push(&node.test);
        }
        source_order::walk_expr(self, expr);
    }
}

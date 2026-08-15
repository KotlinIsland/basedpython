//! What the branches below a line will do, given what a debugger saw at it.
//!
//! [`unreachable_code`](super::unreachable_code) answers which code the *source alone* proves
//! cannot run. This answers the other half of the same question: which of the branches that are
//! reachable on paper will actually be taken, given the state a program was really in.
//!
//! Both are the same machinery. The difference is entirely in the program the file is read
//! under — a seeded one pins some names to what was observed, and every reachability constraint
//! that depends on them then evaluates to something definite instead of to
//! [`Truthiness::Ambiguous`]. See [`crate::assumed`].
//!
//! ## Only what the seed added
//!
//! A verdict the unseeded analysis already reaches is not reported here. The editor is already
//! drawing those — an `if False:` is greyed out whether or not anything is being debugged — and
//! reporting them again would either double-draw them or, worse, attribute an ordinary static
//! finding to the debugger. What this returns is the difference the runtime state made.

use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor};
use ruff_python_ast::{self as ast, AnyNodeRef};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_core::{ProgramFile, Truthiness};

use crate::Db;
use crate::semantic_model::{HasType, SemanticModel};
use crate::types::context::ProgramEnvironment;

use super::unreachable_code::{UnreachableRange, unreachable_ranges};

/// What one condition will do when the program reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConditionVerdict {
    /// The condition expression itself, so an editor can draw against it.
    pub range: TextRange,
    /// Which way it goes. Never [`Truthiness::Ambiguous`] — an ambiguous condition is not a
    /// verdict and is left out rather than reported as a shrug.
    pub verdict: Truthiness,
}

/// What the runtime state settles that the source alone does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFlow {
    /// The conditions below the stop line whose value is now decided.
    pub conditions: Box<[ConditionVerdict]>,
    /// The code below the stop line that is now known not to run.
    pub unreachable: Box<[UnreachableRange]>,
}

/// What a seeded reading of `file` decides that the unseeded reading of `unseeded` does not.
///
/// Both arguments are the same physical file. They differ only in their program: one carries the
/// debugger's observations and one does not, which is what makes them separate semantic
/// identities — see [`ty_python_core::assumptions`].
pub fn data_flow<'db>(
    db: &'db dyn Db,
    seeded: ProgramFile<'db>,
    unseeded: ProgramFile<'db>,
    below: TextSize,
) -> DataFlow {
    let with_seed = verdicts(db, seeded, below);
    let without = verdicts(db, unseeded, below);

    let conditions = with_seed
        .into_iter()
        .filter(|decided| {
            // A condition the source alone already decides is not this feature's to report
            !without.iter().any(|already| already.range == decided.range)
        })
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

/// Every condition below `below` whose value this reading settles.
fn verdicts<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    below: TextSize,
) -> Vec<ConditionVerdict> {
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let mut collector = Conditions {
        below,
        found: Vec::new(),
    };
    source_order::walk_body(&mut collector, parsed.suite());

    let model = SemanticModel::new(db, file);
    let env = ProgramEnvironment::from_file(file);

    collector
        .found
        .into_iter()
        .filter_map(|condition| {
            let ty = condition.inferred_type(&model)?;
            let verdict = ty.bool(db, &env);
            // An ambiguous condition is what an unseeded reading says about nearly everything.
            // Reporting it would be filling the editor with hints that say nothing
            (!verdict.is_ambiguous()).then(|| ConditionVerdict {
                range: condition.range(),
                verdict,
            })
        })
        .collect()
}

/// Collects the expressions that decide which way control flows.
///
/// Not every expression, and not every `bool` — the point is what a reader would draw a `=true`
/// beside. A condition is somewhere the program takes one path or another because of a value.
struct Conditions<'ast> {
    below: TextSize,
    found: Vec<&'ast ast::Expr>,
}

impl<'ast> Conditions<'ast> {
    fn consider(&mut self, condition: &'ast ast::Expr) {
        if condition.range().start() >= self.below {
            self.found.push(condition);
        }
    }
}

impl<'ast> SourceOrderVisitor<'ast> for Conditions<'ast> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        match stmt {
            ast::Stmt::If(node) => {
                self.consider(&node.test);
                for clause in &node.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.consider(test);
                    }
                }
            }
            ast::Stmt::While(node) => self.consider(&node.test),
            ast::Stmt::Assert(node) => self.consider(&node.test),
            _ => {}
        }
        source_order::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        if let ast::Expr::If(node) = expr {
            self.consider(&node.test);
        }
        source_order::walk_expr(self, expr);
    }

    fn enter_node(&mut self, _node: AnyNodeRef<'ast>) -> source_order::TraversalSignal {
        source_order::TraversalSignal::Traverse
    }
}

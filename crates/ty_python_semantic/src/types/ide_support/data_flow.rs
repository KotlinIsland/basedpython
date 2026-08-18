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

/// what one read of a name will find when the program reaches it
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct ValueVerdict {
    /// the read itself — the `discount` in `return discount`, not the statement around it
    pub range: TextRange,
    /// the value, written the way a source writes it: `0.0`, `3`, `False`, `"hi"`
    pub value: String,
}

/// what the runtime state settles that the source alone does not
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFlow {
    /// the conditions below the stop line whose value is now decided
    pub conditions: Box<[ConditionVerdict]>,
    /// the code below the stop line that is now known not to run
    pub unreachable: Box<[UnreachableRange]>,
    /// the reads below the stop line that will find exactly one value
    pub values: Box<[ValueVerdict]>,
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

    let conditions: Box<[ConditionVerdict]> = verdicts(db, seeded)
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

    let already_known = values(db, unseeded);
    let values = values(db, seeded)
        .iter()
        .filter(|read| read.range.start() >= below)
        // the same comparison the conditions get, and for the same reason: a value the source
        // alone already fixes is not the debugger's doing, and comparing the *value* rather than
        // only the range means a disagreement between the two readings is reported instead of
        // quietly dropped here
        .filter(|read| !already_known.contains(read))
        // a read inside a condition this pass has already decided is that same finding written
        // twice. `qty >= 10` gets a `= false`; adding `qty = 3` beside it is the working rather
        // than the answer, and both labels land in the one margin, so it is also the only place
        // two of this feature's labels would compete for the same space
        .filter(|read| {
            !conditions
                .iter()
                .any(|decided: &ConditionVerdict| decided.range.contains_range(read.range))
        })
        .cloned()
        .collect();

    DataFlow {
        conditions,
        unreachable,
        values,
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

/// every read in the file that this reading pins to one value
///
/// the whole file and cached per program, for the same reason [`verdicts`] is: the unseeded half of
/// the comparison is computed once and answered from salsa for the rest of the debug session
///
/// ## the value is the type, and that is the whole check
///
/// a read is decided when its inferred type stands for exactly one value — `Literal[3]`, `0.0`,
/// `Literal[False]`. that is not a second analysis bolted on beside the reachability one, it is the
/// same one asked a different question, and it is what makes "only what follows from decided
/// branches plus observed seeds" true by construction rather than by a rule written here:
///
/// * a value that depends on anything unobserved is a union or an instance type, not one value, so
///   it answers nothing. there is no "probably" to invent — the type system has no way to spell one
/// * a name rebound below the stop line by a branch that will not run has that binding dropped by
///   the reachability the seed decided, so the one live binding is what is left. this is the case
///   the feature is for: `discount = 0.0` still holds at `return discount` because the two `if`s
///   that would have touched it are dead
/// * a fact that goes stale is not expressible as one value in the first place. a list's length is
///   a property of a `list[int]`, and `list[int]` is not a value — so the rule that a fact only
///   travels to code that has not run when it will still be true there needs no separate guard
///
/// [`Type::display_value`] is the same rendering the enum-value inlay hint uses, so a value has one
/// spelling in the editor however it got there. it answers nothing for a `LiteralString`, a
/// template or an enum member, which are a *set* of values, a shape, and a name rather than a
/// value — leaving those out is that helper's own rule, and giving them a second spelling here
/// would be this feature disagreeing with the rest of the editor about what a value looks like
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn values<'db>(db: &'db dyn Db, file: ProgramFile<'db>) -> Box<[ValueVerdict]> {
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let mut collector = Reads { found: Vec::new() };
    source_order::walk_body(&mut collector, parsed.suite());

    let model = SemanticModel::new(db, file);
    let env = ProgramEnvironment::from_file(file);

    collector
        .found
        .into_iter()
        .filter_map(|read| {
            let value = read.inferred_type(&model)?.display_value(db, &env)?;
            Some(ValueVerdict {
                range: read.range(),
                value: value.to_string(),
            })
        })
        .collect()
}

/// collects the places a value is read out of
///
/// loads only. a store's value is spelled on the line the store is written on, so annotating it
/// would be repeating the source back at the reader — a load is where somebody has to work out
/// what arrived
///
/// attributes as well as bare names, because the observations are a vocabulary of "a name or a
/// dotted path": a `self.limit` a debugger saw can decide a branch, and a feature that then refused
/// to say what `self.limit` itself holds would be inconsistent for no reason. nothing else — a
/// subscript or a call is a place where deciding the value means deciding what the call did, which
/// is precisely what a seeded reading does not claim to know
struct Reads<'ast> {
    found: Vec<&'ast ast::Expr>,
}

impl<'ast> SourceOrderVisitor<'ast> for Reads<'ast> {
    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        match expr {
            ast::Expr::Name(node) if node.ctx.is_load() => self.found.push(expr),
            ast::Expr::Attribute(node) if node.ctx.is_load() => self.found.push(expr),
            _ => {}
        }
        source_order::walk_expr(self, expr);
    }
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

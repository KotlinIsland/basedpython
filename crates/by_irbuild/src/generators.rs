//! generators, as a state machine on a generated state class
//!
//! a generator has to survive its own frame: the locals it was using are still live
//! when it suspends, and the next `next()` picks up where it left off. that is the
//! same requirement a closure over a mutated name has, and it has the same answer —
//! the values live in fields of an object rather than in registers.
//!
//! so the whole shape reuses what closures built:
//!
//! - the state class is a [`by_ir::function::ClassIr`], one **cell** per local, plus
//!   a `$state` field holding which resumption point to enter at
//! - the body becomes one method, `$resume`, whose first block dispatches on `$state`
//! - calling the generator function does not run the body at all: it allocates the
//!   state object with the parameters seeded and hands it back
//!
//! ## no new ops
//!
//! a `yield` is a field write and a return. exhaustion is a field write and a raise.
//! the dispatch is a chain of branches — which is a jump table, and the C compiler
//! builds the table. the state machine needed nothing the IR did not already have,
//! which is the strongest evidence the closure design was the right shape.
//!
//! ## resuming *by raising*
//!
//! `throw` and `close` have to raise **at the suspension point**, not at the entry —
//! otherwise a `yield` inside `try` would skip its own handler. so a `$thrown` field
//! carries the exception, and every resumption point checks it before continuing.
//! raising there enters the enclosing handler exactly as an exception in the body
//! would, which is what makes `close()` run a `finally`.
//!
//! ## a temporary is parked too
//!
//! a local has a field because it has a name. a *temporary* has neither, and python
//! puts one across a suspension in the most ordinary code there is: `total + await
//! step(i)` reads `total` before it awaits. so a backward liveness over the flow a
//! suspended frame actually has says which registers reach a resumption point, and
//! each of those gets a field written at the suspension and read back at the
//! resumption — see [`park_live_registers`]
//!
//! ## what is declined, and why it would be wrong rather than slow
//!
//! - a generator that is also a closure — one object cannot be two environments

use std::collections::{BTreeSet, HashSet};

use by_ir::ops::{BlockId, Op, RegisterId, Terminator, Value};
use ruff_python_ast::{self as ast, Expr, Stmt};

use crate::closures::{statement_expressions, written_names};
use crate::mapper::{Decline, Lowered};

/// the field holding which resumption point to enter at
pub(crate) const STATE_FIELD: &str = "$state";
/// the field holding the value passed to `send`, which is what `yield` evaluates to
pub(crate) const SENT_FIELD: &str = "$sent";
/// the field holding an exception `throw` or `close` wants raised *at* the suspension
///
/// this is what makes `yield` inside `try` work: the resumption point checks it and
/// raises, which enters the enclosing handler exactly as an exception there would
pub(crate) const THROWN_FIELD: &str = "$thrown";

/// which kind of suspension the resume method last made
///
/// only an *async generator* has two: a `yield` produces an item for the awaitable
/// `__anext__` handed back, and an `await` suspends and has to reach the event loop
/// instead. one `resume` returns for both, so the driver reads this to tell them
/// apart. `1` is a yield
pub(crate) const KIND_FIELD: &str = "$kind";
/// the method the iterator protocol drives
pub(crate) const RESUME_METHOD: &str = "$resume";

/// the state class's name, derived from the generator it belongs to
pub(crate) fn state_name(owner: &str) -> String {
    format!("{owner}$gen")
}

/// the direct edition's name, derived from the coroutine it is the body of
///
/// `$` is not an identifier character in python, so this can never collide with a
/// name someone wrote
pub(crate) fn direct_name(owner: &str) -> String {
    format!("{owner}$direct")
}

/// whether an `async def` reaches its end without ever suspending
///
/// a coroutine suspends where the body says `await`, and at the awaits an `async for`,
/// an `async with` or an `async` comprehension clause make on its behalf. a body with
/// none of those runs straight through on its first `send`: the state machine it lowers
/// to has one entry and one exit, and only ever writes `$state = -1`.
///
/// such a coroutine is still a coroutine — `f(...)` builds the object and hands it back
/// like any other. what the property licenses is at the *await site*: `await f(...)`
/// can call the body, because the object it would have built is one nothing else can
/// reach and one `send` is the whole of what the await does to it.
///
/// a `yield` makes the function an async *generator*, which is a different surface
/// with a different answer, so it is excluded here rather than counted as a suspension
///
/// deliberately over-eager: it looks inside nested `def`s and lambdas too, where an
/// `await` belongs to the nested frame rather than this one. saying "suspends" of a
/// coroutine that does not costs it this optimisation, and nothing else
pub(crate) fn never_suspends(function: &ast::StmtFunctionDef) -> bool {
    if !function.is_async {
        return false;
    }
    let mut search = Suspensions { found: false };
    ast::visitor::walk_body(&mut search, &function.body);
    !search.found
}

/// whether any suspension point is written under here
struct Suspensions {
    found: bool,
}

impl<'a> ast::visitor::Visitor<'a> for Suspensions {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        self.found |= match stmt {
            Stmt::For(node) => node.is_async,
            Stmt::With(node) => node.is_async,
            _ => false,
        };
        ast::visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        self.found |= matches!(expr, Expr::Await(_) | Expr::Yield(_) | Expr::YieldFrom(_));
        ast::visitor::walk_expr(self, expr);
    }

    fn visit_comprehension(&mut self, comprehension: &'a ast::Comprehension) {
        self.found |= comprehension.is_async;
        ast::visitor::walk_comprehension(self, comprehension);
    }
}

/// whether a function body yields
pub(crate) fn is_generator(body: &[Stmt]) -> bool {
    crate::walk(body).into_iter().any(|stmt| {
        crate::closures::statement_expressions(stmt)
            .into_iter()
            .any(yields)
    })
}

/// whether an expression contains a `yield` of its own
fn yields(expr: &Expr) -> bool {
    let mut found = false;
    crate::closures::visit_expressions(expr, &mut |child| {
        if matches!(child, Expr::Yield(_) | Expr::YieldFrom(_)) {
            found = true;
        }
    });
    found
}

/// reject the shapes whose state machine this does not build
///
/// each of these would be *wrong* rather than slow if it compiled, which is why it
/// is a decline and not a missing optimization
pub(crate) fn check(function: &ast::StmtFunctionDef) -> Lowered<()> {
    // an `async def` that also yields is an *async generator*, whose surface is
    for stmt in crate::walk(&function.body) {
        if let Stmt::FunctionDef(_) = stmt {
            return Err(Decline::new(
                "a nested function inside a generator is not lowered yet",
            ));
        }
    }
    Ok(())
}

/// the names a generator's state object holds: every parameter and every local
///
/// conservatively all of them rather than only those live across a yield. a liveness
/// analysis would shrink the object; it would not change what compiles, and getting
/// it wrong would lose a value across a suspension
pub(crate) fn state_names(function: &ast::StmtFunctionDef, locals: &[String]) -> Vec<String> {
    let mut out = vec![
        STATE_FIELD.to_string(),
        SENT_FIELD.to_string(),
        THROWN_FIELD.to_string(),
        KIND_FIELD.to_string(),
    ];
    // one per `for`, because the *iterator* is the value most likely to be live
    // across a suspension and it has no source name to hold it
    for index in 0..for_loops(&function.body) {
        out.push(iterator_field(index));
    }
    let mut seen: HashSet<String> = out.iter().cloned().collect();
    for name in function
        .parameters
        .iter_non_variadic_params()
        .map(|parameter| parameter.parameter.name.to_string())
        .chain(locals.iter().cloned())
    {
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

/// what leaving a block early costs in fields
///
/// a `return`, `break` or `continue` runs the cleanups between it and its target
/// before it goes — so inside an `async with` it awaits an `__aexit__` the block's
/// own two exits never see, and a returned value has to survive that suspension.
/// `depth` is how many async context managers enclose this point
fn early_exits(body: &[Stmt], depth: usize) -> usize {
    let mut total = 0;
    for stmt in body {
        total += match stmt {
            Stmt::Return(node) => depth + usize::from(node.value.is_some()),
            Stmt::Break(_) | Stmt::Continue(_) => depth,
            _ => 0,
        };
        total += match stmt {
            Stmt::If(node) => {
                early_exits(&node.body, depth)
                    + node
                        .elif_else_clauses
                        .iter()
                        .map(|clause| early_exits(&clause.body, depth))
                        .sum::<usize>()
            }
            Stmt::While(node) => early_exits(&node.body, depth) + early_exits(&node.orelse, depth),
            Stmt::For(node) => early_exits(&node.body, depth) + early_exits(&node.orelse, depth),
            Stmt::Try(node) => {
                early_exits(&node.body, depth)
                    + node
                        .handlers
                        .iter()
                        .map(|handler| {
                            let ast::ExceptHandler::ExceptHandler(handler) = handler;
                            early_exits(&handler.body, depth)
                        })
                        .sum::<usize>()
                    + early_exits(&node.orelse, depth)
                    + early_exits(&node.finalbody, depth)
            }
            Stmt::With(node) => early_exits(
                &node.body,
                depth + if node.is_async { node.items.len() } else { 0 },
            ),
            _ => 0,
        };
    }
    total
}

/// the field holding a `for` loop's iterator, numbered in source order
pub(crate) fn iterator_field(index: usize) -> String {
    format!("$iter{index}")
}

/// how many values a body needs a field for: one per `for`, one per context
/// manager, and one per delegation — `yield from` and `await` each drive an inner iterator that has to
/// survive every suspension they make
///
/// an `async for` needs *two*: the asynchronous iterator itself, and the
/// delegation that awaits each step, which is synthesized rather than written and
/// so is not among the `await` expressions counted below
fn for_loops(body: &[Stmt]) -> usize {
    let loops: usize = crate::walk(body)
        .into_iter()
        .map(|stmt| match stmt {
            Stmt::For(node) => 1 + usize::from(node.is_async),
            // each context manager is parked too: the body suspends and `__exit__`
            // still has to run, whether the resumption returns or raises
            // an `async with` item also awaits `__aenter__` and `__aexit__`, and
            // the exit is awaited on both the normal and the raising path
            Stmt::With(node) => node.items.len() * if node.is_async { 5 } else { 1 },
            _ => 0,
        })
        .sum();
    let exits = early_exits(body, 0);
    let delegations: usize = crate::walk(body)
        .into_iter()
        .flat_map(crate::closures::statement_expressions)
        .map(|expr| {
            let mut count = 0;
            crate::closures::visit_expressions(expr, &mut |child| {
                if matches!(child, Expr::YieldFrom(_) | Expr::Await(_)) {
                    count += 1;
                }
                // an `async for` clause parks three: the two an `async for`
                // statement does, and the comprehension's own accumulator
                let clauses = match child {
                    Expr::ListComp(node) => node.generators.as_slice(),
                    Expr::SetComp(node) => node.generators.as_slice(),
                    Expr::DictComp(node) => node.generators.as_slice(),
                    Expr::Generator(node) => node.generators.as_slice(),
                    _ => &[],
                };
                count += 3 * clauses.iter().filter(|clause| clause.is_async).count();
            });
            count
        })
        .sum();
    loops + exits + delegations
}

/// where one suspension leaves the frame and where it comes back
pub(crate) struct Resumption {
    /// the value `$state` holds while the frame is suspended here
    pub(crate) state: i64,
    /// the block whose `return` *is* the suspension
    pub(crate) suspend: BlockId,
    /// the block `$resume` re-enters at
    pub(crate) resume: BlockId,
}

/// the field a register is parked in while the frame is suspended
///
/// keyed by register rather than by suspension, so a value crossing several of them
/// crosses one field — and two registers can never want the same one
fn park_field(id: RegisterId) -> String {
    format!("$park{}", id.0)
}

/// no register may hold a value across a suspension, so move the ones that would
///
/// this is the invariant the whole design rests on and the one that is easy to lose:
/// a `yield` *returns*, so every register is gone when `$resume` is entered again.
/// anything that has to survive lives in a field.
///
/// a named local already has one. what does not is a *temporary*: python evaluates
/// `total + await step(i)` left to right, so the read of `total` is on the stack
/// while the `await` suspends, and it has no name to build a field from. so each one
/// gets a field of its own, written just before the `return` that suspends and read
/// back at the top of the resumption block.
///
/// the field takes the **register's own representation**. a state field is otherwise a
/// cell, forced to `object` because unset has to be distinguishable from every value —
/// but a park slot is written on the only path that reaches its read, so there is no
/// unset case and an unboxed value survives the suspension unboxed
pub(crate) fn park_live_registers(
    function: &mut by_ir::function::Function,
    class: &str,
    resumptions: &[Resumption],
) -> Lowered<Vec<by_ir::function::FieldDecl>> {
    let crossing = live_in(function, resumptions);

    let mut fields: Vec<by_ir::function::FieldDecl> = Vec::new();
    let mut declared: BTreeSet<RegisterId> = BTreeSet::new();
    for point in resumptions {
        let mut live: Vec<RegisterId> = crossing
            .get(point.resume.index())
            .into_iter()
            .flatten()
            .copied()
            .filter(|id| *id != RegisterId(0))
            .collect();
        // the field order is part of the emitted layout, so it may not depend on a
        // hash set's iteration order
        live.sort_unstable();
        if live.is_empty() {
            continue;
        }

        for id in &live {
            let Some(decl) = function.register(*id) else {
                return Err(Decline::new(format!(
                    "r{} would be parked across a suspension and is not declared",
                    id.0
                )));
            };
            if declared.insert(*id) {
                fields.push(by_ir::function::FieldDecl {
                    name: park_field(*id),
                    ty: decl.ty.clone(),
                    default: None,
                    optional: false,
                    defaulted_by: None,
                });
            }
        }

        if let Some(block) = function.blocks.get_mut(point.suspend.index()) {
            block.ops.extend(live.iter().map(|id| Op::SetField {
                receiver: Value::Register(RegisterId(0)),
                class: class.to_string(),
                field: park_field(*id),
                value: Value::Register(*id),
            }));
        }
        let moved = BlockId(function.blocks.len());
        if let Some(block) = function.blocks.get_mut(point.resume.index()) {
            // the reloads take the resumption block, and what was in it moves to a
            // block of its own. a handler is entered from *before* any of its block's
            // writes — the first operation can be the one that failed — so a reload
            // sharing a block with an error edge would read as absent in the handler.
            // a field read cannot fail, so the block holding only reloads has no error
            // edge to be entered from
            let rest = by_ir::function::BasicBlock {
                ops: std::mem::replace(
                    &mut block.ops,
                    live.iter()
                        .map(|id| Op::GetField {
                            dest: *id,
                            receiver: Value::Register(RegisterId(0)),
                            class: class.to_string(),
                            field: park_field(*id),
                        })
                        .collect(),
                ),
                terminator: std::mem::replace(&mut block.terminator, Terminator::Goto(moved)),
                owned_at_exit: None,
                range: block.range,
                error_target: block.error_target.take(),
            };
            function.blocks.push(rest);
        }
    }

    // a local some path reads without having written carries a byte saying which,
    // and that byte is a register too — it would be read back unset after the
    // suspension, so the value would arrive with `UnboundLocalError` attached
    by_ir::unbound_locals::mark(function);
    for id in &declared {
        if function
            .register(*id)
            .is_some_and(|decl| decl.may_be_unassigned)
        {
            return Err(Decline::new(format!(
                "`{}` has to survive a suspension and may be unbound where it is read — \
                 a path that skips its assignment, or a `del` — and whether it is bound \
                 does not survive with it",
                register_name(function, *id)
            )));
        }
    }

    // the post-condition, and the check this used to be: over the *static* flow,
    // where a resumption block is entered from the dispatch and nothing else, no
    // register may still be live there. a bug in the analysis above costs this
    // generator its speed rather than a value read out of a dead frame
    let remaining = live_in(function, &[]);
    for point in resumptions {
        let live = remaining
            .get(point.resume.index())
            .into_iter()
            .flatten()
            .copied()
            .filter(|id| *id != RegisterId(0))
            .min();
        if let Some(id) = live {
            return Err(Decline::new(format!(
                "`{}` would have to survive the suspension at yield {}, and a register \
                 does not — it needs a field",
                register_name(function, id),
                point.state
            )));
        }
    }

    Ok(fields)
}

/// what to call a register in a message: its source name, or its number
fn register_name(function: &by_ir::function::Function, id: RegisterId) -> String {
    function
        .register(id)
        .and_then(|decl| decl.name.clone())
        .unwrap_or_else(|| format!("r{}", id.0))
}

/// the registers live on entry to each block, over the flow a *suspended* frame has
///
/// a suspension is a `return`, so the block that makes one has no successor at all
/// and the resumption block is only reachable from the dispatch — which is the
/// static shape, and the wrong one to ask about liveness. dynamically the frame
/// carries on from the suspension into its own resumption point, and that is the
/// edge this walks. without it a value read only after a *later* suspension looks
/// dead at the earlier one and would be dropped between the two
fn live_in(
    function: &by_ir::function::Function,
    resumptions: &[Resumption],
) -> Vec<HashSet<RegisterId>> {
    let count = function.blocks.len();
    let resume_blocks: HashSet<BlockId> = resumptions.iter().map(|point| point.resume).collect();
    let mut successors: Vec<Vec<BlockId>> = (0..count)
        .map(|index| {
            function
                .block(BlockId(index))
                .map(|block| {
                    block
                        .successors()
                        .into_iter()
                        .filter(|target| !resume_blocks.contains(target))
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect();
    for point in resumptions {
        if let Some(edges) = successors.get_mut(point.suspend.index()) {
            edges.push(point.resume);
        }
    }

    let reads = |value: &Value| match value {
        Value::Register(id) => Some(*id),
        _ => None,
    };

    let mut live_in: Vec<HashSet<RegisterId>> = vec![HashSet::new(); count];
    let mut changed = true;
    while changed {
        changed = false;
        for index in (0..count).rev() {
            let Some(block) = function.block(BlockId(index)) else {
                continue;
            };
            // live-out is the union of the successors' live-in
            let mut live: HashSet<RegisterId> = successors[index]
                .iter()
                .filter_map(|target| live_in.get(target.index()))
                .flatten()
                .copied()
                .collect();
            live.extend(block.terminator.operands().iter().filter_map(|v| reads(v)));
            // walk the block backwards: a write kills, a read revives
            for op in block.ops.iter().rev() {
                if let Some(dest) = op.dest() {
                    live.remove(&dest);
                }
                // `del x` reads its destination before leaving it unbound, so a name
                // deleted after a suspension is live across it — which is what puts it
                // in the parked set, where the check below refuses it because the byte
                // saying whether it was bound does not survive with it
                live.extend(op.unbinds());
                live.extend(op.operands().iter().filter_map(|v| reads(v)));
            }
            if live != live_in[index] {
                live_in[index] = live;
                changed = true;
            }
        }
    }
    live_in
}

/// the names a generator's state object holds *unboxed*
///
/// a state field is a cell by default: it starts unset, so NULL has to be
/// distinguishable from every value it could hold, which forces `object`. that costs
/// real speed — `i = i + 1` inside a generator goes through the object protocol.
///
/// a name that is *definitely assigned* before every read needs no unset check, and can
/// take the local's own representation. the rule is deliberately syntactic and simple:
/// **the first of the body's top-level statements that mentions the name must be an
/// assignment to it**. top-level statements run in order, so that assignment dominates
/// everything after it — and by "first mention", nothing before it reads the name.
///
/// what does *not* qualify, and why each one would be wrong:
///
/// - an augmented assignment reads before it writes
/// - a `for` target, or an assignment nested in an `if` or a loop — the body may not run
/// - a name whose first mention is a read, which is `UnboundLocalError` territory
pub(crate) fn definitely_assigned(function: &ast::StmtFunctionDef) -> HashSet<String> {
    // a parameter is assigned on entry: the constructor seeds its field
    let mut out: HashSet<String> = function
        .parameters
        .iter_non_variadic_params()
        .map(|parameter| parameter.parameter.name.to_string())
        .collect();

    let mut mentioned: HashSet<String> = HashSet::new();
    for stmt in &function.body {
        // the assignment has to come before any mention, so read the *names it
        // mentions* first and record the target after
        let assigned = simple_target(stmt);
        for name in mentions(stmt) {
            if Some(name) == assigned.as_deref() {
                continue;
            }
            mentioned.insert(name.to_string());
        }
        if let Some(target) = assigned
            && !mentioned.contains(&target)
        {
            out.insert(target.clone());
            mentioned.insert(target);
        }
    }
    out
}

/// the single plain name a statement assigns, when that is all it does
fn simple_target(stmt: &Stmt) -> Option<String> {
    match stmt {
        Stmt::Assign(node) => match node.targets.as_slice() {
            [Expr::Name(name)] => Some(name.id.to_string()),
            _ => None,
        },
        Stmt::AnnAssign(node) if node.value.is_some() => match node.target.as_ref() {
            Expr::Name(name) => Some(name.id.to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// every name a statement mentions, including inside nested statements
fn mentions(stmt: &Stmt) -> Vec<&str> {
    let mut out = Vec::new();
    for nested in crate::walk(std::slice::from_ref(stmt)) {
        out.extend(written_names(std::slice::from_ref(nested)));
        for expr in statement_expressions(nested) {
            crate::closures::visit_expressions(expr, &mut |child| {
                if let Expr::Name(name) = child {
                    out.push(name.id.as_str());
                }
            });
        }
    }
    out
}

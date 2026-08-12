//! duplicating a counting loop whose bound is invariant
//!
//! an unboxed counter compared against a bound that is still tagged tests the bound's
//! shortness on every trip. the test cannot be hoisted on its own — the bound is an
//! ordinary python `int` and may be arbitrarily large, so the answer has to be known
//! before the comparison — but when nothing in the loop writes the bound, the *answer*
//! is the same on every trip even though the test is not.
//!
//! so the loop is duplicated: one copy whose guard is a machine comparison against a
//! bound narrowed once on the way in, one copy exactly as it stands today, and a
//! narrowing terminator ahead of both that picks between them. on a scalar float loop
//! that is 6% of the whole running time, and it is what closes the last of the gap to
//! mypyc on `mandel` — the shortness branch, not its computation, was the cost.
//!
//! see `Terminator::NarrowShort` for why the test and the narrowing are one thing.

use std::collections::BTreeMap;

use by_ir::function::{BasicBlock, Function, ModuleIr, RegisterDecl};
use by_ir::ops::{BlockId, Op, RegisterId, Terminator, Value};
use by_ir::rtype::{IntWidth, Primitive, RType};

/// how many loops in one function may be duplicated
///
/// every unswitch copies a loop body, so an unbounded pass would grow a deeply nested
/// function geometrically. four covers a triply-nested loop and its own copies, which
/// is past anything the benchmarks contain
const MAX_PER_FUNCTION: usize = 4;

/// the largest loop worth duplicating, in blocks
///
/// the win is a couple of instructions per trip, so it pays on a tight loop and not on
/// a long one — where the duplicate would cost more in instruction cache than the guard
/// ever cost in branches
const MAX_BODY: usize = 32;

pub(crate) fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        for _ in 0..MAX_PER_FUNCTION {
            if !unswitch_one(function) {
                break;
            }
        }
    }
}

/// duplicate the first loop whose bound is invariant, reporting whether one was found
fn unswitch_one(function: &mut Function) -> bool {
    let Some(candidate) = candidate(function) else {
        return false;
    };
    let Candidate {
        header,
        bound,
        body,
    } = candidate;

    let narrowed = RegisterId(function.registers.len());
    function.registers.push(RegisterDecl {
        name: None,
        ty: RType::fixed(IntWidth::I64),
        borrowed: false,
        may_be_unassigned: false,
    });

    // the copies land at the end, so every existing block keeps its id and only the
    // edges into the header have to move
    let copies: BTreeMap<BlockId, BlockId> = body
        .iter()
        .enumerate()
        .map(|(offset, id)| (*id, BlockId(function.blocks.len() + offset)))
        .collect();
    for id in &body {
        let mut block = function.blocks[id.index()].clone();
        retarget(&mut block, &copies);
        if *id == header {
            narrow_guard(&mut block, bound, narrowed);
        }
        function.blocks.push(block);
    }

    // the preheader replaces the header at every edge that entered the loop from
    // outside it, so both copies are entered only through the narrowing
    let preheader = BlockId(function.blocks.len());
    function.blocks.push(BasicBlock {
        ops: Vec::new(),
        terminator: Terminator::NarrowShort {
            dest: narrowed,
            src: Value::Register(bound),
            fits: copies[&header],
            otherwise: header,
        },
        owned_at_exit: None,
        range: function.blocks[header.index()].range,
        error_target: None,
    });
    // the preheader is excluded along with the loop: its own `otherwise` edge *is* an
    // edge to the header, and redirecting that one would send it to itself
    for id in 0..function.blocks.len() {
        let block = BlockId(id);
        if block == preheader || body.contains(&block) || copies.values().any(|copy| *copy == block)
        {
            continue;
        }
        redirect(&mut function.blocks[id], header, preheader);
    }
    true
}

/// a loop that can be duplicated
struct Candidate {
    /// the block holding the guard, which is the loop's header
    header: BlockId,
    /// the tagged bound the guard compares against
    bound: RegisterId,
    /// every block on a path from the header back to itself, the header included
    body: Vec<BlockId>,
}

fn candidate(function: &Function) -> Option<Candidate> {
    for (index, block) in function.blocks.iter().enumerate() {
        let header = BlockId(index);
        let Some(bound) = guard_bound(function, block) else {
            continue;
        };
        let body = cycle_through(function, header);
        if body.len() < 2 || body.len() > MAX_BODY {
            continue;
        }
        // the bound is only invariant if nothing round the loop writes it. a narrowing
        // ahead of the loop would otherwise describe a value the body has replaced
        if body
            .iter()
            .any(|id| writes(&function.blocks[id.index()], bound))
        {
            continue;
        }
        // an already-duplicated loop is entered through its own narrowing, and doing
        // it again would narrow a bound that is no longer read here
        if function
            .blocks
            .iter()
            .any(|block| enters_by_narrowing(block, header))
        {
            continue;
        }
        // the preheader takes the loop's entry edges, so without one it would be
        // unreachable and the copy dead. a header that is the function's own entry has
        // no such edge
        if header == Function::entry()
            || !function.blocks.iter().enumerate().any(|(id, block)| {
                !body.contains(&BlockId(id)) && block.successors().contains(&header)
            })
        {
            continue;
        }
        return Some(Candidate {
            header,
            bound,
            body,
        });
    }
    None
}

/// the tagged register this block's guard compares an unboxed counter against
///
/// only a guard reading a *register* qualifies: an immediate bound is already folded,
/// and there would be nothing to narrow once
fn guard_bound(function: &Function, block: &BasicBlock) -> Option<RegisterId> {
    block.ops.iter().find_map(|op| {
        let Op::IntCompare { lhs, rhs, .. } = op else {
            return None;
        };
        let Value::Register(bound) = rhs else {
            return None;
        };
        let fixed = matches!(
            operand_type(function, lhs),
            Some(RType::Primitive(Primitive::Fixed(_)))
        );
        let tagged = function.register(*bound).map(|decl| &decl.ty) == Some(&RType::INT);
        (fixed && tagged).then_some(*bound)
    })
}

fn operand_type(function: &Function, value: &Value) -> Option<RType> {
    match value {
        Value::Register(id) => function.register(*id).map(|decl| decl.ty.clone()),
        other => other.immediate_type(),
    }
}

/// every block that is both reachable from `header` and can reach it again
fn cycle_through(function: &Function, header: BlockId) -> Vec<BlockId> {
    let forward = reachable(function, header, false);
    let backward = reachable(function, header, true);
    (0..function.blocks.len())
        .map(BlockId)
        .filter(|id| forward.contains(id) && backward.contains(id))
        .collect()
}

/// the blocks reachable from `start`, following edges backwards when `reverse`
fn reachable(function: &Function, start: BlockId, reverse: bool) -> Vec<BlockId> {
    let edges = |id: BlockId| -> Vec<BlockId> {
        if !reverse {
            return function
                .block(id)
                .map(BasicBlock::successors)
                .unwrap_or_default();
        }
        (0..function.blocks.len())
            .map(BlockId)
            .filter(|from| {
                function
                    .block(*from)
                    .is_some_and(|block| block.successors().contains(&id))
            })
            .collect()
    };
    let mut seen = vec![start];
    let mut queue = vec![start];
    while let Some(id) = queue.pop() {
        for next in edges(id) {
            if !seen.contains(&next) {
                seen.push(next);
                queue.push(next);
            }
        }
    }
    seen
}

fn writes(block: &BasicBlock, register: RegisterId) -> bool {
    block.ops.iter().any(|op| op.dest() == Some(register))
        || block.terminator.dest() == Some(register)
}

fn enters_by_narrowing(block: &BasicBlock, header: BlockId) -> bool {
    matches!(
        block.terminator,
        Terminator::NarrowShort { otherwise, .. } if otherwise == header
    )
}

/// send this block's edges into the loop to the copies instead
fn retarget(block: &mut BasicBlock, copies: &BTreeMap<BlockId, BlockId>) {
    let map = |id: &mut BlockId| {
        if let Some(copy) = copies.get(id) {
            *id = *copy;
        }
    };
    match &mut block.terminator {
        Terminator::Goto(target) => map(target),
        Terminator::Branch {
            then_block,
            else_block,
            ..
        } => {
            map(then_block);
            map(else_block);
        }
        Terminator::NarrowShort {
            fits, otherwise, ..
        } => {
            map(fits);
            map(otherwise);
        }
        Terminator::Return(_) | Terminator::Unreachable => {}
    }
    if let Some(target) = &mut block.error_target {
        map(target);
    }
}

/// point every edge to `from` at `to`, leaving the error edge alone
///
/// an error edge reaches a handler, never a loop header, and redirecting one through a
/// narrowing would run the narrowing with an exception set
fn redirect(block: &mut BasicBlock, from: BlockId, to: BlockId) {
    let map = |id: &mut BlockId| {
        if *id == from {
            *id = to;
        }
    };
    match &mut block.terminator {
        Terminator::Goto(target) => map(target),
        Terminator::Branch {
            then_block,
            else_block,
            ..
        } => {
            map(then_block);
            map(else_block);
        }
        Terminator::NarrowShort {
            fits, otherwise, ..
        } => {
            map(fits);
            map(otherwise);
        }
        Terminator::Return(_) | Terminator::Unreachable => {}
    }
}

/// read the guard's bound out of the narrowed register instead of the tagged one
fn narrow_guard(block: &mut BasicBlock, bound: RegisterId, narrowed: RegisterId) {
    for op in &mut block.ops {
        if let Op::IntCompare { rhs, .. } = op
            && *rhs == Value::Register(bound)
        {
            *rhs = Value::Register(narrowed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::CmpOp;
    use by_ir::verify::verify;

    /// `i = 0; while i < bound: i = i + 1`, with `bound` a parameter
    ///
    /// `step_writes_bound` reassigns the bound in the body instead, which is the one
    /// thing that makes the loop ineligible
    fn counting_loop(step_writes_bound: bool) -> Function {
        let mut builder = FunctionBuilder::new("count", RType::INT);
        let bound = builder.param("bound", RType::INT);
        let index = builder.local("i", RType::INT);
        let more = builder.temp(RType::BIT);
        let next = builder.temp(RType::INT);
        builder.assign(index, Value::Int(0));

        let header = builder.new_block();
        let body = builder.new_block();
        let exit = builder.new_block();
        builder.terminate(Terminator::Goto(header));

        builder.switch_to(header);
        builder.push(Op::IntCompare {
            dest: more,
            op: CmpOp::Lt,
            lhs: Value::Register(index),
            rhs: Value::Register(bound),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(more),
            then_block: body,
            else_block: exit,
        });

        builder.switch_to(body);
        builder.push(Op::IntBinary {
            dest: next,
            op: by_ir::ops::BinOp::Add,
            lhs: Value::Register(index),
            rhs: Value::Int(1),
        });
        builder.assign(index, Value::Register(next));
        if step_writes_bound {
            builder.assign(bound, Value::Register(next));
        }
        builder.terminate(Terminator::Goto(header));

        builder.switch_to(exit);
        builder.terminate(Terminator::Return(Value::Register(index)));
        builder.finish()
    }

    fn module_with(function: Function) -> ModuleIr {
        ModuleIr {
            name: "app".to_string(),
            functions: vec![function],
            declined: Vec::new(),
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
        }
    }

    fn narrowings(function: &Function) -> usize {
        function
            .blocks
            .iter()
            .filter(|block| matches!(block.terminator, Terminator::NarrowShort { .. }))
            .count()
    }

    #[test]
    fn a_loop_whose_bound_is_a_parameter_is_duplicated() {
        let mut module = module_with(counting_loop(false));
        crate::unbox_counters::run(&mut module);
        let before = module.functions[0].blocks.len();
        run(&mut module);
        let function = &module.functions[0];

        assert_eq!(narrowings(function), 1);
        // the body and its header, plus the preheader
        assert_eq!(function.blocks.len(), before + 3);
        assert!(verify(function).is_ok(), "{:?}", verify(function));
    }

    #[test]
    fn the_duplicate_compares_two_machine_integers() {
        let mut module = module_with(counting_loop(false));
        crate::unbox_counters::run(&mut module);
        run(&mut module);
        let function = &module.functions[0];

        let Some(Terminator::NarrowShort { dest, fits, .. }) = function
            .blocks
            .iter()
            .map(|block| &block.terminator)
            .find(|terminator| matches!(terminator, Terminator::NarrowShort { .. }))
            .cloned()
        else {
            panic!("the loop was not duplicated");
        };
        // the whole point: on the edge the narrowing takes, the guard reads the machine
        // register rather than the tagged one, which is what makes it a plain compare
        let guard = function.blocks[fits.index()]
            .ops
            .iter()
            .find_map(|op| match op {
                Op::IntCompare { rhs, .. } => Some(rhs.clone()),
                _ => None,
            })
            .expect("the copy keeps its guard");
        assert_eq!(guard, Value::Register(dest));
    }

    #[test]
    fn a_bound_the_body_writes_is_left_alone() {
        let mut module = module_with(counting_loop(true));
        crate::unbox_counters::run(&mut module);
        let before = module.functions[0].blocks.len();
        run(&mut module);

        assert_eq!(narrowings(&module.functions[0]), 0);
        assert_eq!(module.functions[0].blocks.len(), before);
    }

    #[test]
    fn duplicating_twice_is_not_attempted() {
        // the pass runs to a fixpoint inside one call; running it again must find
        // nothing, or a loop would be copied on every pipeline run
        let mut module = module_with(counting_loop(false));
        crate::unbox_counters::run(&mut module);
        run(&mut module);
        let after_once = module.functions[0].blocks.len();
        run(&mut module);

        assert_eq!(module.functions[0].blocks.len(), after_once);
        assert_eq!(narrowings(&module.functions[0]), 1);
    }
}

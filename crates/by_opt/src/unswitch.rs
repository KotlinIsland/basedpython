//! duplicating a counting loop, to hold its counters as machine integers and narrow its
//! invariant bounds
//!
//! a counter is a machine integer only inside a copy of its loop — see
//! [`crate::unbox_counters`] for why — so a loop whose counters are worth that is
//! duplicated: one copy that holds them as machine integers, one copy exactly as it stands,
//! and narrowings ahead of both that pick between them.
//!
//! the same copy narrows the loop's bounds. a machine counter compared against a bound that
//! is still tagged tests the bound's shortness on every trip. the test cannot be hoisted on
//! its own — the bound is an ordinary python `int` and may be arbitrarily large, so the
//! answer has to be known before the comparison — but when nothing in the loop writes the
//! bound, the *answer* is the same on every trip even though the test is not. on a scalar
//! float loop that is 6% of the whole running time, and it is what closes the last of the
//! gap to mypyc on `mandel` — the shortness branch, not its computation, was the cost.
//!
//! see `Terminator::NarrowShort` for why the test and the narrowing are one thing.

use std::collections::{BTreeMap, HashSet};

use by_ir::function::{BasicBlock, Function, ModuleIr};
use by_ir::ops::{BlockId, Op, Terminator};

use crate::guard_loops::{map_edges, natural_loop};
use crate::liveness::{live_in, read_registers};
use crate::unbox_counters::{bounds, counters_given, enter, unbox};

/// how many loops in one function may be duplicated
///
/// every unswitch copies a loop body, so an unbounded pass would grow a deeply nested
/// function geometrically. a nest is one duplicate however deep it is, so four is spent
/// only where a nest's loops have to be taken one at a time
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

/// duplicate the outermost loop worth it, reporting whether one was found
fn unswitch_one(function: &mut Function) -> bool {
    let Some(candidate) = candidate(function) else {
        return false;
    };
    let Candidate {
        header,
        counters,
        bounds,
        body,
    } = candidate;

    // the copies land at the end, so every existing block keeps its id and only the
    // edges into the header have to move
    let before = function.blocks.len();
    let copies: BTreeMap<BlockId, BlockId> = body
        .iter()
        .enumerate()
        .map(|(offset, id)| (*id, BlockId(before + offset)))
        .collect();
    for id in &body {
        let mut block = function.blocks[id.index()].clone();
        map_edges(&mut block, true, |id| {
            copies.get(&id).copied().unwrap_or(id)
        });
        function.blocks.push(block);
    }
    let narrowings = unbox(function, header, &copies, &counters, &bounds);

    // the copy is entered only once everything it reads has narrowed
    let entries: Vec<BlockId> = (0..before)
        .map(BlockId)
        .filter(|id| {
            !body.contains(id)
                && function.blocks[id.index()]
                    .terminator
                    .successors()
                    .contains(&header)
        })
        .collect();
    let preheader = enter(function, header, &entries, &narrowings, copies[&header]);

    // the preheader replaces the header at every edge that entered the loop from
    // outside it. every block added here is inside: the narrowings' own `otherwise` edges
    // *are* edges to the header, and so are the back edges of the pieces of the loop as
    // written that a step leaves for
    for id in 0..before {
        if body.contains(&BlockId(id)) {
            continue;
        }
        map_edges(&mut function.blocks[id], false, |target| {
            if target == header { preheader } else { target }
        });
    }
    clear_unreached(function, &body);
    true
}

/// empty the blocks of the loop as written that nothing reaches any more
///
/// a copy that narrows nothing is entered unconditionally, and one whose steps cannot
/// leave the machine word has nowhere to leave for, so the loop as written would only be
/// dead C
fn clear_unreached(function: &mut Function, body: &[BlockId]) {
    let mut seen: HashSet<BlockId> = HashSet::from([Function::entry()]);
    let mut queue = vec![Function::entry()];
    while let Some(id) = queue.pop() {
        let block = &function.blocks[id.index()];
        for next in block.successors().into_iter().chain(block.error_target) {
            if seen.insert(next) {
                queue.push(next);
            }
        }
    }
    for id in body {
        if !seen.contains(id) {
            let block = &mut function.blocks[id.index()];
            block.ops.clear();
            block.terminator = Terminator::Unreachable;
            block.error_target = None;
        }
    }
}

/// a loop that can be duplicated
struct Candidate {
    /// the block holding the guard, which is the loop's header
    header: BlockId,
    /// the registers the copy holds as machine integers
    counters: Vec<by_ir::ops::RegisterId>,
    /// every tagged bound a guard inside the loop compares a counter against that nothing
    /// inside it writes — each narrowed once on the way in
    bounds: Vec<by_ir::ops::RegisterId>,
    /// the loop's blocks, the header and every loop nested inside it included
    body: Vec<BlockId>,
}

/// the loop worth duplicating first: of every loop with a counter to hold as a machine
/// integer or a bound to narrow, the one holding the most blocks
///
/// that is the outermost of a nest, and its copy takes every counter and bound the nest
/// reads that it can — so an inner loop gets its fast form from the one copy rather than
/// from a copy of its own inside each copy of the loops around it
///
/// only a loop the program reaches without leaving a copy is asked about. the loop as
/// written that a copy leaves for runs only where a value did not narrow or a step left
/// the machine word, and a copy of it would be code for a path that is already the rare one
fn candidate(function: &Function) -> Option<Candidate> {
    let fast = reached_without_leaving(function);
    let live = live_in(function, &read_registers);
    let mut best: Option<Candidate> = None;
    for index in 0..function.blocks.len() {
        let header = BlockId(index);
        // a header that is the function's own entry has no edge into it from outside, and
        // the preheader takes the loop's entry edges, so without one it would be
        // unreachable and the copy dead
        if header == Function::entry() || !fast.contains(&header) {
            continue;
        }
        let body = loop_blocks(function, header);
        if body.len() < 2 || body.len() > MAX_BODY {
            continue;
        }
        if best
            .as_ref()
            .is_some_and(|best| best.body.len() >= body.len())
        {
            continue;
        }
        if !function
            .blocks
            .iter()
            .enumerate()
            .any(|(id, block)| !body.contains(&BlockId(id)) && block.successors().contains(&header))
        {
            continue;
        }
        let counters = counters_given(function, &body, &live);
        let bounds = bounds(function, &body, &counters);
        if counters.is_empty() && bounds.is_empty() {
            continue;
        }
        best = Some(Candidate {
            header,
            counters,
            bounds,
            body,
        });
    }
    best
}

/// every block reached from the entry without taking an edge that leaves a copy for the
/// loop it was made of: a narrowing that did not fit, a step that left the machine word,
/// or an entry test that answered no
fn reached_without_leaving(function: &Function) -> HashSet<BlockId> {
    let mut seen: HashSet<BlockId> = HashSet::from([Function::entry()]);
    let mut queue = vec![Function::entry()];
    while let Some(id) = queue.pop() {
        let block = &function.blocks[id.index()];
        let taken = match &block.terminator {
            Terminator::NarrowShort { fits, .. } | Terminator::MachineStep { fits, .. } => {
                vec![*fits]
            }
            Terminator::Branch { then_block, .. }
                if matches!(block.ops.last(), Some(Op::LoopGuardsHold { .. })) =>
            {
                vec![*then_block]
            }
            _ => block.successors(),
        };
        for next in taken.into_iter().chain(block.error_target) {
            if seen.insert(next) {
                queue.push(next);
            }
        }
    }
    seen
}

/// the blocks of the loop `header` heads
///
/// the natural loop, which leaves out the loops around it. a loop entered other than
/// through its header — the original a versioned loop leaves for, entered again from its
/// copy's exits — has no natural loop, and is every block on a cycle through its header
fn loop_blocks(function: &Function, header: BlockId) -> Vec<BlockId> {
    let natural = natural_loop(function, header);
    if natural.is_empty() {
        cycle_through(function, header)
    } else {
        natural
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{BinOp, CmpOp, Mutation, RegisterId, Value};
    use by_ir::rtype::RType;
    use by_ir::verify::verify;

    use crate::unbox_counters::is_fixed;

    /// `i = 0; while i < bound: i = i + 1; return i`, with `bound` a parameter
    ///
    /// `body_writes_bound` also multiplies the bound in the body, which is a write no
    /// machine integer takes over and one that makes the bound no longer invariant.
    /// `from_parameter` starts `i` from a second parameter instead of from zero
    fn counting_loop(body_writes_bound: bool, from_parameter: bool) -> Function {
        let mut builder = FunctionBuilder::new("count", RType::INT);
        let bound = builder.param("bound", RType::INT);
        let start = builder.param("start", RType::INT);
        let index = builder.local("i", RType::INT);
        let more = builder.temp(RType::BIT);
        let next = builder.temp(RType::INT);
        builder.assign(
            index,
            if from_parameter {
                Value::Register(start)
            } else {
                Value::Int(0)
            },
        );

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
            op: BinOp::Add,
            lhs: Value::Register(index),
            rhs: Value::Int(1),
            mutation: Mutation::Fresh,
        });
        builder.assign(index, Value::Register(next));
        if body_writes_bound {
            builder.push(Op::IntBinary {
                dest: bound,
                op: BinOp::Mul,
                lhs: Value::Register(bound),
                rhs: Value::Int(1),
                mutation: Mutation::Fresh,
            });
        }
        builder.terminate(Terminator::Goto(header));

        builder.switch_to(exit);
        builder.terminate(Terminator::Return(Value::Register(index)));
        builder.finish()
    }

    pub(crate) fn module_with(function: Function) -> ModuleIr {
        let mut module = ModuleIr::new("app");
        module.functions.push(function);
        module
    }

    fn unswitched(function: Function) -> Function {
        let mut module = module_with(function);
        run(&mut module);
        let function = module.functions.remove(0);
        assert_eq!(
            verify(&function),
            Ok(()),
            "{}",
            by_ir::print::print_function(&function)
        );
        function
    }

    /// the tagged registers the narrowings ahead of the copies read
    fn narrowed(function: &Function) -> Vec<RegisterId> {
        function
            .blocks
            .iter()
            .filter_map(|block| match block.terminator {
                Terminator::NarrowShort {
                    src: Value::Register(src),
                    ..
                } => Some(src),
                _ => None,
            })
            .collect()
    }

    /// the operations of every block the function reaches without leaving a copy
    pub(crate) fn fast_ops(function: &Function) -> Vec<&Op> {
        let fast = reached_without_leaving(function);
        let mut ids: Vec<BlockId> = fast.into_iter().collect();
        ids.sort_unstable();
        ids.iter()
            .flat_map(|id| &function.blocks[id.index()].ops)
            .collect()
    }

    fn compares(ops: &[&Op]) -> Vec<(Value, Value)> {
        ops.iter()
            .filter_map(|op| match op {
                Op::IntCompare { lhs, rhs, .. } => Some((lhs.clone(), rhs.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_counting_loop_is_duplicated_with_its_counter_and_its_bound_narrowed() {
        let function = unswitched(counting_loop(false, true));
        let printed = by_ir::print::print_function(&function);
        let mut sources = narrowed(&function);
        sources.sort_unstable();
        assert_eq!(sources, vec![RegisterId(0), RegisterId(2)], "{printed}");
        // the whole point: the copy's guard is a comparison of two machine integers
        let guards = compares(&fast_ops(&function));
        assert_eq!(guards.len(), 1, "{printed}");
        for (lhs, rhs) in guards {
            assert!(
                is_fixed(&function, &lhs) && is_fixed(&function, &rhs),
                "{printed}"
            );
        }
    }

    #[test]
    fn a_counter_set_to_a_short_literal_ahead_of_the_loop_is_not_tested_on_the_way_in() {
        let function = unswitched(counting_loop(false, false));
        let printed = by_ir::print::print_function(&function);
        assert_eq!(narrowed(&function), vec![RegisterId(0)], "{printed}");
        assert!(
            fast_ops(&function).iter().any(|op| matches!(
                op,
                Op::Assign {
                    src: Value::Fixed(0),
                    ..
                }
            )),
            "{printed}"
        );
    }

    #[test]
    fn the_loop_as_written_keeps_its_tagged_counter() {
        let function = unswitched(counting_loop(false, true));
        let printed = by_ir::print::print_function(&function);
        let tagged = compares(
            &function
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .collect::<Vec<_>>(),
        )
        .into_iter()
        .filter(|(lhs, _)| *lhs == Value::Register(RegisterId(2)))
        .count();
        assert_eq!(tagged, 1, "{printed}");
    }

    #[test]
    fn a_step_that_leaves_the_machine_word_writes_the_counter_back_and_steps_it_as_written() {
        let function = unswitched(counting_loop(false, true));
        let printed = by_ir::print::print_function(&function);
        let Some(Terminator::MachineStep {
            lhs: Value::Register(machine),
            overflows,
            ..
        }) = function
            .blocks
            .iter()
            .map(|block| &block.terminator)
            .find(|terminator| matches!(terminator, Terminator::MachineStep { .. }))
        else {
            panic!("the copy has no machine step: {printed}");
        };
        let landing = &function.blocks[overflows.index()];
        assert_eq!(
            landing.ops,
            vec![Op::Box {
                dest: RegisterId(2),
                src: Value::Register(*machine),
            }],
            "{printed}"
        );
        let Terminator::Goto(resume) = landing.terminator else {
            panic!("the landing does not continue in the loop as written: {printed}");
        };
        assert!(
            matches!(
                function.blocks[resume.index()].ops.first(),
                Some(Op::IntBinary {
                    lhs: Value::Register(RegisterId(2)),
                    ..
                })
            ),
            "{printed}"
        );
    }

    #[test]
    fn a_bound_the_body_writes_is_not_narrowed() {
        let function = unswitched(counting_loop(true, true));
        let printed = by_ir::print::print_function(&function);
        assert_eq!(narrowed(&function), vec![RegisterId(2)], "{printed}");
    }

    /// `total = 0.0; y = 0; while y < rows: x = 0; while x < columns: total = total +
    /// float(columns); x = x + 1; y = y + 1`, with `rows` and `columns` parameters
    ///
    /// `inner_bound_written` reassigns `columns` in the outer body instead, which leaves the
    /// inner bound invariant across the inner loop alone
    fn nested_loops(inner_bound_written: bool) -> Function {
        let mut builder = FunctionBuilder::new("grid", RType::FLOAT);
        let rows = builder.param("rows", RType::INT);
        let columns = builder.param("columns", RType::INT);
        let y = builder.local("y", RType::INT);
        let x = builder.local("x", RType::INT);
        let total = builder.local("total", RType::FLOAT);
        let outer_more = builder.temp(RType::BIT);
        let inner_more = builder.temp(RType::BIT);
        let widened = builder.temp(RType::FLOAT);
        let next_x = builder.temp(RType::INT);
        let next_y = builder.temp(RType::INT);
        builder.push(Op::Assign {
            dest: total,
            src: Value::Float(0.0),
        });
        builder.assign(y, Value::Int(0));

        let outer = builder.new_block();
        let outer_body = builder.new_block();
        let inner = builder.new_block();
        let inner_body = builder.new_block();
        let outer_step = builder.new_block();
        let exit = builder.new_block();
        builder.terminate(Terminator::Goto(outer));

        builder.switch_to(outer);
        builder.push(Op::IntCompare {
            dest: outer_more,
            op: CmpOp::Lt,
            lhs: Value::Register(y),
            rhs: Value::Register(rows),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(outer_more),
            then_block: outer_body,
            else_block: exit,
        });

        builder.switch_to(outer_body);
        builder.assign(x, Value::Int(0));
        if inner_bound_written {
            builder.push(Op::IntBinary {
                dest: columns,
                op: BinOp::Mul,
                lhs: Value::Register(rows),
                rhs: Value::Int(1),
                mutation: Mutation::Fresh,
            });
        }
        builder.terminate(Terminator::Goto(inner));

        builder.switch_to(inner);
        builder.push(Op::IntCompare {
            dest: inner_more,
            op: CmpOp::Lt,
            lhs: Value::Register(x),
            rhs: Value::Register(columns),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(inner_more),
            then_block: inner_body,
            else_block: outer_step,
        });

        builder.switch_to(inner_body);
        builder.push(Op::IntToFloat {
            dest: widened,
            src: Value::Register(columns),
        });
        builder.push(Op::FloatBinary {
            dest: total,
            op: BinOp::Add,
            lhs: Value::Register(total),
            rhs: Value::Register(widened),
        });
        builder.push(Op::IntBinary {
            dest: next_x,
            op: BinOp::Add,
            lhs: Value::Register(x),
            rhs: Value::Int(1),
            mutation: Mutation::Fresh,
        });
        builder.assign(x, Value::Register(next_x));
        builder.terminate(Terminator::Goto(inner));

        builder.switch_to(outer_step);
        builder.push(Op::IntBinary {
            dest: next_y,
            op: BinOp::Add,
            lhs: Value::Register(y),
            rhs: Value::Int(1),
            mutation: Mutation::Fresh,
        });
        builder.assign(y, Value::Register(next_y));
        builder.terminate(Terminator::Goto(outer));

        builder.switch_to(exit);
        builder.terminate(Terminator::Return(Value::Register(total)));
        builder.finish()
    }

    #[test]
    fn a_nest_whose_bounds_are_all_invariant_is_duplicated_once() {
        let function = unswitched(nested_loops(false));
        let printed = by_ir::print::print_function(&function);
        // `x` is set before it is read on every trip round the outer loop, and `y` is set to
        // zero just ahead of the nest, so only the two bounds are narrowed, in one chain
        let mut sources = narrowed(&function);
        sources.sort_unstable();
        assert_eq!(sources, vec![RegisterId(0), RegisterId(1)], "{printed}");
        let fast = fast_ops(&function);
        let guards = compares(&fast);
        assert_eq!(guards.len(), 2, "{printed}");
        for (lhs, rhs) in guards {
            assert!(
                is_fixed(&function, &lhs) && is_fixed(&function, &rhs),
                "{printed}"
            );
        }
        // the bound widened to a double inside the copy reads the narrowed register too
        assert!(
            fast.iter()
                .any(|op| matches!(op, Op::IntToFloat { src, .. } if is_fixed(&function, src))),
            "{printed}"
        );
    }

    #[test]
    fn an_inner_loop_whose_bound_the_outer_one_writes_is_duplicated_on_its_own() {
        let function = unswitched(nested_loops(true));
        let printed = by_ir::print::print_function(&function);
        // the nest narrows `rows` on the way in, and the inner copy inside it narrows
        // `columns` once per trip round the outer loop. the nest as written, which runs only
        // where something did not narrow, is not duplicated again
        let mut sources = narrowed(&function);
        sources.sort_unstable();
        assert_eq!(sources, vec![RegisterId(0), RegisterId(1)], "{printed}");
        for (lhs, rhs) in compares(&fast_ops(&function)) {
            assert!(
                is_fixed(&function, &lhs) && is_fixed(&function, &rhs),
                "{printed}"
            );
        }
    }

    #[test]
    fn duplicating_twice_is_not_attempted() {
        for function in [
            counting_loop(false, true),
            nested_loops(false),
            nested_loops(true),
        ] {
            let mut module = module_with(function);
            run(&mut module);
            let once = module.functions[0].blocks.len();
            run(&mut module);
            assert_eq!(module.functions[0].blocks.len(), once);
        }
    }

    /// `i = start; while i < bound: doubled = i * 2; i = i + 1`, with the step moved ahead
    /// of the multiplication where `stepped_first`
    fn doubling_loop(start: i64, stepped_first: bool) -> Function {
        let mut builder = FunctionBuilder::new("count", RType::INT);
        let bound = builder.param("bound", RType::INT);
        let index = builder.local("i", RType::INT);
        let more = builder.temp(RType::BIT);
        let next = builder.temp(RType::INT);
        let doubled = builder.temp(RType::INT);
        builder.assign(index, Value::Int(start));

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
        let step = |builder: &mut FunctionBuilder| {
            builder.push(Op::IntBinary {
                dest: next,
                op: BinOp::Add,
                lhs: Value::Register(index),
                rhs: Value::Int(1),
                mutation: Mutation::Fresh,
            });
            builder.assign(index, Value::Register(next));
        };
        if stepped_first {
            step(&mut builder);
        }
        // a multiplication has no machine form, so it reads the counter's tagged value
        builder.push(Op::IntBinary {
            dest: doubled,
            op: BinOp::Mul,
            lhs: Value::Register(index),
            rhs: Value::Int(2),
            mutation: Mutation::Fresh,
        });
        if !stepped_first {
            step(&mut builder);
        }
        builder.terminate(Terminator::Goto(header));

        builder.switch_to(exit);
        builder.terminate(Terminator::Return(Value::Register(index)));
        builder.finish()
    }

    fn tags(function: &Function) -> usize {
        fast_ops(function)
            .iter()
            .filter(|op| matches!(op, Op::TagShort { .. }))
            .count()
    }

    #[test]
    fn a_counter_the_copy_s_guard_keeps_short_is_tagged_with_no_range_test() {
        let function = unswitched(doubling_loop(0, false));
        let printed = by_ir::print::print_function(&function);
        assert_eq!(tags(&function), 1, "{printed}");
    }

    #[test]
    fn a_counter_written_after_its_guard_is_not_proven_short() {
        let function = unswitched(doubling_loop(0, true));
        let printed = by_ir::print::print_function(&function);
        assert_eq!(tags(&function), 0, "{printed}");
    }

    #[test]
    fn a_counter_is_proven_short_by_the_narrowing_it_entered_the_copy_through() {
        // the counter starts below the short range, where it does not narrow and the loop
        // as written runs instead. the copy is only ever entered with a short, and a step
        // up from one stays above the smallest
        let function = unswitched(doubling_loop(-(1 << 62) - 1, false));
        let printed = by_ir::print::print_function(&function);
        assert_eq!(tags(&function), 1, "{printed}");
    }
}

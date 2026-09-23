//! keeping the length of a buffer a loop appends to in a machine register
//!
//! an append reads the buffer's length and capacity out of its header, stores the
//! element, and stores the length back. the next append loads the length it has just
//! stored, and a load of a value still on its way to memory waits for the store to land:
//! measured on `comp`, that wait is two thirds of the loop's cycles, though it barely shows
//! in the instructions it runs
//!
//! ```python
//! out = []
//! for x in xs:
//!     out.append(x + 0.5)   # load `len`, compare against `cap`, store `len + 1`
//! ```
//!
//! so where a loop does nothing with a buffer but append to it, the length lives in a
//! register while the loop runs: loaded once on the way in, advanced by each append, and
//! written back on every way out that reaches something reading the buffer — an exception
//! a handler catches included. nothing inside the loop reads the header's length, and no
//! other name can reach the buffer, so nothing can tell the header was behind

use std::collections::{BTreeMap, BTreeSet};

use by_ir::function::{BasicBlock, Function, ModuleIr, RegisterDecl};
use by_ir::ops::{BlockId, Op, RegisterId, Terminator, Value};
use by_ir::rtype::{IntWidth, RType};

use crate::guard_loops::{map_edges, natural_loop};
use crate::liveness::{live_in, read_registers};

pub(crate) fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        hold_lengths(function);
    }
}

fn hold_lengths(function: &mut Function) {
    let mut held: Vec<(RegisterId, Vec<BlockId>)> = Vec::new();
    // each loop is taken with every register in the state the loops taken before it left
    // them, largest first, so a nest keeps one length for the whole of it
    while let Some((header, body, array)) = candidate(function, &held) {
        hold(function, header, &body, array);
        held.push((array, body));
    }
}

/// the largest loop that only appends to a buffer, with the buffer
fn candidate(
    function: &Function,
    held: &[(RegisterId, Vec<BlockId>)],
) -> Option<(BlockId, Vec<BlockId>, RegisterId)> {
    let mut best: Option<(BlockId, Vec<BlockId>, RegisterId)> = None;
    for index in 0..function.blocks.len() {
        let header = BlockId(index);
        if header == Function::entry() {
            continue;
        }
        let body = natural_loop(function, header);
        if body.is_empty()
            || best
                .as_ref()
                .is_some_and(|(_, best, _)| best.len() >= body.len())
        {
            continue;
        }
        let appended: BTreeSet<RegisterId> = body
            .iter()
            .flat_map(|id| &function.blocks[id.index()].ops)
            .filter_map(|op| match op {
                Op::ArrayPush {
                    array: Value::Register(array),
                    length: None,
                    ..
                } => Some(*array),
                _ => None,
            })
            .collect();
        let found = appended.into_iter().find(|array| {
            !held
                .iter()
                .any(|(done, region)| done == array && region.contains(&header))
                && only_appended(function, &body, *array)
        });
        if let Some(array) = found {
            best = Some((header, body, array));
        }
    }
    best
}

/// whether the loop made of `body` does nothing with `array` but append to it, and nothing
/// but `array` can reach the buffer it holds
///
/// a buffer is only ever reached through registers, so the question is what the registers
/// can hold. a register written only by building a new buffer holds one no other register
/// was handed, since handing it over would be a write of that other register — and a
/// parameter's buffer is its caller's too. so the loop may append to any number of those,
/// each of which then has a length of its own, and may read no other buffer at all
fn only_appended(function: &Function, body: &[BlockId], array: RegisterId) -> bool {
    let is_array = |register: RegisterId| {
        function
            .register(register)
            .is_some_and(|decl| matches!(decl.ty, RType::Array(_)))
    };
    let made_here = |register: RegisterId| {
        register.index() >= function.param_count
            && is_array(register)
            && function
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .filter(|op| op.dest() == Some(register))
                .all(|op| matches!(op, Op::ArrayNew { .. }))
            && function
                .blocks
                .iter()
                .all(|block| block.terminator.dest() != Some(register))
    };
    if !made_here(array) {
        return false;
    }
    let reads_a_buffer =
        |value: &Value| matches!(value, Value::Register(register) if is_array(*register));
    body.iter().all(|id| {
        let block = &function.blocks[id.index()];
        block.ops.iter().all(|op| match op {
            Op::ArrayPush {
                array: Value::Register(pushed),
                value,
                ..
            } => made_here(*pushed) && !reads_a_buffer(value),
            other => {
                !other.operands().into_iter().any(reads_a_buffer)
                    && other.dest().is_none_or(|dest| !is_array(dest))
            }
        }) && !block.terminator.operands().into_iter().any(reads_a_buffer)
    })
}

fn hold(function: &mut Function, header: BlockId, body: &[BlockId], array: RegisterId) {
    let live = live_in(function, &read_registers);
    let length = RegisterId(function.registers.len());
    function.registers.push(RegisterDecl {
        name: None,
        ty: RType::fixed(IntWidth::I64),
        borrowed: false,
        may_be_unassigned: false,
    });
    let existing = function.blocks.len();
    let (range, position) = {
        let block = &function.blocks[header.index()];
        (block.range, block.position)
    };
    let block = |ops: Vec<Op>, terminator: Terminator| BasicBlock {
        ops,
        terminator,
        owned_at_exit: None,
        range,
        position,
        error_target: None,
    };
    let stored = || Op::ArrayStoreLength {
        array: Value::Register(array),
        length: Value::Register(length),
    };

    for id in body {
        for op in &mut function.blocks[id.index()].ops {
            if let Op::ArrayPush {
                array: Value::Register(pushed),
                length: counted @ None,
                ..
            } = op
                && *pushed == array
            {
                *counted = Some(Value::Register(length));
            }
        }
    }

    // on the way out, and on the way to a handler outside the loop, wherever the buffer is
    // read again
    let read_at = |target: BlockId| live[target.index()].contains(&array);
    let mut handlers: BTreeMap<BlockId, BlockId> = BTreeMap::new();
    for id in body {
        let targets = function.blocks[id.index()].terminator.successors();
        for target in targets {
            if body.contains(&target) || target.index() >= existing || !read_at(target) {
                continue;
            }
            let landing = BlockId(function.blocks.len());
            function
                .blocks
                .push(block(vec![stored()], Terminator::Goto(target)));
            for edge in function.blocks[id.index()].terminator.successors_mut() {
                if *edge == target {
                    *edge = landing;
                }
            }
        }
        if let Some(handler) = function.blocks[id.index()].error_target
            && !body.contains(&handler)
            && read_at(handler)
        {
            let landing = *handlers.entry(handler).or_insert_with(|| {
                let landing = BlockId(function.blocks.len());
                function
                    .blocks
                    .push(block(vec![stored()], Terminator::Goto(handler)));
                landing
            });
            function.blocks[id.index()].error_target = Some(landing);
        }
    }

    // and on the way in, from wherever the loop is entered
    let preheader = BlockId(function.blocks.len());
    function.blocks.push(block(
        vec![Op::ArrayLen {
            dest: length,
            array: Value::Register(array),
        }],
        Terminator::Goto(header),
    ));
    for id in 0..existing {
        if body.contains(&BlockId(id)) {
            continue;
        }
        map_edges(&mut function.blocks[id], false, |target| {
            if target == header { preheader } else { target }
        });
    }
}

#[cfg(test)]
mod tests {
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{CmpOp, Mutation};
    use by_ir::verify::verify;

    use super::*;

    /// `out = [0.0]; i = 0; while i < n: out.append(x); [len(out)]; i = i + 1; return out`,
    /// with a handler reading `out` on the body's error edge where `handled`
    fn appending(read_inside: bool, handled: bool) -> Function {
        let mut builder = FunctionBuilder::new("fill", RType::INT);
        let n = builder.param("n", RType::INT);
        let x = builder.param("x", RType::FLOAT);
        let out = builder.local("out", RType::Array(Box::new(RType::FLOAT)));
        let i = builder.local("i", RType::INT);
        let more = builder.temp(RType::BIT);
        let status = builder.temp(RType::BIT);
        let size = builder.temp(RType::INT);
        let answer = builder.temp(RType::INT);
        builder.push(Op::ArrayNew {
            dest: out,
            items: vec![Value::Float(0.0)],
        });
        builder.assign(i, Value::Int(0));
        let header = builder.new_block();
        let body = builder.new_block();
        let exit = builder.new_block();
        let handler = builder.new_block();
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(header);
        builder.push(Op::IntCompare {
            dest: more,
            op: CmpOp::Lt,
            lhs: Value::Register(i),
            rhs: Value::Register(n),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(more),
            then_block: body,
            else_block: exit,
        });
        builder.switch_to(body);
        builder.push(Op::ArrayPush {
            dest: status,
            array: Value::Register(out),
            value: Value::Register(x),
            length: None,
        });
        if read_inside {
            builder.push(Op::ArrayLen {
                dest: size,
                array: Value::Register(out),
            });
        }
        builder.push(Op::IntBinary {
            dest: i,
            op: by_ir::ops::BinOp::Add,
            lhs: Value::Register(i),
            rhs: Value::Int(1),
            mutation: Mutation::Fresh,
        });
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(exit);
        builder.push(Op::ArrayLen {
            dest: answer,
            array: Value::Register(out),
        });
        builder.terminate(Terminator::Return(Value::Register(answer)));
        builder.switch_to(handler);
        builder.push(Op::ArrayLen {
            dest: answer,
            array: Value::Register(out),
        });
        builder.terminate(Terminator::Return(Value::Register(answer)));
        let mut function = builder.finish();
        if handled {
            function.blocks[body.index()].error_target = Some(handler);
        }
        function
    }

    fn held(function: Function) -> Function {
        let mut module = ModuleIr::new("app");
        module.functions.push(function);
        run(&mut module);
        let function = module.functions.remove(0);
        assert_eq!(verify(&function), Ok(()));
        function
    }

    fn stores(function: &Function) -> Vec<BlockId> {
        (0..function.blocks.len())
            .map(BlockId)
            .filter(|id| {
                function.blocks[id.index()]
                    .ops
                    .iter()
                    .any(|op| matches!(op, Op::ArrayStoreLength { .. }))
            })
            .collect()
    }

    #[test]
    fn a_loop_that_only_appends_counts_in_a_register_and_writes_it_back_on_the_way_out() {
        let function = held(appending(false, false));
        let printed = by_ir::print::print_function(&function);
        assert!(
            function
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .any(|op| matches!(
                    op,
                    Op::ArrayPush {
                        length: Some(_),
                        ..
                    }
                )),
            "{printed}"
        );
        assert_eq!(stores(&function).len(), 1, "{printed}");
    }

    #[test]
    fn a_handler_that_reads_the_buffer_is_reached_through_a_write_back() {
        let function = held(appending(false, true));
        let printed = by_ir::print::print_function(&function);
        let stored = stores(&function);
        assert_eq!(stored.len(), 2, "{printed}");
        assert!(
            function.blocks.iter().any(|block| block
                .error_target
                .is_some_and(|target| stored.contains(&target))),
            "{printed}"
        );
    }

    #[test]
    fn a_loop_that_reads_the_buffer_keeps_the_header_current() {
        let function = held(appending(true, false));
        let printed = by_ir::print::print_function(&function);
        assert!(stores(&function).is_empty(), "{printed}");
        assert!(
            !function
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .any(|op| matches!(
                    op,
                    Op::ArrayPush {
                        length: Some(_),
                        ..
                    }
                )),
            "{printed}"
        );
    }
}

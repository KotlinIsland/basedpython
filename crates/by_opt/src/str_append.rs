//! grow a string in place, where the concatenation is its last reader
//!
//! `PyUnicode_Concat` always copies. that is not a cpython oversight — it cannot
//! know whether anyone else can see the left operand, so it has to assume someone
//! can. `PyUnicode_Append` is the version that asks, and it grows the string in
//! place when the answer is "nobody". the difference is the whole cost of building
//! a string a piece at a time: a copy per step is quadratic in the result, a resize
//! per step is linear.
//!
//! the answer is "nobody" exactly when the operand's register holds the only
//! reference and hands it over. so this pass looks for the concatenations whose
//! left operand register is **dead the moment it has been read** — nothing reads it
//! again before something overwrites it — and marks them as taking the reference
//! with them.
//!
//! ## the error edge is the interesting half
//!
//! a consuming concatenation empties its left operand's register, and a failed one
//! empties it without putting anything back. python leaves `out` bound to what it
//! was when `out = out + x` raises, so a handler that reads `out` would see the
//! difference.
//!
//! it cannot, because the same liveness that licenses the consumption is computed
//! *across the error edge*: a register a handler could read is live there, and a
//! live register is not consumed. the condition that makes the append fast is the
//! condition that makes the failure unobservable, and they are the same condition
//! rather than two that have to be kept in step.
//!
//! ## what is left alone
//!
//! - a **parameter**, whose reference the frame owns rather than the register
//! - a **borrowed** register, which owns nothing to hand over
//! - the operand of `s + s`, where the sole owner is reading the buffer it would be
//!   resizing. the runtime helper refuses this case too, so the hazard does not
//!   depend on this pass being right about it

use std::collections::HashSet;

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{BlockId, Op, RegisterId, Value};

pub(crate) fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        consume_dying_operands(function);
    }
}

fn consume_dying_operands(function: &mut Function) {
    let live_in = live_in_sets(function);
    let mut consumed: Vec<(BlockId, usize)> = Vec::new();

    for (index, block) in function.blocks.iter().enumerate() {
        let error_live = block
            .error_target
            .and_then(|target| live_in.get(target.index()))
            .cloned()
            .unwrap_or_default();
        let mut live: HashSet<RegisterId> = block
            .successors()
            .iter()
            .filter_map(|successor| live_in.get(successor.index()))
            .flatten()
            .copied()
            .collect();
        live.extend(block.terminator.operands().into_iter().filter_map(register));

        for (position, op) in block.ops.iter().enumerate().rev() {
            // an exception leaves from the middle of the block, so what the handler
            // reads is live here whatever the rest of the block goes on to write
            live.extend(error_live.iter().copied());
            if let Op::StrConcat {
                lhs: Value::Register(source),
                rhs,
                ..
            } = op
            {
                // a register this operation writes cannot be read again through the
                // value it held: every later read on this edge sees the new one
                let overwritten = op.dest() == Some(*source);
                let read_again = live.contains(source) && !overwritten;
                // a failed append has no reference left to put back, so a register a
                // handler could still read has to keep its own
                if !read_again
                    && !error_live.contains(source)
                    && rhs != &Value::Register(*source)
                    && may_hand_over(function, *source)
                {
                    consumed.push((BlockId(index), position));
                }
            }
            if let Some(dest) = op.dest() {
                live.remove(&dest);
            }
            // `del x` leaves its destination unbound, but it reads the value first —
            // to release it — so the reference is still needed here and must not be
            // handed to an append below
            live.extend(op.unbinds());
            live.extend(op.operands().into_iter().filter_map(register));
        }
    }

    for (block, position) in consumed {
        if let Some(Op::StrConcat { consumes_lhs, .. }) = function
            .blocks
            .get_mut(block.index())
            .and_then(|block| block.ops.get_mut(position))
        {
            *consumes_lhs = true;
        }
    }
}

/// whether the register owns the reference it holds, and so has one to give away
fn may_hand_over(function: &Function, register: RegisterId) -> bool {
    if register.index() < function.param_count {
        return false;
    }
    function
        .register(register)
        .is_some_and(|decl| !decl.borrowed)
}

/// the registers live on entry to each block
fn live_in_sets(function: &Function) -> Vec<HashSet<RegisterId>> {
    let mut live_in: Vec<HashSet<RegisterId>> = vec![HashSet::new(); function.blocks.len()];

    let mut changed = true;
    while changed {
        changed = false;
        for index in (0..function.blocks.len()).rev() {
            let Some(block) = function.block(BlockId(index)) else {
                continue;
            };
            let mut live: HashSet<RegisterId> = block
                .successors()
                .iter()
                .filter_map(|successor| live_in.get(successor.index()))
                .flatten()
                .copied()
                .collect();
            live.extend(block.terminator.operands().into_iter().filter_map(register));

            // an exception leaves from the middle of the block, so the handler's
            // needs survive every kill below it
            let error_live = block
                .error_target
                .and_then(|target| live_in.get(target.index()))
                .cloned()
                .unwrap_or_default();
            for op in block.ops.iter().rev() {
                live.extend(error_live.iter().copied());
                if let Some(dest) = op.dest() {
                    live.remove(&dest);
                }
                live.extend(op.unbinds());
                live.extend(op.operands().into_iter().filter_map(register));
            }

            if live != live_in[index] {
                live_in[index] = live;
                changed = true;
            }
        }
    }

    live_in
}

fn register(value: &Value) -> Option<RegisterId> {
    match value {
        Value::Register(id) => Some(*id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{CmpOp, Op, Terminator, Value};
    use by_ir::rtype::RType;
    use by_ir::verify::verify;

    fn module(function: Function) -> ModuleIr {
        ModuleIr {
            name: by_ir::ModuleName::new("app"),
            functions: vec![function],
            declined: Vec::new(),
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
            fallback_code: None,
            shims: None,
        }
    }

    /// whether the concatenation at `position` in `block` takes its operand over
    fn consumes(module: &ModuleIr, block: usize, position: usize) -> bool {
        matches!(
            module.functions[0].blocks[block].ops[position],
            Op::StrConcat {
                consumes_lhs: true,
                ..
            }
        )
    }

    #[test]
    fn a_dying_temporary_hands_its_reference_over() {
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let piece = builder.param("p", RType::STR);
        let first = builder.temp(RType::STR);
        let second = builder.temp(RType::STR);
        builder.push(Op::StrConcat {
            dest: first,
            lhs: Value::Str("a".to_string()),
            rhs: Value::Register(piece),
            consumes_lhs: false,
        });
        builder.push(Op::StrConcat {
            dest: second,
            lhs: Value::Register(first),
            rhs: Value::Str("b".to_string()),
            consumes_lhs: false,
        });
        builder.terminate(Terminator::Return(Value::Register(second)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert!(consumes(&module, 0, 1), "{:?}", module.functions[0].blocks);
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn an_operand_read_again_keeps_its_reference() {
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let held = builder.local("held".to_string(), RType::STR);
        let joined = builder.temp(RType::STR);
        let doubled = builder.temp(RType::STR);
        builder.assign(held, Value::Str("a".to_string()));
        builder.push(Op::StrConcat {
            dest: joined,
            lhs: Value::Register(held),
            rhs: Value::Str("b".to_string()),
            consumes_lhs: false,
        });
        // the second read is what makes the first one not the last
        builder.push(Op::StrConcat {
            dest: doubled,
            lhs: Value::Register(joined),
            rhs: Value::Register(held),
            consumes_lhs: false,
        });
        builder.terminate(Terminator::Return(Value::Register(doubled)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert!(!consumes(&module, 0, 1), "{:?}", module.functions[0].blocks);
        assert!(consumes(&module, 0, 2), "{:?}", module.functions[0].blocks);
    }

    #[test]
    fn a_concatenation_into_its_own_operand_hands_it_over() {
        // `out = out + piece` after coalescing: the register is both operand and
        // destination, so the value it held is dead even though the register is not
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let piece = builder.param("p", RType::STR);
        let out = builder.local("out".to_string(), RType::STR);
        builder.assign(out, Value::Str(String::new()));
        builder.push(Op::StrConcat {
            dest: out,
            lhs: Value::Register(out),
            rhs: Value::Register(piece),
            consumes_lhs: false,
        });
        builder.terminate(Terminator::Return(Value::Register(out)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert!(consumes(&module, 0, 1), "{:?}", module.functions[0].blocks);
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn a_concatenation_into_its_own_operand_still_defers_to_a_handler() {
        // the operation writes the register, but a *failed* one does not — so the
        // handler would find it holding the value that was handed away
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let piece = builder.param("p", RType::STR);
        let out = builder.local("out".to_string(), RType::STR);
        let handler = builder.new_block();
        builder.assign(out, Value::Str(String::new()));
        builder.set_error_target(Some(handler));
        builder.push(Op::StrConcat {
            dest: out,
            lhs: Value::Register(out),
            rhs: Value::Register(piece),
            consumes_lhs: false,
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        builder.switch_to(handler);
        builder.terminate(Terminator::Return(Value::Register(out)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert!(!consumes(&module, 0, 1), "{:?}", module.functions[0].blocks);
    }

    #[test]
    fn a_parameter_is_left_alone() {
        // the frame owns a parameter's reference, so the register has none to give
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let text = builder.param("s", RType::STR);
        let joined = builder.temp(RType::STR);
        builder.push(Op::StrConcat {
            dest: joined,
            lhs: Value::Register(text),
            rhs: Value::Str("b".to_string()),
            consumes_lhs: false,
        });
        builder.terminate(Terminator::Return(Value::Register(joined)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert!(!consumes(&module, 0, 0), "{:?}", module.functions[0].blocks);
    }

    #[test]
    fn an_accumulator_hands_over_on_every_iteration() {
        // `out = out + piece` in a loop: `out` is read here and written at the end of
        // the same statement, so the read is its last one even though the register
        // outlives the block
        let mut builder = FunctionBuilder::new("build", RType::STR);
        let piece = builder.param("p", RType::STR);
        let limit = builder.param("n", RType::INT);
        let out = builder.local("out".to_string(), RType::STR);
        let counter = builder.local("i".to_string(), RType::INT);
        let cond = builder.temp(RType::BIT);
        let grown = builder.temp(RType::STR);
        let header = builder.new_block();
        let body = builder.new_block();
        let exit = builder.new_block();
        builder.assign(out, Value::Str(String::new()));
        builder.assign(counter, Value::Int(0));
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(header);
        builder.push(Op::IntCompare {
            dest: cond,
            op: CmpOp::Lt,
            lhs: Value::Register(counter),
            rhs: Value::Register(limit),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block: body,
            else_block: exit,
        });
        builder.switch_to(body);
        builder.push(Op::StrConcat {
            dest: grown,
            lhs: Value::Register(out),
            rhs: Value::Register(piece),
            consumes_lhs: false,
        });
        builder.assign(out, Value::Register(grown));
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(exit);
        builder.terminate(Terminator::Return(Value::Register(out)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert!(
            consumes(&module, body.index(), 0),
            "{:?}",
            module.functions[0].blocks
        );
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn a_handler_that_reads_the_operand_keeps_it() {
        // the divergence this rules out: a failed concatenation cannot put the
        // reference back, so a register the handler still reads must keep its own
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let held = builder.local("held".to_string(), RType::STR);
        let joined = builder.temp(RType::STR);
        let handler = builder.new_block();
        builder.assign(held, Value::Str("a".to_string()));
        builder.set_error_target(Some(handler));
        builder.push(Op::StrConcat {
            dest: joined,
            lhs: Value::Register(held),
            rhs: Value::Str("b".to_string()),
            consumes_lhs: false,
        });
        builder.terminate(Terminator::Return(Value::Register(joined)));
        builder.switch_to(handler);
        builder.terminate(Terminator::Return(Value::Register(held)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert!(!consumes(&module, 0, 1), "{:?}", module.functions[0].blocks);
    }

    #[test]
    fn appending_a_string_to_itself_is_not_a_hand_over() {
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let piece = builder.param("p", RType::STR);
        let made = builder.temp(RType::STR);
        let doubled = builder.temp(RType::STR);
        builder.push(Op::StrConcat {
            dest: made,
            lhs: Value::Str("a".to_string()),
            rhs: Value::Register(piece),
            consumes_lhs: false,
        });
        builder.push(Op::StrConcat {
            dest: doubled,
            lhs: Value::Register(made),
            rhs: Value::Register(made),
            consumes_lhs: false,
        });
        builder.terminate(Terminator::Return(Value::Register(doubled)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert!(!consumes(&module, 0, 1), "{:?}", module.functions[0].blocks);
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }
}

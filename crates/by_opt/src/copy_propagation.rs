//! copy propagation
//!
//! the frontend computes every expression into a fresh temporary and then copies
//! it into the destination, because it does not know the destination until after
//! the operands are lowered. so `x = a + b` arrives as
//!
//! ```text
//! r1 = a + b
//! x = r1
//! ```
//!
//! this pass folds the pair back into `x = a + b` when it is safe to do so. the
//! win is not the instruction — the C compiler coalesces those — it is the
//! *register*: each live register of a refcounted type costs an entry in the
//! function's cleanup path on every exit, and the ownership discipline retains
//! and releases through the copy.
//!
//! ## when it is safe
//!
//! only for a temporary that is written exactly once, read exactly once, and
//! read by a copy in the very next operation of the same block. anything looser
//! needs real liveness, and this pass is deliberately the cheap version.
//!
//! ## the other direction
//!
//! folding a redundant `box` leaves the mirror image — `r7 = b` followed by reads
//! of `r7`. that one is substituted rather than moved, and the safety condition is
//! different: the *source* must not change between the copy and any read. a block
//! is straight-line, so "in between" is decidable inside one block and nothing
//! else is attempted.

use std::collections::HashMap;

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{Op, RegisterId, Value};

pub fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        propagate(function);
    }
}

fn propagate(function: &mut Function) {
    substitute_copies(function);
    let counts = usage_counts(function);
    let param_count = function.param_count;
    // snapshot what the loop needs from the register table, so the blocks can be
    // borrowed mutably without also borrowing the function
    let register_types: Vec<by_ir::rtype::RType> = function
        .registers
        .iter()
        .map(|decl| decl.ty.clone())
        .collect();
    let is_anonymous: Vec<bool> = function
        .registers
        .iter()
        .map(|decl| decl.name.is_none())
        .collect();

    for block in &mut function.blocks {
        let mut index = 0;
        while index + 1 < block.ops.len() {
            let Some(temp) = block.ops[index].dest() else {
                index += 1;
                continue;
            };
            // a parameter is not a temporary, and a named local is a place the
            // printed IR and any future debugger will want to keep
            let is_anonymous_temp = temp.index() >= param_count
                && is_anonymous.get(temp.index()).copied() == Some(true);
            if !is_anonymous_temp {
                index += 1;
                continue;
            }
            let Some(&(writes, reads)) = counts.get(&temp) else {
                index += 1;
                continue;
            };
            if writes != 1 || reads != 1 {
                index += 1;
                continue;
            }

            // the next operation must be exactly `dest = temp`
            let Op::Assign {
                dest,
                src: Value::Register(src),
            } = &block.ops[index + 1]
            else {
                index += 1;
                continue;
            };
            if *src != temp {
                index += 1;
                continue;
            }
            let dest = *dest;
            // the two registers must agree on representation, which they do by
            // construction, but the verifier is the only thing that guarantees it
            if register_types.get(dest.index()) != register_types.get(temp.index()) {
                index += 1;
                continue;
            }
            // writing into a register that the producing op also reads would
            // change what that op sees
            if block.ops[index]
                .operands()
                .iter()
                .any(|operand| matches!(operand, Value::Register(id) if *id == dest))
            {
                index += 1;
                continue;
            }

            retarget(&mut block.ops[index], dest);
            block.ops.remove(index + 1);
            index += 1;
        }
    }
}

/// `(writes, reads)` for every register in the function
/// replace a temporary that is only a copy of another register
///
/// safe when the copy and every read sit in one block, the reads all follow the
/// copy, and nothing between them writes the source — a block is straight-line, so
/// that is the whole question. the retain/release pair around the copy goes with it
fn substitute_copies(function: &mut Function) {
    let counts = usage_counts(function);
    let mut substitutions: HashMap<RegisterId, RegisterId> = HashMap::new();

    for block in &function.blocks {
        for (at, op) in block.ops.iter().enumerate() {
            let Op::Assign {
                dest,
                src: Value::Register(source),
            } = op
            else {
                continue;
            };
            let is_temp = dest.index() >= function.param_count
                && function
                    .register(*dest)
                    .is_some_and(|decl| decl.name.is_none())
                && counts.get(dest).is_some_and(|&(writes, _)| writes == 1);
            if !is_temp || dest == source {
                continue;
            }
            // the two must agree on representation. an `assign` that crosses one —
            // a `str` into an `object`, say — is a widening, and substituting the
            // source back hands its *reader* a representation it did not ask for
            if function.value_type(&Value::Register(*dest))
                != function.value_type(&Value::Register(*source))
            {
                continue;
            }

            // every read must be in this block, after the copy
            let reads_here = block.ops[at + 1..]
                .iter()
                .enumerate()
                .filter(|(_, later)| reads(&later.operands(), *dest))
                .map(|(offset, _)| at + 1 + offset)
                .collect::<Vec<_>>();
            let read_in_terminator = reads(&block.terminator.operands(), *dest);
            let total_reads = reads_here.len() + usize::from(read_in_terminator);
            if counts.get(dest).map(|&(_, all)| all) != Some(total_reads) {
                continue;
            }
            if total_reads == 0 {
                continue;
            }

            // and nothing up to the last read may write the source
            let last = if read_in_terminator {
                block.ops.len()
            } else {
                reads_here.last().copied().unwrap_or(at) + 1
            };
            if block.ops[at + 1..last]
                .iter()
                .any(|later| later.dest() == Some(*source))
            {
                continue;
            }
            substitutions.insert(*dest, *source);
        }
    }
    if substitutions.is_empty() {
        return;
    }

    for block in &mut function.blocks {
        block.ops.retain(|op| match op {
            Op::Assign { dest, .. } => !substitutions.contains_key(dest),
            _ => true,
        });
        for op in &mut block.ops {
            for operand in op.operands_mut() {
                if let Value::Register(id) = operand
                    && let Some(replacement) = substitutions.get(&*id)
                {
                    *id = *replacement;
                }
            }
        }
        for operand in block.terminator.operands_mut() {
            if let Value::Register(id) = operand
                && let Some(replacement) = substitutions.get(&*id)
            {
                *id = *replacement;
            }
        }
    }
}

fn reads(operands: &[&Value], register: RegisterId) -> bool {
    operands
        .iter()
        .any(|operand| matches!(operand, Value::Register(id) if *id == register))
}

fn usage_counts(function: &Function) -> HashMap<RegisterId, (usize, usize)> {
    let mut counts: HashMap<RegisterId, (usize, usize)> = HashMap::new();
    for block in &function.blocks {
        for op in &block.ops {
            if let Some(dest) = op.dest() {
                counts.entry(dest).or_default().0 += 1;
            }
            for operand in op.operands() {
                if let Value::Register(id) = operand {
                    counts.entry(*id).or_default().1 += 1;
                }
            }
        }
        for operand in block.terminator.operands() {
            if let Value::Register(id) = operand {
                counts.entry(*id).or_default().1 += 1;
            }
        }
    }
    counts
}

/// point an operation's destination at a different register
fn retarget(op: &mut Op, new_dest: RegisterId) {
    match op {
        Op::Assign { dest, .. }
        | Op::IntBinary { dest, .. }
        | Op::FloatBinary { dest, .. }
        | Op::FloatObjectBinary { dest, .. }
        | Op::FloatObjectCompare { dest, .. }
        | Op::Identity { dest, .. }
        | Op::Contains { dest, .. }
        | Op::IsInstance { dest, .. }
        | Op::IsSequence { dest, .. }
        | Op::MatchAttr { dest, .. }
        | Op::MatchKey { dest, .. }
        | Op::MatchRest { dest, .. }
        | Op::IsMapping { dest, .. }
        | Op::AsyncIter { dest, .. }
        | Op::AsyncContext { dest, .. }
        | Op::IsMissing { dest, .. }
        | Op::MatchSlice { dest, .. }
        | Op::IntCompare { dest, .. }
        | Op::FloatCompare { dest, .. }
        | Op::ObjectBinary { dest, .. }
        | Op::ObjectCompare { dest, .. }
        | Op::StrCompare { dest, .. }
        | Op::Truthy { dest, .. }
        | Op::Len { dest, .. }
        | Op::CallPython { dest, .. }
        | Op::CallValue { dest, .. }
        | Op::LoadGlobal { dest, .. }
        | Op::LoadClass { dest, .. }
        | Op::ImportModule { dest, .. }
        | Op::ImportFrom { dest, .. }
        | Op::NewInstance { dest, .. }
        | Op::GetCell { dest, .. }
        | Op::Enter { dest, .. }
        | Op::ExitContext { dest, .. }
        | Op::DelegateIter { dest, .. }
        | Op::DelegateStep { dest, .. }
        | Op::MakeClosure { dest, .. }
        | Op::GetIter { dest, .. }
        | Op::IterNext { dest, .. }
        | Op::IsNull { dest, .. }
        | Op::CallMethod { dest, .. }
        | Op::GetAttr { dest, .. }
        | Op::GetField { dest, .. }
        | Op::SetAttr { dest, .. }
        | Op::BuildList { dest, .. }
        | Op::BuildSet { dest, .. }
        | Op::BuildTuple { dest, .. }
        | Op::BuildDict { dest, .. }
        | Op::GetItem { dest, .. }
        | Op::StrGetItem { dest, .. }
        | Op::StrItemCompare { dest, .. }
        | Op::SetItem { dest, .. }
        | Op::Format { dest, .. }
        | Op::FetchException { dest }
        | Op::ExceptionMatches { dest, .. }
        | Op::PushHandled { dest, .. }
        | Op::Unpack { dest, .. }
        | Op::ToTuple { dest, .. }
        | Op::ArrayNew { dest, .. }
        | Op::ArrayGet { dest, .. }
        | Op::ArraySet { dest, .. }
        | Op::ArrayLen { dest, .. }
        | Op::ArrayRead { dest, .. }
        | Op::DeleteItem { dest, .. }
        | Op::DeleteAttr { dest, .. }
        | Op::ArrayPush { dest, .. }
        | Op::Extend { dest, .. }
        | Op::CallUnpacked { dest, .. }
        | Op::StrConcat { dest, .. }
        | Op::Unary { dest, .. }
        | Op::Box { dest, .. }
        | Op::IntToFloat { dest, .. }
        | Op::Unbox { dest, .. }
        | Op::TupleBuild { dest, .. }
        | Op::TupleGet { dest, .. } => *dest = new_dest,
        Op::CallNative { dest, .. } => *dest = Some(new_dest),
        Op::RaiseStandard { .. }
        | Op::RaiseWith { .. }
        | Op::RaiseObject { .. }
        | Op::PopHandled { .. }
        | Op::Reraise { .. }
        | Op::SetField { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{BinOp, Terminator};
    use by_ir::print::print_function;
    use by_ir::rtype::RType;
    use by_ir::verify::verify;

    fn module(function: Function) -> ModuleIr {
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

    /// the shape the frontend produces for `x = a + b`
    fn compute_then_copy() -> Function {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let temp = builder.temp(RType::INT);
        let x = builder.local("x", RType::INT);
        builder.push(Op::IntBinary {
            dest: temp,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Int(1),
        });
        builder.assign(x, Value::Register(temp));
        builder.terminate(Terminator::Return(Value::Register(x)));
        builder.finish()
    }

    #[test]
    fn a_compute_then_copy_pair_becomes_one_operation() {
        let mut m = module(compute_then_copy());
        run(&mut m);
        let text = print_function(&m.functions[0]);
        assert!(text.contains("x = a + 1"), "{text}");
        assert_eq!(m.functions[0].blocks[0].ops.len(), 1, "{text}");
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn a_temporary_read_twice_is_left_alone() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let temp = builder.temp(RType::INT);
        let x = builder.local("x", RType::INT);
        builder.push(Op::IntBinary {
            dest: temp,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Int(1),
        });
        builder.assign(x, Value::Register(temp));
        // the second read makes the copy load-bearing
        builder.push(Op::IntBinary {
            dest: x,
            op: BinOp::Add,
            lhs: Value::Register(x),
            rhs: Value::Register(temp),
        });
        builder.terminate(Terminator::Return(Value::Register(x)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert_eq!(m.functions[0].blocks[0].ops.len(), 3);
    }

    #[test]
    fn a_named_local_is_not_folded_away() {
        // names carry into the printed IR and, later, into a debugger
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let named = builder.local("mid", RType::INT);
        let x = builder.local("x", RType::INT);
        builder.push(Op::IntBinary {
            dest: named,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Int(1),
        });
        builder.assign(x, Value::Register(named));
        builder.terminate(Terminator::Return(Value::Register(x)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert_eq!(m.functions[0].blocks[0].ops.len(), 2);
    }

    #[test]
    fn a_parameter_is_never_folded_away() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let x = builder.local("x", RType::INT);
        builder.assign(x, Value::Register(a));
        builder.terminate(Terminator::Return(Value::Register(x)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert_eq!(m.functions[0].blocks[0].ops.len(), 1);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn a_destination_the_producer_also_reads_is_left_alone() {
        // retargeting `r1 = x + 1; x = r1` onto `x = x + 1` happens to be fine
        // here, but the pass must not assume that in general — a producer that
        // reads its own new destination would see a different value
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let x = builder.local("x", RType::INT);
        let temp = builder.temp(RType::INT);
        builder.assign(x, Value::Register(a));
        builder.push(Op::IntBinary {
            dest: temp,
            op: BinOp::Add,
            lhs: Value::Register(x),
            rhs: Value::Int(1),
        });
        builder.assign(x, Value::Register(temp));
        builder.terminate(Terminator::Return(Value::Register(x)));
        let mut m = module(builder.finish());
        let before = m.functions[0].blocks[0].ops.len();
        run(&mut m);
        assert_eq!(m.functions[0].blocks[0].ops.len(), before);
    }

    #[test]
    fn a_copy_that_is_not_the_next_operation_is_left_alone() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let temp = builder.temp(RType::INT);
        let other = builder.local("other", RType::INT);
        let x = builder.local("x", RType::INT);
        builder.push(Op::IntBinary {
            dest: temp,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Int(1),
        });
        builder.assign(other, Value::Int(9));
        builder.assign(x, Value::Register(temp));
        builder.terminate(Terminator::Return(Value::Register(x)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert_eq!(m.functions[0].blocks[0].ops.len(), 3);
    }

    #[test]
    fn a_call_result_can_be_folded_into_its_destination() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let temp = builder.temp(RType::INT);
        let x = builder.local("x", RType::INT);
        builder.push(Op::CallNative {
            owner: None,
            dest: Some(temp),
            callee: "g".to_string(),
            args: vec![Value::Register(a)],
        });
        builder.assign(x, Value::Register(temp));
        builder.terminate(Terminator::Return(Value::Register(x)));
        let mut m = module(builder.finish());
        run(&mut m);
        let text = print_function(&m.functions[0]);
        assert!(text.contains("x = call g(a)"), "{text}");
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn the_result_still_verifies_on_every_shape() {
        for mut m in [
            module(compute_then_copy()),
            module({
                let mut builder = FunctionBuilder::new("chain", RType::INT);
                let a = builder.param("a", RType::INT);
                let t1 = builder.temp(RType::INT);
                let t2 = builder.temp(RType::INT);
                let x = builder.local("x", RType::INT);
                builder.push(Op::IntBinary {
                    dest: t1,
                    op: BinOp::Add,
                    lhs: Value::Register(a),
                    rhs: Value::Int(1),
                });
                builder.push(Op::IntBinary {
                    dest: t2,
                    op: BinOp::Mul,
                    lhs: Value::Register(t1),
                    rhs: Value::Int(2),
                });
                builder.assign(x, Value::Register(t2));
                builder.terminate(Terminator::Return(Value::Register(x)));
                builder.finish()
            }),
        ] {
            run(&mut m);
            assert_eq!(
                verify(&m.functions[0]),
                Ok(()),
                "{}",
                print_function(&m.functions[0])
            );
        }
    }

    #[test]
    fn a_temp_that_only_copies_a_register_is_substituted_away() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let copy = builder.temp(RType::STR);
        let out = builder.temp(RType::INT);
        builder.assign(copy, Value::Register(text));
        builder.push(Op::Len {
            dest: out,
            src: Value::Register(copy),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        run(&mut m);
        let text_out = print_function(&m.functions[0]);
        assert!(text_out.contains("= len s"), "{text_out}");
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn a_copy_whose_source_is_rewritten_before_the_read_is_kept() {
        // the whole safety condition: `s` changes in between, so the temp is not
        // the same value the read would get
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let source = builder.local("s", RType::STR);
        let copy = builder.temp(RType::STR);
        let out = builder.temp(RType::INT);
        builder.assign(source, Value::Str("a".to_string()));
        builder.assign(copy, Value::Register(source));
        builder.assign(source, Value::Str("bb".to_string()));
        builder.push(Op::Len {
            dest: out,
            src: Value::Register(copy),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        run(&mut m);
        let printed = print_function(&m.functions[0]);
        assert!(printed.contains("= len r1"), "{printed}");
    }

    #[test]
    fn a_copy_read_in_another_block_is_kept() {
        // the safety argument is one block of straight-line code wide
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let text = builder.param("s", RType::STR);
        let copy = builder.temp(RType::STR);
        let next = builder.new_block();
        builder.assign(copy, Value::Register(text));
        builder.terminate(Terminator::Goto(next));
        builder.switch_to(next);
        builder.terminate(Terminator::Return(Value::Register(copy)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(matches!(m.functions[0].blocks[0].ops[0], Op::Assign { .. }));
    }

    #[test]
    fn a_copy_read_by_the_terminator_is_still_substituted() {
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let text = builder.param("s", RType::STR);
        let copy = builder.temp(RType::STR);
        builder.assign(copy, Value::Register(text));
        builder.terminate(Terminator::Return(Value::Register(copy)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert_eq!(
            m.functions[0].blocks[0].terminator,
            Terminator::Return(Value::Register(text))
        );
        assert!(m.functions[0].blocks[0].ops.is_empty());
    }
}

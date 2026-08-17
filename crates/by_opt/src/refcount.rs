//! narrow the set of registers an exit path has to release
//!
//! codegen's own discipline is correct but wasteful: every refcounted register is
//! released on every exit path, so a function with four `return`s pays four
//! releases for each — including for registers that cannot possibly hold anything
//! yet at that point.
//!
//! ## the analysis is forward, not backward
//!
//! the tempting one is liveness, and it is the wrong question. liveness says where
//! a value is still *needed*; releasing asks where a register may still *hold a
//! reference*. a register assigned before an early `return` and never read again is
//! dead by liveness and absolutely must still be released.
//!
//! so this is a forward "may have been written" analysis: the release set at an
//! exit is every owned register some path from entry could have written. that is
//! strictly smaller than "all of them" whenever a register is first written after
//! the exit, which is the common shape of a guard clause.
//!
//! the conservative answer — every refcounted register — is what a `None` result
//! produces in the emitter, so a bug here degrades to extra work, never to a leak.
//!
//! ## the exception edge is an edge
//!
//! following terminators alone never visits a handler block, so its release set came
//! out as "nothing written yet" and everything the `try` body had written leaked on
//! the exceptional path. propagating the *outgoing* set across the error edge is the
//! safe direction here: more registers means more releases, never fewer.

use std::collections::{BTreeSet, HashSet};

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{BlockId, RegisterId};

pub fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        let sets = release_sets(function);
        for (block, set) in function.blocks.iter_mut().zip(sets) {
            block.owned_at_exit = Some(set.into_iter().collect());
        }
    }
}

/// for each block, the registers an exit in it must release
fn release_sets(function: &Function) -> Vec<BTreeSet<RegisterId>> {
    let count = function.blocks.len();
    // written *before* the block runs
    let mut incoming: Vec<HashSet<RegisterId>> = vec![HashSet::new(); count];

    // a parameter holds a value from the first instruction, so it is written on
    // entry as far as this analysis is concerned. whether the *frame* owns it is
    // a separate question the emitter answers
    let entry: HashSet<RegisterId> = (0..function.param_count).map(RegisterId).collect();
    if let Some(slot) = incoming.first_mut() {
        *slot = entry;
    }

    let writes = |block: &by_ir::function::BasicBlock| -> HashSet<RegisterId> {
        block.ops.iter().filter_map(by_ir::ops::Op::dest).collect()
    };

    let mut changed = true;
    while changed {
        changed = false;
        for index in 0..count {
            let Some(block) = function.block(BlockId(index)) else {
                continue;
            };
            let mut outgoing = incoming[index].clone();
            outgoing.extend(writes(block));
            for successor in block.successors() {
                let Some(slot) = incoming.get_mut(successor.index()) else {
                    continue;
                };
                let before = slot.len();
                slot.extend(outgoing.iter().copied());
                if slot.len() != before {
                    changed = true;
                }
            }
        }
    }

    (0..count)
        .map(|index| {
            let mut set: BTreeSet<RegisterId> = incoming[index].iter().copied().collect();
            if let Some(block) = function.block(BlockId(index)) {
                set.extend(writes(block));
            }
            set
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{BinOp, Op, Terminator, Value};
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
        }
    }

    #[test]
    fn a_register_written_before_an_exit_is_still_released_there() {
        // the bug this pass nearly shipped: `scratch` is dead by liveness at the
        // early return, and releasing it there is mandatory anyway
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let cond = builder.param("c", RType::BIT);
        let scratch = builder.temp(RType::STR);
        let early = builder.new_block();
        let late = builder.new_block();
        builder.assign(scratch, Value::Str("x".to_string()));
        builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block: early,
            else_block: late,
        });
        builder.switch_to(early);
        builder.terminate(Terminator::Return(Value::Int(1)));
        builder.switch_to(late);
        builder.terminate(Terminator::Return(Value::Int(0)));

        let mut m = module(builder.finish());
        run(&mut m);
        for index in [1, 2] {
            let owned = m.functions[0].blocks[index]
                .owned_at_exit
                .clone()
                .expect("the pass ran");
            assert!(
                owned.contains(&scratch),
                "block {index} must still release it: {owned:?}"
            );
        }
    }

    #[test]
    fn a_register_first_written_after_an_exit_is_not_released_there() {
        // the reduction this pass exists for: a guard clause cannot be holding
        // something the code below it has not created yet
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let cond = builder.param("c", RType::BIT);
        let later = builder.temp(RType::STR);
        let early = builder.new_block();
        let rest = builder.new_block();
        builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block: early,
            else_block: rest,
        });
        builder.switch_to(early);
        builder.terminate(Terminator::Return(Value::Int(1)));
        builder.switch_to(rest);
        builder.assign(later, Value::Str("x".to_string()));
        builder.terminate(Terminator::Return(Value::Int(0)));

        let mut m = module(builder.finish());
        run(&mut m);
        let guard = m.functions[0].blocks[1]
            .owned_at_exit
            .clone()
            .expect("the pass ran");
        assert!(!guard.contains(&later), "{guard:?}");
        let body = m.functions[0].blocks[2]
            .owned_at_exit
            .clone()
            .expect("the pass ran");
        assert!(body.contains(&later), "{body:?}");
    }

    #[test]
    fn a_parameter_counts_as_written_on_entry() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        builder.terminate(Terminator::Return(Value::Register(a)));
        let mut m = module(builder.finish());
        run(&mut m);
        let owned = m.functions[0].blocks[0]
            .owned_at_exit
            .clone()
            .expect("the pass ran");
        assert!(owned.contains(&a), "{owned:?}");
    }

    #[test]
    fn a_loop_carried_register_is_released_at_every_exit_in_the_loop() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let limit = builder.param("n", RType::INT);
        let index = builder.local("i", RType::INT);
        let cond = builder.temp(RType::BIT);
        let header = builder.new_block();
        let body = builder.new_block();
        let exit = builder.new_block();
        builder.assign(index, Value::Int(0));
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(header);
        builder.push(Op::IntCompare {
            dest: cond,
            op: by_ir::ops::CmpOp::Lt,
            lhs: Value::Register(index),
            rhs: Value::Register(limit),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block: body,
            else_block: exit,
        });
        builder.switch_to(body);
        builder.push(Op::IntBinary {
            dest: index,
            op: BinOp::Add,
            lhs: Value::Register(index),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(exit);
        builder.terminate(Terminator::Return(Value::Register(index)));

        let mut m = module(builder.finish());
        run(&mut m);
        for block in 0..4 {
            let owned = m.functions[0].blocks[block]
                .owned_at_exit
                .clone()
                .expect("the pass ran");
            assert!(owned.contains(&index), "block {block}: {owned:?}");
        }
    }

    #[test]
    fn the_pass_leaves_the_ir_verifiable() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let sum = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: sum,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Return(Value::Register(sum)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn a_handler_releases_what_the_try_body_wrote() {
        // an exception edge is a CFG edge. following terminators alone never visits
        // the handler, so its release set came out empty and everything the body had
        // written leaked on the exceptional path
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let held = builder.local("held", RType::STR);
        let handler = builder.new_block();
        let done = builder.new_block();
        builder.assign(held, Value::Str("x".to_string()));
        builder.set_error_target(Some(handler));
        let scratch = builder.temp(RType::OBJECT);
        builder.push(Op::CallPython {
            dest: scratch,
            callee: "boom".to_string(),
            args: Vec::new(),
        });
        builder.terminate(Terminator::Goto(done));
        builder.switch_to(handler);
        builder.terminate(Terminator::Return(Value::Int(1)));
        builder.switch_to(done);
        builder.terminate(Terminator::Return(Value::Int(0)));

        let mut m = module(builder.finish());
        run(&mut m);
        let owned = m.functions[0].blocks[handler.index()]
            .owned_at_exit
            .clone()
            .expect("the pass ran");
        assert!(
            owned.contains(&held),
            "the handler must release it: {owned:?}"
        );
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }
}

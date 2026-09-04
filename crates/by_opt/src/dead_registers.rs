//! dead register elimination
//!
//! copy propagation leaves behind temporaries nothing reads any more. each one
//! still costs a C local *and* — for a refcounted representation — an entry in
//! the cleanup emitted on every exit path, so a function with three exits pays
//! for a dead register three times.
//!
//! parameters are never removed: they are the leading registers and they are the
//! signature.

use std::collections::HashSet;

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{RegisterId, Value};

pub(crate) fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        eliminate(function);
    }
}

fn eliminate(function: &mut Function) {
    let live = live_registers(function);
    if live.len() == function.registers.len() {
        return;
    }

    // renumber the survivors, keeping parameters at the front and in order
    let mut remap = Vec::with_capacity(function.registers.len());
    let mut kept = Vec::with_capacity(live.len());
    for (index, decl) in function.registers.iter().enumerate() {
        if index < function.param_count || live.contains(&RegisterId(index)) {
            remap.push(Some(RegisterId(kept.len())));
            kept.push(decl.clone());
        } else {
            remap.push(None);
        }
    }
    function.registers = kept;

    // the rewrite asks the op what it reads and writes rather than listing the
    // variants itself. it did list them, and the list drifted: `MatchAttr`'s `class`
    // operand was swallowed by a `..`, so `case str(slot)` kept a *stale* register id
    // through the renumbering — which landed on whichever register had taken that
    // number, and the generated C passed a `char` where a `PyObject *` belonged.
    // `Terminator::NarrowShort` was missing for the same reason. both are counted as
    // live by the scan below, which reads the same accessors, so only the rewrite
    // disagreed — and a second list that has to agree with the first is how that
    // happened
    let rewrite_register = |id: &mut RegisterId| {
        if let Some(Some(new)) = remap.get(id.index()) {
            *id = *new;
        }
    };
    let rewrite_value = |value: &mut Value| {
        if let Value::Register(id) = value {
            rewrite_register(id);
        }
    };

    for block in &mut function.blocks {
        for op in &mut block.ops {
            if let Some(dest) = op.dest_mut() {
                rewrite_register(dest);
            }
            // a loop cursor is read and written in place, so it is neither a dest nor
            // an operand
            if let Some(cursor) = op.loop_cursor_mut() {
                rewrite_register(cursor);
            }
            for operand in op.operands_mut() {
                rewrite_value(operand);
            }
        }
        for operand in block.terminator.operands_mut() {
            rewrite_value(operand);
        }
    }
}

/// every register read or written anywhere in the function
fn live_registers(function: &Function) -> HashSet<RegisterId> {
    let mut live = HashSet::new();
    for block in &function.blocks {
        for op in &block.ops {
            if let Some(dest) = op.dest() {
                live.insert(dest);
            }
            // a loop cursor is read and written in place, so it is neither a dest nor
            // an operand — and nothing else here would keep it alive
            if let Some(cursor) = op.loop_cursor() {
                live.insert(cursor);
            }
            for operand in op.operands() {
                if let Value::Register(id) = operand {
                    live.insert(*id);
                }
            }
        }
        for operand in block.terminator.operands() {
            if let Value::Register(id) = operand {
                live.insert(*id);
            }
        }
    }
    live
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{BinOp, Op, Terminator};
    use by_ir::print::print_function;
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

    #[test]
    fn a_register_nothing_touches_is_removed() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let _dead = builder.temp(RType::INT);
        let live = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: live,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Return(Value::Register(live)));

        let mut m = module(builder.finish());
        assert_eq!(m.functions[0].registers.len(), 3);
        run(&mut m);
        assert_eq!(m.functions[0].registers.len(), 2);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn the_survivors_are_renumbered_consistently() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let _dead = builder.temp(RType::INT);
        let live = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: live,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Return(Value::Register(live)));
        let mut m = module(builder.finish());
        run(&mut m);
        // r2 became r1, and both the write and the read followed it
        let text = print_function(&m.functions[0]);
        assert!(text.contains("r1 = a + 1"), "{text}");
        assert!(text.contains("return r1"), "{text}");
        assert!(!text.contains("r2"), "{text}");
    }

    #[test]
    fn an_unused_parameter_is_kept() {
        // removing it would change the signature the wrapper and every caller use
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let _unused = builder.param("b", RType::INT);
        builder.terminate(Terminator::Return(Value::Register(a)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert_eq!(m.functions[0].param_count, 2);
        assert_eq!(m.functions[0].registers.len(), 2);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn a_function_with_nothing_dead_is_left_untouched() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let out = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: out,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        let before = m.functions[0].clone();
        run(&mut m);
        assert_eq!(m.functions[0], before);
    }

    #[test]
    fn registers_used_only_in_a_branch_condition_stay_live() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let cond = builder.temp(RType::BIT);
        let then_block = builder.new_block();
        let else_block = builder.new_block();
        builder.push(Op::IntCompare {
            dest: cond,
            op: by_ir::ops::CmpOp::Lt,
            lhs: Value::Register(a),
            rhs: Value::Int(0),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block,
            else_block,
        });
        builder.switch_to(then_block);
        builder.terminate(Terminator::Return(Value::Int(0)));
        builder.switch_to(else_block);
        builder.terminate(Terminator::Return(Value::Int(1)));

        let mut m = module(builder.finish());
        run(&mut m);
        assert_eq!(m.functions[0].registers.len(), 2);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn every_operand_an_op_reads_is_renumbered_and_not_only_the_first() {
        // `case str(slot)` is where this was found. the rewrite used to list the
        // variants itself and read `MatchAttr` as though `subject` were its only
        // operand, so `class` kept an id that now named a different register — a `bit`,
        // which reached the C compiler as a `char` passed where a `PyObject *` belonged
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let subject = builder.param("v", RType::OBJECT);
        let _dead = builder.temp(RType::OBJECT);
        let class = builder.temp(RType::OBJECT);
        let out = builder.temp(RType::OBJECT);
        builder.push(Op::LoadGlobal {
            dest: class,
            name: "str".to_string(),
        });
        builder.push(Op::MatchAttr {
            dest: out,
            subject: Value::Register(subject),
            name: None,
            class: Some(Value::Register(class)),
            index: 0,
            count: 1,
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        run(&mut m);
        // the dead temporary went, so the class register moved down one — and the
        // operand reading it has to have moved with it
        let function = &m.functions[0];
        assert_eq!(verify(function), Ok(()));
        let Some(Op::MatchAttr {
            class: Some(Value::Register(read)),
            ..
        }) = function.blocks[0].ops.last()
        else {
            panic!("{}", print_function(function));
        };
        let Some(Op::LoadGlobal { dest, .. }) = function.blocks[0].ops.first() else {
            panic!("{}", print_function(function));
        };
        assert_eq!(read, dest);
    }
}

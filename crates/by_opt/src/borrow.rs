//! let a register borrow, where it provably outlives its own use
//!
//! codegen's discipline is that every register owns its value, so a field read
//! retains and the frame later releases. for an *intermediate* in a chained read
//! — the `n.inner` in `n.inner.label` — that pair is pure overhead: the value is
//! only ever used as the receiver of another field read, and it cannot go away in
//! between.
//!
//! ## why "in between" is the whole question
//!
//! a borrow is unsound the moment anything can drop the last owning reference to
//! the value. in cpython there are two ways for that to happen and the second is
//! easy to miss:
//!
//! - **any allocation can trigger a collection**, and a collection can run
//!   `__del__`, which is arbitrary python
//! - **releasing any reference can run `__del__`** for the same reason — so an
//!   assignment over a refcounted register is every bit as dangerous as a call
//!
//! so the window between the read and the last use has to be free of anything that
//! allocates, calls out, *or* overwrites a refcounted place.
//!
//! that sounds crippling and is not, because the shape this exists for has
//! *nothing* in between: two adjacent field reads.

use std::collections::HashMap;

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{Op, RegisterId, Value};

pub fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        borrow(function);
    }
}

fn borrow(function: &mut Function) {
    let candidates = candidates(function);
    for register in candidates {
        if let Some(decl) = function.registers.get_mut(register.index()) {
            decl.borrowed = true;
        }
    }
}

/// every register that may borrow rather than own
fn candidates(function: &Function) -> Vec<RegisterId> {
    // a parameter is already borrowed by the frame, and a named local outlives any
    // single statement — only a temporary is a candidate
    let mut writes: HashMap<RegisterId, usize> = HashMap::new();
    for block in &function.blocks {
        for op in &block.ops {
            if let Some(dest) = op.dest() {
                *writes.entry(dest).or_default() += 1;
            }
        }
    }

    let mut candidates = Vec::new();
    for block in &function.blocks {
        for (index, op) in block.ops.iter().enumerate() {
            let Op::GetField { dest, .. } = op else {
                continue;
            };
            // written once, unnamed, and refcounted — otherwise there is either
            // nothing to save or no single window to reason about
            if writes.get(dest) != Some(&1) {
                continue;
            }
            let Some(decl) = function.register(*dest) else {
                continue;
            };
            if decl.name.is_some() || !decl.ty.is_refcounted() {
                continue;
            }
            if reads_outside(function, block, *dest) {
                continue;
            }
            if borrow_is_safe(function, block, index, *dest) {
                candidates.push(*dest);
            }
        }
    }
    candidates
}

/// whether every use of `register` sits inside a window nothing can invalidate
///
/// the window is the rest of the block the read is in. crossing a terminator
/// would mean reasoning about every path, and the shape worth optimizing does not
/// need it
fn borrow_is_safe(
    function: &Function,
    block: &by_ir::function::BasicBlock,
    read_at: usize,
    register: RegisterId,
) -> bool {
    // the terminator counts as a use: returning a borrowed value would hand out a
    // reference the frame does not own
    if block
        .terminator
        .operands()
        .iter()
        .any(|operand| reads(operand, register))
    {
        return false;
    }

    let mut used = false;
    for op in &block.ops[read_at + 1..] {
        let reads_it = op.operands().iter().any(|operand| reads(operand, register));
        if reads_it {
            // the only safe use is as the receiver of a *read*. a read takes the
            // field's value before releasing anything, so nothing can run in
            // between — where a `SetField` releases the old field value first, and
            // that `__del__` could free the very object being written through
            match op {
                Op::GetField { receiver, .. } if reads(receiver, register) => {}
                _ => return false,
            }
            used = true;
            continue;
        }
        // an unrelated op after the last use is harmless; one *before* it is only
        // harmless if it cannot drop the value
        if !is_inert(function, op) {
            // every use behind us means nothing later matters
            return used;
        }
    }
    // a register with no use at all is dead, and the dead-register pass owns that
    used
}

/// whether any block but this one reads `register`
///
/// the safety argument only covers one block's worth of straight-line code, so a
/// use anywhere else takes the borrow off the table
fn reads_outside(
    function: &Function,
    home: &by_ir::function::BasicBlock,
    register: RegisterId,
) -> bool {
    function
        .blocks
        .iter()
        .filter(|block| !std::ptr::eq(*block, home))
        .any(|block| {
            block
                .ops
                .iter()
                .flat_map(Op::operands)
                .chain(block.terminator.operands())
                .any(|operand| reads(operand, register))
        })
}

fn reads(operand: &Value, register: RegisterId) -> bool {
    matches!(operand, Value::Register(id) if *id == register)
}

/// whether an operation can neither allocate, call out, nor release a reference
///
/// anything not named here is assumed to be able to do all three. the ones that
/// *are* named still have to earn it: writing over a refcounted place releases
/// what was there, and that runs `__del__`
fn is_inert(function: &Function, op: &Op) -> bool {
    let plain = |register: &RegisterId| {
        function
            .register(*register)
            .is_some_and(|decl| !decl.ty.is_refcounted() || decl.borrowed)
    };
    match op {
        // pure double arithmetic touches no reference at all
        Op::FloatBinary { .. } | Op::FloatCompare { .. } => true,
        Op::Assign { dest, .. } | Op::TupleGet { dest, .. } | Op::GetField { dest, .. } => {
            plain(dest)
        }
        Op::SetField { value, .. } => function
            .value_type(value)
            .is_none_or(|ty| !ty.is_refcounted()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{BinOp, Terminator};
    use by_ir::rtype::RType;
    use by_ir::verify::verify;

    fn module(function: Function) -> ModuleIr {
        let mut module = ModuleIr::new("app");
        module.functions.push(function);
        module
    }

    fn nested() -> RType {
        RType::Instance {
            class: "Holder".to_string(),
            exact: false,
        }
    }

    /// `n.inner.label`
    fn chain() -> Function {
        let mut builder = FunctionBuilder::new("label_of", RType::STR);
        let outer = builder.param(
            "n",
            RType::Instance {
                class: "Nest".to_string(),
                exact: false,
            },
        );
        let inner = builder.temp(nested());
        let label = builder.temp(RType::STR);
        builder.push(Op::GetField {
            dest: inner,
            receiver: Value::Register(outer),
            class: "Nest".to_string(),
            field: "inner".to_string(),
        });
        builder.push(Op::GetField {
            dest: label,
            receiver: Value::Register(inner),
            class: "Holder".to_string(),
            field: "label".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(label)));
        builder.finish()
    }

    #[test]
    fn an_intermediate_in_a_chained_read_borrows() {
        let mut m = module(chain());
        run(&mut m);
        let function = &m.functions[0];
        assert!(
            function.registers[1].borrowed,
            "the intermediate must borrow"
        );
        // the value that leaves the function must still be owned
        assert!(!function.registers[2].borrowed);
        assert_eq!(verify(function), Ok(()));
    }

    #[test]
    fn a_returned_field_read_does_not_borrow() {
        let mut builder = FunctionBuilder::new("get", RType::STR);
        let holder = builder.param("h", nested());
        let label = builder.temp(RType::STR);
        builder.push(Op::GetField {
            dest: label,
            receiver: Value::Register(holder),
            class: "Holder".to_string(),
            field: "label".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(label)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[1].borrowed);
    }

    #[test]
    fn a_call_between_the_read_and_the_use_blocks_the_borrow() {
        // the call can run arbitrary python, which can reassign the field the
        // borrowed value came from
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let outer = builder.param("n", nested());
        let inner = builder.temp(nested());
        let noise = builder.temp(RType::OBJECT);
        let label = builder.temp(RType::STR);
        builder.push(Op::GetField {
            dest: inner,
            receiver: Value::Register(outer),
            class: "Nest".to_string(),
            field: "inner".to_string(),
        });
        builder.push(Op::CallPython {
            dest: noise,
            callee: "print".to_string(),
            args: Vec::new(),
        });
        builder.push(Op::GetField {
            dest: label,
            receiver: Value::Register(inner),
            class: "Holder".to_string(),
            field: "label".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(label)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[1].borrowed);
    }

    #[test]
    fn an_allocating_op_between_the_read_and_the_use_blocks_the_borrow() {
        // a box allocates, an allocation can collect, and a collection can run
        // `__del__` — which is arbitrary python
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let outer = builder.param("n", nested());
        let inner = builder.temp(nested());
        let boxed = builder.temp(RType::OBJECT);
        let label = builder.temp(RType::STR);
        builder.push(Op::GetField {
            dest: inner,
            receiver: Value::Register(outer),
            class: "Nest".to_string(),
            field: "inner".to_string(),
        });
        builder.push(Op::Box {
            dest: boxed,
            src: Value::Int(1),
        });
        builder.push(Op::GetField {
            dest: label,
            receiver: Value::Register(inner),
            class: "Holder".to_string(),
            field: "label".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(label)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[1].borrowed);
    }

    #[test]
    fn a_field_write_through_a_borrowed_receiver_blocks_the_borrow() {
        // the write releases the old field value first, and that `__del__` could
        // free the object being written through
        let mut builder = FunctionBuilder::new("f", RType::NONE);
        let outer = builder.param("n", nested());
        let inner = builder.temp(nested());
        builder.push(Op::GetField {
            dest: inner,
            receiver: Value::Register(outer),
            class: "Nest".to_string(),
            field: "inner".to_string(),
        });
        builder.push(Op::SetField {
            receiver: Value::Register(inner),
            class: "Holder".to_string(),
            field: "label".to_string(),
            value: Value::Str("x".to_string()),
        });
        builder.terminate(Terminator::Return(Value::None));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[1].borrowed);
    }

    #[test]
    fn an_overwrite_of_a_refcounted_register_blocks_the_borrow() {
        // releasing the old value runs `__del__`, which is arbitrary python
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let outer = builder.param("n", nested());
        let inner = builder.temp(nested());
        let other = builder.local("other", RType::STR);
        let label = builder.temp(RType::STR);
        builder.push(Op::GetField {
            dest: inner,
            receiver: Value::Register(outer),
            class: "Nest".to_string(),
            field: "inner".to_string(),
        });
        builder.assign(other, Value::Str("y".to_string()));
        builder.push(Op::GetField {
            dest: label,
            receiver: Value::Register(inner),
            class: "Holder".to_string(),
            field: "label".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(label)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[1].borrowed);
    }

    #[test]
    fn a_use_in_another_block_blocks_the_borrow() {
        // the safety argument is one block of straight-line code wide
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let outer = builder.param("n", nested());
        let inner = builder.temp(nested());
        let label = builder.temp(RType::STR);
        let next = builder.new_block();
        builder.push(Op::GetField {
            dest: inner,
            receiver: Value::Register(outer),
            class: "Nest".to_string(),
            field: "inner".to_string(),
        });
        builder.terminate(Terminator::Goto(next));
        builder.switch_to(next);
        builder.push(Op::GetField {
            dest: label,
            receiver: Value::Register(inner),
            class: "Holder".to_string(),
            field: "label".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(label)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[1].borrowed);
    }

    #[test]
    fn a_use_that_stores_the_value_blocks_the_borrow() {
        let mut builder = FunctionBuilder::new("f", RType::NONE);
        let outer = builder.param("n", nested());
        let inner = builder.temp(nested());
        builder.push(Op::GetField {
            dest: inner,
            receiver: Value::Register(outer),
            class: "Nest".to_string(),
            field: "inner".to_string(),
        });
        builder.push(Op::SetField {
            receiver: Value::Register(outer),
            class: "Nest".to_string(),
            field: "other".to_string(),
            value: Value::Register(inner),
        });
        builder.terminate(Terminator::Return(Value::None));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[1].borrowed);
    }

    #[test]
    fn an_unboxed_field_is_never_a_candidate() {
        // there is no reference to save
        let mut builder = FunctionBuilder::new("f", RType::FLOAT);
        let holder = builder.param("h", nested());
        let value = builder.temp(RType::FLOAT);
        let doubled = builder.temp(RType::FLOAT);
        builder.push(Op::GetField {
            dest: value,
            receiver: Value::Register(holder),
            class: "Holder".to_string(),
            field: "weight".to_string(),
        });
        builder.push(Op::FloatBinary {
            dest: doubled,
            op: BinOp::Add,
            lhs: Value::Register(value),
            rhs: Value::Register(value),
        });
        builder.terminate(Terminator::Return(Value::Register(doubled)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[1].borrowed);
    }

    #[test]
    fn a_named_local_never_borrows() {
        // a name outlives the statement that wrote it, so the window is not the
        // rest of one block
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let outer = builder.param("n", nested());
        let inner = builder.local("inner", nested());
        let label = builder.temp(RType::STR);
        builder.push(Op::GetField {
            dest: inner,
            receiver: Value::Register(outer),
            class: "Nest".to_string(),
            field: "inner".to_string(),
        });
        builder.push(Op::GetField {
            dest: label,
            receiver: Value::Register(inner),
            class: "Holder".to_string(),
            field: "label".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(label)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[1].borrowed);
    }
}

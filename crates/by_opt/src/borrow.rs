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
//!
//! ## a copy is a different question, and an easier one
//!
//! the other shape here is a plain copy of one register into another, which the
//! frontend emits wherever an operand has to be widened: `len(line)` over a `str`
//! parameter is `r = line` and then `len r`, because a length is defined on
//! anything at all and the operand position says `object`. both registers are a
//! `PyObject *`, so the copy itself is free — but the retain and release around it
//! are not, and in a loop they are paid every trip. on a scan of a line that pair
//! is *forty per cent* of the running time, which is more than the character
//! comparison, the length and the loop counter put together.
//!
//! what makes this the easier case is that the copy has a second register holding
//! the very same value. the window does not have to be free of allocation or of
//! calls, because it is not the copy's own reference keeping the value alive — it
//! is the source's, and the source goes on owning until something writes over it.
//! so the condition is only that nothing writes the source before the copy's last
//! use, which is a question about one block's worth of straight-line code.

use std::collections::HashMap;

use by_ir::function::{BasicBlock, Function, ModuleIr};
use by_ir::ops::{Op, RegisterId, Value};

pub fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        borrow(function);
    }
}

fn borrow(function: &mut Function) {
    // the constants settle first, so that a copy — whose borrow rests on its source
    // still owning — can see that a register holding one owns nothing to lend
    mark(function, constants(function));
    mark(function, field_reads(function));
    // and the field reads before the copies, for the same reason
    mark(function, copies(function));
}

fn mark(function: &mut Function, registers: Vec<RegisterId>) {
    for register in registers {
        if let Some(decl) = function.registers.get_mut(register.index()) {
            decl.borrowed = true;
        }
    }
}

/// how many times each register is written, over the whole function
fn write_counts(function: &Function) -> HashMap<RegisterId, usize> {
    let mut writes: HashMap<RegisterId, usize> = HashMap::new();
    for block in &function.blocks {
        for op in &block.ops {
            if let Some(dest) = op.dest() {
                *writes.entry(dest).or_default() += 1;
            }
        }
    }
    writes
}

/// every register that only ever holds a literal, which may borrow rather than own
///
/// a string or bytes literal is a module static the emitter builds once at import and
/// never gives back, so unlike a copy there is no window to reason about at all: the
/// value cannot go away while the frame is running, whatever happens in between.
///
/// what this is worth is a loop. `if part.startswith("w")` reads the literal into a
/// register of its own every trip, and today that is a retain and a release per trip
/// on a value that was never going anywhere
fn constants(function: &Function) -> Vec<RegisterId> {
    let writes = write_counts(function);

    let mut candidates = Vec::new();
    for block in &function.blocks {
        for op in &block.ops {
            let Op::Assign {
                dest,
                src: Value::Str(_) | Value::Bytes(_),
            } = op
            else {
                continue;
            };
            // written once, unnamed, and refcounted — as for a copy, this is what
            // makes the literal the register's whole life
            if writes.get(dest) != Some(&1) {
                continue;
            }
            let Some(decl) = function.register(*dest) else {
                continue;
            };
            if decl.name.is_some() || !decl.ty.is_refcounted() {
                continue;
            }
            // a use in a terminator would hand a reference out of the frame
            if function.blocks.iter().any(|block| {
                block
                    .terminator
                    .operands()
                    .iter()
                    .any(|operand| reads(operand, *dest))
            }) {
                continue;
            }
            candidates.push(*dest);
        }
    }
    candidates
}

/// every intermediate field read that may borrow rather than own
fn field_reads(function: &Function) -> Vec<RegisterId> {
    // a parameter is already borrowed by the frame, and a named local outlives any
    // single statement — only a temporary is a candidate
    let writes = write_counts(function);

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

/// every copy of one register into another that may borrow rather than own
fn copies(function: &Function) -> Vec<RegisterId> {
    let writes = write_counts(function);

    let mut candidates = Vec::new();
    for block in &function.blocks {
        for (index, op) in block.ops.iter().enumerate() {
            let Op::Assign {
                dest,
                src: Value::Register(source),
            } = op
            else {
                continue;
            };
            // written once, unnamed, and refcounted — as for a field read, this is
            // what makes the copy the register's whole life
            if dest == source || writes.get(dest) != Some(&1) {
                continue;
            }
            let Some(decl) = function.register(*dest) else {
                continue;
            };
            if decl.name.is_some() || !decl.ty.is_refcounted() {
                continue;
            }
            // the whole argument is that the source goes on holding the value, so a
            // source that owns nothing itself has nothing to lend. a parameter does
            // qualify: the caller owns its argument for the length of the call
            if function
                .register(*source)
                .is_none_or(|decl| decl.borrowed || !decl.ty.is_refcounted())
            {
                continue;
            }
            if reads_outside(function, block, *dest) {
                continue;
            }
            if copy_borrow_is_safe(block, index, *dest, *source) {
                candidates.push(*dest);
            }
        }
    }
    candidates
}

/// whether the source still holds the value at every use of the copy
///
/// unlike a field read, the window may allocate and may call out — a `__del__` can
/// run in it and the value is still there, because the source is holding it. only
/// writing over the source ends that, so that is the only thing looked for.
///
/// a use *before* the copy is a use of whatever the previous trip round a loop left
/// in the register, which a borrow no longer keeps alive, and a use in the
/// terminator would hand a reference out of the frame — both take the borrow off
/// the table
fn copy_borrow_is_safe(
    block: &BasicBlock,
    copy_at: usize,
    register: RegisterId,
    source: RegisterId,
) -> bool {
    let uses = |op: &Op| op.operands().iter().any(|operand| reads(operand, register));
    if block
        .terminator
        .operands()
        .iter()
        .any(|operand| reads(operand, register))
    {
        return false;
    }
    if block.ops[..copy_at].iter().any(uses) {
        return false;
    }
    let tail = &block.ops[copy_at + 1..];
    // a copy with no use at all is dead, and the dead-register pass owns that
    let Some(last_use) = tail.iter().rposition(uses) else {
        return false;
    };
    !tail[..=last_use]
        .iter()
        .any(|op| op.dest() == Some(source) || op.unbinds() == Some(source))
}

/// whether every use of `register` sits inside a window nothing can invalidate
///
/// the window is the rest of the block the read is in. crossing a terminator
/// would mean reasoning about every path, and the shape worth optimizing does not
/// need it
fn borrow_is_safe(
    function: &Function,
    block: &BasicBlock,
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
fn reads_outside(function: &Function, home: &BasicBlock, register: RegisterId) -> bool {
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

    /// `len(line)` over a `str` parameter: the operand position says `object`, so
    /// the frontend widens the parameter into a temporary of its own
    ///
    /// `noisy` puts a call between the copy and the length, which is what says the
    /// window may contain arbitrary work
    fn widened_length(noisy: bool) -> Function {
        let mut builder = FunctionBuilder::new("size", RType::INT);
        let line = builder.param("line", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        let noise = builder.temp(RType::OBJECT);
        builder.assign(widened, Value::Register(line));
        if noisy {
            builder.push(Op::CallPython {
                dest: noise,
                callee: "print".to_string(),
                args: Vec::new(),
            });
        }
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        builder.finish()
    }

    #[test]
    fn a_copy_of_a_parameter_borrows() {
        let mut m = module(widened_length(false));
        run(&mut m);
        let function = &m.functions[0];
        assert!(function.registers[1].borrowed, "the copy must borrow");
        assert_eq!(verify(function), Ok(()));
    }

    #[test]
    fn a_call_between_a_copy_and_its_use_does_not_block_the_borrow() {
        // unlike a field read, the copy is not the only thing holding the value:
        // the parameter is, and the caller owns that for the length of the call.
        // so a `__del__` running in the window cannot take it away
        let mut m = module(widened_length(true));
        run(&mut m);
        assert!(m.functions[0].registers[1].borrowed);
    }

    #[test]
    fn a_copy_whose_source_is_rebound_before_the_use_does_not_borrow() {
        // the source is what holds the value, so writing over it drops the last
        // reference and leaves the copy pointing at freed memory
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let held = builder.local("held", RType::OBJECT);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        builder.assign(held, Value::Str("a".to_string()));
        builder.assign(widened, Value::Register(held));
        builder.assign(held, Value::Str("b".to_string()));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[widened.index()].borrowed);
    }

    #[test]
    fn a_returned_copy_does_not_borrow() {
        // the frame would be handing out a reference it never took
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let line = builder.param("line", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        builder.assign(widened, Value::Register(line));
        builder.terminate(Terminator::Return(Value::Register(widened)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[1].borrowed);
    }

    #[test]
    fn a_copy_read_before_itself_does_not_borrow() {
        // a loop header reading the register before writing it reads what the last
        // trip left there, which nothing is holding any more
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let line = builder.param("line", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let first = builder.temp(RType::INT);
        let second = builder.temp(RType::INT);
        let body = builder.new_block();
        builder.push(Op::Len {
            dest: first,
            src: Value::Register(widened),
        });
        builder.assign(widened, Value::Register(line));
        builder.push(Op::Len {
            dest: second,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Goto(body));
        builder.switch_to(body);
        builder.terminate(Terminator::Return(Value::Register(second)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[widened.index()].borrowed);
    }

    #[test]
    fn a_copy_used_in_another_block_does_not_borrow() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let line = builder.param("line", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        let body = builder.new_block();
        builder.assign(widened, Value::Register(line));
        builder.terminate(Terminator::Goto(body));
        builder.switch_to(body);
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[widened.index()].borrowed);
    }

    #[test]
    fn a_copy_of_a_borrowed_register_does_not_borrow() {
        // the source's own window ends at *its* last use, and the copy's uses come
        // after that — so a borrowed source is not proof of anything here.
        //
        // no field read reaches this today, because reading one as anything but the
        // receiver of another field read already takes its borrow away. the source is
        // marked by hand so the guard is tested for the borrow kinds to come rather
        // than only for the one that exists
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let holder = builder.param("h", nested());
        let inner = builder.temp(RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        builder.push(Op::GetField {
            dest: inner,
            receiver: Value::Register(holder),
            class: "Holder".to_string(),
            field: "label".to_string(),
        });
        builder.assign(widened, Value::Register(inner));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut function = builder.finish();
        assert_eq!(copies(&function), vec![widened]);

        function.registers[inner.index()].borrowed = true;
        assert!(copies(&function).is_empty());
    }

    #[test]
    fn a_named_destination_never_borrows_a_copy() {
        // a name outlives the statement that wrote it
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let line = builder.param("line", RType::STR);
        let held = builder.local("held", RType::OBJECT);
        let length = builder.temp(RType::INT);
        builder.assign(held, Value::Register(line));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(held),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[held.index()].borrowed);
    }

    #[test]
    fn a_register_holding_a_literal_borrows_it() {
        // there is no source register here at all, and none is wanted: the literal is
        // a module static the emitter never gives back, so nothing has to keep it
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        builder.assign(widened, Value::Str("abc".to_string()));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(m.functions[0].registers[widened.index()].borrowed);
    }

    #[test]
    fn a_returned_literal_does_not_borrow() {
        // as for a copy, the frame would be handing out a reference it never took
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let widened = builder.temp(RType::OBJECT);
        builder.assign(widened, Value::Str("abc".to_string()));
        builder.terminate(Terminator::Return(Value::Register(widened)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[widened.index()].borrowed);
    }

    #[test]
    fn a_register_that_holds_a_literal_only_some_of_the_time_does_not_borrow() {
        // the other write may leave something owned in it, and a register is one
        // discipline or the other for its whole life
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let line = builder.param("line", RType::STR);
        let held = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        builder.assign(held, Value::Str("abc".to_string()));
        builder.assign(held, Value::Register(line));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(held),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[held.index()].borrowed);
    }

    #[test]
    fn a_copy_of_a_register_holding_a_literal_does_not_borrow() {
        // the literal's own register has stopped owning, so it has nothing to lend —
        // which is why the literals are settled before the copies are looked at
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let held = builder.temp(RType::OBJECT);
        let copy = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        builder.assign(held, Value::Str("abc".to_string()));
        builder.assign(copy, Value::Register(held));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(copy),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(m.functions[0].registers[held.index()].borrowed);
        assert!(!m.functions[0].registers[copy.index()].borrowed);
    }
}

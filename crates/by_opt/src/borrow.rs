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
//!
//! ## an element read off a fixed-length tuple is the same question again
//!
//! `whole, part = split(i)` compiles to a call answering with a two-field struct and
//! two reads off it, and each read retains. the struct register owns both elements
//! until something writes it, so the argument is the copy's argument with the source
//! being a *place* the register owns rather than the register itself.
//!
//! ## why neither asks for a temporary written once
//!
//! the obvious way to say "this copy is the register's whole life" is to ask that the
//! register be written exactly once over the function and carry no name from the
//! source program. both stand in for something rather than saying it, and both are
//! wrong often enough to matter:
//!
//! - `unswitch` runs before this pass and emits a *second copy of every loop body*,
//!   reusing the same registers. so no register written inside a duplicated loop is
//!   ever written once, however plainly its copy dominates its uses. this is what was
//!   keeping a table subscripted three times a trip — the key widened into an operand
//!   of its own each time, in the hottest block of the program — paying a retain and
//!   a release per subscript
//! - `pair = Pair(i, i + 1)` in a loop body writes a register the source program
//!   named, and a name says nothing at all about whether the copy still owns
//!
//! so the property those two stood for is stated instead: every write of the register
//! lends from the same source, at most one per block, no block reads the register
//! without having written it first, no terminator reads it, and nothing writes over
//! the source between the write and the last read. the first three together are what
//! make each write's window its own block, which is the window the last one is stated
//! against.

use std::collections::{HashMap, HashSet};

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
    // and the field reads before the copies, for the same reason
    mark(function, field_reads(function));

    // the copies and the element reads both rest on their source going on owning, so
    // neither may lend to the other: a chain of two borrows has nothing owning at the
    // end of it. settling them together and dropping any candidate whose source is
    // also one keeps the answer from depending on which kind is looked at first
    let mut lending = copies(function);
    lending.extend(tuple_elements(function));
    let borrowing: HashSet<RegisterId> = lending.iter().map(|(register, _)| *register).collect();
    mark(
        function,
        lending
            .iter()
            .filter(|(_, source)| !borrowing.contains(source))
            .map(|(register, _)| *register)
            .collect(),
    );
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

/// every register filled only by copies of one other register, paired with the
/// register it would borrow from
fn copies(function: &Function) -> Vec<(RegisterId, RegisterId)> {
    lending_writes(function, |op| match op {
        Op::Assign {
            src: Value::Register(source),
            ..
        } => Some(*source),
        _ => None,
    })
}

/// every register filled only by element reads off a fixed-length tuple that goes on
/// owning what it holds, paired with the tuple register it would borrow from
fn tuple_elements(function: &Function) -> Vec<(RegisterId, RegisterId)> {
    lending_writes(function, |op| match op {
        Op::TupleGet {
            src: Value::Register(source),
            ..
        } => Some(*source),
        _ => None,
    })
}

/// every register whose every write lends from the same other register, paired with
/// that register
///
/// `lends` says which operations lend and what they lend from; anything else written
/// into the register takes it off the table
fn lending_writes(
    function: &Function,
    lends: fn(&Op) -> Option<RegisterId>,
) -> Vec<(RegisterId, RegisterId)> {
    let mut candidates = Vec::new();
    // a parameter is already borrowed by the frame, so there is nothing to save on it
    for index in function.param_count..function.registers.len() {
        let register = RegisterId(index);
        // a register with nothing to save is not worth an answer either way
        if function
            .register(register)
            .is_none_or(|decl| !decl.ty.is_refcounted())
        {
            continue;
        }
        if let Some(source) = lender(function, register, lends) {
            candidates.push((register, source));
        }
    }
    candidates
}

/// the register every write of `register` lends from, where the borrow holds
///
/// `None` where any write is something else, where a read reaches the register from
/// outside the block that wrote it, or where the source stops holding the value
/// before the last read of it
fn lender(
    function: &Function,
    register: RegisterId,
    lends: fn(&Op) -> Option<RegisterId>,
) -> Option<RegisterId> {
    let uses = |op: &Op| op.operands().iter().any(|operand| reads(operand, register));
    let mut held: Option<RegisterId> = None;
    let mut read_somewhere = false;

    for block in &function.blocks {
        // a use in a terminator would hand a reference out of the frame, or carry one
        // across an edge this argument does not follow
        if block
            .terminator
            .operands()
            .iter()
            .any(|operand| reads(operand, register))
        {
            return None;
        }
        let mut writes = block
            .ops
            .iter()
            .enumerate()
            .filter(|(_, op)| op.dest() == Some(register))
            .map(|(at, _)| at);
        let last_read = block.ops.iter().rposition(uses);
        let Some(write_at) = writes.next() else {
            // reading without writing reads what some earlier block left in the
            // register, which a borrow no longer keeps alive
            if last_read.is_some() {
                return None;
            }
            continue;
        };
        // two writes in one block would be two windows, and only one is reasoned
        // about below
        if writes.next().is_some() {
            return None;
        }
        let source = lends(&block.ops[write_at])?;
        // the whole argument is that the source goes on holding the value, so one
        // that owns nothing itself has nothing to lend. a parameter does qualify: the
        // caller owns its argument for the length of the call
        if source == register
            || function
                .register(source)
                .is_none_or(|decl| decl.borrowed || !decl.ty.is_refcounted())
        {
            return None;
        }
        // one register is one discipline for its whole life, so two writes lending
        // from different sources would still need one answer — and the window below
        // is stated against a single source
        if held.is_some_and(|already| already != source) {
            return None;
        }
        held = Some(source);

        let Some(last_read) = last_read else { continue };
        // a read before the write reads what the previous trip round the loop left
        // there, which nothing is holding any more
        if last_read < write_at || block.ops[..write_at].iter().any(uses) {
            return None;
        }
        read_somewhere = true;
        // and the source has to still hold the value, all the way to the last read
        if block.ops[write_at + 1..=last_read]
            .iter()
            .any(|op| op.dest() == Some(source) || op.unbinds() == Some(source))
        {
            return None;
        }
    }
    // a register with no use at all is dead, and the dead-register pass owns that
    read_somewhere.then_some(held).flatten()
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
        assert_eq!(copies(&function), vec![(widened, inner)]);

        function.registers[inner.index()].borrowed = true;
        assert!(copies(&function).is_empty());
    }

    #[test]
    fn a_named_destination_borrows_a_copy() {
        // `v = build()` in a loop body writes a register the source program named, and
        // the name says nothing about whether the copy still owns: the write is the
        // only one, and every read of it follows that write in the same block
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
        assert!(m.functions[0].registers[held.index()].borrowed);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn a_copy_read_in_its_block_and_returned_does_not_borrow() {
        // the read inside the block is exactly what a borrow would serve, so this is
        // the shape where the terminator is the only thing left to refuse on — the
        // returned-only copy above never gets that far, because a register nothing
        // reads is dead rather than borrowed
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let line = builder.param("line", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        builder.assign(widened, Value::Register(line));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(widened)));
        let function = builder.finish();
        assert!(copies(&function).is_empty());
    }

    #[test]
    fn a_copy_read_again_in_a_later_block_does_not_borrow() {
        // the later read takes what the writing block left in the register, and the
        // window the borrow is proved over ends with that block. a copy read *only*
        // in a later block is refused for being dead in its own, so this is the shape
        // that holds the condition up
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let line = builder.param("line", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let first = builder.temp(RType::INT);
        let second = builder.temp(RType::INT);
        let next = builder.new_block();
        builder.assign(widened, Value::Register(line));
        builder.push(Op::Len {
            dest: first,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Goto(next));
        builder.switch_to(next);
        builder.push(Op::Len {
            dest: second,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(second)));
        let function = builder.finish();
        assert!(copies(&function).is_empty());
    }

    #[test]
    fn a_deleted_local_does_not_borrow() {
        // `del held` releases what the register holds, and a borrow never took a
        // reference to give back. only a named local can be deleted, so this is a
        // refusal the copies could not need until they admitted one
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let line = builder.param("line", RType::STR);
        let held = builder.local("held", RType::OBJECT);
        let length = builder.temp(RType::INT);
        builder.assign(held, Value::Register(line));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(held),
        });
        builder.push(Op::DeleteLocal { dest: held });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[held.index()].borrowed);
    }

    #[test]
    fn a_copy_borrows_in_every_copy_of_a_loop_body() {
        // `unswitch` runs before this pass and emits a second copy of every loop body,
        // reusing the same registers — so a copy inside one is written twice however
        // plainly it dominates its own uses. each write's window is its own block, so
        // both of them lend
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let line = builder.param("line", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        let second = builder.new_block();
        builder.assign(widened, Value::Register(line));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Goto(second));
        builder.switch_to(second);
        builder.assign(widened, Value::Register(line));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(m.functions[0].registers[widened.index()].borrowed);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn two_copies_into_one_register_in_one_block_do_not_borrow() {
        // two writes in one block are two windows, and only the first one's is
        // reasoned about — so the register is left owning rather than half answered
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let line = builder.param("line", RType::STR);
        let other = builder.param("other", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        builder.assign(widened, Value::Register(line));
        builder.assign(widened, Value::Register(other));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let function = builder.finish();
        assert!(copies(&function).is_empty());
    }

    #[test]
    fn copies_of_two_different_registers_into_one_do_not_borrow() {
        // one register is one discipline for its whole life, and the two branches of
        // `x = a if c else b` would need two answers
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let line = builder.param("line", RType::STR);
        let other = builder.param("other", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        let second = builder.new_block();
        builder.assign(widened, Value::Register(line));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Goto(second));
        builder.switch_to(second);
        builder.assign(widened, Value::Register(other));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let function = builder.finish();
        assert!(copies(&function).is_empty());
    }

    #[test]
    fn a_register_written_by_anything_but_a_copy_does_not_borrow() {
        // a register the copy shares with a call's result owns what the call answered
        // with, and nothing else is holding that
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let line = builder.param("line", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        let second = builder.new_block();
        builder.assign(widened, Value::Register(line));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Goto(second));
        builder.switch_to(second);
        builder.push(Op::CallPython {
            dest: widened,
            callee: "build".to_string(),
            args: Vec::new(),
        });
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let function = builder.finish();
        assert!(copies(&function).is_empty());
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

    /// the layout `split(s) -> tuple[str, str]` answers with
    fn pair_type() -> RType {
        RType::Tuple(Box::new([RType::STR, RType::STR]))
    }

    fn call_pair(builder: &mut FunctionBuilder, dest: RegisterId, argument: RegisterId) {
        builder.push(Op::CallNative {
            owner: None,
            dest: Some(dest),
            callee: "split".to_string(),
            args: vec![Value::Register(argument)],
        });
    }

    #[test]
    fn an_element_read_off_a_tuple_borrows() {
        // the tuple register owns both elements, so reading one is a copy of a place
        // it holds — and the destination is a name, which is what `head, tail = ...`
        // always produces
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let pair = builder.temp(pair_type());
        let head = builder.local("head", RType::STR);
        let length = builder.temp(RType::INT);
        call_pair(&mut builder, pair, text);
        builder.push(Op::TupleGet {
            dest: head,
            src: Value::Register(pair),
            index: 0,
        });
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(head),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        let function = &m.functions[0];
        assert!(function.registers[head.index()].borrowed);
        // the tuple is what holds the value, so it goes on owning
        assert!(!function.registers[pair.index()].borrowed);
        assert_eq!(verify(function), Ok(()));
    }

    #[test]
    fn an_element_read_borrows_in_every_copy_of_a_loop_body() {
        // `unswitch` emits a second copy of a loop body, so the register is written
        // twice and read twice — which is why this cannot ask for a register written
        // once. each write's window is its own block
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let pair = builder.temp(pair_type());
        let head = builder.local("head", RType::STR);
        let length = builder.temp(RType::INT);
        let second = builder.new_block();
        for block in [None, Some(second)] {
            if let Some(block) = block {
                builder.switch_to(block);
            }
            call_pair(&mut builder, pair, text);
            builder.push(Op::TupleGet {
                dest: head,
                src: Value::Register(pair),
                index: 0,
            });
            builder.push(Op::Len {
                dest: length,
                src: Value::Register(head),
            });
            match block {
                None => builder.terminate(Terminator::Goto(second)),
                Some(_) => builder.terminate(Terminator::Return(Value::Register(length))),
            }
        }
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(m.functions[0].registers[head.index()].borrowed);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn an_element_read_in_a_block_that_did_not_write_it_does_not_borrow() {
        // the read takes what an earlier block left in the register, which a borrow
        // no longer keeps alive
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let pair = builder.temp(pair_type());
        let head = builder.local("head", RType::STR);
        let length = builder.temp(RType::INT);
        let next = builder.new_block();
        call_pair(&mut builder, pair, text);
        builder.push(Op::TupleGet {
            dest: head,
            src: Value::Register(pair),
            index: 0,
        });
        builder.terminate(Terminator::Goto(next));
        builder.switch_to(next);
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(head),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[head.index()].borrowed);
    }

    #[test]
    fn an_element_read_before_its_own_write_does_not_borrow() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let pair = builder.temp(pair_type());
        let head = builder.local("head", RType::STR);
        let first = builder.temp(RType::INT);
        let second = builder.temp(RType::INT);
        builder.push(Op::Len {
            dest: first,
            src: Value::Register(head),
        });
        call_pair(&mut builder, pair, text);
        builder.push(Op::TupleGet {
            dest: head,
            src: Value::Register(pair),
            index: 0,
        });
        builder.push(Op::Len {
            dest: second,
            src: Value::Register(head),
        });
        builder.terminate(Terminator::Return(Value::Register(second)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[head.index()].borrowed);
    }

    #[test]
    fn an_element_whose_tuple_is_rewritten_before_the_use_does_not_borrow() {
        // the tuple is what holds the element, so writing over it releases the
        // element and leaves the borrow pointing at freed memory
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let pair = builder.temp(pair_type());
        let head = builder.local("head", RType::STR);
        let length = builder.temp(RType::INT);
        call_pair(&mut builder, pair, text);
        builder.push(Op::TupleGet {
            dest: head,
            src: Value::Register(pair),
            index: 0,
        });
        call_pair(&mut builder, pair, text);
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(head),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[head.index()].borrowed);
    }

    #[test]
    fn a_returned_element_does_not_borrow() {
        // the frame would be handing out a reference it never took
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let text = builder.param("s", RType::STR);
        let pair = builder.temp(pair_type());
        let head = builder.local("head", RType::STR);
        call_pair(&mut builder, pair, text);
        builder.push(Op::TupleGet {
            dest: head,
            src: Value::Register(pair),
            index: 0,
        });
        builder.terminate(Terminator::Return(Value::Register(head)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[head.index()].borrowed);
    }

    #[test]
    fn a_register_written_by_anything_but_an_element_read_does_not_borrow() {
        // a register is one discipline or the other for its whole life, and the other
        // write leaves something owned in it
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let pair = builder.temp(pair_type());
        let head = builder.local("head", RType::STR);
        let length = builder.temp(RType::INT);
        let next = builder.new_block();
        call_pair(&mut builder, pair, text);
        builder.push(Op::TupleGet {
            dest: head,
            src: Value::Register(pair),
            index: 0,
        });
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(head),
        });
        builder.terminate(Terminator::Goto(next));
        builder.switch_to(next);
        builder.push(Op::StrConcat {
            dest: head,
            lhs: Value::Str("a".to_string()),
            rhs: Value::Str("b".to_string()),
            consumes_lhs: false,
        });
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(head),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[head.index()].borrowed);
    }

    #[test]
    fn two_element_reads_into_one_register_in_one_block_do_not_borrow() {
        // two windows in one block, and only one is reasoned about
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let pair = builder.temp(pair_type());
        let head = builder.local("head", RType::STR);
        let length = builder.temp(RType::INT);
        call_pair(&mut builder, pair, text);
        for index in [0, 1] {
            builder.push(Op::TupleGet {
                dest: head,
                src: Value::Register(pair),
                index,
            });
            builder.push(Op::Len {
                dest: length,
                src: Value::Register(head),
            });
        }
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[head.index()].borrowed);
    }

    #[test]
    fn elements_of_two_different_tuples_in_one_register_do_not_borrow() {
        // the register would be lending from two places at once, and the borrow is
        // one answer for its whole life
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let first = builder.temp(pair_type());
        let second = builder.temp(pair_type());
        let head = builder.local("head", RType::STR);
        let length = builder.temp(RType::INT);
        let next = builder.new_block();
        for (pair, last) in [(first, false), (second, true)] {
            call_pair(&mut builder, pair, text);
            builder.push(Op::TupleGet {
                dest: head,
                src: Value::Register(pair),
                index: 0,
            });
            builder.push(Op::Len {
                dest: length,
                src: Value::Register(head),
            });
            if last {
                builder.terminate(Terminator::Return(Value::Register(length)));
            } else {
                builder.terminate(Terminator::Goto(next));
                builder.switch_to(next);
            }
        }
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[head.index()].borrowed);
    }

    #[test]
    fn an_element_of_a_borrowed_tuple_does_not_borrow() {
        // a tuple that owns nothing itself has nothing to lend, and the chain would
        // have no owner at the end of it
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let pair = builder.temp(pair_type());
        let head = builder.local("head", RType::STR);
        let length = builder.temp(RType::INT);
        call_pair(&mut builder, pair, text);
        builder.push(Op::TupleGet {
            dest: head,
            src: Value::Register(pair),
            index: 0,
        });
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(head),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut function = builder.finish();
        assert_eq!(tuple_elements(&function), vec![(head, pair)]);

        function.registers[pair.index()].borrowed = true;
        assert!(tuple_elements(&function).is_empty());
    }

    #[test]
    fn a_copy_of_an_element_that_is_itself_borrowing_does_not_borrow() {
        // the two kinds are settled together for this: the element's own window ends
        // at a write to the tuple, and the copy's uses may come after that
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let pair = builder.temp(pair_type());
        let head = builder.local("head", RType::STR);
        let widened = builder.temp(RType::OBJECT);
        let length = builder.temp(RType::INT);
        call_pair(&mut builder, pair, text);
        builder.push(Op::TupleGet {
            dest: head,
            src: Value::Register(pair),
            index: 0,
        });
        builder.assign(widened, Value::Register(head));
        builder.push(Op::Len {
            dest: length,
            src: Value::Register(widened),
        });
        builder.terminate(Terminator::Return(Value::Register(length)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(m.functions[0].registers[head.index()].borrowed);
        assert!(!m.functions[0].registers[widened.index()].borrowed);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn an_unboxed_element_is_never_a_candidate() {
        // there is no reference to save
        let mut builder = FunctionBuilder::new("f", RType::FLOAT);
        let text = builder.param("s", RType::STR);
        let pair = builder.temp(RType::Tuple(Box::new([RType::FLOAT, RType::FLOAT])));
        let head = builder.local("head", RType::FLOAT);
        let doubled = builder.temp(RType::FLOAT);
        call_pair(&mut builder, pair, text);
        builder.push(Op::TupleGet {
            dest: head,
            src: Value::Register(pair),
            index: 0,
        });
        builder.push(Op::FloatBinary {
            dest: doubled,
            op: BinOp::Add,
            lhs: Value::Register(head),
            rhs: Value::Register(head),
        });
        builder.terminate(Terminator::Return(Value::Register(doubled)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(!m.functions[0].registers[head.index()].borrowed);
    }
}

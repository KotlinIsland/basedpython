//! dropping an allocation nothing ever reads
//!
//! [`crate::dead_registers`] removes a register no op mentions, but an op that
//! *writes* a register mentions it — so a value that is computed and then never
//! looked at keeps both the register and the work that filled it. the folds are what
//! leave those behind: rewriting `pair[0]` into the element that went into the tuple
//! is what makes the tuple itself unread, and until something removes it the
//! allocation stays in the loop.
//!
//! this only ever removes an op from the whitelist below, and only when nothing in
//! the function reads what it wrote. dropping one op orphans whatever it read, so it
//! runs to a fixpoint: the tuple goes first, and its two boxed elements are only
//! unread once it has.

use std::collections::HashSet;

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{Op, RegisterId, Value};

pub(crate) fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        // bounded by the op count: every round removes at least one op, or stops
        while drop_unread(function) {}
    }
}

/// whether an op does nothing but compute the value it names
///
/// the whitelist is deliberately short and every entry is checked against the C it
/// emits: an allocation, a refcount and a store. none of them calls into user code,
/// none writes anything another expression could observe, and the only failure any
/// of them has is memory exhaustion. so one whose result nothing reads is work the
/// program cannot tell apart from work that was never done.
///
/// a `list`, `set` or `dict` display is deliberately *not* here even though a list
/// looks the same: a set and a dict hash their elements, and hashing runs whatever
/// `__hash__` the element's class wrote
fn computes_only(op: &Op) -> bool {
    matches!(
        op,
        Op::Assign { .. }
            | Op::Box { .. }
            | Op::BuildTuple { .. }
            | Op::TupleBuild { .. }
            | Op::TupleGet { .. }
    )
}

/// one round: drop every whitelisted op whose destination no operand names
fn drop_unread(function: &mut Function) -> bool {
    let mut read: HashSet<RegisterId> = HashSet::new();
    let note = |value: &Value, read: &mut HashSet<RegisterId>| {
        if let Value::Register(id) = value {
            read.insert(*id);
        }
    };
    for block in &function.blocks {
        for op in &block.ops {
            for operand in op.operands() {
                note(operand, &mut read);
            }
        }
        for operand in block.terminator.operands() {
            note(operand, &mut read);
        }
    }
    // a default is an immediate today, but it is a `Value` and a register named there
    // would be read by every call that omits the parameter
    for default in function.defaults.iter().flatten() {
        note(default, &mut read);
    }

    // a register python can find *unbound* carries a byte saying whether it has been
    // written, and every write sets it. so the write is observable even where the
    // value is not:
    //
    //     x = n
    //     del x        # raises `UnboundLocalError` if `x = n` never ran
    //
    // nothing reads `x` there, and dropping the assignment made the first `del` raise
    //
    // and a store into a name is a binding python keeps until the name is rebound or
    // deleted, whether or not anything reads it. where the value could be, or hold,
    // an object with a finalizer, dropping the store changes when that runs: the
    // value would be let go of along with whatever it was built from, rather than when
    // the name is rebound
    //
    //     held = Cell(1)
    //     held = Cell(2)   # `Cell(1)` is let go of here
    let finalizer_free = finalizer_free(function);
    let observable: HashSet<RegisterId> = function
        .registers
        .iter()
        .enumerate()
        .filter(|(index, decl)| {
            decl.may_be_unassigned
                || (decl.name.is_some() && !finalizer_free.contains(&RegisterId(*index)))
        })
        .map(|(index, _)| RegisterId(index))
        .collect();

    // a tuple holds what it was built from until it is let go of, and then lets go of
    // its elements last to first. one nothing reads is let go of as soon as it is built,
    // which for a display is after its last element is made — so without it, an element
    // could go before the elements after it were made:
    //
    //     (make(1), make(2))      # `make(1)`'s value outlives `make(2)`
    //     x = (make(1), make(2))[1]
    let holds_its_elements = |op: &Op| {
        matches!(op, Op::TupleBuild { .. } | Op::BuildTuple { .. })
            && op
                .dest()
                .is_some_and(|dest| !finalizer_free.contains(&dest))
    };

    let mut dropped = false;
    for block in &mut function.blocks {
        let before = block.ops.len();
        block.ops.retain(|op| {
            !computes_only(op)
                || holds_its_elements(op)
                || op
                    .dest()
                    .is_none_or(|dest| read.contains(&dest) || observable.contains(&dest))
        });
        dropped |= block.ops.len() != before;
    }
    dropped
}

/// the registers that can only ever hold a value whose release runs nothing
///
/// a representation with no object reference in it is one, and so is an object every
/// write of which boxes such a value, or builds a tuple of them. that is `pair` in
/// `pair = (whole, part)` over two `int`s, once its reads have been folded into reads
/// of the elements. a tagged `int` counts as having no finalizer, as it does for the
/// release pass
fn finalizer_free(function: &Function) -> HashSet<RegisterId> {
    let mut free: HashSet<RegisterId> = function
        .registers
        .iter()
        .enumerate()
        .filter(|(_, decl)| !decl.ty.holds_object_reference())
        .map(|(index, _)| RegisterId(index))
        .collect();
    loop {
        let mut grew = false;
        for index in function.param_count..function.registers.len() {
            let register = RegisterId(index);
            if free.contains(&register) {
                continue;
            }
            let mut writes = function
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .filter(|op| op.dest() == Some(register))
                .peekable();
            if writes.peek().is_none() {
                continue;
            }
            let value_free = |value: &Value| match value {
                Value::Register(id) => free.contains(id),
                _ => true,
            };
            let builds_free = writes.all(|op| match op {
                Op::Assign { src, .. } | Op::Box { src, .. } => value_free(src),
                Op::BuildTuple { items, .. } | Op::TupleBuild { items, .. } => {
                    items.iter().all(value_free)
                }
                _ => false,
            });
            if builds_free {
                free.insert(register);
                grew = true;
            }
        }
        if !grew {
            return free;
        }
    }
}

#[cfg(test)]
mod tests {
    use by_ir::builder::FunctionBuilder;
    use by_ir::function::ModuleIr;
    use by_ir::ops::{Op, Terminator, Value};
    use by_ir::rtype::RType;

    fn module(function: by_ir::function::Function) -> ModuleIr {
        let mut module = ModuleIr::new("app");
        module.functions.push(function);
        module
    }

    fn call(dest: by_ir::ops::RegisterId, arg: by_ir::ops::RegisterId) -> Op {
        Op::CallPython {
            dest,
            callee: "make".to_string(),
            args: vec![Value::Register(arg)],
        }
    }

    #[test]
    fn a_store_into_a_name_nothing_reads_is_kept() {
        // `held = make(p)` with `held` never read: the name still holds the value until
        // it is rebound, and the store is what lets the rebinding let go of it
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        let held = builder.local("held".to_string(), RType::OBJECT);
        builder.push(call(made, p));
        builder.assign(held, Value::Register(made));
        builder.terminate(Terminator::Return(Value::Register(p)));

        let mut module = module(builder.finish());
        super::run(&mut module);

        assert_eq!(module.functions[0].blocks[0].ops.len(), 2);
    }

    #[test]
    fn a_store_into_a_temporary_nothing_reads_goes() {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        let copy = builder.temp(RType::OBJECT);
        builder.push(call(made, p));
        builder.assign(copy, Value::Register(made));
        builder.terminate(Terminator::Return(Value::Register(p)));

        let mut module = module(builder.finish());
        super::run(&mut module);

        assert_eq!(module.functions[0].blocks[0].ops.len(), 1);
    }

    #[test]
    fn a_name_nothing_reads_holding_only_ints_goes() {
        // `pair = (whole, part)` over two `int`s, with its reads folded into reads of
        // the elements: letting go of a tuple of boxed ints runs nothing, so when it
        // happens cannot be told apart, and the tuple and both boxes go
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let whole = builder.param("whole", RType::INT);
        let part = builder.param("part", RType::INT);
        let first = builder.temp(RType::OBJECT);
        let second = builder.temp(RType::OBJECT);
        let pair = builder.local("pair".to_string(), RType::OBJECT);
        builder.push(Op::Box {
            dest: first,
            src: Value::Register(whole),
        });
        builder.push(Op::Box {
            dest: second,
            src: Value::Register(part),
        });
        builder.push(Op::BuildTuple {
            dest: pair,
            items: vec![Value::Register(first), Value::Register(second)],
        });
        builder.terminate(Terminator::Return(Value::Register(whole)));

        let mut module = module(builder.finish());
        super::run(&mut module);

        assert!(module.functions[0].blocks[0].ops.is_empty());
    }

    #[test]
    fn a_tuple_nothing_reads_holding_objects_stays() {
        // `(make(p), make(p))` thrown away: the tuple is what keeps the first element
        // alive while the second is made, and what lets both go, last to first
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let first = builder.temp(RType::OBJECT);
        let second = builder.temp(RType::OBJECT);
        let pair = builder.temp(RType::Tuple(Box::new([RType::OBJECT, RType::OBJECT])));
        builder.push(call(first, p));
        builder.push(call(second, p));
        builder.push(Op::TupleBuild {
            dest: pair,
            items: vec![Value::Register(first), Value::Register(second)],
        });
        builder.terminate(Terminator::Return(Value::Register(p)));

        let mut module = module(builder.finish());
        super::run(&mut module);

        assert_eq!(module.functions[0].blocks[0].ops.len(), 3);
    }
}

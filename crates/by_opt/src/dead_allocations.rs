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
    let observable: HashSet<RegisterId> = function
        .registers
        .iter()
        .enumerate()
        .filter(|(_, decl)| decl.may_be_unassigned)
        .map(|(index, _)| RegisterId(index))
        .collect();

    let mut dropped = false;
    for block in &mut function.blocks {
        let before = block.ops.len();
        block.ops.retain(|op| {
            !computes_only(op)
                || op
                    .dest()
                    .is_none_or(|dest| read.contains(&dest) || observable.contains(&dest))
        });
        dropped |= block.ops.len() != before;
    }
    dropped
}

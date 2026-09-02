//! build the `str` of an integer from the digits, rather than through an object
//!
//! `"k" + str(i)` in a loop is most of what a program building dictionary keys
//! spends its time on, and almost none of that time is the concatenation. the
//! general lowering has to give the counter — which is a machine word by the time
//! `unbox_counters` has been over it — an object representation to pass as an
//! argument, so the loop allocates a `PyLong` it throws away immediately; then
//! `str` of that object reaches `int.__str__`, which builds its answer through a
//! unicode writer. two allocations and a formatter, for a handful of ascii digits
//! that were in a register the whole time.
//!
//! writing the digits straight into one string instead is worth a third of the
//! benchmark that does nothing else.
//!
//! ## what this does *not* change
//!
//! the name `str` is still looked up through the module namespace on every trip,
//! and the fast path is taken only when that lookup answered with the builtin type
//! object itself. a module that binds its own `str`, or writes one into
//! `globals()`, is obeyed exactly as it was — the emitted code compares what it
//! found rather than assuming what it will find.
//!
//! the other half of the guard is the *representation*: a tagged integer is a
//! machine word or a `PyLongObject`, and only the machine word is formatted
//! directly. the reason is identity rather than width — a tagged word carries no
//! object, so the general path boxes it with `PyLong_FromSsize_t`, which builds a
//! plain `int` and never a subclass, and `str` of a plain `int` is its decimal
//! digits. a tagged value holding a real object may be holding an `IntEnum` member
//! or anything else with a `__str__` of its own, so it goes the long way and is
//! asked.
//!
//! ## why the boxing goes away with the call
//!
//! the argument register is only there to carry the integer into the call, so
//! leaving it behind would mean allocating the `PyLong` anyway and never looking at
//! it. it may be dropped exactly when every write of it is one of the fused
//! boxings and every read of it is one of the fused calls — which is the shape a
//! frontend-emitted temporary has, and is checked rather than assumed, because a
//! later pass may have merged that temporary with a register something else uses.

use std::collections::{HashMap, HashSet};

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{BlockId, Op, RegisterId, Value};
use by_ir::rtype::RType;

pub fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        fuse(function);
    }
}

/// one `str(box(n))` found in a block: where it is, and what it is about
struct Pair {
    block: BlockId,
    /// the index of the `Box`; the call is the operation after it
    index: usize,
    /// the register the boxing writes and the call reads
    boxed: RegisterId,
    /// the integer the boxing reads
    integer: Value,
}

fn fuse(function: &mut Function) {
    let mut pairs = pairs(function);
    if pairs.is_empty() {
        return;
    }
    let droppable = droppable_temporaries(function, &pairs);

    // rewritten back to front, so removing an earlier operation cannot move the index
    // a later one was found at
    pairs.sort_by_key(|pair| std::cmp::Reverse((pair.block.index(), pair.index)));
    for pair in pairs {
        let Some(block) = function.blocks.get_mut(pair.block.index()) else {
            continue;
        };
        let Some(Op::CallPython { dest, .. }) = block.ops.get(pair.index + 1) else {
            continue;
        };
        let fused = Op::StrOfInt {
            dest: *dest,
            value: pair.integer,
        };
        if droppable.contains(&pair.boxed) {
            block.ops[pair.index] = fused;
            block.ops.remove(pair.index + 1);
        } else {
            block.ops[pair.index + 1] = fused;
        }
    }
}

/// every `box` of a tagged integer immediately followed by `str` of that box
fn pairs(function: &Function) -> Vec<Pair> {
    let mut out = Vec::new();
    for (index, block) in function.blocks.iter().enumerate() {
        for (at, window) in block.ops.windows(2).enumerate() {
            let [Op::Box { dest, src }, call] = window else {
                continue;
            };
            let Op::CallPython { callee, args, .. } = call else {
                continue;
            };
            if callee != "str" || args.as_slice() != [Value::Register(*dest)] {
                continue;
            }
            if type_of(function, src) != Some(RType::INT) {
                continue;
            }
            out.push(Pair {
                block: BlockId(index),
                index: at,
                boxed: *dest,
                integer: src.clone(),
            });
        }
    }
    out
}

/// the boxed registers whose whole life is the pairs found above
///
/// a register only these boxings write and only these calls read has no reader
/// left once they are fused, so the allocation it held can go. anything else — a
/// name, a second reader, a write from somewhere else — keeps its boxing, and the
/// call is fused around it
fn droppable_temporaries(function: &Function, pairs: &[Pair]) -> HashSet<RegisterId> {
    // one boxing writes the register and one call reads it, per pair, so the same
    // count answers for both sides
    let mut fused: HashMap<RegisterId, usize> = HashMap::new();
    for pair in pairs {
        *fused.entry(pair.boxed).or_default() += 1;
    }

    let mut writes: HashMap<RegisterId, usize> = HashMap::new();
    let mut reads: HashMap<RegisterId, usize> = HashMap::new();
    for block in &function.blocks {
        for op in &block.ops {
            if let Some(dest) = op.dest() {
                *writes.entry(dest).or_default() += 1;
            }
            // `del x` reads the register on its way to emptying it, and says so
            // through neither `dest` nor `operands`
            if let Some(unbound) = op.unbinds() {
                *reads.entry(unbound).or_default() += 1;
            }
            for operand in op.operands() {
                if let Value::Register(id) = operand {
                    *reads.entry(*id).or_default() += 1;
                }
            }
        }
        if let Some(dest) = block.terminator.dest() {
            *writes.entry(dest).or_default() += 1;
        }
        for operand in block.terminator.operands() {
            if let Value::Register(id) = operand {
                *reads.entry(*id).or_default() += 1;
            }
        }
    }

    pairs
        .iter()
        .map(|pair| pair.boxed)
        .filter(|register| {
            function
                .register(*register)
                .is_some_and(|decl| decl.name.is_none())
                && writes.get(register) == fused.get(register)
                && reads.get(register) == fused.get(register)
        })
        .collect()
}

fn type_of(function: &Function, value: &Value) -> Option<RType> {
    match value {
        Value::Register(id) => function.register(*id).map(|decl| decl.ty.clone()),
        other => other.immediate_type(),
    }
}

//! which registers are still going to be read, at the start of each block
//!
//! an exception leaves a block from the middle, so what its handler reads is live at
//! every point of the block rather than only at its end. a pass that asks whether a
//! register is read again has to ask across that edge too, or a handler reads a value
//! the pass has already given away

use std::collections::HashSet;
use std::hash::Hash;

use by_ir::function::{BasicBlock, Function};
use by_ir::ops::{BlockId, Op, RegisterId, Value};

/// the registers live on entry to each block
///
/// `reads` says which registers an operation reads. most passes want
/// [`read_registers`]; one that knows a register can be read *through* another hands
/// in a wider answer
pub(crate) fn live_in(
    function: &Function,
    reads: &impl Fn(&Op) -> Vec<RegisterId>,
) -> Vec<HashSet<RegisterId>> {
    live_in_by(function, reads, &|register| vec![register])
}

/// [`live_in`], over any finer division of a register than the register itself
///
/// `whole` names every part of a register, which is what a write overwrites and what a
/// terminator reads. a pass that follows the elements of a fixed-length tuple apart
/// answers with one part per element
pub(crate) fn live_in_by<P: Clone + Eq + Hash>(
    function: &Function,
    reads: &impl Fn(&Op) -> Vec<P>,
    whole: &impl Fn(RegisterId) -> Vec<P>,
) -> Vec<HashSet<P>> {
    let mut live_in: Vec<HashSet<P>> = vec![HashSet::new(); function.blocks.len()];

    let mut changed = true;
    while changed {
        changed = false;
        for index in (0..function.blocks.len()).rev() {
            let Some(block) = function.block(BlockId(index)) else {
                continue;
            };
            let mut live = live_out_by(block, &live_in, whole);
            let error_live = error_live(block, &live_in);
            for op in block.ops.iter().rev() {
                live.extend(error_live.iter().cloned());
                if let Some(dest) = op.dest() {
                    for part in whole(dest) {
                        live.remove(&part);
                    }
                }
                live.extend(reads(op));
            }

            if live != live_in[index] {
                live_in[index] = live;
                changed = true;
            }
        }
    }

    live_in
}

/// what is live when `block` ends: its successors' needs, and what its terminator reads
pub(crate) fn live_out(block: &BasicBlock, live_in: &[HashSet<RegisterId>]) -> HashSet<RegisterId> {
    live_out_by(block, live_in, &|register| vec![register])
}

/// [`live_out`], over the parts [`live_in_by`] divides a register into
pub(crate) fn live_out_by<P: Clone + Eq + Hash>(
    block: &BasicBlock,
    live_in: &[HashSet<P>],
    whole: &impl Fn(RegisterId) -> Vec<P>,
) -> HashSet<P> {
    let mut live: HashSet<P> = block
        .successors()
        .iter()
        .filter_map(|successor| live_in.get(successor.index()))
        .flatten()
        .cloned()
        .collect();
    live.extend(
        block
            .terminator
            .operands()
            .into_iter()
            .filter_map(register)
            .flat_map(whole),
    );
    live
}

/// what the block's handler reads, which is live at every point of the block
pub(crate) fn error_live<P: Clone>(block: &BasicBlock, live_in: &[HashSet<P>]) -> HashSet<P> {
    block
        .error_target
        .and_then(|target| live_in.get(target.index()))
        .cloned()
        .unwrap_or_default()
}

/// the registers an operation reads
///
/// `del x` leaves its destination unbound, but it reads the value first — to release
/// it — so the reference is still needed up to that point
pub(crate) fn read_registers(op: &Op) -> Vec<RegisterId> {
    op.operands()
        .into_iter()
        .filter_map(register)
        .chain(op.unbinds())
        .collect()
}

pub(crate) fn register(value: &Value) -> Option<RegisterId> {
    match value {
        Value::Register(id) => Some(*id),
        _ => None,
    }
}

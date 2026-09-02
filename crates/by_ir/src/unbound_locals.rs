//! compiling a local some path may not have assigned
//!
//! python has no declaration, so a name bound only inside an `if` is simply absent on
//! the paths that skipped it, and reading it raises `UnboundLocalError`. a register
//! machine has the slot either way, so what is missing is the *answer to whether it was
//! written* — one byte beside the register, set by every write and tested by every
//! read.
//!
//! this is the same shape the verifier already uses to *reject* such a function, run
//! here to fix it instead: the fixpoint below is the verifier's, and what it finds
//! becomes a flag rather than an error.
//!
//! the byte is also what `del x` clears, and that is the one thing the fixpoint cannot
//! find on its own: a deletion looks like a write to any forward analysis, so a local
//! assigned on every path reaching it still comes out definitely-written. so the
//! deletion asks for the byte directly, below.

use std::collections::VecDeque;

use crate::function::Function;
use crate::ops::{BlockId, Terminator, Value};

/// flag every local this function may read before writing
///
/// runs at lowering time rather than as an optimization: the verifier rejects such a
/// read, so the flag has to be there before it looks
pub fn mark(function: &mut Function) {
    let mut flagged: Vec<usize> = unassigned_reads(function);
    // a register `del` can unbind is maybe-unassigned by construction, and the
    // fixpoint above cannot see it: `del x` writes its destination as far as any
    // forward analysis is concerned, so a local assigned on every path that reaches
    // the `del` still comes out "definitely written". the byte is the unbound state
    // the deletion needs, so the deletion is what asks for it
    for block in &function.blocks {
        for op in &block.ops {
            if let Some(id) = op.unbinds() {
                flagged.push(id.index());
            }
        }
    }
    for id in flagged {
        if let Some(decl) = function.registers.get_mut(id) {
            decl.may_be_unassigned = true;
        }
    }
}

/// the registers some path reads without having written
///
/// only *named* ones: a temporary is written by the operation that made it, and a
/// nameless register with no writer is a malformed lowering rather than a local the
/// program can observe
fn unassigned_reads(function: &Function) -> Vec<usize> {
    let block_count = function.blocks.len();
    let register_count = function.registers.len();

    // a parameter arrives written; everything else starts absent
    let entry: Vec<bool> = (0..register_count)
        .map(|index| index < function.param_count)
        .collect();

    // `None` is a block nothing reaches yet, which is how an unreached one avoids
    // constraining its successors
    let mut incoming: Vec<Option<Vec<bool>>> = vec![None; block_count];
    if block_count == 0 {
        return Vec::new();
    }
    incoming[0] = Some(entry);

    let mut queue = VecDeque::from([Function::entry()]);
    while let Some(id) = queue.pop_front() {
        let Some(block) = function.block(id) else {
            continue;
        };
        let Some(entry_state) = incoming[id.index()].clone() else {
            continue;
        };
        let mut state = entry_state.clone();
        for op in &block.ops {
            if let Some(dest) = op.dest()
                && dest.index() < state.len()
            {
                state[dest.index()] = true;
            }
        }
        // a handler is entered from *before* any of this block's writes: the very
        // first operation can be the one that failed
        let narrowed = match &block.terminator {
            Terminator::NarrowShort { dest, fits, .. } => Some((*dest, *fits)),
            _ => None,
        };
        let edges = block
            .terminator
            .successors()
            .into_iter()
            .map(|target| {
                let mut state = state.clone();
                if let Some((dest, fits)) = narrowed
                    && target == fits
                    && dest.index() < state.len()
                {
                    state[dest.index()] = true;
                }
                (target, state)
            })
            .chain(
                block
                    .error_target
                    .into_iter()
                    .map(|target| (target, entry_state.clone())),
            );
        for (target, state) in edges {
            if target.index() >= block_count {
                continue;
            }
            let merged = match &incoming[target.index()] {
                // written on entry only when every predecessor wrote it
                Some(existing) => {
                    let merged: Vec<bool> =
                        existing.iter().zip(&state).map(|(a, b)| *a && *b).collect();
                    if merged == *existing {
                        continue;
                    }
                    merged
                }
                None => state.clone(),
            };
            incoming[target.index()] = Some(merged);
            queue.push_back(target);
        }
    }

    let mut found = vec![false; register_count];
    for (index, entry) in incoming.iter().enumerate() {
        let Some(block) = function.block(BlockId(index)) else {
            continue;
        };
        let Some(mut state) = entry.clone() else {
            continue; // unreachable: nothing to prove about it
        };
        let note = |value: &Value, state: &[bool], found: &mut Vec<bool>| {
            let Value::Register(id) = value else {
                return;
            };
            if state.get(id.index()) == Some(&false)
                && function
                    .register(*id)
                    .is_some_and(|decl| decl.name.is_some())
            {
                found[id.index()] = true;
            }
        };
        for op in &block.ops {
            for operand in op.operands() {
                note(operand, &state, &mut found);
            }
            if let Some(dest) = op.dest()
                && dest.index() < state.len()
            {
                state[dest.index()] = true;
            }
        }
        for operand in block.terminator.operands() {
            note(operand, &state, &mut found);
        }
    }

    found
        .into_iter()
        .enumerate()
        .filter_map(|(index, hit)| hit.then_some(index))
        .collect()
}

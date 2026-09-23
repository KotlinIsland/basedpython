//! store into a field by handing the register's reference over, where the store is
//! the last thing that reads it
//!
//! a field store copies: it retains the value for the field, and the register goes on
//! owning its own reference until something writes over it or the frame returns. when
//! nothing reads the register again that is a retain and a release around nothing —
//! `self.depth = self.depth + 1` pays both on every call. so a store that is its value's
//! last use takes the register's reference instead, and leaves the register empty, which
//! every later release of it then finds
//!
//! ## what is left alone
//!
//! - a **parameter**, whose reference the caller owns rather than the frame
//! - a **borrowed** register, which owns nothing to hand over
//! - a value a **name** holds: python keeps a named value alive until the name is
//!   rebound or the frame returns, so the field is not the only thing holding it, and
//!   rewriting the field later must not be what frees it
//! - a value a handler could read: the store leaves the register empty, and a handler
//!   entered from later in the block would find nothing there
//!
//! the release pass has already put a release straight after such a store — the store
//! was the value's last read — and that release is what the move replaces, so it goes
//!
//! ## a copy into another register moves the same way
//!
//! `held = Cell(i)` builds the value into a temporary and copies it into the name, and
//! `held, part = split(i)` reads each element off the tuple the call answered. the name
//! owns what it holds, so python can let go of it when the name is rebound — and the
//! temporary is let go of straight after the copy, which is a retain and a release
//! around nothing. so a copy or an element read followed by the release of the place it
//! read hands that place's reference over instead
//!
//! the release may sit a little after the store, past operations the release pass would
//! not put it inside. the move empties the place early, which nothing reads, and holds
//! the value in the destination instead — so the scan from the store to the release
//! gives up at anything that reads the place, writes or deletes its register, or writes
//! or deletes the destination, which would let go of the value in the meantime

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{BlockId, Op, RegisterId, Value};
use by_ir::rtype::RType;

use crate::liveness;

pub(crate) fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        move_dying_stores(function);
    }
}

fn move_dying_stores(function: &mut Function) {
    // a release of the stored register reads nothing: the move is what it would have
    // been doing, and counting it as a read would keep every store from qualifying
    let reads = |op: &Op| match op {
        Op::Release { .. } => Vec::new(),
        other => liveness::read_registers(other),
    };
    let live_in = liveness::live_in(function, &reads);

    let mut moved: Vec<(BlockId, usize, RegisterId)> = Vec::new();
    for (index, block) in function.blocks.iter().enumerate() {
        let error_live = liveness::error_live(block, &live_in);
        let mut live = liveness::live_out(block, &live_in);
        for (position, op) in block.ops.iter().enumerate().rev() {
            live.extend(error_live.iter().copied());
            if let Op::SetField {
                receiver,
                value: Value::Register(value),
                ..
            } = op
                && !live.contains(value)
                && !error_live.contains(value)
                && receiver != &Value::Register(*value)
                && may_hand_over(function, *value)
            {
                moved.push((BlockId(index), position, *value));
            }
            if let Some(dest) = op.dest() {
                live.remove(&dest);
            }
            live.extend(reads(op));
        }
    }

    for (block, position, value) in moved {
        let Some(block) = function.blocks.get_mut(block.index()) else {
            continue;
        };
        if let Some(Op::SetField { moves, .. }) = block.ops.get_mut(position) {
            *moves = true;
        }
        // the release the store's being the last read put here is now the move's job
        let released = block.ops[position + 1..].iter().position(|op| {
            matches!(
                op,
                Op::Release { value: Value::Register(id), path } if *id == value && path.is_empty()
            )
        });
        if let Some(offset) = released {
            block.ops.remove(position + 1 + offset);
        }
    }

    // after the field stores, whose positions were found before any release went
    move_dying_copies(function);
}

/// one operation's read of a place that is released straight after it, so the read may
/// take the reference rather than have one taken for it
struct Handover {
    block: usize,
    /// where the reading operation sits
    at: usize,
    /// which slot of a tuple build reads it, where that is the reading operation
    item: Option<usize>,
    /// where the release of the place sits, which the handover replaces
    release: usize,
}

/// each slot of a container build that reads a register, and the type the slot holds
///
/// a typed tuple's slots each have their own type, read off the destination; a `list` or
/// a `tuple` object holds objects, whatever the destination register is declared as. a
/// `set` and a `dict` are not here: their insertions take references of their own, so
/// there is nothing for a slot of one to be handed
struct BuiltSlots {
    /// each slot that reads a register, by index into the build's items
    slots: Vec<(usize, RegisterId)>,
    /// the type every slot holds, where the build settles it rather than the
    /// destination register's own type doing so
    held: Option<RType>,
}

fn built_slots(op: &Op) -> Option<BuiltSlots> {
    let (items, held) = match op {
        Op::TupleBuild { items, .. } => (items, None),
        Op::BuildTuple { items, .. } | Op::BuildList { items, .. } => (items, Some(RType::OBJECT)),
        _ => return None,
    };
    let slots = items
        .iter()
        .enumerate()
        .filter_map(|(slot, item)| match item {
            Value::Register(source) => Some((slot, *source)),
            _ => None,
        })
        .collect();
    Some(BuiltSlots { slots, held })
}

/// turn each read of a dying place into a move: a copy, an element read off a tuple, or
/// one slot of a container being built
fn move_dying_copies(function: &mut Function) {
    let mut moved: Vec<Handover> = Vec::new();
    for (index, block) in function.blocks.iter().enumerate() {
        for (position, op) in block.ops.iter().enumerate() {
            // where a built slot holds its own type rather than the destination's
            let mut held: Option<RType> = None;
            let reads: Vec<(Option<usize>, RegisterId, Vec<usize>)> = match op {
                Op::Assign {
                    dest: _,
                    src: Value::Register(source),
                } => vec![(None, *source, Vec::new())],
                Op::TupleGet {
                    dest: _,
                    src: Value::Register(source),
                    index,
                } => vec![(None, *source, vec![*index])],
                built => {
                    let Some(built) = built_slots(built) else {
                        continue;
                    };
                    held = built.held;
                    built
                        .slots
                        .into_iter()
                        .map(|(slot, source)| (Some(slot), source, Vec::new()))
                        .collect()
                }
            };
            let Some(dest) = op.dest() else {
                continue;
            };
            for (item, source, path) in reads {
                // a widening read changes the representation the reference is held in,
                // which a move does not
                let slot = match item {
                    Some(slot) => vec![slot],
                    None => Vec::new(),
                };
                let written = match &held {
                    Some(ty) => Some(ty),
                    None => function
                        .register(dest)
                        .and_then(|decl| decl.ty.element(&slot)),
                };
                let same_type = function
                    .register(source)
                    .and_then(|decl| decl.ty.element(&path))
                    .zip(written)
                    .is_some_and(|(read, written)| read == written);
                if dest == source
                    || !same_type
                    || function.register(dest).is_none_or(|decl| decl.borrowed)
                    || !owns_place(function, source, &path)
                {
                    continue;
                }
                let touches = |later: &Op| {
                    later.dest() == Some(source)
                        || later.unbinds() == Some(source)
                        || later.dest() == Some(dest)
                        || later.unbinds() == Some(dest)
                        || reads_place(later, source, &path)
                };
                let Some(offset) = block.ops[position + 1..]
                    .iter()
                    .position(|later| touches(later) || releases_place(later, source, &path))
                else {
                    continue;
                };
                let at = position + 1 + offset;
                // one release lets go of one reference, so it pays for one slot: a
                // register named twice in the same build retains for the second
                if releases_place(&block.ops[at], source, &path)
                    && !moved
                        .iter()
                        .any(|held| held.block == index && held.release == at)
                {
                    moved.push(Handover {
                        block: index,
                        at: position,
                        item,
                        release: at,
                    });
                }
            }
        }
    }

    for handover in &moved {
        let Some(op) = function
            .blocks
            .get_mut(handover.block)
            .and_then(|block| block.ops.get_mut(handover.at))
        else {
            continue;
        };
        if let Some(slot) = handover.item {
            match op {
                Op::TupleBuild { moves, .. }
                | Op::BuildTuple { moves, .. }
                | Op::BuildList { moves, .. } => {
                    moves.insert(slot);
                }
                _ => {}
            }
            continue;
        }
        let replacement = match op {
            Op::Assign { dest, src } => Op::Move {
                dest: *dest,
                src: src.clone(),
                path: Box::default(),
            },
            Op::TupleGet { dest, src, index } => Op::Move {
                dest: *dest,
                src: src.clone(),
                path: Box::new([*index]),
            },
            _ => continue,
        };
        *op = replacement;
    }
    // the releases go last and back to front, so removing one cannot move another
    let mut releases: Vec<(usize, usize)> = moved
        .into_iter()
        .map(|handover| (handover.block, handover.release))
        .collect();
    releases.sort_unstable_by(|a, b| b.cmp(a));
    for (index, release) in releases {
        if let Some(block) = function.blocks.get_mut(index)
            && release < block.ops.len()
        {
            block.ops.remove(release);
        }
    }
}

/// whether `register` owns the one reference at `path` inside it, which a move may take
fn owns_place(function: &Function, register: RegisterId, path: &[usize]) -> bool {
    register.index() >= function.param_count
        && function.register(register).is_some_and(|decl| {
            !decl.borrowed
                && decl.name.is_none()
                && decl.ty.element(path).is_some_and(RType::owns_one_reference)
        })
}

/// whether `op` is the release of exactly the place at `path` inside `register`
fn releases_place(op: &Op, register: RegisterId, path: &[usize]) -> bool {
    matches!(
        op,
        Op::Release { value: Value::Register(id), path: released }
            if *id == register && **released == *path
    )
}

/// whether `op` reads anything at or around the place at `path` inside `register`
///
/// an element read reads its own element and nothing else of the tuple, and a release
/// of a different element reads nothing
fn reads_place(op: &Op, register: RegisterId, path: &[usize]) -> bool {
    let overlaps = |other: &[usize]| other.starts_with(path) || path.starts_with(other);
    match op {
        Op::TupleGet {
            src: Value::Register(id),
            index,
            ..
        } if *id == register => overlaps(&[*index]),
        Op::Release {
            value: Value::Register(id),
            path: released,
        } if *id == register => overlaps(released),
        other => liveness::read_registers(other).contains(&register),
    }
}

/// whether the register owns a reference of its own to give the field
fn may_hand_over(function: &Function, register: RegisterId) -> bool {
    if register.index() < function.param_count {
        return false;
    }
    function
        .register(register)
        .is_some_and(|decl| !decl.borrowed && decl.name.is_none() && decl.ty.owns_one_reference())
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{BinOp, Mutation, Terminator};
    use std::collections::BTreeSet;

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
            verify_install: true,
            follow_recursion_limit: true,
        }
    }

    fn moves(module: &ModuleIr) -> Vec<bool> {
        module.functions[0]
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .filter_map(|op| match op {
                Op::SetField { moves, .. } => Some(*moves),
                _ => None,
            })
            .collect()
    }

    fn store(receiver: RegisterId, value: RegisterId) -> Op {
        Op::SetField {
            receiver: Value::Register(receiver),
            class: "C".to_string(),
            field: "depth".to_string(),
            value: Value::Register(value),
            moves: false,
            present: false,
        }
    }

    #[test]
    fn a_store_that_is_its_value_s_last_read_hands_the_reference_over() {
        let mut builder = FunctionBuilder::new("f", RType::NONE);
        let receiver = builder.param("self", RType::OBJECT);
        let k = builder.param("k", RType::INT);
        let sum = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: sum,
            op: BinOp::Add,
            lhs: Value::Register(k),
            rhs: Value::Int(1),
            mutation: Mutation::Fresh,
        });
        builder.push(store(receiver, sum));
        builder.terminate(Terminator::Return(Value::None));

        let mut module = module(builder.finish());
        run(&mut module);

        assert_eq!(moves(&module), vec![true]);
    }

    #[test]
    fn a_value_read_again_is_copied() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let receiver = builder.param("self", RType::OBJECT);
        let k = builder.param("k", RType::INT);
        let sum = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: sum,
            op: BinOp::Add,
            lhs: Value::Register(k),
            rhs: Value::Int(1),
            mutation: Mutation::Fresh,
        });
        builder.push(store(receiver, sum));
        builder.terminate(Terminator::Return(Value::Register(sum)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert_eq!(moves(&module), vec![false]);
    }

    #[test]
    fn a_parameter_or_a_named_value_is_copied() {
        let mut builder = FunctionBuilder::new("f", RType::NONE);
        let receiver = builder.param("self", RType::OBJECT);
        let k = builder.param("k", RType::INT);
        let named = builder.local("n".to_string(), RType::INT);
        builder.push(store(receiver, k));
        builder.push(Op::IntBinary {
            dest: named,
            op: BinOp::Add,
            lhs: Value::Register(k),
            rhs: Value::Int(1),
            mutation: Mutation::Fresh,
        });
        builder.push(store(receiver, named));
        builder.terminate(Terminator::Return(Value::None));

        let mut module = module(builder.finish());
        run(&mut module);

        assert_eq!(moves(&module), vec![false, false]);
    }

    fn call(dest: RegisterId, arg: RegisterId) -> Op {
        Op::CallPython {
            dest,
            callee: "make".to_string(),
            args: vec![Value::Register(arg)],
        }
    }

    fn release(register: RegisterId, path: &[usize]) -> Op {
        Op::Release {
            value: Value::Register(register),
            path: path.into(),
        }
    }

    #[test]
    fn a_copy_released_straight_after_hands_its_reference_over() {
        // `held = make(p)` arrives as a call into a temporary, a copy into the name, and
        // the release of the temporary the copy was the last read of
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        let held = builder.local("held".to_string(), RType::OBJECT);
        builder.push(call(made, p));
        builder.assign(held, Value::Register(made));
        builder.push(release(made, &[]));
        builder.terminate(Terminator::Return(Value::Register(p)));

        let mut module = module(builder.finish());
        run(&mut module);

        let ops = &module.functions[0].blocks[0].ops;
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert_eq!(
            ops[1],
            Op::Move {
                dest: held,
                src: Value::Register(made),
                path: Box::default(),
            }
        );
        assert_eq!(by_ir::verify::verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn an_element_read_released_after_hands_that_element_over() {
        // `head, tail = split(p)`: each element is read off the tuple and let go of, and
        // the release of the other element in between reads nothing of this one
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let pair = builder.temp(RType::Tuple(Box::new([RType::OBJECT, RType::OBJECT])));
        let head = builder.local("head".to_string(), RType::OBJECT);
        builder.push(Op::TupleBuild {
            dest: pair,
            items: vec![Value::Register(p), Value::Register(p)],
            moves: BTreeSet::new(),
        });
        builder.push(release(pair, &[1]));
        builder.push(Op::TupleGet {
            dest: head,
            src: Value::Register(pair),
            index: 0,
        });
        builder.push(release(pair, &[0]));
        builder.terminate(Terminator::Return(Value::Register(head)));

        let mut module = module(builder.finish());
        run(&mut module);

        let ops = &module.functions[0].blocks[0].ops;
        assert_eq!(ops.len(), 3, "{ops:?}");
        assert_eq!(
            ops[2],
            Op::Move {
                dest: head,
                src: Value::Register(pair),
                path: Box::new([0]),
            }
        );
        assert_eq!(by_ir::verify::verify(&module.functions[0]), Ok(()));
    }

    /// which slots of the one tuple build in `block` hand their reference over
    fn built_moves(module: &ModuleIr) -> Vec<usize> {
        module.functions[0].blocks[0]
            .ops
            .iter()
            .find_map(|op| match op {
                Op::TupleBuild { moves, .. } => Some(moves.iter().copied().collect()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_tuple_takes_the_reference_of_a_slot_released_after_the_build() {
        // `return a // 7, a % 7`: each part is built into a temporary and put in the
        // tuple, which retains it, and the temporary is then let go of. the retain and
        // the release are around nothing, and the tuple can have the reference instead
        let mut builder = FunctionBuilder::new("f", RType::Tuple(Box::new([RType::INT; 2])));
        let p = builder.param("p", RType::INT);
        let first = builder.temp(RType::INT);
        let second = builder.temp(RType::INT);
        let pair = builder.temp(RType::Tuple(Box::new([RType::INT; 2])));
        for (dest, op) in [(first, BinOp::FloorDiv), (second, BinOp::Mod)] {
            builder.push(Op::IntBinary {
                dest,
                op,
                lhs: Value::Register(p),
                rhs: Value::Int(7),
                mutation: Mutation::Fresh,
            });
        }
        builder.push(Op::TupleBuild {
            dest: pair,
            items: vec![Value::Register(first), Value::Register(second)],
            moves: BTreeSet::new(),
        });
        builder.push(release(first, &[]));
        builder.push(release(second, &[]));
        builder.terminate(Terminator::Return(Value::Register(pair)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert_eq!(built_moves(&module), vec![0, 1]);
        let ops = &module.functions[0].blocks[0].ops;
        assert!(
            !ops.iter().any(|op| matches!(op, Op::Release { .. })),
            "{ops:?}"
        );
        assert_eq!(by_ir::verify::verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn one_release_pays_for_one_slot_of_a_tuple_built_from_it_twice() {
        // `(x, x)` needs two references and the release lets go of one, so the second
        // slot takes one of its own
        let mut builder = FunctionBuilder::new("f", RType::Tuple(Box::new([RType::OBJECT; 2])));
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        let pair = builder.temp(RType::Tuple(Box::new([RType::OBJECT; 2])));
        builder.push(call(made, p));
        builder.push(Op::TupleBuild {
            dest: pair,
            items: vec![Value::Register(made), Value::Register(made)],
            moves: BTreeSet::new(),
        });
        builder.push(release(made, &[]));
        builder.terminate(Terminator::Return(Value::Register(pair)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert_eq!(built_moves(&module), vec![0]);
        assert_eq!(by_ir::verify::verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn a_slot_read_again_before_its_release_keeps_a_reference_of_its_own() {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        let pair = builder.temp(RType::Tuple(Box::new([RType::OBJECT; 2])));
        let again = builder.temp(RType::OBJECT);
        builder.push(call(made, p));
        builder.push(Op::TupleBuild {
            dest: pair,
            items: vec![Value::Register(made), Value::Register(p)],
            moves: BTreeSet::new(),
        });
        builder.push(call(again, made));
        builder.push(release(made, &[]));
        builder.terminate(Terminator::Return(Value::Register(again)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert_eq!(built_moves(&module), Vec::<usize>::new());
    }

    #[test]
    fn a_copy_whose_destination_is_written_before_the_release_is_left_alone() {
        // the second write lets go of what the first one stored, and a move would have
        // left nothing else holding it until the release
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        let held = builder.local("held".to_string(), RType::OBJECT);
        builder.push(call(made, p));
        builder.assign(held, Value::Register(made));
        builder.assign(held, Value::Register(p));
        builder.push(release(made, &[]));
        builder.terminate(Terminator::Return(Value::Register(p)));

        let mut module = module(builder.finish());
        run(&mut module);

        let ops = &module.functions[0].blocks[0].ops;
        assert!(
            !ops.iter().any(|op| matches!(op, Op::Move { .. })),
            "{ops:?}"
        );
    }

    #[test]
    fn a_copy_read_again_before_the_release_is_left_alone() {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        let held = builder.local("held".to_string(), RType::OBJECT);
        let again = builder.temp(RType::OBJECT);
        builder.push(call(made, p));
        builder.assign(held, Value::Register(made));
        builder.push(call(again, made));
        builder.push(release(made, &[]));
        builder.terminate(Terminator::Return(Value::Register(again)));

        let mut module = module(builder.finish());
        run(&mut module);

        let ops = &module.functions[0].blocks[0].ops;
        assert!(
            !ops.iter().any(|op| matches!(op, Op::Move { .. })),
            "{ops:?}"
        );
    }
}

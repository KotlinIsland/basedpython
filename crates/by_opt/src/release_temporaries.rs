//! drop a value nothing names as soon as nothing will read it again
//!
//! python keeps a value alive for exactly as long as something refers to it. a
//! temporary — the file `open(p, "w")` answers in `open(p, "w").write(s)`, or the
//! result of a call made only for its effect — is referred to by nothing once the
//! expression that made it is done with it, so python drops it there, and a finalizer
//! runs at that point: the file is closed, and so flushed, before the next line reads
//! it back.
//!
//! a register otherwise holds its reference until something writes over it or the
//! frame returns, which is late by however much of the function is left. so after the
//! last read of an unnamed temporary this pass releases it and leaves the register
//! empty: the release every exit makes of it then finds nothing, and so does the
//! release a later write makes of what the register held before. a named local is left
//! alone: it owns a reference of its own, which a later write or `del` of the name lets
//! go of, as python does when the name is rebound or deleted
//!
//! ## a tuple is let go of an element at a time
//!
//! a fixed-length tuple is held as a struct of its elements, each owning a reference of
//! its own, so each element is a temporary on its own account. `part = make(i)[1]` names
//! one element, and python drops the rest of the tuple as soon as the subscript is done
//! with it — so an element read off the tuple reads that element and no other, and the
//! elements nobody reads again are let go of, last to first as a tuple drops them, while
//! the named one stays
//!
//! ## a borrow reads through its lender
//!
//! the borrow pass lets a register hold a value on loan from one its defining operation
//! read — a copy's source, a field read's receiver, the tuple an element came out of.
//! the lender's reference is what keeps the borrowed value alive, so a read of the
//! borrower counts as a read of every register that could be lending to it, and a
//! lender stays until its borrowers are done
//!
//! ## a release can run anything
//!
//! dropping the last reference to a value runs its finalizer, which is arbitrary
//! python. the borrow pass proved each borrowed register's window free of anything that
//! could run python, so no release is placed inside one: it waits for the window to
//! close, which it always does before the block ends
//!
//! ## the error edge
//!
//! a handler can read a register the block it covers is otherwise done with, so what a
//! handler reads is live at every point of that block, exactly as it is for the
//! in-place append. and a value the block was holding when it raised is dropped once
//! the handler has taken the exception, which is where python's unwinding has dropped
//! it by

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, HashMap, HashSet};

use by_ir::function::{BasicBlock, Function, ModuleIr};
use by_ir::ops::{BlockId, Op, RegisterId, Value};
use by_ir::rtype::RType;

use crate::liveness;

pub(crate) fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        release_temporaries(function);
    }
}

/// one reference this pass can let go of on its own: a whole register, or one element
/// of a fixed-length tuple a register holds as a struct, as a path of elements
/// outermost first
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Place {
    register: RegisterId,
    path: Vec<usize>,
}

/// the elements of one register go last to first, so that where several are let go of at
/// one point it is in the order a `tuple` drops its elements in
impl Ord for Place {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.register, Reverse(&self.path)).cmp(&(other.register, Reverse(&other.path)))
    }
}

impl PartialOrd for Place {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// every reference a value of `ty` holds, as the path to each: one per element of a
/// fixed-length tuple, into nested ones too, and otherwise the value itself
fn leaves(ty: &RType, prefix: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
    match ty {
        RType::Tuple(items) => {
            for (index, item) in items.iter().enumerate() {
                prefix.push(index);
                leaves(item, prefix, out);
                prefix.pop();
            }
        }
        _ => out.push(prefix.clone()),
    }
}

/// every place under `prefix` in `register`'s value
fn places_under(function: &Function, register: RegisterId, prefix: &[usize]) -> Vec<Place> {
    let Some(ty) = function
        .register(register)
        .and_then(|decl| decl.ty.element(prefix))
    else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    leaves(ty, &mut prefix.to_vec(), &mut paths);
    paths
        .into_iter()
        .map(|path| Place { register, path })
        .collect()
}

/// every place a register's value is made of
fn places_of(function: &Function, register: RegisterId) -> Vec<Place> {
    places_under(function, register, &[])
}

/// the places an operation reads: an element read off a tuple reads that element and
/// nothing else of it, and any other read of a register reads all of it
fn read_places(function: &Function, op: &Op) -> Vec<Place> {
    if let Op::TupleGet {
        src: Value::Register(src),
        index,
        ..
    } = op
    {
        return places_under(function, *src, &[*index]);
    }
    liveness::read_registers(op)
        .into_iter()
        .flat_map(|register| places_of(function, register))
        .collect()
}

fn release_temporaries(function: &mut Function) {
    let candidates = candidates(function);
    if candidates.is_empty() {
        return;
    }
    // a terminator never reads a borrowed register — returning one would hand out a
    // reference the frame does not own, and the borrow pass refuses it — so the
    // lending below only has to be followed through operations. a function where that
    // does not hold is left as it was rather than reasoned about
    let borrowed: HashSet<RegisterId> = function
        .registers
        .iter()
        .enumerate()
        .filter(|(_, decl)| decl.borrowed)
        .map(|(index, _)| RegisterId(index))
        .collect();
    if function.blocks.iter().any(|block| {
        block
            .terminator
            .operands()
            .into_iter()
            .filter_map(liveness::register)
            .any(|register| borrowed.contains(&register))
    }) {
        return;
    }

    let lenders = lenders(function, &borrowed);
    let reads = |op: &Op| through_lenders(function, read_places(function, op), &lenders);
    let whole = |register: RegisterId| places_of(function, register);
    let live_in = liveness::live_in_by(function, &reads, &whole);

    let mut releases: BTreeSet<(BlockId, usize, Place)> = BTreeSet::new();
    for (index, block) in function.blocks.iter().enumerate() {
        let id = BlockId(index);
        let windows = borrow_windows(block, &borrowed);
        let error_live = liveness::error_live(block, &live_in);
        let mut live = liveness::live_out_by(block, &live_in, &whole);
        for (position, op) in block.ops.iter().enumerate().rev() {
            // an exception leaves from the middle of the block, so what the handler
            // reads is live after every operation in it
            live.extend(error_live.iter().cloned());
            let touched = reads(op)
                .into_iter()
                .chain(op.dest().into_iter().flat_map(whole));
            for place in touched {
                if candidates.contains(&place) && !live.contains(&place) {
                    releases.insert((id, clear_of(&windows, position + 1), place));
                }
            }
            if let Some(dest) = op.dest() {
                for place in whole(dest) {
                    live.remove(&place);
                }
            }
            live.extend(reads(op));
        }
    }
    for (id, place, at) in entered_dead(function, &live_in, &candidates) {
        if let Some(block) = function.block(id) {
            let windows = borrow_windows(block, &borrowed);
            releases.insert((id, clear_of(&windows, at), place));
        }
    }

    // inserted from the back of each block, so an earlier position still means what it
    // meant when it was chosen
    for (id, at, place) in releases.into_iter().rev() {
        if let Some(block) = function.blocks.get_mut(id.index()) {
            let at = at.min(block.ops.len());
            block.ops.insert(
                at,
                Op::Release {
                    value: Value::Register(place.register),
                    path: place.path.into(),
                },
            );
        }
    }
}

/// the places this pass may release: those of temporaries that own one reference apiece
///
/// a tagged `int` is refcounted as well, but an `int` has no finalizer to run, so it is
/// left for the frame to let go of, inside a tuple as much as on its own
fn candidates(function: &Function) -> HashSet<Place> {
    // a loop's cursor is read and written by the operation that advances it, outside
    // the operands this pass follows, so it is never offered
    let cursors: HashSet<RegisterId> = function
        .blocks
        .iter()
        .flat_map(|block| &block.ops)
        .filter_map(Op::loop_cursor)
        .collect();
    (function.param_count..function.registers.len())
        .map(RegisterId)
        .filter(|register| !cursors.contains(register))
        .filter_map(|register| Some((register, function.register(register)?)))
        .filter(|(_, decl)| decl.name.is_none() && !decl.borrowed)
        .flat_map(|(register, decl)| {
            places_of(function, register).into_iter().filter(|place| {
                decl.ty
                    .element(&place.path)
                    .is_some_and(RType::is_object_reference)
            })
        })
        .collect()
}

/// where a borrowed register's value is on loan from
enum Loan {
    /// the value at `at` inside `of`, which a place inside the borrower is a place
    /// inside too: a copy of a register, or an element read off a tuple
    View { of: RegisterId, at: Vec<usize> },
    /// somewhere among these
    Among(Vec<Place>),
}

/// where each borrowed register could be holding its value on loan from
///
/// every write of it is followed, which over-counts rather than under-counts: a lender
/// held a little longer costs nothing, and one let go early is a value freed under a
/// register still reading it. an element read off a tuple borrows from that element
/// alone, which is what lets the rest of the tuple go
fn lenders(function: &Function, borrowed: &HashSet<RegisterId>) -> HashMap<RegisterId, Vec<Loan>> {
    let mut lenders: HashMap<RegisterId, Vec<Loan>> = HashMap::new();
    for op in function.blocks.iter().flat_map(|block| &block.ops) {
        let Some(dest) = op.dest().filter(|dest| borrowed.contains(dest)) else {
            continue;
        };
        let loan = match op {
            Op::TupleGet {
                src: Value::Register(src),
                index,
                ..
            } => Loan::View {
                of: *src,
                at: vec![*index],
            },
            Op::Assign {
                src: Value::Register(src),
                ..
            } => Loan::View {
                of: *src,
                at: Vec::new(),
            },
            other => Loan::Among(read_places(function, other)),
        };
        lenders.entry(dest).or_default().push(loan);
    }
    lenders
}

/// `reads`, with every place a borrowed register among them could be lending from
///
/// a place inside a view is a place inside what it views, where the two agree on the
/// layout. where they do not, the whole of the lender is read
fn through_lenders(
    function: &Function,
    reads: Vec<Place>,
    lenders: &HashMap<RegisterId, Vec<Loan>>,
) -> Vec<Place> {
    let mut seen: HashSet<Place> = HashSet::new();
    let mut pending = reads;
    while let Some(place) = pending.pop() {
        if seen.contains(&place) {
            continue;
        }
        for loan in lenders.get(&place.register).into_iter().flatten() {
            match loan {
                Loan::View { of, at } => {
                    let path: Vec<usize> = at.iter().chain(&place.path).copied().collect();
                    let inside = function
                        .register(*of)
                        .and_then(|decl| decl.ty.element(&path))
                        .is_some();
                    if inside {
                        pending.push(Place {
                            register: *of,
                            path,
                        });
                    } else {
                        pending.extend(places_of(function, *of));
                    }
                }
                Loan::Among(places) => pending.extend(places.iter().cloned()),
            }
        }
        seen.insert(place);
    }
    seen.into_iter().collect()
}

/// the stretches of a block no release may be placed inside, as `(write, last read)`
///
/// a release placed before operation `at` is inside a window when `write < at <= last
/// read`. a borrowed register the block reads without writing is held from the
/// block's start
fn borrow_windows(
    block: &BasicBlock,
    borrowed: &HashSet<RegisterId>,
) -> Vec<(Option<usize>, usize)> {
    borrowed
        .iter()
        .filter_map(|register| {
            let reads = |op: &Op| {
                op.operands()
                    .into_iter()
                    .filter_map(liveness::register)
                    .any(|read| read == *register)
            };
            let last_read = block.ops.iter().rposition(reads)?;
            let write = block.ops.iter().position(|op| op.dest() == Some(*register));
            Some((write.filter(|write| *write < last_read), last_read))
        })
        .collect()
}

/// the first position at or after `at` that no window covers
fn clear_of(windows: &[(Option<usize>, usize)], mut at: usize) -> usize {
    while let Some(&(_, last_read)) = windows
        .iter()
        .find(|(write, last_read)| write.is_none_or(|write| write < at) && at <= *last_read)
    {
        at = last_read + 1;
    }
    at
}

/// the candidates a block is entered holding and does not need, with where to drop them
///
/// a place one successor goes on to read and another does not is still holding a
/// value on the edge to the second. a handler is entered holding whatever the block it
/// covers had written, and it drops them once it has taken the exception: a finalizer
/// run while an exception is still pending would find it there
fn entered_dead(
    function: &Function,
    live_in: &[HashSet<Place>],
    candidates: &HashSet<Place>,
) -> Vec<(BlockId, Place, usize)> {
    let whole = |register: RegisterId| places_of(function, register);
    let mut holding: Vec<HashSet<Place>> = vec![HashSet::new(); function.blocks.len()];
    for block in &function.blocks {
        let out = liveness::live_out_by(block, live_in, &whole);
        for successor in block.successors() {
            if let Some(slot) = holding.get_mut(successor.index()) {
                slot.extend(out.iter().cloned());
            }
        }
        if let Some(handler) = block.error_target
            && let Some(slot) = holding.get_mut(handler.index())
        {
            slot.extend(block.ops.iter().flat_map(|op| {
                read_places(function, op)
                    .into_iter()
                    .chain(op.dest().into_iter().flat_map(whole))
            }));
        }
    }

    let handlers: HashSet<BlockId> = function
        .blocks
        .iter()
        .filter_map(|block| block.error_target)
        .collect();
    let mut out = Vec::new();
    for (index, held) in holding.iter().enumerate() {
        let id = BlockId(index);
        let Some(block) = function.block(id) else {
            continue;
        };
        let at = if handlers.contains(&id) {
            match block.ops.first() {
                Some(Op::FetchException { .. }) => 1,
                // a handler that does not begin by taking the exception is left alone
                _ => continue,
            }
        } else {
            0
        };
        let needed = live_in.get(index);
        let mut dropped: Vec<Place> = held
            .iter()
            .filter(|place| candidates.contains(place))
            .filter(|place| needed.is_none_or(|needed| !needed.contains(place)))
            .cloned()
            .collect();
        dropped.sort_unstable();
        out.extend(dropped.into_iter().map(|place| (id, place, at)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::Terminator;
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
            verify_install: true,
            follow_recursion_limit: true,
        }
    }

    fn call(dest: RegisterId, callee: &str, arg: RegisterId) -> Op {
        Op::CallPython {
            dest,
            callee: callee.to_string(),
            args: vec![Value::Register(arg)],
        }
    }

    /// where in `block` each release sits, and of which register
    fn releases(module: &ModuleIr, block: usize) -> Vec<(usize, RegisterId)> {
        module.functions[0].blocks[block]
            .ops
            .iter()
            .enumerate()
            .filter_map(|(at, op)| match op {
                Op::Release {
                    value: Value::Register(id),
                    path,
                } if path.is_empty() => Some((at, *id)),
                _ => None,
            })
            .collect()
    }

    /// every release in `block`: where it sits, of which register, and of which element
    fn every_release(module: &ModuleIr, block: usize) -> Vec<(usize, RegisterId, Vec<usize>)> {
        module.functions[0].blocks[block]
            .ops
            .iter()
            .enumerate()
            .filter_map(|(at, op)| match op {
                Op::Release {
                    value: Value::Register(id),
                    path,
                } => Some((at, *id, path.to_vec())),
                _ => None,
            })
            .collect()
    }

    /// the layout `pair(i) -> tuple[Loud, Loud]` answers with
    fn pair() -> RType {
        RType::Tuple(Box::new([RType::OBJECT, RType::OBJECT]))
    }

    /// a tuple of `items`, which retains each
    fn build(dest: RegisterId, items: &[RegisterId]) -> Op {
        Op::TupleBuild {
            dest,
            items: items.iter().copied().map(Value::Register).collect(),
        }
    }

    #[test]
    fn a_tuple_nothing_reads_drops_its_elements_last_to_first() {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(pair());
        builder.push(build(made, &[p, p]));
        builder.terminate(Terminator::Return(Value::Register(p)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert_eq!(
            every_release(&module, 0),
            vec![(1, made, vec![1]), (2, made, vec![0])],
            "{:?}",
            module.functions[0].blocks
        );
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn a_name_given_one_element_holds_a_reference_of_its_own() {
        // `part = pair(p)[0]`: python drops the tuple as soon as the subscript is done,
        // and the name goes on holding the element it was given. so the tuple lets go
        // of the element nobody reads once it is built, and of the named one once it
        // has been read
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(pair());
        let part = builder.local("part".to_string(), RType::OBJECT);
        builder.push(build(made, &[p, p]));
        builder.push(Op::TupleGet {
            dest: part,
            src: Value::Register(made),
            index: 0,
        });
        builder.terminate(Terminator::Return(Value::Register(part)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert_eq!(
            every_release(&module, 0),
            vec![(1, made, vec![1]), (3, made, vec![0])],
            "{:?}",
            module.functions[0].blocks
        );
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn an_element_on_loan_is_held_until_the_borrower_is_done() {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(pair());
        let first = builder.temp(RType::OBJECT);
        let answer = builder.temp(RType::OBJECT);
        builder.push(build(made, &[p, p]));
        builder.push(Op::TupleGet {
            dest: first,
            src: Value::Register(made),
            index: 0,
        });
        builder.push(call(answer, "len", first));
        builder.terminate(Terminator::Return(Value::Register(answer)));
        let mut function = builder.finish();
        function.registers[first.index()].borrowed = true;

        let mut module = module(function);
        run(&mut module);

        // the element nobody reads goes as soon as the tuple is made, and the one on
        // loan once the call reading it is done. the borrower owns nothing to let go of
        assert_eq!(
            every_release(&module, 0),
            vec![(1, made, vec![1]), (4, made, vec![0])],
            "{:?}",
            module.functions[0].blocks
        );
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn a_tuple_on_loan_owns_nothing_to_let_go_of() {
        // `nested(p)[0][1]`: the inner tuple is read off the outer one on loan, so its
        // elements are the outer tuple's references. only the outer tuple lets go, and
        // what the element read off the loan goes on reading is one element deep
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let inner = builder.temp(pair());
        let outer = builder.temp(RType::Tuple(Box::new([pair(), RType::OBJECT])));
        let lent = builder.temp(pair());
        let answer = builder.temp(RType::OBJECT);
        let shown = builder.temp(RType::OBJECT);
        builder.push(build(inner, &[p, p]));
        builder.push(build(outer, &[inner, p]));
        builder.push(Op::TupleGet {
            dest: lent,
            src: Value::Register(outer),
            index: 0,
        });
        builder.push(Op::TupleGet {
            dest: answer,
            src: Value::Register(lent),
            index: 1,
        });
        builder.push(call(shown, "str", answer));
        builder.terminate(Terminator::Return(Value::Register(shown)));
        let mut function = builder.finish();
        function.registers[lent.index()].borrowed = true;
        function.registers[answer.index()].borrowed = true;

        let mut module = module(function);
        run(&mut module);

        // the inner tuple's first element is read by nothing past the loan, but no
        // release goes inside a loan's window, so it waits for the call that reads its
        // second, and the two then go last to first as the inner tuple would drop them
        assert_eq!(
            every_release(&module, 0),
            vec![
                (2, inner, vec![1]),
                (3, inner, vec![0]),
                (4, outer, vec![1]),
                (8, outer, vec![0, 1]),
                (9, outer, vec![0, 0]),
            ],
            "{:?}",
            module.functions[0].blocks
        );
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn a_temporary_is_released_after_its_last_read() {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        let answer = builder.temp(RType::OBJECT);
        builder.push(call(made, "repr", p));
        builder.push(call(answer, "len", made));
        builder.terminate(Terminator::Return(Value::Register(answer)));

        let mut module = module(builder.finish());
        run(&mut module);

        // the returned value is read by the return itself, so it is not released
        assert_eq!(
            releases(&module, 0),
            vec![(2, made)],
            "{:?}",
            module.functions[0].blocks
        );
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn a_named_local_is_held_to_the_end() {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.local("made".to_string(), RType::OBJECT);
        let answer = builder.temp(RType::OBJECT);
        builder.push(call(made, "repr", p));
        builder.push(call(answer, "len", made));
        builder.terminate(Terminator::Return(Value::Register(answer)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert_eq!(releases(&module, 0), Vec::new());
    }

    #[test]
    fn a_lender_is_held_until_its_borrower_is_done() {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let made = builder.temp(RType::OBJECT);
        let copy = builder.temp(RType::OBJECT);
        let answer = builder.temp(RType::OBJECT);
        builder.push(call(made, "repr", p));
        builder.assign(copy, Value::Register(made));
        builder.push(call(answer, "len", copy));
        builder.terminate(Terminator::Return(Value::Register(answer)));
        let mut function = builder.finish();
        function.registers[copy.index()].borrowed = true;

        let mut module = module(function);
        run(&mut module);

        // `made` is last read directly by the copy, but the copy reads through it, so
        // it goes after the copy's own last read — and the copy owns nothing to let go
        assert_eq!(
            releases(&module, 0),
            vec![(3, made)],
            "{:?}",
            module.functions[0].blocks
        );
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn no_release_lands_inside_a_borrow_window() {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let early = builder.temp(RType::OBJECT);
        let lent = builder.temp(RType::OBJECT);
        let copy = builder.temp(RType::OBJECT);
        let shown = builder.temp(RType::OBJECT);
        let answer = builder.temp(RType::OBJECT);
        builder.push(call(early, "repr", p));
        builder.push(call(lent, "str", p));
        builder.assign(copy, Value::Register(lent));
        // the last read of `early`, inside the window the copy is borrowing across
        builder.push(call(shown, "str", early));
        builder.push(call(answer, "len", copy));
        builder.terminate(Terminator::Return(Value::Register(answer)));
        let mut function = builder.finish();
        function.registers[copy.index()].borrowed = true;

        let mut module = module(function);
        run(&mut module);

        // both wait for the window to close after the copy's last read, at 4
        let placed = releases(&module, 0);
        assert!(
            placed.iter().all(|(at, _)| *at > 4),
            "{placed:?} in {:?}",
            module.functions[0].blocks
        );
        assert!(placed.iter().any(|(_, id)| *id == early), "{placed:?}");
        assert!(placed.iter().any(|(_, id)| *id == lent), "{placed:?}");
        assert!(placed.iter().any(|(_, id)| *id == shown), "{placed:?}");
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }

    #[test]
    fn a_value_one_successor_needs_is_dropped_entering_the_other() {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param("p", RType::OBJECT);
        let cond = builder.param("c", RType::BIT);
        let made = builder.temp(RType::OBJECT);
        let answer = builder.temp(RType::OBJECT);
        let reads = builder.new_block();
        let skips = builder.new_block();
        builder.push(call(made, "repr", p));
        builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block: reads,
            else_block: skips,
        });
        builder.switch_to(reads);
        builder.push(call(answer, "len", made));
        builder.terminate(Terminator::Return(Value::Register(answer)));
        builder.switch_to(skips);
        builder.terminate(Terminator::Return(Value::Register(p)));

        let mut module = module(builder.finish());
        run(&mut module);

        assert_eq!(releases(&module, reads.index()), vec![(1, made)]);
        assert_eq!(releases(&module, skips.index()), vec![(0, made)]);
        assert_eq!(verify(&module.functions[0]), Ok(()));
    }
}

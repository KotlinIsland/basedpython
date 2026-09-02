//! constant folding, and the branch folding it enables
//!
//! folding an operation on two immediates is worth little on its own — the C
//! compiler does it too. it is worth doing here because it feeds the *branch*
//! fold: once a condition is a known bit, one arm of the branch becomes
//! unreachable, and the reachability of a block is something only this IR knows.
//!
//! nothing here may fold an operation that could raise. `1 // 0` stays a division
//! so that it still raises `ZeroDivisionError` at the right point in the program.

use std::collections::{HashMap, HashSet};

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{BinOp, CmpOp, Op, RegisterId, Terminator, UnaryOp, Value};
use by_ir::rtype::RType;

pub fn run(module: &mut ModuleIr) {
    // a `frozen` class's field cannot change after the constructor wrote it, so two
    // reads of one are the same read — *across an arbitrary call*, which is the part
    // an optimizer without the type system's word for it cannot assume
    let frozen: HashSet<String> = module
        .classes
        .iter()
        .filter(|class| class.immutable)
        .map(|class| class.name.clone())
        .collect();
    for function in module.all_functions_mut() {
        hoist_immutable_reads(function, &frozen);
        fold(function, &frozen);
    }
}

/// read an immutable field of a *parameter* once, at entry
///
/// this is loop-invariant code motion without needing to name a loop. two facts do
/// the work and both come from the type system rather than from an analysis: the
/// field cannot change, so the read gives the same answer wherever it happens; and
/// `GetField` cannot fail, so doing it on a path that would not have reached it
/// costs a retain and observes nothing.
///
/// the receiver has to be a *parameter* — those are live from entry, so the entry
/// block is somewhere the read is certainly valid. that is what stands in for the
/// preheader a real loop pass would have to find
fn hoist_immutable_reads(function: &mut Function, immutable: &HashSet<String>) {
    // a write to the field anywhere gives up on it: the fold rests on what the ops
    // do, not on the declaration alone
    let written: HashSet<(String, String)> = function
        .blocks
        .iter()
        .flat_map(|block| &block.ops)
        .filter_map(|op| match op {
            Op::SetField { class, field, .. } => Some((class.clone(), field.clone())),
            _ => None,
        })
        .collect();

    let mut hoistable: Vec<(RegisterId, String, String, RType)> = Vec::new();
    for (index, block) in function.blocks.iter().enumerate() {
        if index == 0 {
            continue;
        }
        for op in &block.ops {
            let Op::GetField {
                dest,
                receiver: Value::Register(receiver),
                class,
                field,
            } = op
            else {
                continue;
            };
            if !immutable.contains(class)
                || receiver.index() >= function.param_count
                || written.contains(&(class.clone(), field.clone()))
            {
                continue;
            }
            let Some(ty) = function.register(*dest).map(|decl| decl.ty.clone()) else {
                continue;
            };
            let entry = (*receiver, class.clone(), field.clone(), ty);
            if !hoistable.contains(&entry) {
                hoistable.push(entry);
            }
        }
    }
    if hoistable.is_empty() {
        return;
    }

    let mut prelude = Vec::with_capacity(hoistable.len());
    let mut held: HashMap<(RegisterId, String, String), RegisterId> = HashMap::new();
    for (receiver, class, field, ty) in hoistable {
        let dest = RegisterId(function.registers.len());
        function.registers.push(by_ir::function::RegisterDecl {
            name: None,
            ty,
            borrowed: false,
            may_be_unassigned: false,
        });
        prelude.push(Op::GetField {
            dest,
            receiver: Value::Register(receiver),
            class: class.clone(),
            field: field.clone(),
        });
        held.insert((receiver, class, field), dest);
    }

    for (index, block) in function.blocks.iter_mut().enumerate() {
        if index == 0 {
            continue;
        }
        for op in &mut block.ops {
            let Op::GetField {
                dest,
                receiver: Value::Register(receiver),
                class,
                field,
            } = op
            else {
                continue;
            };
            if let Some(source) = held.get(&(*receiver, class.clone(), field.clone())) {
                *op = Op::Assign {
                    dest: *dest,
                    src: Value::Register(*source),
                };
            }
        }
    }
    if let Some(entry) = function.blocks.first_mut() {
        prelude.append(&mut entry.ops);
        entry.ops = prelude;
    }
}

fn fold(function: &mut Function, frozen: &HashSet<String>) {
    // the redundant-box fold needs the register table, which the borrow checker
    // will not lend out while the blocks are borrowed mutably
    let types: Vec<RType> = function
        .registers
        .iter()
        .map(|decl| decl.ty.clone())
        .collect();
    for block in &mut function.blocks {
        // the python tuples this block builds, so an unpack of one can take its
        // items directly instead of driving an iterator over them
        let mut built: HashMap<RegisterId, Vec<Value>> = HashMap::new();
        // and the reads of a frozen field it has already done
        let mut read: HashMap<(RegisterId, String, String), RegisterId> = HashMap::new();
        // what each register in this block was boxed *from*, so an unbox back to that
        // representation is the round trip it undoes
        let mut boxed_from: HashMap<RegisterId, Value> = HashMap::new();
        for op in &mut block.ops {
            if let Some(folded) = fold_op(op)
                .or_else(|| fold_box(&types, op))
                .or_else(|| fold_unbox(&types, op))
                .or_else(|| fold_box_round_trip(&types, &boxed_from, op))
                .or_else(|| fold_unpack(&types, &built, op))
                .or_else(|| fold_tuple_get(&built, op))
                .or_else(|| fold_get_item(&types, &built, op))
                .or_else(|| fold_frozen_read(frozen, &read, op))
            {
                *op = folded;
            }
            // a write invalidates the entry it lands in *and* every entry whose
            // items name it: `a, b = b, a` boxes both sides before assigning
            // either, and forwarding the second read after the first assignment
            // would hand back the value that assignment just wrote
            if let Some(dest) = op.dest() {
                built.remove(&dest);
                built.retain(|_, items| {
                    !items
                        .iter()
                        .any(|item| matches!(item, Value::Register(id) if *id == dest))
                });
                // a read is only reusable while both the receiver it came from and
                // the register holding it still say what they said
                read.retain(|(receiver, _, _), held| *receiver != dest && *held != dest);
                // and a box remembers a *register*, so rewriting either end of the
                // pair makes what it remembers no longer true
                boxed_from.remove(&dest);
                boxed_from
                    .retain(|_, source| !matches!(source, Value::Register(id) if *id == dest));
            }
            // a write to the field invalidates every read of it, whatever the
            // declaration says: the fold rests on what the ops do, not on trust
            if let Op::SetField { class, field, .. } = op {
                let (class, field) = (class.clone(), field.clone());
                read.retain(|(_, owner, name), _| *owner != class || *name != field);
            }
            match op {
                Op::BuildTuple { dest, items } | Op::TupleBuild { dest, items } => {
                    built.insert(*dest, items.clone());
                }
                Op::GetField {
                    dest,
                    receiver: Value::Register(receiver),
                    class,
                    field,
                } if frozen.contains(class) => {
                    read.insert((*receiver, class.clone(), field.clone()), *dest);
                }
                Op::Box {
                    dest,
                    src: source @ Value::Register(_),
                }
                | Op::Assign {
                    dest,
                    src: source @ Value::Register(_),
                } => {
                    boxed_from.insert(*dest, source.clone());
                }
                _ => {}
            }
        }
        if let Some(folded) = fold_terminator(&block.terminator) {
            block.terminator = folded;
        }
    }
}

/// an operation on immediates becomes an assignment of the result
fn fold_op(op: &Op) -> Option<Op> {
    match op {
        Op::IntBinary {
            dest,
            op: binop,
            lhs: Value::Int(lhs),
            rhs: Value::Int(rhs),
        } => fold_int_binary(*binop, *lhs, *rhs).map(|value| Op::Assign {
            dest: *dest,
            src: Value::Int(value),
        }),
        Op::IntCompare {
            dest,
            op: cmp,
            lhs: Value::Int(lhs),
            rhs: Value::Int(rhs),
        } => Some(Op::Assign {
            dest: *dest,
            src: Value::Bit(compare_ints(*cmp, *lhs, *rhs)),
        }),
        Op::FloatBinary {
            dest,
            op: binop,
            lhs: Value::Float(lhs),
            rhs: Value::Float(rhs),
        } => fold_float_binary(*binop, *lhs, *rhs).map(|value| Op::Assign {
            dest: *dest,
            src: Value::Float(value),
        }),
        Op::Unary {
            dest,
            op: UnaryOp::Not,
            operand: Value::Bit(value),
        } => Some(Op::Assign {
            dest: *dest,
            src: Value::Bit(!value),
        }),
        Op::Unary {
            dest,
            op: UnaryOp::Neg,
            operand: Value::Float(value),
        } => Some(Op::Assign {
            dest: *dest,
            src: Value::Float(-value),
        }),
        _ => None,
    }
}

/// boxing a value that is already a `PyObject *` is a copy
///
/// `str`, `list`, `dict` — all of them are pointers already, so the `box` does
/// nothing but retain and release. rewriting it as an assignment is what lets copy
/// propagation delete it outright.
///
/// a native class is excluded: its pointer is to its own struct, so widening it to
/// `object` is a C cast, and `Box` is where that cast lives
fn fold_box(types: &[RType], op: &Op) -> Option<Op> {
    let Op::Box { dest, src } = op else {
        return None;
    };
    let src_ty = match src {
        Value::Register(id) => types.get(id.index())?.clone(),
        other => other.immediate_type()?,
    };
    if src_ty.is_unboxed() || matches!(src_ty, RType::Instance { .. }) {
        return None;
    }
    Some(Op::Assign {
        dest: *dest,
        src: src.clone(),
    })
}

/// a *narrowing* whose source already has the representation being narrowed to
///
/// this is the sound half of "skip the check discipline in a fully typed module".
/// the checks that establish the representation invariant — at the wrapper, at
/// `tp_init`, after a call *out* of the unit — are load-bearing, and no flag makes
/// them redundant: `--no-any` says this module is fully typed, and says nothing
/// about what code we did not compile hands back.
///
/// what *is* redundant is a narrowing whose source is already narrow. that is not a
/// contract to trust, it is a fact about the register, so the fold needs no flag and
/// applies everywhere. an `Instance` is excluded for the same reason `Box` is: the
/// unbox is where the pointer cast lives
/// `box x` then `unbox` back to `x`'s own representation is `x`
///
/// copy propagation cannot reach this one: substituting the source for the widened
/// temporary would hand its *other* readers a narrower representation than they asked
/// for, so the pair has to be recognised together. the box may already have become an
/// assign — [`fold_box`] does that where the value was a pointer all along — which is
/// why both forms are recorded
fn fold_box_round_trip(
    types: &[RType],
    boxed_from: &HashMap<RegisterId, Value>,
    op: &Op,
) -> Option<Op> {
    let Op::Unbox { dest, src, to } = op else {
        return None;
    };
    if matches!(to, RType::Instance { .. }) {
        return None;
    }
    let Value::Register(id) = src else {
        return None;
    };
    let source = boxed_from.get(id)?;
    let source_ty = match source {
        Value::Register(id) => types.get(id.index())?.clone(),
        other => other.immediate_type()?,
    };
    (source_ty == *to).then(|| Op::Assign {
        dest: *dest,
        src: source.clone(),
    })
}

fn fold_unbox(types: &[RType], op: &Op) -> Option<Op> {
    let Op::Unbox { dest, src, to } = op else {
        return None;
    };
    if matches!(to, RType::Instance { .. }) {
        return None;
    }
    let src_ty = match src {
        Value::Register(id) => types.get(id.index())?.clone(),
        other => other.immediate_type()?,
    };
    (src_ty == *to).then(|| Op::Assign {
        dest: *dest,
        src: src.clone(),
    })
}

/// unpacking a tuple this block just built is a *move*
///
/// `a, b = b, a` builds a python tuple and drives an iterator over it, and both are
/// this module's own doing — the items are right here, with their arity known. this
/// is the in-unit provenance the check discipline is allowed to trust: not a
/// declaration to take on faith, but an op a few lines up
fn fold_unpack(types: &[RType], built: &HashMap<RegisterId, Vec<Value>>, op: &Op) -> Option<Op> {
    let Op::Unpack {
        dest,
        src: Value::Register(src),
        starred: None,
    } = op
    else {
        return None;
    };
    let items = built.get(src)?;
    let RType::Tuple(slots) = types.get(dest.index())? else {
        return None;
    };
    // a mismatched arity is a `ValueError` at runtime, and reporting it is the
    // unpack's job. and every item has to be an `object` already — the slots are,
    // and a fold may not invent a widening
    if slots.len() != items.len() {
        return None;
    }
    let boxed = |value: &Value| {
        let ty = match value {
            Value::Register(id) => types.get(id.index()).cloned(),
            other => other.immediate_type(),
        };
        ty.is_some_and(|ty| !ty.is_unboxed() && !matches!(ty, RType::Instance { .. }))
    };
    items.iter().all(boxed).then(|| Op::TupleBuild {
        dest: *dest,
        items: items.clone(),
    })
}

/// reading an element of a tuple this block just built is a *move*
///
/// with [`fold_unpack`] this collapses the whole of `a, b = b, a`: no python tuple,
/// no iterator, and — once the copy is propagated — no narrowing either, because the
/// element is the register it came from
fn fold_tuple_get(built: &HashMap<RegisterId, Vec<Value>>, op: &Op) -> Option<Op> {
    let Op::TupleGet {
        dest,
        src: Value::Register(src),
        index,
    } = op
    else {
        return None;
    };
    let item = built.get(src)?.get(*index)?;
    Some(Op::Assign {
        dest: *dest,
        src: item.clone(),
    })
}

/// `pair[0]`, where `pair` is a tuple this block just built
///
/// the move [`fold_tuple_get`] makes, for the *subscript* rather than for the
/// unpack. a tuple is immutable, so an element of one built a few lines up is the
/// value that went into it whoever else is holding the tuple by then — which is why
/// this needs no escape analysis, only the guarantee that neither the tuple nor the
/// item has been written since
///
/// a negative index counts from the end, as python's own subscript does. an index
/// outside the tuple is left alone: raising `IndexError`, in python's wording, is
/// the subscript's job and it already does it
fn fold_get_item(types: &[RType], built: &HashMap<RegisterId, Vec<Value>>, op: &Op) -> Option<Op> {
    let Op::GetItem {
        dest,
        container: Value::Register(container),
        index: Value::Int(index),
    } = op
    else {
        return None;
    };
    let items = built.get(container)?;
    let length = i64::try_from(items.len()).ok()?;
    let at = if *index < 0 {
        index.checked_add(length)?
    } else {
        *index
    };
    let item = items.get(usize::try_from(at).ok()?)?;
    // a fold may not invent a conversion, so the item has to have the representation
    // the read was going to produce already
    let dest_ty = types.get(dest.index())?;
    let item_ty = match item {
        Value::Register(id) => types.get(id.index()).cloned(),
        other => other.immediate_type(),
    }?;
    (item_ty == *dest_ty).then(|| Op::Assign {
        dest: *dest,
        src: item.clone(),
    })
}

/// reading a `frozen` field this block has already read is a *copy*
///
/// what makes it sound is the type system rather than the analysis: a frozen class
/// has no setters, so nothing between the two reads can change the field — not an
/// assignment, and not an arbitrary call. an optimizer that has to assume any call
/// may mutate any object has to reload
fn fold_frozen_read(
    frozen: &HashSet<String>,
    read: &HashMap<(RegisterId, String, String), RegisterId>,
    op: &Op,
) -> Option<Op> {
    let Op::GetField {
        dest,
        receiver: Value::Register(receiver),
        class,
        field,
    } = op
    else {
        return None;
    };
    if !frozen.contains(class) {
        return None;
    }
    let held = read.get(&(*receiver, class.clone(), field.clone()))?;
    Some(Op::Assign {
        dest: *dest,
        src: Value::Register(*held),
    })
}

/// a branch on a known bit becomes a jump
fn fold_terminator(terminator: &Terminator) -> Option<Terminator> {
    match terminator {
        Terminator::Branch {
            cond: Value::Bit(taken),
            then_block,
            else_block,
        } => Some(Terminator::Goto(if *taken {
            *then_block
        } else {
            *else_block
        })),
        // a `bool` immediate is a valid condition too
        Terminator::Branch {
            cond: Value::Bool(taken),
            then_block,
            else_block,
        } => Some(Terminator::Goto(if *taken {
            *then_block
        } else {
            *else_block
        })),
        _ => None,
    }
}

/// `None` where the operation could raise, or could leave the immediate range
fn fold_int_binary(op: BinOp, lhs: i64, rhs: i64) -> Option<i64> {
    match op {
        BinOp::Add => lhs.checked_add(rhs),
        BinOp::Sub => lhs.checked_sub(rhs),
        BinOp::Mul => lhs.checked_mul(rhs),
        BinOp::BitAnd => Some(lhs & rhs),
        BinOp::BitOr => Some(lhs | rhs),
        BinOp::BitXor => Some(lhs ^ rhs),
        // `//` and `%` raise on a zero divisor, and python floors rather than
        // truncating — leaving them to the runtime keeps one implementation
        BinOp::FloorDiv | BinOp::Mod | BinOp::TrueDiv | BinOp::Pow => None,
        // a shift can leave the range, and a negative count raises
        BinOp::Shl | BinOp::Shr => None,
    }
}

fn fold_float_binary(op: BinOp, lhs: f64, rhs: f64) -> Option<f64> {
    match op {
        BinOp::Add => Some(lhs + rhs),
        BinOp::Sub => Some(lhs - rhs),
        BinOp::Mul => Some(lhs * rhs),
        // division by zero raises in `.by`, unlike IEEE
        _ => None,
    }
}

fn compare_ints(op: CmpOp, lhs: i64, rhs: i64) -> bool {
    match op {
        CmpOp::Eq => lhs == rhs,
        CmpOp::Ne => lhs != rhs,
        CmpOp::Lt => lhs < rhs,
        CmpOp::Le => lhs <= rhs,
        CmpOp::Gt => lhs > rhs,
        CmpOp::Ge => lhs >= rhs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{BlockId, RegisterId};
    use by_ir::print::print_function;
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
        }
    }

    #[test]
    fn arithmetic_on_two_immediates_folds() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let out = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: out,
            op: BinOp::Add,
            lhs: Value::Int(2),
            rhs: Value::Int(3),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        run(&mut m);
        let text = print_function(&m.functions[0]);
        assert!(text.contains("r0 = 5"), "{text}");
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn an_overflowing_fold_is_left_to_the_runtime() {
        // the tagged path would go to a PyLongObject; folding it into an i64
        // immediate would silently wrap
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let out = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: out,
            op: BinOp::Mul,
            lhs: Value::Int(i64::MAX),
            rhs: Value::Int(2),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(matches!(
            m.functions[0].blocks[0].ops[0],
            Op::IntBinary { .. }
        ));
    }

    #[test]
    fn a_division_is_never_folded() {
        // `1 // 0` must still raise, and at the right point in the program
        for op in [BinOp::FloorDiv, BinOp::Mod, BinOp::TrueDiv] {
            assert_eq!(fold_int_binary(op, 1, 0), None, "{op:?}");
            assert_eq!(fold_int_binary(op, 6, 3), None, "{op:?}");
        }
    }

    #[test]
    fn a_comparison_of_immediates_folds_to_a_bit() {
        let mut builder = FunctionBuilder::new("f", RType::BIT);
        let out = builder.temp(RType::BIT);
        builder.push(Op::IntCompare {
            dest: out,
            op: CmpOp::Lt,
            lhs: Value::Int(1),
            rhs: Value::Int(2),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(print_function(&m.functions[0]).contains("r0 = 1b"));
    }

    #[test]
    fn a_branch_on_a_known_bit_becomes_a_jump() {
        // this is the whole point of folding: a dead arm the C compiler cannot see
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let cond = builder.temp(RType::BIT);
        let then_block = builder.new_block();
        let else_block = builder.new_block();
        builder.push(Op::IntCompare {
            dest: cond,
            op: CmpOp::Lt,
            lhs: Value::Int(1),
            rhs: Value::Int(2),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block,
            else_block,
        });
        builder.switch_to(then_block);
        builder.terminate(Terminator::Return(Value::Int(1)));
        builder.switch_to(else_block);
        builder.terminate(Terminator::Return(Value::Int(0)));

        let mut m = module(builder.finish());
        // the first pass folds the comparison into a bit register, and a second
        // needs copy propagation to reach the branch — so fold only what it can
        run(&mut m);
        assert!(matches!(
            m.functions[0].blocks[0].terminator,
            Terminator::Branch { .. }
        ));

        // with the condition written as an immediate, the branch does fold
        let mut direct = FunctionBuilder::new("g", RType::INT);
        let a = direct.new_block();
        let b = direct.new_block();
        direct.terminate(Terminator::Branch {
            cond: Value::Bit(true),
            then_block: a,
            else_block: b,
        });
        direct.switch_to(a);
        direct.terminate(Terminator::Return(Value::Int(1)));
        direct.switch_to(b);
        direct.terminate(Terminator::Return(Value::Int(0)));
        let mut m = module(direct.finish());
        run(&mut m);
        assert_eq!(m.functions[0].blocks[0].terminator, Terminator::Goto(a));
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn a_false_condition_takes_the_other_arm() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.new_block();
        let b = builder.new_block();
        builder.terminate(Terminator::Branch {
            cond: Value::Bool(false),
            then_block: a,
            else_block: b,
        });
        builder.switch_to(a);
        builder.terminate(Terminator::Return(Value::Int(1)));
        builder.switch_to(b);
        builder.terminate(Terminator::Return(Value::Int(0)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert_eq!(m.functions[0].blocks[0].terminator, Terminator::Goto(b));
    }

    #[test]
    fn not_of_a_known_bit_folds() {
        let mut builder = FunctionBuilder::new("f", RType::BIT);
        let out = builder.temp(RType::BIT);
        builder.push(Op::Unary {
            dest: out,
            op: UnaryOp::Not,
            operand: Value::Bit(true),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(print_function(&m.functions[0]).contains("r0 = 0b"));
    }

    #[test]
    fn folding_leaves_block_indices_alone() {
        // a folded branch still references the same blocks, so nothing else has
        // to be renumbered
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.new_block();
        let b = builder.new_block();
        builder.terminate(Terminator::Branch {
            cond: Value::Bit(true),
            then_block: a,
            else_block: b,
        });
        builder.switch_to(a);
        builder.terminate(Terminator::Return(Value::Int(1)));
        builder.switch_to(b);
        builder.terminate(Terminator::Return(Value::Int(0)));
        let mut m = module(builder.finish());
        let before = m.functions[0].blocks.len();
        run(&mut m);
        assert_eq!(m.functions[0].blocks.len(), before);
        assert_eq!(a, BlockId(1));
        let _ = RegisterId(0);
    }

    #[test]
    fn boxing_a_str_is_a_copy() {
        // a `str` register already holds a `PyObject *`, so the box is a retain and
        // a release and nothing else
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let text = builder.param("s", RType::STR);
        let boxed = builder.temp(RType::OBJECT);
        let out = builder.temp(RType::INT);
        builder.push(Op::Box {
            dest: boxed,
            src: Value::Register(text),
        });
        builder.push(Op::Len {
            dest: out,
            src: Value::Register(boxed),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(
            matches!(m.functions[0].blocks[0].ops[0], Op::Assign { .. }),
            "{}",
            print_function(&m.functions[0])
        );
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn narrowing_a_value_that_is_already_narrow_is_a_copy() {
        // the round trip a `str` takes through `object` and back: `fold_box` makes
        // the box a copy, copy propagation substitutes the source, and then the
        // narrowing is checking a register against its own representation
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let text = builder.param("s", RType::STR);
        let boxed = builder.temp(RType::OBJECT);
        let back = builder.temp(RType::STR);
        builder.push(Op::Box {
            dest: boxed,
            src: Value::Register(text),
        });
        builder.push(Op::Unbox {
            dest: back,
            src: Value::Register(boxed),
            to: RType::STR,
        });
        builder.terminate(Terminator::Return(Value::Register(back)));
        let mut m = module(builder.finish());
        // the whole pipeline: the fold alone cannot see it, because the source is
        // only narrow once copy propagation has substituted it
        crate::optimize(&mut m).expect("the pipeline verifies");
        assert!(
            !m.functions[0].blocks[0]
                .ops
                .iter()
                .any(|op| matches!(op, Op::Unbox { .. })),
            "{}",
            print_function(&m.functions[0])
        );
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn a_real_narrowing_is_left_alone() {
        // the source is a genuine `object` — from a call out of the unit, or the
        // iteration protocol — and the check is what establishes the invariant
        let mut builder = FunctionBuilder::new("f", RType::STR);
        let any = builder.param("a", RType::OBJECT);
        let out = builder.temp(RType::STR);
        builder.push(Op::Unbox {
            dest: out,
            src: Value::Register(any),
            to: RType::STR,
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(
            m.functions[0].blocks[0]
                .ops
                .iter()
                .any(|op| matches!(op, Op::Unbox { .. })),
            "{}",
            print_function(&m.functions[0])
        );
    }

    #[test]
    fn unpacking_a_tuple_built_here_is_a_move() {
        // `a, b = b, a` builds a python tuple and drives an iterator over it, and
        // both are this module's own doing — the items are right here
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let a = builder.param("a", RType::OBJECT);
        let b = builder.param("b", RType::OBJECT);
        let pair = builder.temp(RType::OBJECT);
        let slots = builder.temp(RType::Tuple(
            vec![RType::OBJECT, RType::OBJECT].into_boxed_slice(),
        ));
        let first = builder.temp(RType::OBJECT);
        builder.push(Op::BuildTuple {
            dest: pair,
            items: vec![Value::Register(b), Value::Register(a)],
        });
        builder.push(Op::Unpack {
            dest: slots,
            src: Value::Register(pair),
            starred: None,
        });
        builder.push(Op::TupleGet {
            dest: first,
            src: Value::Register(slots),
            index: 0,
        });
        builder.terminate(Terminator::Return(Value::Register(first)));
        let mut m = module(builder.finish());
        crate::optimize(&mut m).expect("the pipeline verifies");
        let text = print_function(&m.functions[0]);
        assert!(
            !m.functions[0].blocks[0]
                .ops
                .iter()
                .any(|op| matches!(op, Op::Unpack { .. })),
            "{text}"
        );
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn an_item_rewritten_before_the_read_is_left_alone() {
        // the hazard `a, b = b, a` exists for: forwarding the second read after the
        // first assignment would hand back the value that assignment just wrote
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let a = builder.param("a", RType::OBJECT);
        let b = builder.param("b", RType::OBJECT);
        let slots = builder.temp(RType::Tuple(
            vec![RType::OBJECT, RType::OBJECT].into_boxed_slice(),
        ));
        builder.push(Op::TupleBuild {
            dest: slots,
            items: vec![Value::Register(b), Value::Register(a)],
        });
        // `a = <the first slot>`, which is `b`
        builder.push(Op::TupleGet {
            dest: a,
            src: Value::Register(slots),
            index: 0,
        });
        // and now the second slot must still be the *old* `a`
        let second = builder.temp(RType::OBJECT);
        builder.push(Op::TupleGet {
            dest: second,
            src: Value::Register(slots),
            index: 1,
        });
        builder.terminate(Terminator::Return(Value::Register(second)));
        let mut m = module(builder.finish());
        run(&mut m);
        let text = print_function(&m.functions[0]);
        assert!(
            m.functions[0].blocks[0]
                .ops
                .iter()
                .any(|op| matches!(op, Op::TupleGet { index: 1, .. })),
            "{text}"
        );
    }

    #[test]
    fn reading_a_frozen_field_twice_is_one_read() {
        // what makes it sound is the type system, not the analysis: a frozen class
        // has no setters, so nothing between the two reads can change the field —
        // not an assignment, and not an arbitrary call
        let mut builder = FunctionBuilder::new("f", RType::FLOAT);
        let v = builder.param(
            "v",
            RType::Instance {
                class: "Vec2".to_string(),
                exact: false,
            },
        );
        let first = builder.temp(RType::FLOAT);
        let ignored = builder.temp(RType::OBJECT);
        let second = builder.temp(RType::FLOAT);
        builder.push(Op::GetField {
            dest: first,
            receiver: Value::Register(v),
            class: "Vec2".to_string(),
            field: "x".to_string(),
        });
        builder.push(Op::CallPython {
            dest: ignored,
            callee: "print".to_string(),
            args: Vec::new(),
        });
        builder.push(Op::GetField {
            dest: second,
            receiver: Value::Register(v),
            class: "Vec2".to_string(),
            field: "x".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(second)));
        let mut m = module(builder.finish());
        m.classes.push(by_ir::function::ClassIr {
            name: "Vec2".to_string(),
            immutable: true,
            resume: None,
            keywords: Vec::new(),
            exported: true,
            base: None,
            inherited_init: false,
            fields: vec![by_ir::function::FieldDecl {
                name: "x".to_string(),
                ty: RType::FLOAT,
                default: None,
                optional: false,
                defaulted_by: None,
            }],
            decorators: Vec::new(),
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            generic: false,
            declares_slots: false,
            methods: Vec::new(),
        });
        run(&mut m);
        let reads = m.functions[0].blocks[0]
            .ops
            .iter()
            .filter(|op| matches!(op, Op::GetField { .. }))
            .count();
        assert_eq!(reads, 1, "{}", print_function(&m.functions[0]));
    }

    #[test]
    fn reading_a_mutable_field_twice_is_two_reads() {
        // an arbitrary call may have changed it, and nothing here says otherwise
        let mut builder = FunctionBuilder::new("f", RType::FLOAT);
        let v = builder.param(
            "v",
            RType::Instance {
                class: "Loose".to_string(),
                exact: false,
            },
        );
        let first = builder.temp(RType::FLOAT);
        let ignored = builder.temp(RType::OBJECT);
        let second = builder.temp(RType::FLOAT);
        builder.push(Op::GetField {
            dest: first,
            receiver: Value::Register(v),
            class: "Loose".to_string(),
            field: "x".to_string(),
        });
        builder.push(Op::CallPython {
            dest: ignored,
            callee: "print".to_string(),
            args: Vec::new(),
        });
        builder.push(Op::GetField {
            dest: second,
            receiver: Value::Register(v),
            class: "Loose".to_string(),
            field: "x".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(second)));
        let mut m = module(builder.finish());
        m.classes.push(by_ir::function::ClassIr {
            name: "Loose".to_string(),
            immutable: false,
            resume: None,
            keywords: Vec::new(),
            exported: true,
            base: None,
            inherited_init: false,
            fields: vec![by_ir::function::FieldDecl {
                name: "x".to_string(),
                ty: RType::FLOAT,
                default: None,
                optional: false,
                defaulted_by: None,
            }],
            decorators: Vec::new(),
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            generic: false,
            declares_slots: false,
            methods: Vec::new(),
        });
        run(&mut m);
        let reads = m.functions[0].blocks[0]
            .ops
            .iter()
            .filter(|op| matches!(op, Op::GetField { .. }))
            .count();
        assert_eq!(reads, 2, "{}", print_function(&m.functions[0]));
    }

    #[test]
    fn an_immutable_read_of_a_parameter_moves_to_entry() {
        // loop-invariant code motion without naming a loop: the field cannot change,
        // and `GetField` cannot fail, so doing it at entry observes nothing
        let mut builder = FunctionBuilder::new("f", RType::FLOAT);
        let v = builder.param(
            "v",
            RType::Instance {
                class: "Vec2".to_string(),
                exact: false,
            },
        );
        let body = builder.new_block();
        let out = builder.temp(RType::FLOAT);
        builder.terminate(Terminator::Goto(body));
        builder.switch_to(body);
        builder.push(Op::GetField {
            dest: out,
            receiver: Value::Register(v),
            class: "Vec2".to_string(),
            field: "x".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        m.classes.push(by_ir::function::ClassIr {
            name: "Vec2".to_string(),
            immutable: true,
            resume: None,
            keywords: Vec::new(),
            exported: true,
            base: None,
            inherited_init: false,
            fields: vec![by_ir::function::FieldDecl {
                name: "x".to_string(),
                ty: RType::FLOAT,
                default: None,
                optional: false,
                defaulted_by: None,
            }],
            decorators: Vec::new(),
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            generic: false,
            declares_slots: false,
            methods: Vec::new(),
        });
        run(&mut m);
        let text = print_function(&m.functions[0]);
        assert!(
            m.functions[0].blocks[0]
                .ops
                .iter()
                .any(|op| matches!(op, Op::GetField { .. })),
            "{text}"
        );
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn a_read_of_a_local_receiver_stays_put() {
        // only a *parameter* is live from entry; a local may not be assigned yet
        let mut builder = FunctionBuilder::new("f", RType::FLOAT);
        let v = builder.local(
            "v".to_string(),
            RType::Instance {
                class: "Vec2".to_string(),
                exact: false,
            },
        );
        let body = builder.new_block();
        let out = builder.temp(RType::FLOAT);
        builder.terminate(Terminator::Goto(body));
        builder.switch_to(body);
        builder.push(Op::GetField {
            dest: out,
            receiver: Value::Register(v),
            class: "Vec2".to_string(),
            field: "x".to_string(),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        m.classes.push(by_ir::function::ClassIr {
            name: "Vec2".to_string(),
            immutable: true,
            resume: None,
            keywords: Vec::new(),
            exported: true,
            base: None,
            inherited_init: false,
            fields: vec![by_ir::function::FieldDecl {
                name: "x".to_string(),
                ty: RType::FLOAT,
                default: None,
                optional: false,
                defaulted_by: None,
            }],
            decorators: Vec::new(),
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            generic: false,
            declares_slots: false,
            methods: Vec::new(),
        });
        run(&mut m);
        assert!(
            m.functions[0].blocks[0]
                .ops
                .iter()
                .all(|op| !matches!(op, Op::GetField { .. })),
            "{}",
            print_function(&m.functions[0])
        );
    }

    #[test]
    fn boxing_an_unboxed_value_is_left_alone() {
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let n = builder.param("n", RType::INT);
        let boxed = builder.temp(RType::OBJECT);
        builder.push(Op::Box {
            dest: boxed,
            src: Value::Register(n),
        });
        builder.terminate(Terminator::Return(Value::Register(boxed)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(matches!(m.functions[0].blocks[0].ops[0], Op::Box { .. }));
    }

    #[test]
    fn boxing_a_native_class_is_left_alone() {
        // its pointer is to its own struct, so widening it is a C cast — and `Box`
        // is where the cast is emitted
        let mut builder = FunctionBuilder::new("f", RType::OBJECT);
        let p = builder.param(
            "p",
            RType::Instance {
                class: "Point".to_string(),
                exact: false,
            },
        );
        let boxed = builder.temp(RType::OBJECT);
        builder.push(Op::Box {
            dest: boxed,
            src: Value::Register(p),
        });
        builder.terminate(Terminator::Return(Value::Register(boxed)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert!(matches!(m.functions[0].blocks[0].ops[0], Op::Box { .. }));
    }
}

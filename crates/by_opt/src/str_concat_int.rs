//! build `prefix + str(i)` into one string, rather than into two
//!
//! `str_of_int` already writes the digits of a machine integer straight into a
//! string instead of formatting a `PyLong` through a writer. what it leaves behind
//! is that the string it writes them into is not the answer: `"k" + str(i)`
//! allocates it, then allocates a second string for the concatenation and copies
//! both halves in. the length of the answer was settled before either allocation
//! happened — a prefix of a known length, and a decimal that is at most twenty
//! digits and a sign — so one allocation can serve for both.
//!
//! on the benchmark that does nothing but this it is worth 1.65x.
//!
//! ## the shape, and why each part of it is required
//!
//! ```text
//! a = str-of-int n        the digits, as an object of unknown type
//! b = unbox-str a         because a rebound `str` may return anything
//! d = str-concat lhs, b
//! ```
//!
//! the three have to be **adjacent and in one block**, which settles three things
//! at once. the line a failure reports is a property of a block here, so an
//! operation that stays in its block reports the line it did. the block's error
//! target is shared, so an `except` around the source cannot have been around one
//! of the three and not the others. and nothing stands between the concatenation
//! and the digits for it to be moved past.
//!
//! the two intermediates have to be **temporaries this chain wholly owns** —
//! unnamed, written once by the operation above them and read once by the operation
//! below. a name, or a second reader, means the program can see the string the
//! digits were built into, and then whether it was ever built is not this pass's
//! business.
//!
//! ## what is left alone
//!
//! a concatenation `str_append` has already claimed. that pass runs first and marks
//! the concatenations whose left operand is dying, which turns `s = s + str(i)`
//! into an in-place resize — linear over a loop where a copy per step would be
//! quadratic. fusing away the intermediate would save one small allocation and cost
//! that, so an accumulation keeps the shape it has.

use std::collections::{HashMap, HashSet};

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{BlockId, Op, RegisterId, Value};
use by_ir::rtype::RType;

pub fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        fuse(function);
    }
}

/// one `lhs + str(n)` found in a block: where it starts, and what it is about
struct Chain {
    block: BlockId,
    /// the index of the `str-of-int`; the unbox and the concatenation are the two
    /// operations after it
    index: usize,
    /// the two temporaries the chain passes the digits along, which nothing else
    /// may read
    digits: RegisterId,
    checked: RegisterId,
}

fn fuse(function: &mut Function) {
    let chains = chains(function);
    if chains.is_empty() {
        return;
    }
    let owned = wholly_owned(function, &chains);

    // rewritten back to front, so removing the earlier operations of one chain
    // cannot move the index a later one was found at
    for chain in chains.iter().rev() {
        if !owned.contains(&chain.digits) || !owned.contains(&chain.checked) {
            continue;
        }
        let Some(block) = function.blocks.get_mut(chain.block.index()) else {
            continue;
        };
        let (Some(Op::StrOfInt { value, .. }), Some(Op::StrConcat { dest, lhs, .. })) = (
            block.ops.get(chain.index).cloned(),
            block.ops.get(chain.index + 2).cloned(),
        ) else {
            continue;
        };
        block.ops[chain.index] = Op::StrConcatInt { dest, lhs, value };
        block.ops.drain(chain.index + 1..chain.index + 3);
    }
}

/// every `str-of-int`, unbox and concatenation standing next to each other in that
/// order, with the digits handed straight along
fn chains(function: &Function) -> Vec<Chain> {
    let mut out = Vec::new();
    for (index, block) in function.blocks.iter().enumerate() {
        for (at, window) in block.ops.windows(3).enumerate() {
            let [
                Op::StrOfInt { dest: digits, .. },
                Op::Unbox {
                    dest: checked,
                    src,
                    to,
                },
                Op::StrConcat {
                    rhs, consumes_lhs, ..
                },
            ] = window
            else {
                continue;
            };
            // an accumulation keeps its in-place append, which is worth more than
            // the allocation this would save
            if *consumes_lhs
                || to != &RType::STR
                || src != &Value::Register(*digits)
                || rhs != &Value::Register(*checked)
            {
                continue;
            }
            out.push(Chain {
                block: BlockId(index),
                index: at,
                digits: *digits,
                checked: *checked,
            });
        }
    }
    out
}

/// the intermediates whose whole life is the chains found above
///
/// each chain writes `digits` once and reads it once, and does the same to
/// `checked`, so one count answers for both sides. a register the chains account
/// for entirely cannot be seen from anywhere else, and the string the digits were
/// built into is this pass's to remove. anything else — a name, a second reader, a
/// write from somewhere else — is left exactly as it is, because a pass before this
/// one may have merged the temporary with a register something else uses.
///
/// the count is not one per register: `unswitch` copies a loop body, so a chain in
/// a loop is two chains over the same pair of registers
fn wholly_owned(function: &Function, chains: &[Chain]) -> HashSet<RegisterId> {
    let mut fused: HashMap<RegisterId, usize> = HashMap::new();
    for chain in chains {
        *fused.entry(chain.digits).or_default() += 1;
        *fused.entry(chain.checked).or_default() += 1;
    }

    let mut writes: HashMap<RegisterId, usize> = HashMap::new();
    let mut reads: HashMap<RegisterId, usize> = HashMap::new();
    let count = |map: &mut HashMap<RegisterId, usize>, id: RegisterId| {
        *map.entry(id).or_default() += 1;
    };
    for block in &function.blocks {
        for op in &block.ops {
            if let Some(dest) = op.dest() {
                count(&mut writes, dest);
            }
            // `del x` reads the register on its way to emptying it, and says so
            // through neither `dest` nor `operands`
            if let Some(unbound) = op.unbinds() {
                count(&mut reads, unbound);
            }
            for operand in op.operands() {
                if let Value::Register(id) = operand {
                    count(&mut reads, *id);
                }
            }
        }
        if let Some(dest) = block.terminator.dest() {
            count(&mut writes, dest);
        }
        for operand in block.terminator.operands() {
            if let Value::Register(id) = operand {
                count(&mut reads, *id);
            }
        }
    }

    fused
        .keys()
        .copied()
        .filter(|register| {
            function
                .register(*register)
                .is_some_and(|decl| decl.name.is_none())
                && writes.get(register) == fused.get(register)
                && reads.get(register) == fused.get(register)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use by_ir::builder::FunctionBuilder;
    use by_ir::function::{Function, ModuleIr};
    use by_ir::ops::{Op, RegisterId, Terminator, Value};
    use by_ir::rtype::RType;

    /// `prefix + str(i)`, in the shape the passes before this one leave it
    fn built(prefix: &str, wire: impl FnOnce(&mut FunctionBuilder, RegisterId)) -> Function {
        let mut builder = FunctionBuilder::new("keys", RType::STR);
        let counter = builder.param("i", RType::INT);
        let digits = builder.temp(RType::OBJECT);
        let checked = builder.temp(RType::STR);
        let out = builder.temp(RType::STR);
        builder.push(Op::StrOfInt {
            dest: digits,
            value: Value::Register(counter),
        });
        builder.push(Op::Unbox {
            dest: checked,
            src: Value::Register(digits),
            to: RType::STR,
        });
        builder.push(Op::StrConcat {
            dest: out,
            lhs: Value::Str(prefix.to_string()),
            rhs: Value::Register(checked),
            consumes_lhs: false,
        });
        wire(&mut builder, checked);
        builder.terminate(Terminator::Return(Value::Register(out)));
        builder.finish()
    }

    fn run_on(function: Function) -> Function {
        let mut module = ModuleIr::new("app");
        module.functions.push(function);
        super::run(&mut module);
        assert_eq!(by_ir::verify::verify_module(&module), Ok(()));
        module.functions.remove(0)
    }

    fn ops(function: &Function) -> Vec<&Op> {
        function
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .collect()
    }

    #[test]
    fn the_digits_and_the_prefix_take_one_allocation() {
        let function = run_on(built("k", |_, _| {}));
        assert!(
            matches!(ops(&function).as_slice(), [Op::StrConcatInt { .. }]),
            "{:?}",
            ops(&function)
        );
    }

    #[test]
    fn an_intermediate_something_else_reads_is_left_alone() {
        // `t = str(i); return "k" + t, t` — the string the digits were built into
        // is the program's own value, so whether it was built is observable
        let function = run_on(built("k", |builder, checked| {
            let pair = builder.temp(RType::Tuple(vec![RType::STR].into()));
            builder.push(Op::TupleBuild {
                dest: pair,
                items: vec![Value::Register(checked)],
            });
        }));
        assert!(
            ops(&function)
                .iter()
                .all(|op| !matches!(op, Op::StrConcatInt { .. })),
            "{:?}",
            ops(&function)
        );
    }

    #[test]
    fn an_accumulation_keeps_its_in_place_append() {
        let mut builder = FunctionBuilder::new("join", RType::STR);
        let counter = builder.param("i", RType::INT);
        let held = builder.local("out", RType::STR);
        let digits = builder.temp(RType::OBJECT);
        let checked = builder.temp(RType::STR);
        builder.assign(held, Value::Str(String::new()));
        builder.push(Op::StrOfInt {
            dest: digits,
            value: Value::Register(counter),
        });
        builder.push(Op::Unbox {
            dest: checked,
            src: Value::Register(digits),
            to: RType::STR,
        });
        builder.push(Op::StrConcat {
            dest: held,
            lhs: Value::Register(held),
            rhs: Value::Register(checked),
            consumes_lhs: true,
        });
        builder.terminate(Terminator::Return(Value::Register(held)));
        let function = run_on(builder.finish());
        assert!(
            ops(&function).iter().any(|op| matches!(
                op,
                Op::StrConcat {
                    consumes_lhs: true,
                    ..
                }
            )),
            "{:?}",
            ops(&function)
        );
    }
}

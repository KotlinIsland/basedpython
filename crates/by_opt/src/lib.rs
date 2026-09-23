//! the BIR optimization passes
//!
//! each pass is a function over a whole [`ModuleIr`]. the pipeline verifies
//! after every pass in debug builds, because a pass that produces ill-typed BIR
//! is a bug that would otherwise surface as miscompiled C rather than as an
//! error.

pub(crate) mod borrow;
mod coalesce;
pub(crate) mod copy_propagation;
mod dead_allocations;
pub(crate) mod dead_registers;
pub(crate) mod dict_find;
mod field_proofs;
pub(crate) mod fold;
mod guard_loops;
pub(crate) mod infallible;
mod liveness;
mod proved_narrowings;
mod push_lengths;
pub(crate) mod refcount;
pub(crate) mod release_temporaries;
mod runs_python;
mod store_moves;
pub(crate) mod str_append;
mod str_concat_int;
pub(crate) mod str_item_compare;
mod str_of_int;
pub(crate) mod unbox_counters;
mod unswitch;

use by_ir::function::ModuleIr;
use by_ir::verify::{VerifyError, verify_module};

/// one named pass over a module
struct Pass {
    name: &'static str,
    run: fn(&mut ModuleIr),
}

/// the passes, in order
const PASSES: &[Pass] = &[
    Pass {
        name: "copy-propagation",
        run: copy_propagation::run,
    },
    // after copy propagation, which is what makes two reads of one object name one
    // register, and before folding, which is what turns a test it answered into a jump
    Pass {
        name: "field-proofs",
        run: field_proofs::run,
    },
    // folding runs after copy propagation, which is what turns a comparison's
    // temp into an immediate the branch can see
    Pass {
        name: "fold",
        run: fold::run,
    },
    // again: folding turns a redundant `box` into a copy, and a `branch` on a
    // folded bit into a jump — neither of which the first run could see
    Pass {
        name: "copy-propagation",
        run: copy_propagation::run,
    },
    Pass {
        name: "fold",
        run: fold::run,
    },
    // and once more, because the group only reaches a fixed point on the third
    // trip. `total + pair[0]` takes two rounds to become `total + whole`: the first
    // fold reads the element off the tuple the block just built, the propagation
    // that follows unifies the element with the register that was boxed into it,
    // and only then can the second fold cancel the box against the unbox — which
    // leaves a copy of its own that nothing was propagating away
    Pass {
        name: "copy-propagation",
        run: copy_propagation::run,
    },
    // after folding, which is what makes the compared-against literal an immediate
    // rather than a register the pass would not recognise, and before
    // dead-registers, which is what removes the character register it orphans
    Pass {
        name: "str-item-compare",
        run: str_item_compare::run,
    },
    // before dead-registers, which is what removes the aliases and the temporary
    // this orphans — the two sides hold a copy of the key each, and the pass sees
    // through those itself rather than waiting for copy-propagation to unify them,
    // because a refcounted copy is one propagation deliberately leaves standing
    Pass {
        name: "dict-find",
        run: dict_find::run,
    },
    // after every fold, which is what orphans the allocations it removes, and before
    // coalesce, which would merge an orphaned register with a live one and so make
    // the write that fills it look read
    Pass {
        name: "dead-allocations",
        run: dead_allocations::run,
    },
    Pass {
        name: "coalesce",
        run: coalesce::run,
    },
    // before dead-registers, which is what removes the register the boxing it
    // fuses away used to fill, and before unswitch, which would otherwise copy the
    // unfused shape into a second body the pass then has to recognise twice
    Pass {
        name: "str-of-int",
        run: str_of_int::run,
    },
    Pass {
        name: "dead-registers",
        run: dead_registers::run,
    },
    // after dead-registers, so the body it copies is the final one, and before
    // infallible/borrow/refcount, which all read the block set
    // after dead-registers, so the loop it copies is the final one, and before unswitch,
    // which then finds the copy's bound as invariant as the original's
    Pass {
        name: "guard-loops",
        run: guard_loops::run,
    },
    Pass {
        name: "unswitch",
        run: unswitch::run,
    },
    // after the passes that copy loops, so each copy keeps a length of its own, and before
    // the ones that decide what a register owns and where it is released
    Pass {
        name: "push-lengths",
        run: push_lengths::run,
    },
    // after every pass that copies a block, so a copy of a test and a copy of the
    // narrowing behind it are matched up in the copy rather than across the two, and
    // before infallible, which is what takes the error edge off the narrowings it proves
    Pass {
        name: "proved-narrowings",
        run: proved_narrowings::run,
    },
    Pass {
        name: "infallible",
        run: infallible::run,
    },
    // before refcount, which must not ask a borrowed register to be released
    Pass {
        name: "borrow",
        run: borrow::run,
    },
    // after borrow, whose marks say which registers own nothing to hand over, and
    // after everything that rewrites operands — the mark names a register, and a
    // later pass replacing it with an immediate would leave nothing to take over
    Pass {
        name: "str-append",
        run: str_append::run,
    },
    // after str-append, whose marks say which concatenations are already an
    // in-place resize — those are worth more than the allocation this would save,
    // so it declines them rather than having to predict them
    Pass {
        name: "str-concat-int",
        run: str_concat_int::run,
    },
    // after every pass that decides what a register owns — the borrow pass above all —
    // because where a temporary can be let go of rests on the final answer
    Pass {
        name: "release-temporaries",
        run: release_temporaries::run,
    },
    // after the release pass, whose release straight after a dying store is what a
    // moving store replaces
    Pass {
        name: "store-moves",
        run: store_moves::run,
    },
    // last: it reads the final shape of every block
    Pass {
        name: "refcount",
        run: refcount::run,
    },
];

/// run the pipeline
///
/// returns the name of the first pass whose output does not verify, which is
/// enough to localize the bug — the module is left in that state so it can be
/// printed
pub fn optimize(module: &mut ModuleIr) -> Result<(), (&'static str, Vec<VerifyError>)> {
    for pass in PASSES {
        (pass.run)(module);
        if cfg!(debug_assertions)
            && let Err(errors) = verify_module(module)
        {
            return Err((pass.name, errors));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::function::CallConvention;
    use by_ir::ops::{BinOp, Mutation, Op, Terminator, Value};
    use by_ir::rtype::RType;

    #[test]
    fn a_buffer_length_is_unboxed_alongside_the_counter_it_bounds() {
        // `while i < len(a): i = i + 1` over an unboxed buffer: the length is a
        // `Py_ssize_t` already, so tagging it only to compare it against a machine counter
        // is the tag going round in a circle. inside the loop's copy both sides are
        // machine integers and the guard is a register compare
        let mut builder = FunctionBuilder::new("scan", RType::INT);
        let array = builder.param("a", RType::Array(Box::new(RType::FLOAT)));
        let index = builder.local("i", RType::INT);
        let length = builder.temp(RType::INT);
        let more = builder.temp(RType::BIT);
        builder.assign(index, Value::Int(0));
        let header = builder.new_block();
        let body = builder.new_block();
        let exit = builder.new_block();
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(header);
        builder.push(Op::ArrayLen {
            dest: length,
            array: Value::Register(array),
        });
        builder.push(Op::IntCompare {
            dest: more,
            op: by_ir::ops::CmpOp::Lt,
            lhs: Value::Register(index),
            rhs: Value::Register(length),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(more),
            then_block: body,
            else_block: exit,
        });
        builder.switch_to(body);
        builder.push(Op::IntBinary {
            dest: index,
            op: BinOp::Add,
            lhs: Value::Register(index),
            rhs: Value::Int(1),
            mutation: Mutation::Fresh,
        });
        builder.terminate(Terminator::Goto(header));
        builder.switch_to(exit);
        builder.terminate(Terminator::Return(Value::Register(index)));

        let mut module = ModuleIr::new("app");
        module.functions.push(builder.finish());
        assert!(optimize(&mut module).is_ok());
        let function = &module.functions[0];
        let fixed = |value: &Value| {
            matches!(
                function.value_type(value),
                Some(RType::Primitive(by_ir::rtype::Primitive::Fixed(_)))
            )
        };
        let guards: Vec<(&Value, &Value)> = function
            .blocks
            .iter()
            .flat_map(|block| &block.ops)
            .filter_map(|op| match op {
                Op::IntCompare { lhs, rhs, .. } => Some((lhs, rhs)),
                _ => None,
            })
            .collect();
        assert!(
            guards.iter().any(|(lhs, rhs)| fixed(lhs) && fixed(rhs)),
            "{}",
            by_ir::print::print_function(function)
        );
    }

    #[test]
    fn the_pipeline_runs_every_pass_and_leaves_verifiable_ir() {
        let mut builder = FunctionBuilder::new("scale", RType::FLOAT);
        let x = builder.param("x", RType::FLOAT);
        let temp = builder.temp(RType::FLOAT);
        let out = builder.local("out", RType::FLOAT);
        builder.push(Op::FloatBinary {
            dest: temp,
            op: BinOp::Mul,
            lhs: Value::Register(x),
            rhs: Value::Register(x),
        });
        builder.assign(out, Value::Register(temp));
        builder.terminate(Terminator::Return(Value::Register(out)));

        let mut module = ModuleIr {
            name: by_ir::ModuleName::new("app"),
            functions: vec![builder.finish()],
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
        };
        assert!(optimize(&mut module).is_ok());
        // both passes fired: the copy is gone and the function is infallible
        assert_eq!(
            module.functions[0].convention,
            CallConvention::NativeInfallible
        );
        assert!(
            module.functions[0].blocks[0]
                .ops
                .iter()
                .all(|op| !matches!(op, Op::Assign { .. })),
            "the copy should have been propagated away"
        );
        // and the temporary it left behind should be gone
        assert_eq!(module.functions[0].registers.len(), 2);
    }

    #[test]
    fn every_pass_covers_a_method_too() {
        // the passes used to iterate `functions` alone, so a class-heavy module
        // got no optimization at all
        let mut builder = FunctionBuilder::new("sum", RType::FLOAT);
        let receiver = builder.param(
            "self",
            RType::Instance {
                class: "Pair".to_string(),
                exact: false,
            },
        );
        let a = builder.temp(RType::FLOAT);
        let b = builder.temp(RType::FLOAT);
        let out = builder.temp(RType::FLOAT);
        builder.push(Op::GetField {
            dest: a,
            receiver: Value::Register(receiver),
            class: "Pair".to_string(),
            field: "a".to_string(),
        });
        builder.push(Op::GetField {
            dest: b,
            receiver: Value::Register(receiver),
            class: "Pair".to_string(),
            field: "b".to_string(),
        });
        builder.push(Op::FloatBinary {
            dest: out,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));

        let mut module = ModuleIr::new("app");
        module.classes.push(by_ir::function::ClassIr {
            resume: None,
            keywords: Vec::new(),
            exported: true,
            name: "Pair".to_string(),
            fields: vec![
                by_ir::function::FieldDecl {
                    cell: false,
                    name: "a".to_string(),
                    ty: RType::FLOAT,
                    default: None,
                    optional: false,
                    defaulted_by: None,
                },
                by_ir::function::FieldDecl {
                    cell: false,
                    name: "b".to_string(),
                    ty: RType::FLOAT,
                    default: None,
                    optional: false,
                    defaulted_by: None,
                },
            ],
            methods: vec![builder.finish()],
            decorators: Vec::new(),
            constants: Vec::new(),
            properties: Vec::new(),
            slot_aliases: Vec::new(),
            generic: false,
            declares_slots: false,
            slots_weak_references: false,
            base: None,
            inherited_init: false,
            fields_are_parameters: true,
            dataclass: false,
            immutable: false,
            environment: false,
        });

        assert_eq!(optimize(&mut module), Ok(()));
        let method = &module.classes[0].methods[0];
        // infallible reached it — a field read and a float add cannot raise
        assert_eq!(method.convention, CallConvention::NativeInfallible);
        // and so did refcount
        assert!(method.blocks[0].owned_at_exit.is_some());
        assert_eq!(by_ir::verify::verify_module(&module), Ok(()));
    }
}

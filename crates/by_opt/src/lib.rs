//! the BIR optimization passes
//!
//! each pass is a function over a whole [`ModuleIr`]. the pipeline verifies
//! after every pass in debug builds, because a pass that produces ill-typed BIR
//! is a bug that would otherwise surface as miscompiled C rather than as an
//! error.

pub mod borrow;
mod coalesce;
pub mod copy_propagation;
pub mod dead_registers;
pub mod fold;
pub mod infallible;
pub mod refcount;
pub mod str_append;
pub mod str_item_compare;
pub mod unbox_counters;
mod unswitch;

use by_ir::function::ModuleIr;
use by_ir::verify::{VerifyError, verify_module};

/// one named pass over a module
pub struct Pass {
    pub name: &'static str,
    pub run: fn(&mut ModuleIr),
}

/// the passes, in order
pub const PASSES: &[Pass] = &[
    Pass {
        name: "copy-propagation",
        run: copy_propagation::run,
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
    // after folding, which is what makes the compared-against literal an immediate
    // rather than a register the pass would not recognise, and before
    // dead-registers, which is what removes the character register it orphans
    Pass {
        name: "str-item-compare",
        run: str_item_compare::run,
    },
    // before dead-registers, which is what removes the temporary it frees up
    // after folding, which is what turns the step's operands into the immediates
    // the analysis looks for, and before coalesce, which would merge the counter
    // with a register of the tagged representation
    Pass {
        name: "unbox-counters",
        run: unbox_counters::run,
    },
    Pass {
        name: "coalesce",
        run: coalesce::run,
    },
    Pass {
        name: "dead-registers",
        run: dead_registers::run,
    },
    // after dead-registers, so the body it copies is the final one, and before
    // infallible/borrow/refcount, which all read the block set
    Pass {
        name: "unswitch",
        run: unswitch::run,
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
    use by_ir::ops::{BinOp, Op, Terminator, Value};
    use by_ir::rtype::RType;

    #[test]
    fn a_buffer_length_is_unboxed_alongside_the_counter_it_bounds() {
        // `while i < len(a)` over an unboxed buffer: the length is a `Py_ssize_t`
        // already, so tagging it only to compare it against a machine counter is the
        // tag going round in a circle. both sides end up unboxed and the guard is a
        // register compare
        let mut builder = FunctionBuilder::new("scan", RType::INT);
        let array = builder.param("a", RType::Array(Box::new(RType::FLOAT)));
        let index = builder.local("i", RType::INT);
        let length = builder.temp(RType::INT);
        let more = builder.temp(RType::BIT);
        builder.assign(index, Value::Int(0));
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
        builder.terminate(Terminator::Return(Value::Register(index)));

        let mut module = ModuleIr {
            name: by_ir::ModuleName::new("app"),
            functions: vec![builder.finish()],
            declined: Vec::new(),
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
        };
        assert!(optimize(&mut module).is_ok());
        let fixed = |id: by_ir::ops::RegisterId| {
            matches!(
                module.functions[0].register(id).map(|decl| &decl.ty),
                Some(RType::Primitive(by_ir::rtype::Primitive::Fixed(_)))
            )
        };
        assert!(fixed(index) && fixed(length), "{:?}", module.functions[0]);
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
                    name: "a".to_string(),
                    ty: RType::FLOAT,
                    default: None,
                    optional: false,
                },
                by_ir::function::FieldDecl {
                    name: "b".to_string(),
                    ty: RType::FLOAT,
                    default: None,
                    optional: false,
                },
            ],
            methods: vec![builder.finish()],
            decorators: Vec::new(),
            constants: Vec::new(),
            generic: false,
            base: None,
            inherited_init: false,
            immutable: false,
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

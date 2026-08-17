//! infallibility inference → error-path elision
//!
//! a call to a function that cannot fail needs no error check after it. in
//! mypyc-style output roughly one branch per call exists solely to propagate
//! errors, so removing them where they cannot fire is a structural reduction in
//! the size and branchiness of the generated code, not a micro-optimization.
//!
//! this is the pass that would eventually be *driven* by the checker's
//! [`raises` clause](../../../docs/basedpython/features/exceptions.md). until
//! that is wired in it derives the same fact structurally, which is strictly
//! weaker but needs nothing from ty.
//!
//! ## why integer arithmetic is still fallible
//!
//! `By_IntAdd` looks infallible — and its fast path is — but the moment an
//! operand leaves the tagged range it allocates a `PyLongObject`, and that can
//! raise `MemoryError`. only range analysis can prove a particular addition
//! never reaches the boxed path. floats have no such path, so float arithmetic
//! (other than division) really is infallible.

use std::collections::{HashMap, HashSet};

use by_ir::function::{CallConvention, Function, ModuleIr, qualify};
use by_ir::ops::{Op, UnaryOp};
use by_ir::rtype::{Primitive, RType};

/// mark every function that provably cannot fail
///
/// this is a fixed point over the call graph: a function is infallible when
/// every one of its own operations is, *and* every function it calls is. so a
/// pair of mutually recursive float functions converges to infallible, and one
/// division anywhere in a cycle makes the whole cycle fallible.
pub fn run(module: &mut ModuleIr) {
    let names: Vec<String> = module
        .all_functions()
        .map(Function::qualified_name)
        .collect();

    // start optimistic and remove: the greatest fixed point is what makes a
    // recursive function infallible rather than assuming the worst about itself
    let mut infallible: HashSet<&str> = names.iter().map(String::as_str).collect();

    let mut own_ops_can_fail: HashMap<String, bool> = HashMap::new();
    let mut callees: HashMap<String, Vec<String>> = HashMap::new();
    for function in module.all_functions() {
        let mut can_fail = false;
        let mut called = Vec::new();
        for block in &function.blocks {
            for op in &block.ops {
                if op_can_fail(module, function, op) {
                    can_fail = true;
                }
                if let Op::CallNative { owner, callee, .. } = op {
                    called.push(qualify(owner.as_deref(), callee));
                }
            }
            // a terminator reads too, and `return value` is the commonest place a
            // maybe-unassigned local is read — a function whose only error path is
            // there would otherwise be called infallible and have nowhere to jump
            if reads_a_maybe_unassigned_local(function, &block.terminator.operands()) {
                can_fail = true;
            }
        }
        own_ops_can_fail.insert(function.qualified_name(), can_fail);
        callees.insert(function.qualified_name(), called);
    }

    loop {
        let mut changed = false;
        for name in &names {
            let name = name.as_str();
            if !infallible.contains(name) {
                continue;
            }
            let fails = own_ops_can_fail.get(name).copied().unwrap_or(true)
                || callees
                    .get(name)
                    .is_none_or(|called| called.iter().any(|c| !infallible.contains(c.as_str())));
            if fails {
                infallible.remove(name);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    for function in module.all_functions_mut() {
        function.convention = if infallible.contains(function.qualified_name().as_str()) {
            CallConvention::NativeInfallible
        } else {
            CallConvention::Native
        };
    }
}

/// whether an operation can raise
fn reads_a_maybe_unassigned_local(
    function: &by_ir::function::Function,
    values: &[&by_ir::ops::Value],
) -> bool {
    values.iter().any(|value| {
        matches!(value, by_ir::ops::Value::Register(id)
            if function.register(*id).is_some_and(|decl| decl.may_be_unassigned))
    })
}

fn op_can_fail(module: &ModuleIr, function: &by_ir::function::Function, op: &Op) -> bool {
    // a read of a local some path never wrote raises `UnboundLocalError`, so it is an
    // error path like any other and the function needs somewhere to jump
    if reads_a_maybe_unassigned_local(function, &op.operands()) {
        return true;
    }

    // an attribute `__init__` assigns on only some paths is a field the instance may
    // not have, and reading one raises `AttributeError` — so this *is* an error path,
    // and a function containing one needs somewhere to jump
    if let Op::GetField { class, field, .. } = op
        && module
            .classes
            .iter()
            .find(|candidate| candidate.name == *class)
            .and_then(|candidate| candidate.fields.iter().find(|decl| decl.name == *field))
            .is_some_and(|decl| decl.optional)
    {
        return true;
    }

    match op {
        // a copy, a comparison of unboxed doubles, and reading or building a
        // fixed tuple cannot raise on their own. neither does naming an emitted
        // class: the type object is this module's own, and already built
        Op::Assign { .. }
        | Op::FloatCompare { .. }
        | Op::TupleGet { .. }
        | Op::LoadClass { .. }
        | Op::TupleBuild { .. } => false,

        // the boxed path of a tagged integer allocates, and allocation raises.
        // the abstract object protocol can raise from user code outright
        Op::IntBinary { .. }
        | Op::IntCompare { .. }
        | Op::ObjectBinary { .. }
        | Op::ObjectCompare { .. }
        | Op::StrCompare { .. }
        | Op::Truthy { .. }
        | Op::Len { .. }
        | Op::StrConcat { .. }
        | Op::CallPython { .. }
        | Op::CallValue { .. }
        | Op::LoadGlobal { .. }
        | Op::StoreGlobal { .. }
        | Op::DeleteGlobal { .. }
        | Op::ImportModule { .. }
        | Op::ImportFrom { .. }
        | Op::NewInstance { .. }
        | Op::GetCell { .. }
        | Op::RaiseWith { .. }
        | Op::Enter { .. }
        | Op::ExitContext { .. }
        | Op::DelegateIter { .. }
        | Op::DelegateStep { .. }
        | Op::MakeClosure { .. }
        | Op::GetIter { .. }
        | Op::IterNext { .. }
        | Op::CallMethod { .. }
        | Op::GetAttr { .. }
        | Op::SetAttr { .. }
        | Op::BuildList { .. }
        | Op::BuildSet { .. }
        | Op::BuildTuple { .. }
        | Op::BuildDict { .. }
        | Op::GetItem { .. }
        | Op::StrGetItem { .. }
        | Op::StrItemCompare { .. }
        | Op::SetItem { .. }
        | Op::Format { .. } => true,

        // a null test reads a pointer, and taking or matching a pending exception
        // touches only the thread state — none of them can fail
        // a field read or write is a struct access at a known offset
        Op::IsNull { .. }
        | Op::FetchException { .. }
        | Op::ExceptionMatches { .. }
        | Op::GetField { .. }
        | Op::PushHandled { .. }
        | Op::PopHandled { .. }
        | Op::SetField { .. } => false,

        // a delete runs the protocol, which can raise
        Op::DeleteItem { .. } | Op::DeleteAttr { .. } => true,

        // a call out of the unit can raise whatever it likes
        Op::CallUnpacked { .. } => true,

        // merging into a display drives an iterator or a mapping, either of which fails
        Op::Extend { .. } => true,

        // allocating, growing and indexing can all fail; reading the length is a
        // field read of a buffer we already hold
        Op::ArrayNew { .. } | Op::ArrayGet { .. } | Op::ArraySet { .. } | Op::ArrayPush { .. } => {
            true
        }
        // the index came from the lowering, so it is in range by construction
        Op::ArrayLen { .. } | Op::ArrayRead { .. } => false,

        // building a tuple drives an iterator, which can fail
        Op::ToTuple { .. } => true,

        // an unpack drives an iterator, and both the arity and the iterator can fail
        Op::Unpack { .. } => true,

        // a raise always leaves through the error path
        Op::Reraise { .. } | Op::RaiseObject { .. } => true,

        // the container protocol reaches `__contains__`, or iterates
        Op::Contains { .. } => true,
        // comparing two pointers asks nothing of either object, and a type flag is
        // read straight off the type
        Op::Identity { .. } | Op::IsSequence { .. } => false,
        // a lookup reaches `__getitem__`, and the rest-dict allocates
        Op::MatchKey { .. } | Op::MatchRest { .. } => true,
        // a type flag, read straight off the type
        Op::IsMapping { .. } => false,
        // an object with no `__aiter__` is a `TypeError`, and the call may raise
        Op::AsyncIter { .. } | Op::AsyncContext { .. } => true,
        // an attribute lookup reaches `__getattr__`, and `__match_args__` with it
        Op::MatchAttr { .. } => true,
        // a pointer comparison against a singleton
        Op::IsMissing { .. } => false,
        // a slice reaches `__getitem__`, and allocates the list it hands back
        Op::MatchSlice { .. } => true,
        // `isinstance` reaches `__instancecheck__`, which can raise
        Op::IsInstance { .. } => true,
        Op::FloatBinary { op, .. } => op.can_fail(),
        // the object side may be anything, so any of them may raise
        Op::FloatObjectBinary { .. } | Op::FloatObjectCompare { .. } => true,

        Op::Unary { operand, op, .. } => match op {
            UnaryOp::Not => false,
            // negating a double is one instruction; negating a tagged integer
            // goes through the same boxed path as subtraction, and `~` always
            // does
            UnaryOp::Neg => !matches!(
                function.value_type(operand),
                Some(RType::Primitive(Primitive::Float))
            ),
            UnaryOp::Invert => true,
        },

        // a call's failure is decided by the callee, in the fixed point above
        Op::CallNative { .. } => false,

        // boxing allocates, unboxing is a checked narrowing, a raise raises, and
        // an integer with no float at all raises `OverflowError`
        Op::Box { .. } | Op::IntToFloat { .. } | Op::Unbox { .. } | Op::RaiseStandard { .. } => {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{BinOp, CmpOp, Terminator, Value};
    use by_ir::rtype::RType;

    fn module(functions: Vec<by_ir::function::Function>) -> ModuleIr {
        ModuleIr {
            name: by_ir::ModuleName::new("app"),
            functions,
            declined: Vec::new(),
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
        }
    }

    fn convention(module: &ModuleIr, name: &str) -> CallConvention {
        module
            .functions
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.convention)
            .expect("the function exists")
    }

    /// `def scale(x: float, y: float) -> float: return x * y`
    fn float_mul(name: &str) -> by_ir::function::Function {
        let mut builder = FunctionBuilder::new(name, RType::FLOAT);
        let x = builder.param("x", RType::FLOAT);
        let y = builder.param("y", RType::FLOAT);
        let out = builder.temp(RType::FLOAT);
        builder.push(Op::FloatBinary {
            dest: out,
            op: BinOp::Mul,
            lhs: Value::Register(x),
            rhs: Value::Register(y),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        builder.finish()
    }

    #[test]
    fn float_multiplication_cannot_fail() {
        let mut m = module(vec![float_mul("scale")]);
        run(&mut m);
        assert_eq!(convention(&m, "scale"), CallConvention::NativeInfallible);
    }

    #[test]
    fn float_division_can_fail() {
        let mut builder = FunctionBuilder::new("div", RType::FLOAT);
        let x = builder.param("x", RType::FLOAT);
        let y = builder.param("y", RType::FLOAT);
        let out = builder.temp(RType::FLOAT);
        builder.push(Op::FloatBinary {
            dest: out,
            op: BinOp::TrueDiv,
            lhs: Value::Register(x),
            rhs: Value::Register(y),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(vec![builder.finish()]);
        run(&mut m);
        assert_eq!(convention(&m, "div"), CallConvention::Native);
    }

    #[test]
    fn integer_arithmetic_stays_fallible_because_of_the_boxed_path() {
        let mut builder = FunctionBuilder::new("add", RType::INT);
        let a = builder.param("a", RType::INT);
        let b = builder.param("b", RType::INT);
        let out = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: out,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(vec![builder.finish()]);
        run(&mut m);
        assert_eq!(convention(&m, "add"), CallConvention::Native);
    }

    #[test]
    fn a_function_that_only_returns_its_argument_cannot_fail() {
        let mut builder = FunctionBuilder::new("id", RType::INT);
        let a = builder.param("a", RType::INT);
        builder.terminate(Terminator::Return(Value::Register(a)));
        let mut m = module(vec![builder.finish()]);
        run(&mut m);
        assert_eq!(convention(&m, "id"), CallConvention::NativeInfallible);
    }

    #[test]
    fn failure_propagates_from_a_callee_to_its_caller() {
        let mut caller = FunctionBuilder::new("outer", RType::FLOAT);
        let arg = caller.param("x", RType::FLOAT);
        let result = caller.temp(RType::FLOAT);
        caller.push(Op::CallNative {
            owner: None,
            dest: Some(result),
            callee: "risky".to_string(),
            args: vec![Value::Register(arg), Value::Register(arg)],
        });
        caller.terminate(Terminator::Return(Value::Register(result)));

        let mut risky = FunctionBuilder::new("risky", RType::FLOAT);
        let left = risky.param("a", RType::FLOAT);
        let right = risky.param("b", RType::FLOAT);
        let quotient = risky.temp(RType::FLOAT);
        risky.push(Op::FloatBinary {
            dest: quotient,
            op: BinOp::TrueDiv,
            lhs: Value::Register(left),
            rhs: Value::Register(right),
        });
        risky.terminate(Terminator::Return(Value::Register(quotient)));

        let mut m = module(vec![caller.finish(), risky.finish()]);
        run(&mut m);
        assert_eq!(convention(&m, "risky"), CallConvention::Native);
        assert_eq!(convention(&m, "outer"), CallConvention::Native);
    }

    #[test]
    fn infallibility_survives_a_call_to_an_infallible_callee() {
        let mut caller = FunctionBuilder::new("outer", RType::FLOAT);
        let x = caller.param("x", RType::FLOAT);
        let out = caller.temp(RType::FLOAT);
        caller.push(Op::CallNative {
            owner: None,
            dest: Some(out),
            callee: "scale".to_string(),
            args: vec![Value::Register(x), Value::Register(x)],
        });
        caller.terminate(Terminator::Return(Value::Register(out)));

        let mut m = module(vec![caller.finish(), float_mul("scale")]);
        run(&mut m);
        assert_eq!(convention(&m, "scale"), CallConvention::NativeInfallible);
        assert_eq!(convention(&m, "outer"), CallConvention::NativeInfallible);
    }

    #[test]
    fn mutual_recursion_converges_to_infallible() {
        // the greatest fixed point is the point of starting optimistic: a
        // least-fixed-point pass would call each of these fallible forever
        let build = |name: &str, other: &str| {
            let mut builder = FunctionBuilder::new(name, RType::FLOAT);
            let x = builder.param("x", RType::FLOAT);
            let out = builder.temp(RType::FLOAT);
            builder.push(Op::CallNative {
                owner: None,
                dest: Some(out),
                callee: other.to_string(),
                args: vec![Value::Register(x)],
            });
            builder.terminate(Terminator::Return(Value::Register(out)));
            builder.finish()
        };
        let mut m = module(vec![build("ping", "pong"), build("pong", "ping")]);
        run(&mut m);
        assert_eq!(convention(&m, "ping"), CallConvention::NativeInfallible);
        assert_eq!(convention(&m, "pong"), CallConvention::NativeInfallible);
    }

    #[test]
    fn one_division_in_a_cycle_makes_the_whole_cycle_fallible() {
        let mut ping = FunctionBuilder::new("ping", RType::FLOAT);
        let x = ping.param("x", RType::FLOAT);
        let out = ping.temp(RType::FLOAT);
        ping.push(Op::CallNative {
            owner: None,
            dest: Some(out),
            callee: "pong".to_string(),
            args: vec![Value::Register(x)],
        });
        ping.terminate(Terminator::Return(Value::Register(out)));

        let mut pong = FunctionBuilder::new("pong", RType::FLOAT);
        let a = pong.param("a", RType::FLOAT);
        let divided = pong.temp(RType::FLOAT);
        pong.push(Op::FloatBinary {
            dest: divided,
            op: BinOp::TrueDiv,
            lhs: Value::Register(a),
            rhs: Value::Register(a),
        });
        let back = pong.temp(RType::FLOAT);
        pong.push(Op::CallNative {
            owner: None,
            dest: Some(back),
            callee: "ping".to_string(),
            args: vec![Value::Register(divided)],
        });
        pong.terminate(Terminator::Return(Value::Register(back)));

        let mut m = module(vec![ping.finish(), pong.finish()]);
        run(&mut m);
        assert_eq!(convention(&m, "ping"), CallConvention::Native);
        assert_eq!(convention(&m, "pong"), CallConvention::Native);
    }

    #[test]
    fn a_call_to_a_function_outside_the_unit_is_fallible() {
        let mut builder = FunctionBuilder::new("outer", RType::FLOAT);
        let x = builder.param("x", RType::FLOAT);
        let out = builder.temp(RType::FLOAT);
        builder.push(Op::CallNative {
            owner: None,
            dest: Some(out),
            callee: "elsewhere".to_string(),
            args: vec![Value::Register(x)],
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(vec![builder.finish()]);
        run(&mut m);
        assert_eq!(convention(&m, "outer"), CallConvention::Native);
    }

    #[test]
    fn a_comparison_of_floats_cannot_fail() {
        let mut builder = FunctionBuilder::new("less", RType::BIT);
        let a = builder.param("a", RType::FLOAT);
        let b = builder.param("b", RType::FLOAT);
        let out = builder.temp(RType::BIT);
        builder.push(Op::FloatCompare {
            dest: out,
            op: CmpOp::Lt,
            lhs: Value::Register(a),
            rhs: Value::Register(b),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(vec![builder.finish()]);
        run(&mut m);
        assert_eq!(convention(&m, "less"), CallConvention::NativeInfallible);
    }

    #[test]
    fn only_the_dividing_float_operators_can_fail() {
        for op in [BinOp::Add, BinOp::Sub, BinOp::Mul] {
            assert!(!op.can_fail(), "{op:?}");
        }
        for op in [BinOp::Mod, BinOp::FloorDiv, BinOp::TrueDiv] {
            assert!(op.can_fail(), "{op:?}");
        }
    }
}

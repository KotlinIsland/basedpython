//! dead register elimination
//!
//! copy propagation leaves behind temporaries nothing reads any more. each one
//! still costs a C local *and* — for a refcounted representation — an entry in
//! the cleanup emitted on every exit path, so a function with three exits pays
//! for a dead register three times.
//!
//! parameters are never removed: they are the leading registers and they are the
//! signature.

use std::collections::HashSet;

use by_ir::function::{Function, ModuleIr};
use by_ir::ops::{Op, RegisterId, Value};

pub fn run(module: &mut ModuleIr) {
    for function in module.all_functions_mut() {
        eliminate(function);
    }
}

fn eliminate(function: &mut Function) {
    let live = live_registers(function);
    if live.len() == function.registers.len() {
        return;
    }

    // renumber the survivors, keeping parameters at the front and in order
    let mut remap = Vec::with_capacity(function.registers.len());
    let mut kept = Vec::with_capacity(live.len());
    for (index, decl) in function.registers.iter().enumerate() {
        if index < function.param_count || live.contains(&RegisterId(index)) {
            remap.push(Some(RegisterId(kept.len())));
            kept.push(decl.clone());
        } else {
            remap.push(None);
        }
    }
    function.registers = kept;

    let rewrite_value = |value: &mut Value, remap: &[Option<RegisterId>]| {
        if let Value::Register(id) = value
            && let Some(Some(new)) = remap.get(id.index())
        {
            *id = *new;
        }
    };

    for block in &mut function.blocks {
        for op in &mut block.ops {
            rewrite_op(op, &remap, &rewrite_value);
        }
        match &mut block.terminator {
            by_ir::ops::Terminator::Branch { cond, .. } => rewrite_value(cond, &remap),
            by_ir::ops::Terminator::Return(value) => rewrite_value(value, &remap),
            _ => {}
        }
    }
}

/// every register read or written anywhere in the function
fn live_registers(function: &Function) -> HashSet<RegisterId> {
    let mut live = HashSet::new();
    for block in &function.blocks {
        for op in &block.ops {
            if let Some(dest) = op.dest() {
                live.insert(dest);
            }
            for operand in op.operands() {
                if let Value::Register(id) = operand {
                    live.insert(*id);
                }
            }
        }
        for operand in block.terminator.operands() {
            if let Value::Register(id) = operand {
                live.insert(*id);
            }
        }
    }
    live
}

fn rewrite_op(
    op: &mut Op,
    remap: &[Option<RegisterId>],
    rewrite_value: &impl Fn(&mut Value, &[Option<RegisterId>]),
) {
    let rewrite_dest = |dest: &mut RegisterId| {
        if let Some(Some(new)) = remap.get(dest.index()) {
            *dest = *new;
        }
    };
    match op {
        Op::AsyncContext {
            dest,
            manager,
            exception: Some(exception),
        } => {
            rewrite_dest(dest);
            rewrite_value(manager, remap);
            rewrite_value(exception, remap);
        }
        Op::Assign { dest, src } => {
            rewrite_dest(dest);
            rewrite_value(src, remap);
        }
        Op::IntBinary { dest, lhs, rhs, .. }
        | Op::FloatBinary { dest, lhs, rhs, .. }
        | Op::FloatObjectBinary { dest, lhs, rhs, .. }
        | Op::FloatObjectCompare { dest, lhs, rhs, .. }
        | Op::Identity { dest, lhs, rhs, .. }
        | Op::Contains {
            dest,
            value: lhs,
            container: rhs,
            ..
        }
        | Op::IsInstance {
            dest,
            src: lhs,
            class: rhs,
        }
        | Op::MatchKey {
            dest,
            map: lhs,
            key: rhs,
        }
        | Op::MatchRest {
            dest,
            map: lhs,
            keys: rhs,
        }
        | Op::IntCompare { dest, lhs, rhs, .. }
        | Op::FloatCompare { dest, lhs, rhs, .. }
        | Op::ObjectBinary { dest, lhs, rhs, .. }
        | Op::ObjectCompare { dest, lhs, rhs, .. }
        | Op::StrCompare { dest, lhs, rhs, .. }
        | Op::StrConcat { dest, lhs, rhs, .. }
        | Op::StrConcatInt {
            dest,
            lhs,
            value: rhs,
        } => {
            rewrite_dest(dest);
            rewrite_value(lhs, remap);
            rewrite_value(rhs, remap);
        }
        Op::Unary { dest, operand, .. } => {
            rewrite_dest(dest);
            rewrite_value(operand, remap);
        }
        Op::IterNext { dest, iter } => {
            rewrite_dest(dest);
            rewrite_value(iter, remap);
        }
        Op::CallNative { dest, args, .. } => {
            if let Some(dest) = dest {
                rewrite_dest(dest);
            }
            for arg in args {
                rewrite_value(arg, remap);
            }
        }
        Op::CallPython { dest, args, .. } => {
            rewrite_dest(dest);
            for arg in args {
                rewrite_value(arg, remap);
            }
        }
        Op::LoadGlobal { dest, .. }
        | Op::ModuleDict { dest }
        | Op::LoadClass { dest, .. }
        | Op::ImportModule { dest, .. } => {
            rewrite_dest(dest);
        }
        Op::Warn {
            dest,
            message,
            category,
            ..
        } => {
            rewrite_dest(dest);
            rewrite_value(message, remap);
            if let Some(category) = category {
                rewrite_value(category, remap);
            }
        }
        Op::StoreGlobal { dest, value, .. } => {
            rewrite_dest(dest);
            rewrite_value(value, remap);
        }
        Op::DeleteGlobal { dest, .. } | Op::DeleteLocal { dest } => {
            rewrite_dest(dest);
        }
        Op::ImportFrom { dest, module, .. } => {
            rewrite_dest(dest);
            rewrite_value(module, remap);
        }
        Op::NewInstance { dest, fields, .. } => {
            rewrite_dest(dest);
            for field in fields.iter_mut().flatten() {
                rewrite_value(field, remap);
            }
        }
        Op::GetCell { dest, receiver, .. } => {
            rewrite_dest(dest);
            rewrite_value(receiver, remap);
        }
        Op::MakeClosure { dest, env, .. } => {
            rewrite_dest(dest);
            rewrite_value(env, remap);
        }
        Op::CallValue { dest, callee, args } => {
            rewrite_dest(dest);
            rewrite_value(callee, remap);
            for arg in args {
                rewrite_value(arg, remap);
            }
        }
        Op::CallMethod {
            dest,
            receiver,
            args,
            ..
        } => {
            rewrite_dest(dest);
            rewrite_value(receiver, remap);
            for arg in args {
                rewrite_value(arg, remap);
            }
        }
        Op::GetAttr { dest, receiver, .. } | Op::GetField { dest, receiver, .. } => {
            rewrite_dest(dest);
            rewrite_value(receiver, remap);
        }
        Op::SetField {
            receiver, value, ..
        } => {
            rewrite_value(receiver, remap);
            rewrite_value(value, remap);
        }
        Op::SetAttr {
            dest,
            receiver,
            value,
            ..
        } => {
            rewrite_dest(dest);
            rewrite_value(receiver, remap);
            rewrite_value(value, remap);
        }
        Op::Box { dest, src }
        | Op::IsSequence { dest, src }
        | Op::IsMapping { dest, src }
        | Op::AsyncIter { dest, src, .. }
        | Op::AsyncContext {
            dest,
            manager: src,
            exception: None,
        }
        | Op::IsMissing { dest, src }
        | Op::MethodStands { dest, src, .. }
        | Op::DictShadows { dest, src, .. }
        | Op::MatchAttr {
            dest, subject: src, ..
        }
        | Op::MatchSlice {
            dest,
            sequence: src,
            ..
        }
        | Op::IntToFloat { dest, src }
        | Op::Unbox { dest, src, .. } => {
            rewrite_dest(dest);
            rewrite_value(src, remap);
        }
        Op::TupleBuild { dest, items }
        | Op::BuildList { dest, items }
        | Op::BuildSet { dest, items }
        | Op::BuildTuple { dest, items } => {
            rewrite_dest(dest);
            for item in items {
                rewrite_value(item, remap);
            }
        }
        Op::TupleGet { dest, src, .. }
        | Op::Truthy { dest, src }
        | Op::Len { dest, src }
        | Op::StrOfInt { dest, value: src }
        | Op::GetIter { dest, src }
        | Op::IsNull { dest, src } => {
            rewrite_dest(dest);
            rewrite_value(src, remap);
        }
        Op::BuildDict { dest, pairs } => {
            rewrite_dest(dest);
            for pair in pairs {
                rewrite_value(pair, remap);
            }
        }
        Op::DictFind {
            dest,
            container,
            key: index,
        }
        | Op::GetItem {
            dest,
            container,
            index,
        }
        | Op::StrGetItem {
            dest,
            container,
            index,
        }
        | Op::StrItemCompare {
            dest,
            container,
            index,
            ..
        } => {
            rewrite_dest(dest);
            rewrite_value(container, remap);
            rewrite_value(index, remap);
        }
        Op::SetItem {
            dest,
            container,
            index,
            value,
        } => {
            rewrite_dest(dest);
            rewrite_value(container, remap);
            rewrite_value(index, remap);
            rewrite_value(value, remap);
        }
        Op::Format {
            dest, value, spec, ..
        } => {
            rewrite_dest(dest);
            rewrite_value(value, remap);
            if let Some(spec) = spec {
                rewrite_value(spec, remap);
            }
        }
        Op::FetchException { dest } => rewrite_dest(dest),
        Op::ExceptionMatches { dest, value, class } => {
            rewrite_dest(dest);
            rewrite_value(value, remap);
            rewrite_value(class, remap);
        }
        Op::Reraise { value } => rewrite_value(value, remap),
        Op::Extend {
            dest,
            container,
            source,
            ..
        } => {
            rewrite_dest(dest);
            rewrite_value(container, remap);
            rewrite_value(source, remap);
        }
        Op::CallUnpacked {
            dest,
            callee,
            args,
            kwargs,
        } => {
            rewrite_dest(dest);
            rewrite_value(callee, remap);
            rewrite_value(args, remap);
            if let Some(kwargs) = kwargs {
                rewrite_value(kwargs, remap);
            }
        }
        Op::ArrayNew { dest, items } => {
            rewrite_dest(dest);
            for item in items {
                rewrite_value(item, remap);
            }
        }
        Op::ArrayGet { dest, array, index } => {
            rewrite_dest(dest);
            rewrite_value(array, remap);
            rewrite_value(index, remap);
        }
        Op::ArraySet {
            dest,
            array,
            index,
            value,
        } => {
            rewrite_dest(dest);
            rewrite_value(array, remap);
            rewrite_value(index, remap);
            rewrite_value(value, remap);
        }
        Op::DeleteItem {
            dest,
            container,
            index,
        } => {
            rewrite_dest(dest);
            rewrite_value(container, remap);
            rewrite_value(index, remap);
        }
        Op::DeleteAttr { dest, receiver, .. } => {
            rewrite_dest(dest);
            rewrite_value(receiver, remap);
        }
        Op::ArrayRead { dest, array, index } => {
            rewrite_dest(dest);
            rewrite_value(array, remap);
            rewrite_value(index, remap);
        }
        Op::ArrayLen { dest, array } => {
            rewrite_dest(dest);
            rewrite_value(array, remap);
        }
        Op::ArrayPush { dest, array, value } => {
            rewrite_dest(dest);
            rewrite_value(array, remap);
            rewrite_value(value, remap);
        }
        Op::ToTuple { dest, src } => {
            rewrite_dest(dest);
            rewrite_value(src, remap);
        }
        Op::Unpack { dest, src, .. } => {
            rewrite_dest(dest);
            rewrite_value(src, remap);
        }
        Op::PushHandled { dest, value } => {
            rewrite_dest(dest);
            rewrite_value(value, remap);
        }
        Op::PopHandled { value } => rewrite_value(value, remap),
        Op::RaiseObject { exception, cause } => {
            rewrite_value(exception, remap);
            if let Some(cause) = cause {
                rewrite_value(cause, remap);
            }
        }
        Op::RaiseStandard { .. } => {}
        Op::RaiseWith { value, .. } | Op::FinishFrame { value } => rewrite_value(value, remap),
        Op::Enter { dest, manager } => {
            rewrite_dest(dest);
            rewrite_value(manager, remap);
        }
        Op::ExitContext {
            dest,
            manager,
            exception,
        } => {
            rewrite_dest(dest);
            rewrite_value(manager, remap);
            rewrite_value(exception, remap);
        }
        Op::DelegateIter { dest, src, .. } => {
            rewrite_dest(dest);
            rewrite_value(src, remap);
        }
        Op::DelegateStep { dest, inner, sent } => {
            rewrite_dest(dest);
            rewrite_value(inner, remap);
            rewrite_value(sent, remap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::builder::FunctionBuilder;
    use by_ir::ops::{BinOp, Terminator};
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
    fn a_register_nothing_touches_is_removed() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let _dead = builder.temp(RType::INT);
        let live = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: live,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Return(Value::Register(live)));

        let mut m = module(builder.finish());
        assert_eq!(m.functions[0].registers.len(), 3);
        run(&mut m);
        assert_eq!(m.functions[0].registers.len(), 2);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn the_survivors_are_renumbered_consistently() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let _dead = builder.temp(RType::INT);
        let live = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: live,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Return(Value::Register(live)));
        let mut m = module(builder.finish());
        run(&mut m);
        // r2 became r1, and both the write and the read followed it
        let text = print_function(&m.functions[0]);
        assert!(text.contains("r1 = a + 1"), "{text}");
        assert!(text.contains("return r1"), "{text}");
        assert!(!text.contains("r2"), "{text}");
    }

    #[test]
    fn an_unused_parameter_is_kept() {
        // removing it would change the signature the wrapper and every caller use
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let _unused = builder.param("b", RType::INT);
        builder.terminate(Terminator::Return(Value::Register(a)));
        let mut m = module(builder.finish());
        run(&mut m);
        assert_eq!(m.functions[0].param_count, 2);
        assert_eq!(m.functions[0].registers.len(), 2);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }

    #[test]
    fn a_function_with_nothing_dead_is_left_untouched() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let out = builder.temp(RType::INT);
        builder.push(Op::IntBinary {
            dest: out,
            op: BinOp::Add,
            lhs: Value::Register(a),
            rhs: Value::Int(1),
        });
        builder.terminate(Terminator::Return(Value::Register(out)));
        let mut m = module(builder.finish());
        let before = m.functions[0].clone();
        run(&mut m);
        assert_eq!(m.functions[0], before);
    }

    #[test]
    fn registers_used_only_in_a_branch_condition_stay_live() {
        let mut builder = FunctionBuilder::new("f", RType::INT);
        let a = builder.param("a", RType::INT);
        let cond = builder.temp(RType::BIT);
        let then_block = builder.new_block();
        let else_block = builder.new_block();
        builder.push(Op::IntCompare {
            dest: cond,
            op: by_ir::ops::CmpOp::Lt,
            lhs: Value::Register(a),
            rhs: Value::Int(0),
        });
        builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block,
            else_block,
        });
        builder.switch_to(then_block);
        builder.terminate(Terminator::Return(Value::Int(0)));
        builder.switch_to(else_block);
        builder.terminate(Terminator::Return(Value::Int(1)));

        let mut m = module(builder.finish());
        run(&mut m);
        assert_eq!(m.functions[0].registers.len(), 2);
        assert_eq!(verify(&m.functions[0]), Ok(()));
    }
}

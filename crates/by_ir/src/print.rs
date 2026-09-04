//! textual BIR
//!
//! the printed form is the snapshot format for IR tests, so it is stable and
//! deliberately terse. it is not parsed back — tests build BIR through the
//! builder and compare the rendering.

use std::fmt::Write;

use crate::function::{Function, ModuleIr};
use crate::ops::{Mutation, Op, RegisterId, Terminator, UnaryOp, Value};

/// render a whole module
pub fn print_module(module: &ModuleIr) -> String {
    let mut out = format!("module {}\n", module.name.dotted());
    for function in &module.functions {
        out.push('\n');
        out.push_str(&print_function(function));
    }
    for declined in &module.declined {
        let _ = writeln!(out, "\ndeclined {} — {}", declined.name, declined.reason);
    }
    out
}

/// render one function
pub fn print_function(function: &Function) -> String {
    let mut out = String::new();

    let params = function
        .params()
        .iter()
        .enumerate()
        .map(|(index, decl)| {
            format!(
                "{}: {}",
                register_name(function, RegisterId(index)),
                decl.ty
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    let convention = match function.convention {
        crate::function::CallConvention::Native => "",
        crate::function::CallConvention::NativeInfallible => " infallible",
    };
    let _ = writeln!(
        out,
        "def {}({}) -> {}{}",
        function.name, params, function.ret, convention
    );

    for (index, decl) in function
        .registers
        .iter()
        .enumerate()
        .skip(function.param_count)
    {
        let _ = writeln!(
            out,
            "  let {}: {}",
            register_name(function, RegisterId(index)),
            decl.ty
        );
    }

    for (index, block) in function.blocks.iter().enumerate() {
        let _ = writeln!(out, "b{index}:");
        for op in &block.ops {
            let _ = writeln!(out, "  {}", print_op(function, op));
        }
        let _ = writeln!(out, "  {}", print_terminator(function, &block.terminator));
    }
    out
}

fn register_name(function: &Function, id: RegisterId) -> String {
    match function.register(id).and_then(|decl| decl.name.as_deref()) {
        Some(name) => name.to_string(),
        None => format!("r{}", id.0),
    }
}

fn print_value(function: &Function, value: &Value) -> String {
    match value {
        Value::Register(id) => register_name(function, *id),
        Value::Int(v) => v.to_string(),
        Value::Fixed(v) => format!("{v}i64"),
        // `{:?}` keeps a round trip: 1.0 prints as `1.0`, not `1`
        Value::Float(v) => format!("{v:?}"),
        Value::Bool(v) => v.to_string(),
        Value::Bit(v) => if *v { "1b" } else { "0b" }.to_string(),
        Value::None => "None".to_string(),
        Value::Str(v) => format!("{v:?}"),
        Value::Bytes(v) => format!("b{:?}", String::from_utf8_lossy(v)),
    }
}

fn print_op(function: &Function, op: &Op) -> String {
    let value = |v: &Value| print_value(function, v);
    let name = |id: RegisterId| register_name(function, id);
    match op {
        Op::Assign { dest, src } => format!("{} = {}", name(*dest), value(src)),
        Op::Truthy { dest, src } => format!("{} = truthy {}", name(*dest), value(src)),
        Op::IsInstance { dest, src, class } => format!(
            "{} = isinstance {} {}",
            name(*dest),
            value(src),
            value(class)
        ),
        Op::MatchKey { dest, map, key } => {
            format!("{} = key {} {}", name(*dest), value(map), value(key))
        }
        Op::MatchRest { dest, map, keys } => {
            format!("{} = rest {} {}", name(*dest), value(map), value(keys))
        }
        Op::AsyncContext {
            dest,
            manager,
            exception,
        } => format!(
            "{} = {} {}",
            name(*dest),
            if exception.is_some() {
                "aexit"
            } else {
                "aenter"
            },
            value(manager)
        ),
        Op::AsyncIter { dest, src, next } => format!(
            "{} = {} {}",
            name(*dest),
            if *next { "anext" } else { "aiter" },
            value(src)
        ),
        Op::IsMapping { dest, src } => {
            format!("{} = is-mapping {}", name(*dest), value(src))
        }
        Op::MatchAttr {
            dest,
            subject,
            name: attribute,
            ..
        } => format!(
            "{} = attr {} {}",
            name(*dest),
            value(subject),
            attribute.as_deref().unwrap_or("<positional>")
        ),
        Op::MethodStands {
            dest,
            src,
            class,
            method,
        } => format!(
            "{} = method-stands {} {class}.{method}",
            name(*dest),
            value(src)
        ),
        Op::DictShadows {
            dest,
            src,
            class,
            method,
        } => format!(
            "{} = dict-shadows {} {class}.{method}",
            name(*dest),
            value(src)
        ),
        Op::IsMissing { dest, src } => {
            format!("{} = is-missing {}", name(*dest), value(src))
        }
        Op::MatchSlice {
            dest,
            sequence,
            start,
            after,
            rest,
        } => format!(
            "{} = {} {}[{start}:-{after}]",
            name(*dest),
            if *rest { "rest" } else { "element" },
            value(sequence)
        ),
        Op::IsSequence { dest, src } => {
            format!("{} = is-sequence {}", name(*dest), value(src))
        }
        Op::Contains {
            dest,
            value: item,
            container,
            negated,
        } => format!(
            "{} = {} {} {}",
            name(*dest),
            value(item),
            if *negated { "not in" } else { "in" },
            value(container)
        ),
        Op::Identity {
            dest,
            lhs,
            rhs,
            negated,
        } => format!(
            "{} = {} {} {}",
            name(*dest),
            value(lhs),
            if *negated { "is not" } else { "is" },
            value(rhs)
        ),
        Op::FloatObjectCompare { dest, op, lhs, rhs } => format!(
            "{} = {} {} {}",
            name(*dest),
            value(lhs),
            op.symbol(),
            value(rhs)
        ),
        Op::IntBinary { dest, op, lhs, rhs }
        | Op::FloatBinary { dest, op, lhs, rhs }
        | Op::FloatObjectBinary { dest, op, lhs, rhs } => {
            format!(
                "{} = {} {} {}",
                name(*dest),
                value(lhs),
                op.symbol(),
                value(rhs)
            )
        }
        Op::ObjectBinary {
            dest,
            op,
            lhs,
            rhs,
            mutation,
        } => {
            format!(
                "{} = {} {}{} {}",
                name(*dest),
                value(lhs),
                op.symbol(),
                if *mutation == Mutation::InPlace {
                    "="
                } else {
                    ""
                },
                value(rhs)
            )
        }
        Op::IntCompare { dest, op, lhs, rhs }
        | Op::FloatCompare { dest, op, lhs, rhs }
        | Op::StrCompare { dest, op, lhs, rhs }
        | Op::ObjectCompare { dest, op, lhs, rhs } => {
            format!(
                "{} = {} {} {}",
                name(*dest),
                value(lhs),
                op.symbol(),
                value(rhs)
            )
        }
        Op::Unary { dest, op, operand } => {
            let symbol = match op {
                UnaryOp::Neg => "-",
                UnaryOp::Not => "not ",
                UnaryOp::Invert => "~",
            };
            format!("{} = {}{}", name(*dest), symbol, value(operand))
        }
        Op::CallNative {
            dest,
            owner,
            callee,
            args,
        } => {
            let args = args.iter().map(value).collect::<Vec<_>>().join(", ");
            let target = match owner {
                Some(owner) => format!("{owner}.{callee}"),
                None => callee.clone(),
            };
            match dest {
                Some(dest) => format!("{} = call {}({})", name(*dest), target, args),
                None => format!("call {target}({args})"),
            }
        }
        Op::Box { dest, src } => format!("{} = box {}", name(*dest), value(src)),
        Op::IntToFloat { dest, src } => format!("{} = float {}", name(*dest), value(src)),
        Op::Unbox { dest, src, to } => {
            format!("{} = unbox {} as {}", name(*dest), value(src), to)
        }
        Op::TupleBuild { dest, items } => {
            let items = items.iter().map(value).collect::<Vec<_>>().join(", ");
            format!("{} = ({})", name(*dest), items)
        }
        Op::CallUnpacked {
            dest,
            callee,
            args,
            kwargs,
        } => match kwargs {
            Some(kwargs) => format!(
                "{} = call {} (*{}, **{})",
                name(*dest),
                value(callee),
                value(args),
                value(kwargs)
            ),
            None => format!(
                "{} = call {} (*{})",
                name(*dest),
                value(callee),
                value(args)
            ),
        },
        Op::ArrayNew { dest, items } => format!(
            "{} = array[{}]",
            name(*dest),
            items.iter().map(value).collect::<Vec<_>>().join(", ")
        ),
        Op::ArrayGet { dest, array, index } => {
            format!("{} = {}[{}]", name(*dest), value(array), value(index))
        }
        Op::ArraySet {
            dest,
            array,
            index,
            value: v,
        } => format!(
            "{} = {}[{}] = {}",
            name(*dest),
            value(array),
            value(index),
            value(v)
        ),
        Op::ArrayLen { dest, array } => format!("{} = arraylen {}", name(*dest), value(array)),
        Op::DeleteItem {
            dest,
            container,
            index,
        } => format!(
            "{} = del {}[{}]",
            name(*dest),
            value(container),
            value(index)
        ),
        Op::DeleteAttr {
            dest,
            receiver,
            name: field,
        } => format!("{} = del {}.{field}", name(*dest), value(receiver)),
        Op::ArrayRead { dest, array, index } => {
            format!(
                "{} = {}[{}] unchecked",
                name(*dest),
                value(array),
                value(index)
            )
        }
        Op::ArrayPush {
            dest,
            array,
            value: v,
        } => {
            format!("{} = {} push {}", name(*dest), value(array), value(v))
        }
        Op::ToTuple { dest, src } => format!("{} = tuple {}", name(*dest), value(src)),
        Op::Extend {
            dest,
            container,
            source,
            mapping,
        } => format!(
            "{} = {} {} {}",
            name(*dest),
            value(container),
            if *mapping { "update" } else { "extend" },
            value(source)
        ),
        Op::Unpack { dest, src, starred } => match starred {
            Some(index) => format!("{} = unpack {} star {index}", name(*dest), value(src)),
            None => format!("{} = unpack {}", name(*dest), value(src)),
        },
        Op::TupleGet { dest, src, index } => {
            format!("{} = {}[{}]", name(*dest), value(src), index)
        }
        Op::Len { dest, src } => format!("{} = len {}", name(*dest), value(src)),
        Op::StrOfInt { dest, value: src } => {
            format!("{} = str-of-int {}", name(*dest), value(src))
        }
        Op::StrConcatInt {
            dest,
            lhs,
            value: src,
        } => {
            format!(
                "{} = str-concat-int {}, {}",
                name(*dest),
                value(lhs),
                value(src)
            )
        }
        Op::NewInstance {
            dest,
            class,
            fields,
        } => {
            let fields = fields
                .iter()
                .map(|field| match field {
                    Some(field) => value(field),
                    None => "unset".to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{} = new {class}({fields})", name(*dest))
        }
        Op::GetCell {
            dest,
            receiver,
            class,
            field,
            ..
        } => format!(
            "{} = cell {}.<{class}.{field}>",
            name(*dest),
            value(receiver)
        ),
        Op::MakeClosure {
            dest,
            class,
            method,
            env,
        } => format!(
            "{} = closure {class}.{method} over {}",
            name(*dest),
            value(env)
        ),
        Op::Enter { dest, manager } => {
            format!("{} = enter {}", name(*dest), value(manager))
        }
        Op::ExitContext {
            dest,
            manager,
            exception,
        } => format!(
            "{} = exit {} with {}",
            name(*dest),
            value(manager),
            value(exception)
        ),
        Op::DelegateIter {
            dest,
            src,
            awaitable,
        } => format!(
            "{} = {} {}",
            name(*dest),
            if *awaitable { "awaititer" } else { "delegiter" },
            value(src)
        ),
        Op::DelegateStep { dest, inner, sent } => {
            format!("{} = step {} <- {}", name(*dest), value(inner), value(sent))
        }
        Op::RaiseWith {
            error,
            value: payload,
        } => format!("raise {error:?}({})", value(payload)),
        Op::FinishFrame { value: payload } => format!("finish {}", value(payload)),
        Op::LoadGlobal { dest, name: global } => {
            format!("{} = global {global}", name(*dest))
        }
        Op::ModuleDict { dest } => format!("{} = globals", name(*dest)),
        Op::Warn {
            dest,
            message,
            category,
            stacklevel,
            offset,
        } => format!(
            "{} = warn {}{} up {stacklevel} at {offset}",
            name(*dest),
            value(message),
            category
                .as_ref()
                .map(|category| format!(" as {}", value(category)))
                .unwrap_or_default()
        ),
        Op::StoreGlobal {
            dest,
            name: global,
            value: v,
        } => format!("{} = global {global} <- {}", name(*dest), value(v)),
        Op::DeleteGlobal { dest, name: global } => {
            format!("{} = del global {global}", name(*dest))
        }
        Op::DeleteLocal { dest } => format!("del {}", name(*dest)),
        Op::LoadClass { dest, class } => {
            format!("{} = class {class}", name(*dest))
        }
        Op::ImportModule {
            dest,
            name: module,
            fromlist,
            level,
        } => format!(
            "{} = import {}{module} for ({})",
            name(*dest),
            ".".repeat(*level as usize),
            fromlist.join(", ")
        ),
        Op::ImportFrom {
            dest,
            module,
            name: imported,
        } => format!("{} = from {} import {imported}", name(*dest), value(module)),
        Op::CallValue { dest, callee, args } => {
            let args = args.iter().map(value).collect::<Vec<_>>().join(", ");
            format!("{} = callobj {}({})", name(*dest), value(callee), args)
        }
        Op::CallPython { dest, callee, args } => {
            let args = args.iter().map(value).collect::<Vec<_>>().join(", ");
            format!("{} = pycall {}({})", name(*dest), callee, args)
        }
        Op::CallMethod {
            dest,
            receiver,
            name: method,
            args,
        } => {
            let args = args.iter().map(value).collect::<Vec<_>>().join(", ");
            format!("{} = {}.{method}({args})", name(*dest), value(receiver))
        }
        Op::GetField {
            dest,
            receiver,
            class,
            field,
        } => format!("{} = {}.<{class}.{field}>", name(*dest), value(receiver)),
        Op::SetField {
            receiver,
            class,
            field,
            value: v,
        } => format!("{}.<{class}.{field}> = {}", value(receiver), value(v)),
        Op::GetAttr {
            dest,
            receiver,
            name: attr,
        } => format!("{} = {}.{attr}", name(*dest), value(receiver)),
        Op::SetAttr {
            dest,
            receiver,
            name: attr,
            value: v,
        } => format!(
            "{} = ({}.{attr} = {})",
            name(*dest),
            value(receiver),
            value(v)
        ),
        Op::BuildList { dest, items } => {
            let items = items.iter().map(value).collect::<Vec<_>>().join(", ");
            format!("{} = [{items}]", name(*dest))
        }
        Op::BuildSet { dest, items } => {
            let items = items.iter().map(value).collect::<Vec<_>>().join(", ");
            format!("{} = {{{items}}}", name(*dest))
        }
        Op::BuildTuple { dest, items } => {
            let items = items.iter().map(value).collect::<Vec<_>>().join(", ");
            format!("{} = tuple({items})", name(*dest))
        }
        Op::BuildDict { dest, pairs } => {
            let pairs = pairs.iter().map(value).collect::<Vec<_>>().join(", ");
            format!("{} = dict({pairs})", name(*dest))
        }
        Op::GetItem {
            dest,
            container,
            index,
        }
        | Op::StrGetItem {
            dest,
            container,
            index,
        } => format!("{} = {}[{}]", name(*dest), value(container), value(index)),
        Op::DictFind {
            dest,
            container,
            key,
        } => format!(
            "{} = find {}[{}]",
            name(*dest),
            value(container),
            value(key)
        ),
        Op::StrItemCompare {
            dest,
            op,
            container,
            index,
            character,
        } => format!(
            "{} = {}[{}] {} {:?}",
            name(*dest),
            value(container),
            value(index),
            op.symbol(),
            character.to_string()
        ),
        Op::SetItem {
            dest,
            container,
            index,
            value: v,
        } => format!(
            "{} = ({}[{}] = {})",
            name(*dest),
            value(container),
            value(index),
            value(v)
        ),
        Op::Format {
            dest,
            value: v,
            spec,
            conversion,
        } => {
            let spec = spec.as_ref().map(value).unwrap_or_default();
            format!(
                "{} = format {} {conversion:?} {spec}",
                name(*dest),
                value(v)
            )
        }
        Op::FetchException { dest } => format!("{} = fetch exception", name(*dest)),
        Op::ExceptionMatches {
            dest,
            value: v,
            class,
        } => {
            format!("{} = {} matches {}", name(*dest), value(v), value(class))
        }
        Op::PushHandled { dest, value: v } => {
            format!("{} = push handled {}", name(*dest), value(v))
        }
        Op::PopHandled { value: v } => format!("pop handled {}", value(v)),
        Op::RaiseObject { exception, cause } => match cause {
            Some(cause) => format!("raise {} from {}", value(exception), value(cause)),
            None => format!("raise {}", value(exception)),
        },
        Op::Reraise { value: v } => format!("reraise {}", value(v)),
        Op::GetIter { dest, src, cursor } => match cursor {
            Some(cursor) => format!("{} = iter {} @{}", name(*dest), value(src), name(*cursor)),
            None => format!("{} = iter {}", name(*dest), value(src)),
        },
        Op::IterNext { dest, iter, cursor } => match cursor {
            Some(cursor) => format!("{} = next {} @{}", name(*dest), value(iter), name(*cursor)),
            None => format!("{} = next {}", name(*dest), value(iter)),
        },
        Op::IsNull { dest, src } => format!("{} = {} is null", name(*dest), value(src)),
        Op::StrConcat {
            dest,
            lhs,
            rhs,
            consumes_lhs,
        } => {
            let take = if *consumes_lhs { "move " } else { "" };
            format!("{} = {take}{} ++ {}", name(*dest), value(lhs), value(rhs))
        }
        Op::RaiseStandard { error, message } => {
            format!("raise {error:?}({message:?})")
        }
    }
}

fn print_terminator(function: &Function, terminator: &Terminator) -> String {
    match terminator {
        Terminator::Goto(target) => format!("goto b{}", target.0),
        Terminator::Branch {
            cond,
            then_block,
            else_block,
        } => format!(
            "branch {} ? b{} : b{}",
            print_value(function, cond),
            then_block.0,
            else_block.0
        ),
        Terminator::Return(value) => format!("return {}", print_value(function, value)),
        Terminator::NarrowShort {
            dest,
            src,
            fits,
            otherwise,
        } => format!(
            "{} = narrow-short {} ? b{} : b{}",
            register_name(function, *dest),
            print_value(function, src),
            fits.0,
            otherwise.0
        ),
        Terminator::Unreachable => "unreachable".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::function::{BasicBlock, CallConvention, Declined, RegisterDecl};
    use crate::ops::{BinOp, BlockId, CmpOp};
    use crate::rtype::RType;

    fn sample() -> Function {
        let mut entry = BasicBlock::new(Terminator::Branch {
            cond: Value::Register(RegisterId(2)),
            then_block: BlockId(1),
            else_block: BlockId(2),
        });
        entry.ops.push(Op::IntCompare {
            dest: RegisterId(2),
            op: CmpOp::Lt,
            lhs: Value::Register(RegisterId(0)),
            rhs: Value::Int(0),
        });
        let mut negate = BasicBlock::new(Terminator::Return(Value::Register(RegisterId(3))));
        negate.ops.push(Op::IntBinary {
            dest: RegisterId(3),
            op: BinOp::Sub,
            lhs: Value::Int(0),
            rhs: Value::Register(RegisterId(0)),
        });
        let identity = BasicBlock::new(Terminator::Return(Value::Register(RegisterId(0))));

        Function {
            posonly: 0,
            kwonly: 0,
            defaults: Vec::new(),
            vararg: false,
            kwarg: false,
            range: None,
            name: "abs".to_string(),
            param_count: 1,
            ret: RType::INT,
            convention: CallConvention::NativeInfallible,
            registers: vec![
                RegisterDecl {
                    borrowed: false,
                    name: Some("n".to_string()),
                    ty: RType::INT,
                    may_be_unassigned: false,
                },
                RegisterDecl {
                    borrowed: false,
                    name: None,
                    ty: RType::INT,
                    may_be_unassigned: false,
                },
                RegisterDecl {
                    borrowed: false,
                    name: None,
                    ty: RType::BIT,
                    may_be_unassigned: false,
                },
                RegisterDecl {
                    borrowed: false,
                    name: None,
                    ty: RType::INT,
                    may_be_unassigned: false,
                },
            ],
            blocks: vec![entry, negate, identity],
            exported: true,
            owner: None,
            decorators: Vec::new(),
            deferring: Vec::new(),
            computed_defaults: Vec::new(),
            defaults_held_by: crate::function::DefaultsHeldBy::Twin,
            binding: crate::function::Binding::Instance,
            coroutine_body: None,
            doc: None,
            takes_a_weak_reference: false,
        }
    }

    #[test]
    fn a_function_renders_to_stable_text() {
        let expected = "\
def abs(n: int) -> int infallible
  let r1: int
  let r2: bit
  let r3: int
b0:
  r2 = n < 0
  branch r2 ? b1 : b2
b1:
  r3 = 0 - n
  return r3
b2:
  return n
";
        assert_eq!(print_function(&sample()), expected);
    }

    #[test]
    fn a_module_lists_declines_after_its_functions() {
        let module = ModuleIr {
            name: crate::ModuleName::new("app"),
            functions: vec![sample()],
            declined: vec![Declined {
                range: None,
                name: "gen".to_string(),
                reason: "generators are not lowered yet".to_string(),
            }],
            classes: Vec::new(),
            gradual: Vec::new(),
            promoted: Vec::new(),
            lines: None,
            fallback_source: None,
            fallback_code: None,
            shims: None,
        };
        let text = print_module(&module);
        assert!(text.starts_with("module app\n"));
        assert!(text.contains("declined gen — generators are not lowered yet"));
    }

    #[test]
    fn floats_keep_their_decimal_point() {
        // `1.0` must not render as `1`, or the snapshot would not say which
        // representation the register holds
        let function = Function {
            posonly: 0,
            kwonly: 0,
            defaults: Vec::new(),
            vararg: false,
            kwarg: false,
            range: None,
            name: "f".to_string(),
            param_count: 0,
            ret: RType::FLOAT,
            convention: CallConvention::NativeInfallible,
            registers: Vec::new(),
            blocks: vec![BasicBlock::new(Terminator::Return(Value::Float(1.0)))],
            exported: true,
            owner: None,
            decorators: Vec::new(),
            deferring: Vec::new(),
            computed_defaults: Vec::new(),
            defaults_held_by: crate::function::DefaultsHeldBy::Twin,
            binding: crate::function::Binding::Instance,
            coroutine_body: None,
            doc: None,
            takes_a_weak_reference: false,
        };
        assert!(print_function(&function).contains("return 1.0"));
    }
}

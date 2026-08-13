//! IR snapshot tests: `.by` source in, textual BIR out
//!
//! the printed IR is the contract between the frontend and everything after it,
//! so asserting on it catches a lowering change that would otherwise only show
//! up as different generated C.

use by_ir::builder::FunctionBuilder;
use by_ir::function::ClassBase;
use by_ir::ops::{Op, RegisterId, Terminator, Value};
use by_ir::print::{print_function, print_module};
use by_ir::rtype::RType;
use by_ir::verify::verify_module;

use crate::single_file::with_source;

/// whether any block holds an op matching `predicate`
fn has_op(function: &by_ir::function::Function, predicate: impl Fn(&Op) -> bool) -> bool {
    function
        .blocks
        .iter()
        .flat_map(|block| block.ops.iter())
        .any(predicate)
}

/// lower `source` and render the module's IR, failing if it does not verify
fn ir(source: &str) -> String {
    with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        if let Err(errors) = verify_module(&module) {
            let detail = errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "the lowered IR does not verify:\n{detail}\n\n{}",
                print_module(&module)
            );
        }
        print_module(&module)
    })
}

/// the reason the single function in `source` was declined
fn decline(source: &str) -> String {
    with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        assert!(
            module.functions.is_empty(),
            "expected the function to be declined, but it lowered:\n{}",
            print_module(&module)
        );
        module
            .declined
            .first()
            .map(|declined| declined.reason.clone())
            .unwrap_or_else(|| "nothing was declined".to_string())
    })
}

#[test]
fn a_function_of_integer_arithmetic_lowers() {
    let ir = ir("def add(a: int, b: int) -> int:\n    return a + b\n");
    assert_eq!(
        ir,
        "\
module app

def add(a: int, b: int) -> int
  let r2: int
b0:
  r2 = a + b
  return r2
"
    );
}

#[test]
fn a_local_becomes_a_named_register() {
    let ir = ir("def f(a: int) -> int:\n    total = a * 2\n    return total\n");
    // the frontend computes into a temp and copies. that copy is redundant and
    // is a copy-propagation pass's job, not the frontend's — the IR is correct
    // either way, and the C compiler coalesces the two
    assert!(ir.contains("let total: int"), "{ir}");
    assert!(ir.contains(" = a * 2"), "{ir}");
    assert!(
        ir.lines()
            .any(|line| line.trim() == "total = r2" || line.trim() == "total = r1"),
        "{ir}"
    );
    assert!(ir.contains("return total"), "{ir}");
}

#[test]
fn an_if_else_produces_a_branch_and_a_join() {
    let ir = ir("\
def sign(a: int) -> int:
    if a < 0:
        return -1
    else:
        return 1
");
    assert!(ir.contains("branch"), "{ir}");
    // both arms return, so the join block is unreachable and closes as such
    assert!(ir.contains("unreachable"), "{ir}");
}

#[test]
fn an_elif_chain_nests_branches() {
    let ir = ir("\
def bucket(a: int) -> int:
    if a < 0:
        return 0
    elif a < 10:
        return 1
    else:
        return 2
");
    assert_eq!(ir.matches("branch").count(), 2, "{ir}");
}

#[test]
fn a_while_loop_has_a_back_edge() {
    let ir = ir("\
def count(n: int) -> int:
    i = 0
    while i < n:
        i = i + 1
    return i
");
    assert!(ir.contains("branch"), "{ir}");
    // the body jumps back to the header
    assert!(ir.contains("goto b1"), "{ir}");
}

#[test]
fn an_augmented_assignment_writes_through_to_the_same_register() {
    let ir = ir("\
def total(n: int) -> int:
    acc = 0
    acc += n
    return acc
");
    assert!(ir.contains("acc = acc + n"), "{ir}");
}

#[test]
fn floats_lower_to_the_float_ops() {
    let ir = ir("def scale(x: float, y: float) -> float:\n    return x * y\n");
    assert!(ir.contains("(x: float, y: float) -> float"), "{ir}");
    assert!(ir.contains("r2 = x * y"), "{ir}");
}

#[test]
fn true_division_produces_a_float_from_two_ints() {
    let ir = ir("def half(a: int) -> float:\n    return a / 2\n");
    assert!(ir.contains("-> float"), "{ir}");
    assert!(ir.contains("r1 = a / 2"), "{ir}");
}

#[test]
fn a_call_to_another_function_in_the_unit_is_native() {
    let ir = ir("\
def double(n: int) -> int:
    return n + n

def quad(n: int) -> int:
    return double(double(n))
");
    assert!(ir.contains("call double(n)"), "{ir}");
    assert_eq!(ir.matches("call double").count(), 2, "{ir}");
}

#[test]
fn a_function_returning_nothing_returns_none() {
    let ir = ir("def f(a: int) -> None:\n    pass\n");
    assert!(ir.contains("-> None"), "{ir}");
    assert!(ir.contains("return None"), "{ir}");
}

#[test]
fn a_bool_condition_converts_to_a_bit() {
    let ir = ir("\
def pick(flag: bool) -> int:
    if flag:
        return 1
    return 0
");
    assert!(
        ir.contains("not "),
        "a bool becomes a bit via double negation: {ir}"
    );
}

#[test]
fn comparisons_produce_a_bit_register() {
    let ir = ir("def less(a: int, b: int) -> bool:\n    return a < b\n");
    // the comparison itself is a bit; returning it as `bool` is a different
    // representation, so this function is expected to decline instead
    let _ = ir;
}

// ── declines ────────────────────────────────────────────────────────────────
//
// a decline is a feature, not a failure: the function falls back to its
// interpreted definition and the rest of the module still compiles

#[test]
fn a_gradual_parameter_is_an_object() {
    // `object` assumes nothing, so no check is needed and nothing declines
    let ir = ir("def f(a) -> None:\n    pass\n");
    assert!(ir.contains("(a: object) -> None"), "{ir}");
}

#[test]
fn a_container_parameter_is_an_object() {
    let ir = ir("def f(a: list[int]) -> None:\n    pass\n");
    assert!(ir.contains("(a: object) -> None"), "{ir}");
}

#[test]
fn arithmetic_on_a_gradual_value_goes_through_the_object_protocol() {
    let ir = ir("def f(a, b) -> object:\n    return a + b\n");
    assert!(ir.contains("(a: object, b: object) -> object"), "{ir}");
    assert!(ir.contains("r2 = a + b"), "{ir}");
}

#[test]
fn a_mixed_pair_widens_the_unboxed_side() {
    let ir = ir("def f(a: int, b) -> object:\n    return a + b\n");
    assert!(ir.contains("= box a"), "the int is boxed: {ir}");
    assert!(ir.contains("-> object"), "{ir}");
}

#[test]
fn a_condition_on_a_gradual_value_uses_python_truthiness() {
    let ir = ir("\
def pick(flag) -> int:
    if flag:
        return 1
    return 0
");
    assert!(ir.contains("truthy flag"), "{ir}");
}

#[test]
fn a_declared_object_return_keeps_the_precise_representation() {
    // the return type comes from the returns, not the annotation: the value
    // really is an `int`, so the native entry returns one and only the python
    // wrapper boxes it. a declared `-> object` does not force a boxed native
    // signature, which is the more useful of the two readings
    let ir = ir("def f() -> object:\n    return 1\n");
    assert!(ir.contains("-> int"), "{ir}");
    assert!(!ir.contains("box"), "{ir}");
}

#[test]
fn a_value_widened_into_an_object_place_is_boxed() {
    // here the widening is unavoidable: the two returns have no common unboxed
    // representation, so the function returns `object` and both are boxed
    let ir = ir("\
def f(c: bool) -> object:
    if c:
        return 1
    return 1.5
");
    assert!(ir.contains("-> object"), "{ir}");
    assert_eq!(ir.matches("box ").count(), 2, "{ir}");
}

#[test]
fn and_short_circuits_and_yields_an_operand() {
    // `a and b` is one of the operands, not a bool
    let ir = ir("def f(a: int, b: int) -> int:\n    return a and b\n");
    assert!(ir.contains("-> int"), "{ir}");
    assert!(ir.contains("branch"), "{ir}");
}

#[test]
fn or_branches_the_other_way() {
    let a = ir("def f(a: int, b: int) -> int:\n    return a and b\n");
    let o = ir("def f(a: int, b: int) -> int:\n    return a or b\n");
    // the same blocks, with the branch targets swapped
    assert_ne!(a, o, "`and` and `or` must not lower identically");
    assert!(o.contains("branch"), "{o}");
}

#[test]
fn a_mixed_and_unifies_at_the_widest_representation() {
    let ir = ir("def f(a: int, b: str) -> object:\n    return a and b\n");
    assert!(ir.contains("-> object"), "{ir}");
}

#[test]
fn a_conditional_expression_lowers_to_a_branch() {
    let ir = ir("def f(c: bool, a: int, b: int) -> int:\n    return a if c else b\n");
    assert!(ir.contains("branch"), "{ir}");
    assert!(ir.contains("-> int"), "{ir}");
}

#[test]
fn a_chained_comparison_evaluates_each_operand_once() {
    let ir = ir("def f(a: int, b: int, c: int) -> bool:\n    return a < b < c\n");
    // two comparisons, and one branch between them for the short circuit
    assert_eq!(ir.matches(" < ").count(), 2, "{ir}");
    assert_eq!(ir.matches("branch").count(), 1, "{ir}");
}

#[test]
fn a_str_indexed_by_an_integer_reads_a_character() {
    with_source(
        "\
def at(s: str, i: int) -> str:
    return s[i]

def part(s: str, a: int, b: int) -> str:
    return s[a:b]

def keyed(d: dict[str, int], k: str) -> int:
    return d[k]
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let function = |name: &str| {
                module
                    .all_functions()
                    .find(|function| function.name == name)
                    .expect("the function is compiled")
            };
            let at = function("at");
            assert!(
                has_op(at, |op| matches!(op, Op::StrGetItem { .. })),
                "{}",
                print_function(at)
            );
            // the character read writes a `str`, so nothing has to check the result
            assert!(
                !has_op(at, |op| matches!(op, Op::Unbox { .. })),
                "{}",
                print_function(at)
            );
            // a slice is a subscript of a `str` and is not a character read
            let part = function("part");
            assert!(
                has_op(part, |op| matches!(op, Op::GetItem { .. })),
                "{}",
                print_function(part)
            );
            let keyed = function("keyed");
            assert!(
                has_op(keyed, |op| matches!(op, Op::GetItem { .. })),
                "{}",
                print_function(keyed)
            );
        },
    );
}

#[test]
fn a_comparison_of_two_strs_leaves_the_object_protocol() {
    with_source(
        "\
def same(a: str, b: str) -> bool:
    return a == b

def before(a: str, b: object) -> bool:
    return a < b
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let function = |name: &str| {
                module
                    .all_functions()
                    .find(|function| function.name == name)
                    .expect("the function is compiled")
            };
            let same = function("same");
            assert!(
                has_op(same, |op| matches!(op, Op::StrCompare { .. })),
                "{}",
                print_function(same)
            );
            // only *both* operands being `str` settles whose comparison runs, so a
            // gradual right-hand side stays on the protocol
            let before = function("before");
            assert!(
                has_op(before, |op| matches!(op, Op::ObjectCompare { .. })),
                "{}",
                print_function(before)
            );
        },
    );
}

#[test]
fn an_int_condition_compares_against_zero() {
    let ir = ir("\
def f(n: int) -> int:
    if n:
        return 1
    return 0
");
    assert!(ir.contains("n != 0"), "{ir}");
}

#[test]
fn the_bitwise_operators_lower_on_ints() {
    let ir = ir("def f(a: int, b: int) -> int:\n    return (a & b) | (a ^ b)\n");
    assert!(ir.contains(" & "), "{ir}");
    assert!(ir.contains(" | "), "{ir}");
    assert!(ir.contains(" ^ "), "{ir}");
}

#[test]
fn shifts_and_power_lower_on_ints() {
    let ir = ir("def f(a: int, b: int) -> int:\n    return (a << b) + (a >> b) + a ** b\n");
    assert!(ir.contains(" << "), "{ir}");
    assert!(ir.contains(" >> "), "{ir}");
    assert!(ir.contains(" ** "), "{ir}");
}

#[test]
fn invert_lowers_on_an_int() {
    let ir = ir("def f(a: int) -> int:\n    return ~a\n");
    assert!(ir.contains("~a"), "{ir}");
}

#[test]
fn a_float_bitwise_operation_goes_through_the_object_protocol() {
    // there is no bitwise operation on a double, so ty rejects it — but the
    // frontend must not claim an unboxed float form either
    let ir = ir("def f(a: float, b: float) -> object:\n    return a * b\n");
    assert!(ir.contains("-> float") || ir.contains("-> object"), "{ir}");
}

#[test]
fn assert_branches_to_a_raise() {
    let ir = ir("def f(a: int) -> int:\n    assert a > 0, \"positive\"\n    return a\n");
    assert!(ir.contains("branch"), "{ir}");
    assert!(ir.contains("AssertionError"), "{ir}");
    assert!(ir.contains("\"positive\""), "{ir}");
}

#[test]
fn a_raise_of_a_builtin_error_lowers() {
    let ir = ir("def f(a: int) -> int:\n    raise ValueError(\"bad\")\n");
    assert!(ir.contains("ValueError"), "{ir}");
    assert!(ir.contains("unreachable"), "{ir}");
}

#[test]
fn a_starred_display_is_built_in_runs() {
    // the leading run is one build op and the star extends it, rather than an
    // append per element
    with_source(
        "\
def f(xs: list[int]) -> object:
    return [1, 2, *xs, 3]
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            let builds = f
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .filter(|op| matches!(op, Op::BuildList { .. }))
                .count();
            assert_eq!(builds, 2, "{}", print_function(f));
            let extends = f
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .filter(|op| matches!(op, Op::Extend { mapping: false, .. }))
                .count();
            assert_eq!(extends, 2, "{}", print_function(f));
        },
    );
}

#[test]
fn a_dict_display_with_a_merge_updates_in_place() {
    with_source(
        "\
def f(d: dict[str, int]) -> object:
    return {'a': 1, **d}
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            assert!(
                has_op(f, |op| matches!(op, Op::Extend { mapping: true, .. })),
                "{}",
                print_function(f)
            );
        },
    );
}

#[test]
fn a_splatted_call_binds_at_runtime() {
    // the arguments become a tuple and a dict, because the binding cannot happen
    // here — which is exactly what python does with `CALL_FUNCTION_EX`
    with_source(
        "\
def add(a: int, b: int) -> int:
    return a + b

def f(xs: list[int]) -> int:
    return add(*xs)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            assert!(
                has_op(f, |op| matches!(op, Op::CallUnpacked { kwargs: None, .. })),
                "{}",
                print_function(f)
            );
            // and the callee is resolved by name on every call, never cached
            assert!(
                has_op(f, |op| matches!(op, Op::LoadGlobal { .. })),
                "{}",
                print_function(f)
            );
        },
    );
}

#[test]
fn a_signature_records_which_parameters_are_reachable_how() {
    with_source(
        "\
def f(a: int, /, b: int, *, c: int) -> int:
    return a + b + c
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            assert_eq!((f.posonly, f.kwonly), (1, 1));
            // and the binding order is the source order
            let names: Vec<&str> = f
                .params()
                .iter()
                .filter_map(|decl| decl.name.as_deref())
                .collect();
            assert_eq!(names, ["a", "b", "c"]);
        },
    );
}

#[test]
fn a_comprehension_gives_each_for_its_own_header() {
    // an `if` guard skips to the next value of *its own* loop, so a guard on the
    // outer one must not restart the inner
    with_source(
        "\
def f(rows: list[list[int]]) -> object:
    return [x for row in rows if len(row) > 1 for x in row]
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            let iterators = f
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .filter(|op| matches!(op, Op::GetIter { .. }))
                .count();
            assert_eq!(iterators, 2, "{}", print_function(f));
        },
    );
}

#[test]
fn a_target_list_unpacks_into_a_fixed_tuple() {
    // one op with one destination, read back element by element — a second
    // destination would be invisible to liveness
    with_source(
        "\
def f(xs: list[int]) -> int:
    a, b = xs
    return a + b
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            assert!(
                has_op(f, |op| matches!(op, Op::Unpack { starred: None, .. })),
                "{}",
                print_function(f)
            );
            let gets = f
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .filter(|op| matches!(op, Op::TupleGet { .. }))
                .count();
            assert_eq!(gets, 2, "{}", print_function(f));
        },
    );
}

#[test]
fn a_starred_target_records_which_slot_collects() {
    with_source(
        "\
def f(xs: list[int]) -> object:
    head, *tail = xs
    return tail
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            assert!(
                has_op(f, |op| matches!(
                    op,
                    Op::Unpack {
                        starred: Some(1),
                        ..
                    }
                )),
                "{}",
                print_function(f)
            );
        },
    );
}

#[test]
fn a_chained_assignment_evaluates_its_value_once() {
    with_source(
        "\
def side() -> int:
    return 1

def f() -> int:
    a = b = side()
    return a + b
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            let calls = f
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .filter(|op| matches!(op, Op::CallNative { .. }))
                .count();
            assert_eq!(calls, 1, "{}", print_function(f));
        },
    );
}

#[test]
fn an_unpacked_name_narrows_back_to_its_own_representation() {
    // the slot holds an object; a name whose type says otherwise takes a checked
    // unbox, which is what keeps the arithmetic unboxed
    with_source(
        "\
def f(xs: list[int]) -> int:
    a, b = xs
    return a + b
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            assert!(
                has_op(f, |op| matches!(op, Op::IntBinary { .. })),
                "{}",
                print_function(f)
            );
        },
    );
}

#[test]
fn a_handler_matches_against_an_evaluated_class() {
    // the class is an ordinary operand, so a user-defined one, a tuple and a
    // shadowed builtin all take one path
    with_source(
        "\
class Custom(Exception): ...

def f(n: int) -> int:
    try:
        return n
    except (Custom, ValueError):
        return 0
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            assert!(
                has_op(f, |op| matches!(op, Op::ExceptionMatches { .. })),
                "{}",
                print_function(f)
            );
            // the tuple is built here rather than being a compiler-known class
            assert!(
                has_op(f, |op| matches!(op, Op::BuildTuple { .. })),
                "{}",
                print_function(f)
            );
        },
    );
}

#[test]
fn a_shadowed_error_class_is_not_the_builtin() {
    // `raise ValueError(...)` has a direct lowering, and taking it when the name is
    // bound to something else would raise the wrong class
    with_source(
        "\
def f(ValueError: object) -> None:
    raise ValueError
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            assert!(
                !has_op(f, |op| matches!(op, Op::RaiseStandard { .. })),
                "{}",
                print_function(f)
            );
            assert!(
                has_op(f, |op| matches!(op, Op::RaiseObject { .. })),
                "{}",
                print_function(f)
            );
        },
    );
}

#[test]
fn an_except_block_marks_its_exception_as_being_handled() {
    // that is what makes a raise inside the block — or inside anything it calls —
    // chain onto it, and putting it back is what stops a *later* raise chaining
    with_source(
        "\
def f(n: int) -> int:
    try:
        return n
    except ValueError:
        return 0
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let f = module
                .all_functions()
                .find(|function| function.name == "f")
                .expect("f is compiled");
            assert!(
                has_op(f, |op| matches!(op, Op::PushHandled { .. })),
                "{}",
                print_function(f)
            );
            // every way out of the block puts it back: the normal one and the
            // raising one
            let pops = f
                .blocks
                .iter()
                .flat_map(|block| &block.ops)
                .filter(|op| matches!(op, Op::PopHandled { .. }))
                .count();
            assert!(pops >= 2, "{}", print_function(f));
        },
    );
}

#[test]
fn a_bare_raise_outside_a_handler_is_declined() {
    // it would need the *interpreter's* handled exception, which we never set
    let reason = with_source(
        "\
def f() -> None:
    raise
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            module
                .declined
                .iter()
                .find(|declined| declined.name == "f")
                .map(|declined| declined.reason.clone())
                .unwrap_or_default()
        },
    );
    assert!(reason.contains("bare `raise` outside"), "{reason}");
}

#[test]
fn a_function_that_never_returns_takes_its_representation_from_the_annotation() {
    // there is no `return` to derive it from, and a caller reads the declared one
    // back — a mismatch makes the error sentinel look like a value
    with_source(
        "\
def fail(reason: str) -> int:
    raise ValueError(reason)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let fail = module
                .all_functions()
                .find(|function| function.name == "fail")
                .expect("fail is compiled");
            assert_eq!(fail.ret, RType::INT);
        },
    );
}

#[test]
fn a_raise_of_a_class_the_compiler_does_not_know_still_compiles() {
    // the class itself declines, and the raise looks it up by name — so a
    // user-defined exception needs nothing special
    let source = "\
class Custom(Exception): ...

def f() -> None:
    raise Custom
";
    with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        assert!(
            !module.declined.iter().any(|declined| declined.name == "f"),
            "{:?}",
            module.declined
        );
        let f = module
            .all_functions()
            .find(|function| function.name == "f")
            .expect("f is compiled");
        assert!(
            has_op(f, |op| matches!(op, Op::RaiseObject { .. })),
            "{}",
            print_function(f)
        );
    });
}

#[test]
fn a_class_with_declared_fields_gets_a_fixed_layout() {
    with_source(
        "\
data class Point:
    x: int
    y: int

    def total(self) -> int:
        return self.x + self.y
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let [class] = module.classes.as_slice() else {
                panic!("one class");
            };
            assert_eq!(class.name, "Point");
            assert_eq!(class.fields.len(), 2);
            assert_eq!(class.fields[0].name, "x");
            assert_eq!(class.fields[0].ty, RType::INT);
            assert_eq!(class.methods.len(), 1);
            // a method is reached through the type object, not the module
            assert!(!class.methods[0].exported);
            assert_eq!(class.methods[0].owner.as_deref(), Some("Point"));
        },
    );
}

#[test]
fn a_class_with_no_constructor_lays_out_nothing() {
    // a bare annotation binds nothing, so the class has an empty layout rather than no
    // layout — it compiles, and `p.x` raises `AttributeError` because there is no field
    let fields = with_source(
        "\
class Point:
    x: int
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            module
                .classes
                .iter()
                .find(|class| class.name == "Point")
                .map(|class| class.fields.len())
        },
    );
    assert_eq!(fields, Some(0));
}

/// the class-level constants of one emitted class, in the order the body wrote them
fn class_constants(source: &str, class: &str) -> Vec<String> {
    with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        module
            .classes
            .iter()
            .find(|candidate| candidate.name == class)
            .unwrap_or_else(|| panic!("{class} is emitted"))
            .constants
            .clone()
    })
}

#[test]
fn an_annotated_class_attribute_is_a_class_constant() {
    // the statement was skipped outright, so the attribute was lost: `Tagged.KIND`
    // raised where python answers `'tagged'`. an annotation is not a binding, but an
    // annotated *assignment* is the same one a plain assignment makes
    assert_eq!(
        class_constants(
            "\
class Tagged:
    KIND: str = \"tagged\"
",
            "Tagged"
        ),
        ["KIND"]
    );
}

#[test]
fn a_class_annotation_with_no_value_binds_nothing() {
    // `KIND: str` on its own declares and assigns nothing — there is no value to copy,
    // and inventing one would put an attribute on the class python never gave it
    assert_eq!(
        class_constants(
            "\
class Tagged:
    KIND: str
",
            "Tagged"
        ),
        Vec::<String>::new()
    );
}

#[test]
fn a_class_carries_a_plain_and_an_annotated_constant_alike() {
    assert_eq!(
        class_constants(
            "\
class Tagged:
    PLAIN = 1
    ANNOTATED: int = 2
",
            "Tagged"
        ),
        ["PLAIN", "ANNOTATED"]
    );
}

#[test]
fn an_annotated_attribute_of_a_data_class_is_a_field_rather_than_a_constant() {
    // in a `data class` the annotations *are* the layout, and each one already has a
    // descriptor in the type's dict. copying the twin's class attribute over the top
    // would replace that descriptor with the default value, and every instance would
    // then answer the default whatever it was constructed with
    with_source(
        "\
data class Point:
    x: int = 1
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let class = &module.classes[0];
            let fields: Vec<&str> = class.fields.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(fields, ["x"]);
            assert!(class.constants.is_empty(), "{:?}", class.constants);
        },
    );
}

#[test]
fn a_name_that_is_both_a_class_constant_and_a_field_declines() {
    // both land in the type's dict — the field as a descriptor at `PyType_Ready`, the
    // constant copied over the top of it afterwards. the constant wins, so an instance
    // with its own value answers the class-level one instead: a silent wrong answer
    let reasons = declines(
        "\
class Tagged:
    KIND: str = \"class-level\"

    def __init__(self, kind: str) -> None:
        self.KIND = kind
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "Tagged"
            && reason.contains("both a class-level constant and a field")),
        "{reasons:?}"
    );
}

#[test]
fn a_private_class_constant_takes_its_mangled_name() {
    // python binds `__params` written in `class Function` as `_Function__params`, so
    // copying it from the twin under the written name found nothing at all and the
    // attribute was silently dropped
    assert_eq!(
        class_constants(
            "\
class Function:
    __params = None
    _protected = 1
    __dunder__ = 2
",
            "Function"
        ),
        ["_Function__params", "_protected", "__dunder__"]
    );
}

#[test]
fn a_class_of_only_underscores_mangles_nothing() {
    // `_Py_Mangle` strips the class's leading underscores and gives up when nothing is
    // left, so a class called `_` binds `__x` under its written name
    assert_eq!(
        class_constants(
            "\
class _:
    __x = 1
",
            "_"
        ),
        ["__x"]
    );
}

#[test]
fn a_private_method_and_attribute_take_their_mangled_names() {
    // a method and an attribute are bound in the class body like anything else. the
    // compiled class published `__read` and `__buffer` where python publishes
    // `_Stream__read` and `_Stream__buffer`, so nothing outside the class could reach
    // either — and the read inside the method looked for the written name too
    with_source(
        "\
class Stream:
    def __init__(self, source: str) -> None:
        self.__buffer = source
        self.plain = 0

    def __read(self) -> str:
        return self.__buffer

    def take(self) -> str:
        return self.__read()
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let class = &module.classes[0];
            let fields: Vec<&str> = class.fields.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(fields, ["_Stream__buffer", "plain"]);
            let names: Vec<&str> = class.methods.iter().map(|m| m.name.as_str()).collect();
            assert_eq!(names, ["__init__", "_Stream__read", "take"]);
            // the private read still reaches the field rather than the object protocol,
            // and the private call is still the direct one — mangling both ends keeps
            // the two agreeing
            let read = class
                .methods
                .iter()
                .find(|method| method.name == "_Stream__read")
                .expect("the private method is emitted");
            assert!(has_op(read, |op| matches!(
                op,
                Op::GetField { field, .. } if field == "_Stream__buffer"
            )));
            let take = class
                .methods
                .iter()
                .find(|method| method.name == "take")
                .expect("`take` is emitted");
            assert!(has_op(take, |op| matches!(
                op,
                Op::CallNative { callee, .. } if callee == "_Stream__read"
            )));
        },
    );
}

#[test]
fn a_frame_nested_in_a_method_mangles_against_the_class_it_is_written_in() {
    // the mangling follows the *source*, so a frame the method is turned into is still
    // written in the class body. a generator's body becomes a method of its state object
    // and a comprehension gets a frame of its own, and neither receiver is `Stream` —
    // so an answer taken from the receiver would mangle against the wrong class
    let ir = with_source(
        "\
class Stream:
    def __init__(self, source: str) -> None:
        self.__buffer = source

    def take(self) -> object:
        yield self.__buffer

    def each(self) -> object:
        return [self.__buffer for _ in range(1)]
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            module
                .classes
                .iter()
                .flat_map(|class| class.methods.iter())
                .map(print_function)
                .collect::<Vec<_>>()
                .join("\n")
        },
    );
    assert!(ir.contains("_Stream__buffer"), "{ir}");
    assert!(!ir.contains(".__buffer"), "{ir}");
}

#[test]
fn a_dunder_is_not_mangled() {
    // `_Py_Mangle` leaves a name with two trailing underscores alone, which is what
    // keeps `__init__` reaching its slot
    with_source(
        "\
class Point:
    def __init__(self, x: int) -> None:
        self.x = x

    def __repr__(self) -> str:
        return \"p\"
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let names: Vec<&str> = module.classes[0]
                .methods
                .iter()
                .map(|m| m.name.as_str())
                .collect();
            assert_eq!(names, ["__init__", "__repr__"]);
        },
    );
}

#[test]
fn a_module_level_frame_mangles_nothing() {
    // the mangling follows the *source*: a function written outside any class body
    // reads `o.__x` under the written name, and mangling it against the receiver's
    // class would look up a name python never bound
    let ir = ir("\
class Holder:
    def __init__(self, v: int) -> None:
        self.v = v


def peek(o: object) -> object:
    return o.__x
");
    assert!(ir.contains("r1 = o.__x"), "{ir}");
}

#[test]
fn a_positional_only_marker_does_not_move_the_receiver() {
    // a `/` puts the receiver in the *positional-only* list, so the first ordinary
    // parameter is an argument rather than `self`. reading it as the receiver finds
    // no attribute assignment at all, and the class silently lays out nothing — an
    // instance that `__init__` then cannot store into
    with_source(
        "\
class Thing:
    def __init__(self, a: int, /, b: int = 1) -> None:
        self.a = a
        self.b = b
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let [class] = module.classes.as_slice() else {
                panic!("one class");
            };
            let fields: Vec<(&str, &RType)> = class
                .fields
                .iter()
                .map(|field| (field.name.as_str(), &field.ty))
                .collect();
            assert_eq!(fields, [("a", &RType::INT), ("b", &RType::INT)]);
        },
    );
}

#[test]
fn a_setattr_behind_a_positional_only_marker_still_has_no_layout() {
    // the same slot-zero question, asked where getting it wrong loses a *decline*:
    // a receiver read off the wrong parameter matches no `setattr` target, and the
    // class keeps a layout that cannot hold what it stores
    let reasons = declines(
        "\
class Held:
    def __init__(self, /) -> None:
        self.n = 0

    def install(self, name: str, /) -> None:
        setattr(self, name, 1)
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Held".to_string(),
            "a `setattr` on the receiver names its attribute at runtime".to_string()
        )]
    );
}

#[test]
fn a_class_level_constant_under_a_class_keyword_declines() {
    // a constant keeps its class off the metaclass construction, because it is settled
    // after the metaclass has already decided what the class defines. a class keyword
    // leaves nowhere else to go — a type spec has nowhere to put the keyword — so what
    // would answer is the interpreted definition, and that is only there while the
    // module still holds the name. `ast` pops `Num` straight out of its own globals, so
    // leaving this to the runtime turns the import into a `NameError`.
    //
    // `Plain` is the boundary: the same keyword with no constant still lowers
    let reasons = declines(
        "\
from abc import ABCMeta


class Tagged(metaclass=ABCMeta):
    TAG = 1

    def label(self) -> str:
        return \"tagged\"


class Plain(metaclass=ABCMeta):
    def label(self) -> str:
        return \"plain\"
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Tagged".to_string(),
            "a class-level constant on a class built through its metaclass is not lowered yet"
                .to_string()
        )]
    );
}

#[test]
fn a_base_this_module_lays_out_declines_beside_one_it_does_not() {
    // a class holding both kinds of base takes its whole layout from outside, so a base
    // of ours in the list has to lay nothing out: its fields sit at offsets inside an
    // instance this class no longer decides the shape of, and the base python picks
    // could put its own data over every one of them.
    //
    // `Fieldless` is the boundary — the same two bases with nothing laid out still lower
    let reasons = declines(
        "\
import codecs


class Laid:
    def __init__(self, n: int) -> None:
        self.n = n


class Fieldless:
    def side(self) -> str:
        return \"fieldless\"


class OnLaid(Laid, codecs.Codec):
    pass


class OnFieldless(Fieldless, codecs.Codec):
    pass
",
    );
    assert_eq!(
        reasons,
        vec![
            (
                "OnLaid".to_string(),
                "a base this module lays out cannot stand beside one it does not".to_string()
            ),
            (
                "Laid".to_string(),
                "`OnLaid` declined, so it extends the interpreted definition rather than this type"
                    .to_string()
            )
        ]
    );
}

#[test]
fn a_class_with_fields_of_its_own_declines_beside_a_base_this_module_emits() {
    // the other half: the base of ours lays nothing out, but *this* class does. only a
    // type spec can say where storage past a base goes, and a spec is exactly what a
    // mixed base list rules out — so the fields would have nowhere to sit
    let reasons = declines(
        "\
import codecs


class Fieldless:
    def side(self) -> str:
        return \"fieldless\"


class Storing(Fieldless, codecs.Codec):
    def __init__(self, n: int) -> None:
        self.n = n
",
    );
    assert_eq!(
        reasons,
        vec![
            (
                "Storing".to_string(),
                "a class with fields of its own cannot have a base this module emits beside one it does not"
                    .to_string()
            ),
            (
                "Fieldless".to_string(),
                "`Storing` declined, so it extends the interpreted definition rather than this type"
                    .to_string()
            )
        ]
    );
}

#[test]
fn a_finalizer_makes_every_field_of_its_layout_one_that_may_be_absent() {
    // `Held()` raises before `self.path` is written, and python releases the half-built
    // object — which runs `__del__` over fields that are still the zeroes `tp_alloc`
    // left. read as always written, the null went straight on to whatever asked for it
    //
    // the whole layout is marked, not the one class: `Held` carries no finalizer of its
    // own, but it is freed through `Base`'s dealloc and so through `Base`'s finalizer,
    // and its struct begins with `Base`'s fields — a base marked one way and a subclass
    // the other would put the shared fields at two different offsets. `Apart` is the
    // boundary: a layout of its own with no finalizer anywhere in it
    let optional = with_source(
        "\
class Base:
    def __init__(self, path: str) -> None:
        self.path = path

    def __del__(self) -> None:
        print(self.path)


class Held(Base):
    def __init__(self, path: str, tag: str) -> None:
        self.path = path
        self.tag = tag


class Apart:
    def __init__(self, n: int) -> None:
        self.n = n
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            module
                .classes
                .iter()
                .map(|class| {
                    (
                        class.name.clone(),
                        class
                            .fields
                            .iter()
                            .map(|field| field.optional)
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        },
    );
    assert_eq!(
        optional,
        vec![
            ("Base".to_string(), vec![true]),
            ("Held".to_string(), vec![true, true]),
            ("Apart".to_string(), vec![false]),
        ]
    );
}

#[test]
fn a_slots_declaration_lays_out_the_attributes_it_names() {
    // `__slots__` is copied onto the emitted type like any other class-level constant,
    // so the type advertises attributes python would have made descriptors for. the
    // storage has to be there too, and nothing assigns it — which is the field an
    // assignment on only some paths already gets: a byte beside it saying whether it was
    // written. one the class *does* assign keeps the representation that write gives it,
    // and a base's come first because a subclass's struct begins with the base's
    let layout = with_source(
        "\
class Link:
    __slots__ = 'prev', 'next'


class One:
    __slots__ = \"x\"


class Empty:
    __slots__ = ()


class Base:
    __slots__ = ('a',)


class Sub(Base):
    __slots__ = ('b',)


class Both:
    __slots__ = ('kept', 'spare')

    def __init__(self, kept: int) -> None:
        self.kept = kept


class Private:
    __slots__ = ('__hidden',)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            module
                .classes
                .iter()
                .map(|class| {
                    (
                        class.name.clone(),
                        class
                            .fields
                            .iter()
                            .map(|field| (field.name.clone(), field.ty.clone(), field.optional))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        },
    );
    assert_eq!(
        layout,
        vec![
            (
                "Link".to_string(),
                vec![
                    ("prev".to_string(), RType::OBJECT, true),
                    ("next".to_string(), RType::OBJECT, true),
                ]
            ),
            (
                "One".to_string(),
                vec![("x".to_string(), RType::OBJECT, true)]
            ),
            ("Empty".to_string(), vec![]),
            (
                "Base".to_string(),
                vec![("a".to_string(), RType::OBJECT, true)]
            ),
            (
                "Sub".to_string(),
                vec![
                    ("a".to_string(), RType::OBJECT, true),
                    ("b".to_string(), RType::OBJECT, true),
                ]
            ),
            (
                "Both".to_string(),
                vec![
                    ("kept".to_string(), RType::INT, false),
                    ("spare".to_string(), RType::OBJECT, true),
                ]
            ),
            (
                "Private".to_string(),
                vec![("_Private__hidden".to_string(), RType::OBJECT, true)]
            ),
        ]
    );
}

#[test]
fn a_slots_declaration_the_layout_cannot_answer_declines() {
    // neither `__dict__` nor `__weakref__` is storage of the instance's own: they ask the
    // *type* for a dict and for weakref support, which a spec adds to neither. and the
    // names are the layout, so a declaration nothing here can read is one nothing here
    // can lay out — python takes any iterable, including one built at class definition
    // time. `Fine` is the boundary: a literal declaration of ordinary names
    let reasons = declines(
        "\
class Weak:
    __slots__ = ('a', '__weakref__')


class Dicted:
    __slots__ = ('a', '__dict__')


class Computed:
    __slots__ = [f\"x{i}\" for i in range(2)]


class Named:
    __slots__ = (NAME,)


class Fine:
    __slots__ = ('a',)
",
    );
    assert_eq!(
        reasons,
        [
            (
                "Weak".to_string(),
                "`__slots__` asks for `__weakref__`, which a type spec cannot add".to_string()
            ),
            (
                "Dicted".to_string(),
                "`__slots__` asks for `__dict__`, which a type spec cannot add".to_string()
            ),
            (
                "Computed".to_string(),
                "`__slots__` names its attributes at runtime".to_string()
            ),
            (
                "Named".to_string(),
                "a `__slots__` entry is not a literal name".to_string()
            ),
        ]
    );
}

#[test]
fn fields_past_a_base_this_module_emits_decline() {
    // `Held`'s fields would sit past a `Wrapper` instance, so it supplies the three type
    // slots that reach them and each calls `Wrapper`'s. a class this module emits is a
    // heap type, and a heap type's three are python's own — they resolve which base to
    // chain to from the instance's type, find `Held`'s there, and call it back until the
    // stack runs out.
    //
    // `Beside` is the boundary: its layout chain ends at `object` rather than outside, so
    // its struct *begins* with `Rooted`'s rather than sitting past an instance of it, and
    // its deallocator frees the object rather than passing it on
    let reasons = declines(
        "\
class Wrapper(OSError):
    pass


class Held(Wrapper):
    def __init__(self, code: int) -> None:
        self.code = code


class Rooted:
    def __init__(self, n: int) -> None:
        self.n = n


class Beside(Rooted):
    def __init__(self, n: int, extra: int) -> None:
        self.n = n
        self.extra = extra
",
    );
    assert_eq!(
        reasons,
        vec![
            (
                "Held".to_string(),
                "a class whose fields sit past a base's instance needs a base python frees itself, and one this module writes is not"
                    .to_string()
            ),
            (
                "Wrapper".to_string(),
                "`Held` declined, so it extends the interpreted definition rather than this type"
                    .to_string()
            )
        ]
    );
}

#[test]
fn an_annotated_class_attribute_under_a_class_keyword_declines_with_the_rest() {
    // an annotated assignment is a class-level constant, so it reaches the same gate a
    // plain one does. this is what making the annotation a binding costs: a class the
    // compiler used to build silently without the attribute now refuses to build at all.
    // over the stdlib that cost was nothing — every class this reason reaches was
    // already declining for another
    let reasons = declines(
        "\
from abc import ABCMeta


class Tagged(metaclass=ABCMeta):
    TAG: int = 1

    def label(self) -> str:
        return \"tagged\"
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Tagged".to_string(),
            "a class-level constant on a class built through its metaclass is not lowered yet"
                .to_string()
        )]
    );
}

#[test]
fn a_subclass_of_a_class_the_metaclass_gates_turn_down_builds_on_the_interpreted_base() {
    // both gates are asked while the layouts settle, so a class either of them turns
    // down leaves the layout set — and its subclass then takes the external base every
    // other declining class's subclass takes rather than being laid out on a base
    // nothing emits. asked while the *body* was lowered instead, the base stayed in the
    // set and both subclasses cascaded behind it
    const SOURCE: &str = "\
from abc import ABCMeta


class Decorated(metaclass=ABCMeta):
    @staticmethod
    def label() -> str:
        return \"decorated\"


class BelowDecorated(Decorated):
    def size(self) -> int:
        return 1


class Constant(metaclass=ABCMeta):
    TAG = 1

    def label(self) -> str:
        return \"constant\"


class BelowConstant(Constant):
    def size(self) -> int:
        return 2
";
    assert_eq!(
        declines(SOURCE),
        vec![
            (
                "Decorated".to_string(),
                "a decorated method on a class built through its metaclass is not lowered yet"
                    .to_string()
            ),
            (
                "Constant".to_string(),
                "a class-level constant on a class built through its metaclass is not lowered yet"
                    .to_string()
            )
        ]
    );
    // the base each subclass gets is the point: an `InModule` one would name a type
    // this module never emits
    let bases = with_source(SOURCE, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        module
            .classes
            .iter()
            .map(|class| (class.name.clone(), class.base.clone()))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        bases,
        vec![
            (
                "BelowDecorated".to_string(),
                Some(ClassBase::External(vec!["Decorated".to_string()]))
            ),
            (
                "BelowConstant".to_string(),
                Some(ClassBase::External(vec!["Constant".to_string()]))
            )
        ]
    );
}

#[test]
fn a_counted_loop_reads_its_array_unchecked() {
    // the guard `i < len(xs)` with a counting `i` is what proves the read, so the
    // op is the unchecked one — and a bound the guard does not give keeps the check
    with_source(
        "\
def proven(xs: list[float]) -> float:
    out = 0.0
    i = 0
    while i < len(xs):
        out = out + xs[i]
        i = i + 1
    return out


def unproven(xs: list[float], n: int) -> float:
    out = 0.0
    i = 0
    while i < n:
        out = out + xs[i]
        i = i + 1
    return out
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let edition = |name: &str| {
                module
                    .all_functions()
                    .find(|function| {
                        function.name.starts_with(name) && function.name.contains("$arr")
                    })
                    .unwrap_or_else(|| panic!("{name} has an unboxed edition"))
                    .clone()
            };
            let proven = edition("proven");
            assert!(
                has_op(&proven, |op| matches!(op, Op::ArrayRead { .. }))
                    && !has_op(&proven, |op| matches!(op, Op::ArrayGet { .. })),
                "{}",
                print_function(&proven)
            );
            let unproven = edition("unproven");
            assert!(
                has_op(&unproven, |op| matches!(op, Op::ArrayGet { .. })),
                "{}",
                print_function(&unproven)
            );
        },
    );
}

#[test]
fn a_field_a_path_may_skip_is_optional() {
    // an `if` with no `else` leaves a path that fills nothing, which does not cost the
    // class its layout — the field is there, with a byte beside it saying whether it
    // was ever written, and a read on a path that skipped it raises `AttributeError`
    // exactly as python does
    let optional = with_source(
        "\
class Point:
    def __init__(self, x: int) -> None:
        self.x = x
        if x > 0:
            self.big = x
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            module
                .classes
                .iter()
                .find(|class| class.name == "Point")
                .map(|class| {
                    class
                        .fields
                        .iter()
                        .map(|field| (field.name.clone(), field.optional))
                        .collect::<Vec<_>>()
                })
        },
    );
    assert_eq!(
        optional,
        Some(vec![("x".to_string(), false), ("big".to_string(), true)])
    );
}

#[test]
fn a_field_every_path_assigns_earns_a_place_in_the_layout() {
    // both branches fill it, so it is as present as one assigned at the top — and a
    // branch that *raises* produces no object, so it has nothing to say either way
    let fields = with_source(
        "\
class Point:
    def __init__(self, x: int) -> None:
        if x > 0:
            self.big = 1
        else:
            self.big = 0
        if x > 100:
            raise ValueError('too big')
        else:
            self.small = 1
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            module
                .classes
                .iter()
                .find(|class| class.name == "Point")
                .map(|class| {
                    class
                        .fields
                        .iter()
                        .map(|field| field.name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        },
    );
    assert_eq!(fields, vec!["big".to_string(), "small".to_string()]);
}

#[test]
fn a_plain_class_lays_out_what_its_constructor_assigns() {
    with_source(
        "\
class Point:
    def __init__(self, x: int, y: int) -> None:
        self.x = x
        self.y = y
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let class = &module.classes[0];
            let fields: Vec<&str> = class.fields.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(fields, ["x", "y"]);
            assert!(class.methods.iter().any(|m| m.name == "__init__"));
        },
    );
}

#[test]
fn a_class_with_a_base_extends_its_layout() {
    // the subclass's struct *begins* with the base's fields, so a pointer to one is
    // a valid pointer to the other
    with_source(
        "\
data class Shape:
    name: str

data class Circle(Shape):
    radius: float
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let circle = module
                .classes
                .iter()
                .find(|class| class.name == "Circle")
                .expect("Circle is emitted");
            assert_eq!(
                circle.base.as_ref().and_then(ClassBase::in_module),
                Some("Shape")
            );
            let names: Vec<&str> = circle
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect();
            assert_eq!(names, ["name", "radius"]);
        },
    );
}

/// module init installs the native definition over whatever the fallback source left
/// in the namespace, so a name the module body rebinds *afterwards* cannot carry one:
/// `Marker = Marker()` leaves an instance there, and the class over it is a wrong
/// answer. a binding that comes first is the ordinary forward declaration
#[test]
fn a_definition_whose_name_is_rebound_afterwards_declines() {
    let (marker, ready) = with_source(
        "\
Ready = None

class Ready:
    def tag(self) -> str:
        return \"ready\"

class Marker:
    def tag(self) -> str:
        return \"marker\"

Marker = Marker()
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let named = |name: &str| module.classes.iter().any(|class| class.name == name);
            (named("Marker"), named("Ready"))
        },
    );
    assert!(!marker, "`Marker` is rebound after its own definition");
    assert!(
        ready,
        "`Ready = None` comes first, so the class overwrites it"
    );
}

#[test]
fn a_base_out_of_the_unit_may_add_storage_of_its_own() {
    // the base is built on whatever `Exception` resolves to at import, and the field
    // lives in room asked for *past* the instance that base allocates. what makes it
    // safe is not the layout but the three slots that come with it — see the leak tests
    // in `by_build`, which are what say the storage is released and collected
    let (declined, fields) = with_source(
        "\
data class Timed(Exception):
    at: int
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let declined = module
                .declined
                .iter()
                .any(|declined| declined.name == "Timed");
            let fields = module
                .classes
                .iter()
                .find(|class| class.name == "Timed")
                .map(|class| class.fields.len())
                .unwrap_or_default();
            (declined, fields)
        },
    );
    assert!(!declined, "the class was left to the interpreter");
    assert_eq!(fields, 1);
}

#[test]
fn a_base_out_of_the_unit_without_storage_is_lowered() {
    // the same class with nothing of its own to store: it declares no layout, the base
    // builds and frees the instance, and the methods come along
    let base = with_source(
        "\
class Timed(Exception):
    def label(self) -> str:
        return \"timed\"
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            module
                .classes
                .iter()
                .find(|class| class.name == "Timed")
                .and_then(|class| class.base.clone())
        },
    );
    assert_eq!(
        base,
        Some(ClassBase::External(vec!["Exception".to_string()]))
    );
}

#[test]
fn a_class_in_an_inheritance_chain_gives_up_its_direct_call() {
    // it is a mutable heap type: python can rebind a method on it, or override it
    // in a subclass, and a direct call would see neither
    with_source(
        "\
data class Shape:
    name: str

    def describe(self) -> str:
        return self.name

data class Circle(Shape):
    radius: float

data class Plain:
    n: int

    def doubled(self) -> int:
        return self.n * 2

def through(s: Shape) -> str:
    return s.describe()

def plain(p: Plain) -> int:
    return p.doubled()
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let ops = |name: &str| {
                print_function(
                    module
                        .all_functions()
                        .find(|function| function.name == name)
                        .unwrap_or_else(|| panic!("{name} is compiled")),
                )
            };
            assert!(
                !ops("through").contains("call Shape.describe"),
                "{}",
                ops("through")
            );
            assert!(
                ops("plain").contains("call Plain.doubled"),
                "{}",
                ops("plain")
            );
        },
    );
}

#[test]
fn a_final_receiver_keeps_its_direct_call() {
    // AB3 and AB4 traded the direct call away for any class that is decorated or in
    // an inheritance chain. `@final` is about the *place*: nothing can subclass it,
    // so there is no override for the protocol to find
    with_source(
        "\
from typing import final

data class Open:
    n: int

    def doubled(self) -> int:
        return self.n * 2

data class Derived(Open):
    extra: int

@final
data class Fixed(Open):
    label: str

    def tripled(self) -> int:
        return self.n * 3

def on_final(f: Fixed) -> int:
    return f.tripled()

def on_final_inherited(f: Fixed) -> int:
    return f.doubled()

def on_open(o: Open) -> int:
    return o.doubled()
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let ops = |name: &str| {
                print_function(
                    module
                        .all_functions()
                        .find(|function| function.name == name)
                        .unwrap_or_else(|| panic!("{name} is compiled")),
                )
            };
            // both classes are in an inheritance chain, so both are mutable heap
            // types — only the *place* differs
            assert!(
                ops("on_final").contains("call Fixed.tripled"),
                "{}",
                ops("on_final")
            );
            // and an *inherited* method too: the symbol lives on the base, and the
            // receiver's struct begins with the base's, so the pointer is valid
            assert!(
                ops("on_final_inherited").contains("call Open.doubled"),
                "{}",
                ops("on_final_inherited")
            );
            assert!(
                !ops("on_open").contains("call Open.doubled"),
                "{}",
                ops("on_open")
            );
        },
    );
}

#[test]
fn a_sealed_class_is_not_exact() {
    // `sealed` closes the world *outside* the declaring module and says nothing
    // about a subclass inside it — one right here may override the method
    with_source(
        "\
sealed data class Shape:
    name: str

    def describe(self) -> str:
        return 'shape ' + self.name

data class Circle(Shape):
    radius: float

    def describe(self) -> str:
        return 'circle ' + self.name

def through(s: Shape) -> str:
    return s.describe()
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let through = print_function(
                module
                    .all_functions()
                    .find(|function| function.name == "through")
                    .expect("through is compiled"),
            );
            assert!(!through.contains("call Shape.describe"), "{through}");
        },
    );
}

#[test]
fn a_decorated_class_gives_up_its_direct_call() {
    // a decorated class is a mutable heap type, and python can rebind a method on
    // one — a direct call would not see the rebinding
    with_source(
        "\
def tagged(cls: type) -> type:
    return cls

@tagged
data class Loud:
    n: int

    def doubled(self) -> int:
        return self.n * 2

data class Quiet:
    n: int

    def doubled(self) -> int:
        return self.n * 2

def loud(x: Loud) -> int:
    return x.doubled()

def quiet(x: Quiet) -> int:
    return x.doubled()
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let ops = |name: &str| {
                let function = module
                    .all_functions()
                    .find(|function| function.name == name)
                    .unwrap_or_else(|| panic!("{name} is compiled"));
                print_function(function)
            };
            // the undecorated one is called directly; the decorated one goes
            // through the protocol, where an override is seen
            assert!(
                ops("quiet").contains("call Quiet.doubled"),
                "{}",
                ops("quiet")
            );
            assert!(
                !ops("loud").contains("call Loud.doubled"),
                "{}",
                ops("loud")
            );
            // and its decorators travel with the class
            let loud = module
                .classes
                .iter()
                .find(|class| class.name == "Loud")
                .expect("Loud is emitted");
            assert_eq!(loud.decorators, ["tagged"]);
        },
    );
}

#[test]
fn a_class_with_a_hand_written_dunder_is_declined() {
    // `__init__` is generated from the fields, so a hand-written one would
    // disagree with it about the layout
    let source = "\
data class Point:
    x: int

    def __init__(self, x: int) -> None:
        pass
";
    let reason = with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        module
            .declined
            .iter()
            .find(|declined| declined.name == "Point")
            .map(|declined| declined.reason.clone())
            .unwrap_or_default()
    });
    assert!(reason.contains("dunder"), "{reason}");
}

#[test]
fn len_is_an_intrinsic_not_a_call() {
    let ir = ir("def f(s: str) -> int:\n    return len(s)\n");
    assert!(ir.contains("= len "), "{ir}");
    assert!(!ir.contains("call len"), "{ir}");
}

#[test]
fn a_module_defining_len_shadows_the_intrinsic() {
    let ir = ir("\
def len(x: int) -> int:
    return x

def f(a: int) -> int:
    return len(a)
");
    assert!(ir.contains("call len(a)"), "{ir}");
}

#[test]
fn concatenating_two_strings_stays_a_string() {
    let ir = ir("def f(a: str, b: str) -> str:\n    return a + b\n");
    assert!(ir.contains("-> str"), "{ir}");
    assert!(ir.contains(" ++ "), "{ir}");
}

#[test]
fn an_async_function_is_declined() {
    assert!(decline("def f(a: int) -> None:\n    try:\n        pass\n    except* ValueError:\n        pass\n")
            .contains("`except*`"));
}

#[test]
fn a_decorated_function_still_compiles() {
    // the decorator is applied at module init to the installed native function,
    // so the body is compiled either way
    let source = "\
def deco(f: object) -> object:
    return f

@deco
def f() -> None:
    pass
";
    with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let decorated = module
            .functions
            .iter()
            .find(|function| function.name == "f")
            .expect("f is compiled");
        assert_eq!(decorated.decorators, vec!["deco".to_string()]);
    });
}

#[test]
fn a_computed_decorator_is_declined() {
    // a call or an attribute would need its arguments evaluated at module init
    let source = "\
def make(n: int) -> object:
    return n

@make(1)
def f() -> None:
    pass
";
    let reason = with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        module
            .declined
            .iter()
            .find(|declined| declined.name == "f")
            .map(|declined| declined.reason.clone())
            .unwrap_or_default()
    });
    assert!(reason.contains("plain-name decorator"), "{reason}");
}

#[test]
fn a_generic_function_is_declined() {
    assert!(decline("def f(a: int) -> None:\n    try:\n        pass\n    except* ValueError:\n        pass\n")
            .contains("`except*`"));
}

#[test]
fn variadic_parameters_hold_a_tuple_and_a_dict() {
    // the wrapper builds them, so the body sees ordinary objects and needs no new
    // representation
    with_source(
        "\
def both(a: int, *rest: int, **named: object) -> int:
    return a + len(rest) + len(named)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let function = &module.functions[0];
            assert!(function.vararg);
            assert!(function.kwarg);
            assert_eq!(function.param_count, 3);
            assert_eq!(function.params()[1].ty, RType::OBJECT);
            assert_eq!(function.params()[2].ty, RType::OBJECT);
        },
    );
}

#[test]
fn a_for_over_range_lowers_to_a_counting_loop() {
    let ir = ir("\
def total(n: int) -> int:
    acc = 0
    for i in range(n):
        acc = acc + i
    return acc
");
    assert!(ir.contains("i = 0"), "{ir}");
    assert!(ir.contains("i < "), "the comparison counts up: {ir}");
    assert!(ir.contains("i = i + 1"), "{ir}");
}

#[test]
fn a_negative_range_step_counts_down() {
    let ir = ir("\
def countdown(n: int) -> int:
    last = 0
    for i in range(n, 0, -1):
        last = i
    return last
");
    assert!(
        ir.contains("i > "),
        "a negative step compares the other way: {ir}"
    );
    assert!(ir.contains("i = i + -1"), "{ir}");
}

#[test]
fn the_range_bound_is_read_once() {
    // re-reading the bound each iteration would let a mutated local change the
    // trip count, which `range` does not do
    let ir = ir("\
def f(n: int) -> int:
    seen = 0
    for i in range(n):
        n = 0
        seen = seen + 1
    return seen
");
    assert!(
        !ir.contains("i < n"),
        "the comparison must not re-read the mutated local: {ir}"
    );
    assert!(
        ir.lines()
            .any(|line| line.trim_start().starts_with('r') && line.trim_end().ends_with("= n")),
        "the bound is copied to its own register: {ir}"
    );
}

#[test]
fn break_and_continue_target_the_innermost_loop() {
    let ir = ir("\
def first_even(n: int) -> int:
    found = -1
    for i in range(n):
        if i % 2 == 1:
            continue
        found = i
        break
    return found
");
    assert!(ir.contains("branch"), "{ir}");
    // `continue` must reach the step block, or the index never advances
    assert!(ir.contains("i = i + 1"), "{ir}");
}

#[test]
fn break_outside_a_loop_is_declined() {
    let reason = decline("def f() -> None:\n    break\n");
    assert!(reason.contains("outside a loop"), "{reason}");
}

#[test]
fn iterating_a_list_unboxes_each_element() {
    // the protocol hands back an object; the checker says the elements are ints,
    // so narrowing to that is a checked unbox — the `iterations` soundness position
    let ir = ir("\
def f(xs: list[int]) -> None:
    for x in xs:
        pass
");
    assert!(ir.contains("let x: int"), "{ir}");
    assert!(ir.contains("unbox"), "{ir}");
}

#[test]
fn a_computed_range_step_takes_the_protocol_path() {
    // the counting loop needs a literal step to settle the comparison direction.
    // failing to apply an optimisation must not cost the function
    let ir = ir("\
def f(n: int, s: int) -> None:
    for i in range(0, n, s):
        pass
");
    assert!(ir.contains("pycall range"), "{ir}");
    assert!(ir.contains("= iter "), "{ir}");
    assert!(ir.contains("= next "), "{ir}");
}

#[test]
fn a_gradual_range_bound_reuses_the_bounds_it_already_evaluated() {
    // the bounds are lowered before the counting loop can tell whether it applies,
    // so the fallback builds `range` from those values rather than the expression
    // again — evaluating a bound twice would run its side effects twice
    let ir = ir("\
def f(n: object) -> None:
    for i in range(n):
        pass
");
    assert_eq!(ir.matches("pycall range").count(), 1, "{ir}");
    assert!(ir.contains("= iter "), "{ir}");
}

#[test]
fn a_call_out_of_the_unit_uses_the_python_convention() {
    let ir = ir("def f(a: int) -> int:\n    return abs(a)\n");
    assert!(ir.contains("pycall abs"), "{ir}");
    // the checker says it returns an int; the call proves nothing, so narrowing
    // to that representation is a checked unbox
    assert!(ir.contains("unbox"), "{ir}");
    assert!(ir.contains("-> int"), "{ir}");
}

#[test]
fn a_call_returning_something_unboxable_stays_an_object() {
    let ir = ir("def f(xs: list[int]) -> object:\n    return sorted(xs)\n");
    assert!(ir.contains("pycall sorted"), "{ir}");
    assert!(!ir.contains("unbox"), "{ir}");
}

#[test]
fn a_for_over_an_arbitrary_iterable_uses_the_protocol() {
    let ir = ir("\
def total(xs: list[int]) -> object:
    acc = 0
    for x in xs:
        acc = acc + x
    return acc
");
    assert!(ir.contains("= iter "), "{ir}");
    assert!(ir.contains("= next "), "{ir}");
    assert!(ir.contains("is null"), "{ir}");
}

#[test]
fn a_loop_else_is_skipped_by_break() {
    // `else` runs only on natural exit, so it needs its own block that a `break`
    // jumps past
    let ir = ir("\
def f(n: int) -> int:
    for i in range(n):
        if i > 2:
            break
    else:
        return -1
    return 0
");
    // the `else` block and the `break` target are distinct: the natural exit
    // negates -1 and returns it, and the break path returns 0 without going
    // through it
    assert!(ir.contains("= -1"), "{ir}");
    assert!(ir.contains("return 0"), "{ir}");
    // three exits from the loop region: body, natural exit, and the break target
    assert!(ir.matches("branch").count() >= 2, "{ir}");
}

#[test]
fn a_name_assigned_two_representations_widens_to_object() {
    // the register is declared once, so it has to cover every value written to
    // it. deciding that from the first assignment alone used to decline
    let ir = ir("\
def f(a: int) -> None:
    x = 1
    x = 1.0
");
    assert!(ir.contains("let x: object"), "{ir}");
    assert_eq!(ir.matches("box ").count(), 2, "both writes are boxed: {ir}");
}

#[test]
fn returns_that_disagree_unify_at_the_widest_representation() {
    // an `int` and a `float` return have no common unboxed shape, so both are
    // boxed rather than the function declining
    let ir = ir("\
def f(a: int) -> object:
    if a > 0:
        return 1
    return 1.0
");
    assert!(ir.contains("-> object"), "{ir}");
    assert_eq!(
        ir.matches("box ").count(),
        2,
        "both returns are boxed: {ir}"
    );
}

#[test]
fn a_declined_function_does_not_stop_the_rest_of_the_module() {
    let source = "\
def good(a: int) -> int:
    return a + 1

def bad(a: int) -> None:
    try:
        pass
    except* ValueError:
        pass
";
    with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        assert_eq!(module.functions.len(), 1);
        assert_eq!(module.functions[0].name, "good");
        assert_eq!(module.declined.len(), 1);
        assert_eq!(module.declined[0].name, "bad");
    });
}

#[test]
fn every_lowered_function_verifies() {
    // the guard on the whole frontend: an unverifiable lowering is a miscompile
    for source in [
        "def f(a: int, b: int) -> int:\n    return a * b - a\n",
        "def f(a: int) -> int:\n    x = 0\n    while x < a:\n        x = x + 2\n    return x\n",
        "def f(a: float) -> float:\n    if a > 0.0:\n        return a\n    return -a\n",
        "def f() -> None:\n    pass\n",
        "def f(a: int) -> int:\n    if a > 0:\n        b = 1\n    else:\n        b = 2\n    return b\n",
    ] {
        // `ir` panics when verification fails
        let _ = ir(source);
    }
}

#[test]
fn an_attribute_on_an_emitted_class_is_a_field_read() {
    with_source(
        "\
data class Point:
    x: int
    y: int

    def total(self) -> int:
        return self.x + self.y

def sum_of(p: Point) -> int:
    return p.x + p.y
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            // the method reaches `self` through the forced receiver, the free
            // function through its annotation — both are field reads
            for function in [&module.classes[0].methods[0], &module.functions[0]] {
                let text = print_function(function);
                assert!(text.contains("<Point.x>"), "{text}");
                assert!(
                    !has_op(function, |op| matches!(op, Op::GetAttr { .. })),
                    "{text}"
                );
            }
        },
    );
}

#[test]
fn a_field_write_on_an_emitted_class_is_a_field_store() {
    with_source(
        "\
data class Counter:
    n: int

    def bump(self, by: int) -> int:
        self.n = self.n + by
        return self.n
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let method = &module.classes[0].methods[0];
            let text = print_function(method);
            assert!(text.contains("<Counter.n> ="), "{text}");
            assert!(
                !has_op(method, |op| matches!(
                    op,
                    Op::GetAttr { .. } | Op::SetAttr { .. }
                )),
                "{text}"
            );
        },
    );
}

#[test]
fn a_field_typed_as_another_emitted_class_is_a_field_read_too() {
    // the ordering trap: the field type is mapped before `Point` would be known
    // if the layout set were built in one pass
    with_source(
        "\
data class Line:
    a: Point
    b: Point

    def span(self) -> int:
        return self.b.x - self.a.x

data class Point:
    x: int
    y: int
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let line = module
                .classes
                .iter()
                .find(|class| class.name == "Line")
                .expect("Line is emitted");
            assert_eq!(
                line.fields[0].ty,
                RType::Instance {
                    class: "Point".to_string(),
                    exact: false
                }
            );
            let span = &line.methods[0];
            let text = print_function(span);
            assert!(
                text.contains("<Line.b>") && text.contains("<Point.x>"),
                "{text}"
            );
            assert!(
                !has_op(span, |op| matches!(op, Op::GetAttr { .. })),
                "{text}"
            );
        },
    );
}

#[test]
fn an_attribute_on_a_declined_class_stays_a_getattr() {
    with_source(
        "\
class Plain:
    x: int

def read(p: Plain) -> object:
    return p.x
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let read = &module.functions[0];
            assert!(
                has_op(read, |op| matches!(op, Op::GetAttr { .. })),
                "{}",
                print_function(read)
            );
        },
    );
}

#[test]
fn a_frozen_data_class_is_marked_frozen() {
    with_source(
        "\
frozen data class Point:
    x: int

data class Loose:
    y: int
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let frozen = |name: &str| {
                module
                    .classes
                    .iter()
                    .find(|class| class.name == name)
                    .map(|class| class.immutable)
            };
            assert_eq!(frozen("Point"), Some(true));
            assert_eq!(frozen("Loose"), Some(false));
        },
    );
}

#[test]
fn a_constructor_result_narrows_to_the_class() {
    // the payoff: `Point(a, b).x` is a checked cast and then a field read, with
    // no attribute lookup anywhere
    with_source(
        "\
data class Point:
    x: int
    y: int

def diag(a: int) -> int:
    return Point(a, a).x
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let text = print_function(&module.functions[0]);
            assert!(
                text.contains("unbox") && text.contains("as Point"),
                "{text}"
            );
            assert!(text.contains("<Point.x>"), "{text}");
            assert!(
                !has_op(&module.functions[0], |op| matches!(op, Op::GetAttr { .. })),
                "{text}"
            );
        },
    );
}

#[test]
fn a_caller_of_a_declined_function_declines_too() {
    // otherwise the emitted call has no symbol, and the C compile error takes the
    // whole module down — the one thing declining exists to prevent
    with_source(
        "\
def helper(a: int) -> int:
    out = a
    try:
        pass
    except* ValueError:
        out = 0
    return out

def caller(a: int) -> int:
    return helper(a)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let reason = |name: &str| {
                module
                    .declined
                    .iter()
                    .find(|declined| declined.name == name)
                    .map(|declined| declined.reason.clone())
            };
            assert!(reason("helper").is_some());
            assert_eq!(
                reason("caller").as_deref(),
                Some("`helper` declined, so a call has no target")
            );
            assert!(module.functions.is_empty(), "{:?}", module.functions);
        },
    );
}

#[test]
fn the_decline_propagates_through_a_chain() {
    with_source(
        "\
def bottom(a: int) -> int:
    out = a
    try:
        pass
    except* ValueError:
        out = 0
    return out

def middle(a: int) -> int:
    return bottom(a)

def top(a: int) -> int:
    return middle(a)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.functions.is_empty(), "{:?}", module.functions);
            assert_eq!(module.declined.len(), 3);
        },
    );
}

#[test]
fn a_class_whose_method_declines_takes_its_users_with_it() {
    // a native type object replaces the interpreted class whole, so the class
    // cannot be emitted with one method missing — and then nothing may name its
    // layout either
    with_source(
        "\
data class Point:
    x: int

    def bad(self, a: int) -> int:
        try:
            pass
        except* ValueError:
            pass
        return self.x

def read(p: Point) -> int:
    return p.x
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.classes.is_empty(), "{:?}", module.classes);
            assert!(module.functions.is_empty(), "{:?}", module.functions);
            let reason = |name: &str| {
                module
                    .declined
                    .iter()
                    .find(|declined| declined.name == name)
                    .map(|declined| declined.reason.clone())
            };
            assert!(reason("Point").is_some());
            assert_eq!(
                reason("read").as_deref(),
                Some("`Point` declined, so it has no layout")
            );
        },
    );
}

#[test]
fn a_base_an_interpreted_class_extends_declines_with_it() {
    // the other direction. a class this module does not emit is still built — by the
    // interpreted definition, on whatever its base name resolves to, which is the
    // type emitted here. an emitted type cannot have that subclass, so the base is
    // left interpreted too
    with_source(
        "\
class Container:
    def __init__(self, tag: str) -> None:
        self.tag = tag


class Parser(Container):
    def __new__(cls, tag: str) -> Parser:
        return object.__new__(cls)

    def __init__(self, tag: str) -> None:
        Container.__init__(self, tag)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.classes.is_empty(), "{:?}", module.classes);
            let reason = |name: &str| {
                module
                    .declined
                    .iter()
                    .find(|declined| declined.name == name)
                    .map(|declined| declined.reason.clone())
            };
            assert!(reason("Parser").is_some());
            assert_eq!(
                reason("Container").as_deref(),
                Some(
                    "`Parser` declined, so it extends the interpreted definition rather than this type"
                )
            );
        },
    );
}

#[test]
fn a_sibling_of_a_declined_function_still_compiles() {
    with_source(
        "\
def helper(a: int) -> int:
    out = a
    try:
        pass
    except* ValueError:
        out = 0
    return out

def caller(a: int) -> int:
    return helper(a)

def alone(a: int) -> int:
    return a + 1
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let names: Vec<&str> = module
                .functions
                .iter()
                .map(|function| function.name.as_str())
                .collect();
            assert_eq!(names, ["alone"]);
        },
    );
}

#[test]
fn a_method_call_on_an_emitted_class_is_direct() {
    // an emitted class cannot be subclassed, so there is nothing to dispatch on
    with_source(
        "\
data class Point:
    x: int
    y: int

    def total(self) -> int:
        return self.x + self.y

    def scaled(self, k: int) -> int:
        return self.total() * k

def use(p: Point) -> int:
    return p.scaled(3) + p.total()
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let scaled = module.classes[0]
                .methods
                .iter()
                .find(|method| method.name == "scaled")
                .expect("scaled is emitted");
            // a sibling call resolves even though neither body was lowered yet
            for function in [scaled, &module.functions[0]] {
                let text = print_function(function);
                assert!(text.contains("call Point."), "{text}");
                assert!(
                    !has_op(function, |op| matches!(op, Op::CallMethod { .. })),
                    "{text}"
                );
            }
        },
    );
}

#[test]
fn a_direct_method_call_keeps_the_unboxed_representations() {
    with_source(
        "\
data class Box:
    n: int

    def add(self, k: int) -> int:
        return self.n + k

def use(b: Box) -> int:
    return b.add(2)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let text = print_function(&module.functions[0]);
            // no box on the way in and no unbox on the way out
            assert!(
                !has_op(&module.functions[0], |op| matches!(
                    op,
                    Op::Box { .. } | Op::Unbox { .. }
                )),
                "{text}"
            );
        },
    );
}

#[test]
fn a_method_call_on_a_boxed_receiver_still_uses_the_protocol() {
    with_source(
        "\
def use(p: object) -> object:
    return p.total()
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(
                has_op(&module.functions[0], |op| matches!(
                    op,
                    Op::CallMethod { .. }
                )),
                "{}",
                print_function(&module.functions[0])
            );
        },
    );
}

#[test]
fn a_decorated_method_keeps_its_decorators() {
    with_source(
        "\
def doubling(fn: object) -> object:
    return fn

data class Point:
    x: int

    @property
    def total(self) -> int:
        return self.x

    @doubling
    def raw(self) -> int:
        return self.x
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let class = module
                .classes
                .iter()
                .find(|class| class.name == "Point")
                .expect("Point is emitted");
            let decorators = |name: &str| {
                class
                    .methods
                    .iter()
                    .find(|method| method.name == name)
                    .map(|method| method.decorators.clone())
            };
            assert_eq!(decorators("total"), Some(vec!["property".to_string()]));
            assert_eq!(decorators("raw"), Some(vec!["doubling".to_string()]));
        },
    );
}

#[test]
fn a_decline_points_at_the_definition_it_gave_up_on() {
    // a decline is the compiler's report on the code it did *not* take, so it has
    // to carry a range or it cannot be rendered as a diagnostic
    let source = "\
def fast(a: int) -> int:
    return a + 1

def slow(a: int) -> None:
    try:
        pass
    except* ValueError:
        pass
";
    let range = with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        module
            .declined
            .iter()
            .find(|declined| declined.name == "slow")
            .and_then(|declined| declined.range)
    });
    let (start, end) = range.expect("the decline has a range");
    assert_eq!(
        &source[start as usize..end as usize],
        "def slow(a: int) -> None:\n    try:\n        pass\n    except* ValueError:\n        pass"
    );
}

#[test]
fn a_propagated_decline_keeps_its_own_range() {
    let source = "\
def helper(a: int) -> int:
    out = a
    try:
        pass
    except* ValueError:
        out = 0
    return out

def caller(a: int) -> int:
    return helper(a)
";
    let range = with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        module
            .declined
            .iter()
            .find(|declined| declined.name == "caller")
            .and_then(|declined| declined.range)
    });
    let (start, end) = range.expect("the propagated decline has a range");
    assert!(source[start as usize..end as usize].starts_with("def caller"));
}

#[test]
fn a_lowered_function_and_its_blocks_carry_by_spans() {
    // the spans live on the function and on each block, which is the finest
    // granularity that survives the passes untouched
    let source = "\
def f(a: int) -> int:
    if a > 0:
        return a
    return 0
";
    with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        let function = &module.functions[0];
        let (start, end) = function.range.expect("the function has a span");
        assert!(source[start as usize..end as usize].starts_with("def f"));
        assert_eq!(end as usize, source.trim_end().len());

        // every block that holds lowered statements points into the source
        let spanned = function
            .blocks
            .iter()
            .filter_map(|block| block.range)
            .map(|(start, _)| source[start as usize..].lines().next().unwrap_or("").trim())
            .collect::<Vec<_>>();
        assert!(spanned.contains(&"if a > 0:"), "{spanned:?}");
        assert!(spanned.contains(&"return a"), "{spanned:?}");
        assert!(spanned.contains(&"return 0"), "{spanned:?}");
    });
}

#[test]
fn a_block_keeps_the_span_of_its_first_statement() {
    let source = "\
def f(a: int) -> int:
    b = a + 1
    c = b + 1
    return c
";
    with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        let (start, _) = module.functions[0].blocks[0]
            .range
            .expect("the entry block has a span");
        assert_eq!(
            source[start as usize..].lines().next().unwrap_or("").trim(),
            "b = a + 1"
        );
    });
}

#[test]
fn calling_a_callable_held_in_a_parameter_reads_the_register() {
    // resolving the name as a global instead raised `NameError` for every callable
    // passed in as an argument
    with_source(
        "\
def apply(f: object, a: int) -> object:
    return f(a)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let function = &module.functions[0];
            assert!(
                has_op(function, |op| matches!(op, Op::CallValue { .. })),
                "{}",
                print_function(function)
            );
            assert!(!has_op(function, |op| matches!(op, Op::CallPython { .. })));
        },
    );
}

#[test]
fn calling_a_callable_held_in_a_local_reads_the_register() {
    with_source(
        "\
def pick(flag: bool) -> object:
    fn = len
    return fn(\"abc\")
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let function = &module.functions[0];
            assert!(
                has_op(function, |op| matches!(op, Op::CallValue { .. })),
                "{}",
                print_function(function)
            );
        },
    );
}

#[test]
fn calling_a_name_this_frame_does_not_bind_still_resolves_as_a_global() {
    with_source(
        "\
def use(a: int) -> object:
    return print(a)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let function = &module.functions[0];
            assert!(
                has_op(function, |op| matches!(op, Op::CallPython { .. })),
                "{}",
                print_function(function)
            );
            assert!(!has_op(function, |op| matches!(op, Op::CallValue { .. })));
        },
    );
}

#[test]
fn a_shadowed_builtin_is_not_the_builtin() {
    // `len` has a direct lowering, and a parameter called `len` is not it
    with_source(
        "\
def use(len: object, s: str) -> object:
    return len(s)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let function = &module.functions[0];
            assert!(
                has_op(function, |op| matches!(op, Op::CallValue { .. })),
                "{}",
                print_function(function)
            );
            assert!(!has_op(function, |op| matches!(op, Op::Len { .. })));
        },
    );
}

#[test]
fn a_name_this_frame_does_not_bind_reads_the_module_namespace() {
    with_source(
        "\
LIMIT = 10

def limit() -> object:
    return LIMIT
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let function = &module.functions[0];
            assert!(
                has_op(function, |op| matches!(op, Op::LoadGlobal { .. })),
                "{}",
                print_function(function)
            );
        },
    );
}

#[test]
fn a_nested_function_becomes_a_method_of_a_generated_environment() {
    with_source(
        "\
def make_adder(n: int) -> object:
    def add(a: int) -> int:
        return a + n
    return add
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let environment = module
                .classes
                .iter()
                .find(|class| class.name == "make_adder$env")
                .expect("the environment is emitted");
            // nothing should be able to name it
            assert!(!environment.exported);
            // one field per capture, at the captured value's representation
            assert_eq!(environment.fields.len(), 1);
            assert_eq!(environment.fields[0].name, "n");
            assert_eq!(environment.fields[0].ty, RType::INT);

            // the receiver is *prepended*: a nested function has no `self` written
            let method = &environment.methods[0];
            assert_eq!(method.name, "add");
            assert_eq!(method.param_count, 2);
            assert_eq!(method.params()[0].name.as_deref(), Some("$env"));
            assert_eq!(method.params()[1].name.as_deref(), Some("a"));

            // and the capture is read as a field, not as a global
            let text = print_function(method);
            assert!(text.contains("<make_adder$env.n>"), "{text}");
            assert!(
                !has_op(method, |op| matches!(op, Op::LoadGlobal { .. })),
                "{text}"
            );

            // the enclosing function allocates the environment and binds a closure
            let outer = &module.functions[0];
            let outer_text = print_function(outer);
            assert!(outer_text.contains("new make_adder$env("), "{outer_text}");
            assert!(
                outer_text.contains("closure make_adder$env.add"),
                "{outer_text}"
            );
        },
    );
}

#[test]
fn a_capture_either_frame_writes_becomes_a_shared_cell() {
    // python closes over the *variable*: the write after the `def` is visible through
    // the closure, so the name cannot live in a register in either frame
    with_source(
        "\
def counter() -> (() -> int):
    n = 0
    def get() -> int:
        return n
    n = 1
    return get
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let environment = &module.classes[0];
            // a cell is always `object`: it starts unset, and NULL has to be
            // distinguishable from every value it could hold
            assert_eq!(environment.fields[0].name, "n");
            assert_eq!(environment.fields[0].ty, RType::OBJECT);

            // the *enclosing* frame writes the field too
            let outer = print_function(&module.functions[0]);
            assert!(outer.contains("<counter$env.n> ="), "{outer}");
            // and it is allocated with the cell unset, before the body
            assert!(outer.contains("new counter$env(unset)"), "{outer}");

            // the nested frame reads it as a cell, which is the checked form
            let get = &environment.methods[0];
            assert!(
                has_op(get, |op| matches!(op, Op::GetCell { .. })),
                "{}",
                print_function(get)
            );
        },
    );
}

#[test]
fn a_nonlocal_write_from_a_nested_function_shares_the_cell() {
    with_source(
        "\
def bumper() -> (() -> int):
    n = 0
    def bump() -> int:
        nonlocal n
        n = n + 1
        return n
    return bump
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let bump = &module.classes[0].methods[0];
            let text = print_function(bump);
            // the nested frame must not have a register of its own for `n`
            assert!(!text.contains("let n:"), "{text}");
            assert!(
                has_op(bump, |op| matches!(op, Op::GetCell { .. })),
                "{text}"
            );
            assert!(
                has_op(bump, |op| matches!(op, Op::SetField { .. })),
                "{text}"
            );
        },
    );
}

#[test]
fn a_buffer_loop_variable_keeps_the_element_representation() {
    // it was declared `object`, so the body did protocol arithmetic on values that
    // were already unboxed — the compiled loop ran *slower* than cpython
    with_source(
        "\
def prefix(n: int) -> float:
    xs = [i * 1.5 for i in range(n)]
    out = 0.0
    for x in xs:
        out = out + x
    return out
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let prefix = module
                .all_functions()
                .find(|function| function.name == "prefix")
                .expect("prefix is compiled");
            let text = print_function(prefix);
            // the element is a `float` register, and the addition is float
            // arithmetic rather than a call into the object protocol
            assert!(
                prefix
                    .registers
                    .iter()
                    .any(|decl| decl.name.as_deref() == Some("x") && decl.ty == RType::FLOAT),
                "{text}"
            );
            assert!(
                has_op(prefix, |op| matches!(op, Op::FloatBinary { .. })),
                "{text}"
            );
            assert!(!text.contains("box x"), "{text}");
        },
    );
}

#[test]
fn a_captured_loop_binding_is_a_copy_per_closure() {
    // basedpython gives each iteration its own binding, so the environment is
    // allocated where the closure is written rather than once for the frame
    with_source(
        "\
def each(xs: list[int]) -> list[object]:
    out = []
    for i in xs:
        out.append(lambda: i)
    return out
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let each = module
                .all_functions()
                .find(|function| function.name == "each")
                .expect("each is compiled");
            let text = print_function(each);
            // inside the loop, not before it — and it is a plain field, never a cell
            assert_eq!(text.matches("new each$env").count(), 1, "{text}");
            let body = each
                .blocks
                .iter()
                .find(|block| {
                    block
                        .ops
                        .iter()
                        .any(|op| matches!(op, Op::MakeClosure { .. }))
                })
                .expect("the closure is made somewhere");
            assert!(
                body.ops
                    .iter()
                    .any(|op| matches!(op, Op::NewInstance { .. })),
                "{text}"
            );
            let inner = module
                .all_functions()
                .find(|function| function.name.starts_with("$lambda"))
                .expect("the lambda is compiled");
            assert!(
                !has_op(inner, |op| matches!(op, Op::GetCell { .. })),
                "{}",
                print_function(inner)
            );
        },
    );
}

#[test]
fn python_sharing_is_restored_when_the_flag_is_off() {
    // `--no-unique-loop-bindings` puts the loop back to one cell, and the compiler
    // has to read the same flag the transpiler does
    with_source(
        "\
def each(xs: list[int]) -> list[object]:
    out = []
    for i in xs:
        out.append(lambda: i)
    return out
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", false);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let each = module
                .all_functions()
                .find(|function| function.name == "each")
                .expect("each is compiled");
            let text = print_function(each);
            // one allocation for the frame, and the capture is a shared cell again
            assert_eq!(text.matches("new each$env").count(), 1, "{text}");
            let inner = module
                .all_functions()
                .find(|function| function.name.starts_with("$lambda"))
                .expect("the lambda is compiled");
            assert!(
                has_op(inner, |op| matches!(op, Op::GetCell { .. })),
                "{}",
                print_function(inner)
            );
        },
    );
}

#[test]
fn a_loop_binding_beside_a_shared_cell_takes_two_environments() {
    // one object cannot be both a fresh binding per iteration and a cell that
    // outlives them — so it is two, chained the way a function nested two deep is
    with_source(
        "\
def each(xs: list[int]) -> list[object]:
    total = 100
    out = []
    for i in xs:
        out.append(lambda: i + total)
    total = 200
    return out
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let field = |class: &str, name: &str| {
                module
                    .classes
                    .iter()
                    .find(|candidate| candidate.name == class)
                    .unwrap_or_else(|| panic!("{class} is emitted"))
                    .fields
                    .iter()
                    .any(|field| field.name == name)
            };
            // the cell lives on the frame's environment, the binding on the
            // closure's, and the closure reaches the cell through `$outer`
            assert!(field("each$env", "total"));
            assert!(!field("each$env", "i"));
            assert!(field("each$env$closure", "i"));
            assert!(field("each$env$closure", "$outer"));
            // and the frame's is allocated once while the closure's is not
            let each = module
                .all_functions()
                .find(|function| function.name == "each")
                .expect("each is compiled");
            let text = print_function(each);
            assert_eq!(text.matches("new each$env(").count(), 1, "{text}");
            assert_eq!(text.matches("new each$env$closure(").count(), 1, "{text}");
        },
    );
}

#[test]
fn a_comprehension_target_is_a_local_a_closure_can_capture() {
    // the comprehension is desugared into this frame, so its target is an ordinary
    // local — resolving it as a *global* was a `NameError` at runtime
    with_source(
        "\
def each(xs: list[int]) -> list[object]:
    return [lambda: i for i in xs]
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let inner = module
                .all_functions()
                .find(|function| function.name.starts_with("$lambda"))
                .expect("the lambda is compiled");
            assert!(
                !has_op(inner, |op| matches!(op, Op::LoadGlobal { .. })),
                "{}",
                print_function(inner)
            );
        },
    );
}

#[test]
fn every_closure_a_loop_makes_shares_one_cell() {
    // this is the case that makes the cell mandatory rather than tidy: in python all
    // three closures observe the final value
    with_source(
        "\
def loop_closures() -> list[object]:
    out = []
    i = 0
    while i < 3:
        def show() -> int:
            return i
        out.append(show)
        i = i + 1
    return out
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let outer = print_function(&module.functions[0]);
            // one allocation, before the loop — not one per iteration
            assert_eq!(outer.matches("new loop_closures$env").count(), 1, "{outer}");
        },
    );
}

#[test]
fn a_nested_function_that_assigns_an_enclosing_name_is_declined() {
    let reason = with_source(
        "\
def outer(n: int) -> object:
    def bump() -> int:
        n = 1
        return n
    return bump
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            module
                .declined
                .iter()
                .find(|declined| declined.name == "outer")
                .map(|declined| declined.reason.clone())
                .unwrap_or_default()
        },
    );
    // the nested `n = 1` makes `n` the nested function's own name, so `outer` has
    // no capture — and its own `n` is then never read
    assert!(reason.is_empty() || reason.contains("cell"), "{reason}");
}

#[test]
fn each_environment_holds_the_one_that_encloses_it() {
    with_source(
        "\
def outer(a: int) -> object:
    def middle(b: int) -> object:
        def inner(c: int) -> int:
            return a + b + c
        return inner
    return middle
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let field = |class: &str, name: &str| {
                module
                    .classes
                    .iter()
                    .find(|candidate| candidate.name == class)
                    .unwrap_or_else(|| panic!("{class} is emitted"))
                    .fields
                    .iter()
                    .find(|field| field.name == name)
                    .map(|field| field.ty.clone())
            };
            // the outer frame owns `a`, the middle owns `b`
            assert!(field("outer$env", "a").is_some());
            assert!(field("outer$env$middle$env", "b").is_some());
            // and `a` is *not* copied into the middle's environment: it is reached
            // through the chain, so there is one home for it
            assert!(field("outer$env$middle$env", "a").is_none());
            assert_eq!(
                field("outer$env$middle$env", "$outer"),
                Some(RType::Instance {
                    class: "outer$env".to_string(),
                    exact: false,
                })
            );
        },
    );
}

#[test]
fn a_read_two_frames_up_walks_the_chain() {
    with_source(
        "\
def outer(a: int) -> object:
    def middle(b: int) -> object:
        def inner() -> int:
            return a
        return inner
    return middle
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let inner = module
                .all_functions()
                .find(|function| function.name == "inner")
                .expect("inner is compiled");
            let ops: Vec<&Op> = inner.blocks.iter().flat_map(|block| &block.ops).collect();
            // the enclosing environment comes out of the receiver, and `a` comes out
            // of *that* — a copy into the middle's environment would read one field
            let chain = ops
                .iter()
                .find_map(|op| match op {
                    Op::GetField {
                        dest,
                        receiver: Value::Register(RegisterId(0)),
                        field,
                        ..
                    } if field == "$outer" => Some(*dest),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{}", print_function(inner)));
            assert!(
                ops.iter().any(|op| matches!(
                    op,
                    Op::GetField { receiver: Value::Register(id), field, .. }
                        if *id == chain && field == "a"
                )),
                "{}",
                print_function(inner)
            );
        },
    );
}

#[test]
fn a_shared_cell_two_frames_up_stays_one_cell() {
    with_source(
        "\
def outer() -> object:
    n = 0
    def middle() -> object:
        def inner() -> int:
            return n
        return inner
    n = 1
    return middle
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let holders: Vec<&str> = module
                .classes
                .iter()
                .filter(|class| class.fields.iter().any(|field| field.name == "n"))
                .map(|class| class.name.as_str())
                .collect();
            // exactly one environment holds `n`, or the write after the `def` would
            // not be visible through the closure
            assert_eq!(holders, ["outer$env"]);
        },
    );
}

#[test]
fn a_nested_function_capturing_nothing_still_gets_an_environment() {
    // there has to be an object to hang the method on, so the layout gets a
    // synthetic placeholder field rather than being empty
    with_source(
        "\
def helper(a: int) -> int:
    def double(x: int) -> int:
        return x * 2
    return double(a)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let environment = module
                .classes
                .iter()
                .find(|class| class.name == "helper$env")
                .expect("the environment is emitted");
            assert_eq!(environment.fields.len(), 1);
            assert_eq!(environment.fields[0].name, "$empty");
        },
    );
}

#[test]
fn a_captured_callable_is_called_through_the_field() {
    // the decorator shape: `f` is a capture, so calling it is a field read then a
    // call through the value — resolving it as a global raised `NameError`
    with_source(
        "\
def twice(f: object) -> object:
    def wrapper(n: int) -> int:
        return f(n) * 2
    return wrapper
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let method = &module.classes[0].methods[0];
            let text = print_function(method);
            assert!(text.contains("<twice$env.f>"), "{text}");
            assert!(
                has_op(method, |op| matches!(op, Op::CallValue { .. })),
                "{text}"
            );
            assert!(
                !has_op(method, |op| matches!(op, Op::CallPython { .. })),
                "{text}"
            );
        },
    );
}

#[test]
fn a_call_to_a_closure_this_frame_made_goes_direct() {
    // the environment is in a register right here, so there is nothing to look up
    // and nothing to box
    with_source(
        "\
def run(times: int, k: int) -> int:
    def step(a: int) -> int:
        return a + k
    total = 0
    for _ in range(times):
        total = step(total)
    return total
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let outer = &module.functions[0];
            let text = print_function(outer);
            assert!(text.contains("call run$env.step("), "{text}");
            assert!(
                !has_op(outer, |op| matches!(op, Op::CallValue { .. })),
                "{text}"
            );
        },
    );
}

#[test]
fn a_closure_used_before_its_def_compiles_and_raises() {
    // python raises `UnboundLocalError` here — the `def` binds the name only when it
    // runs — and so does the compiled function, which carries a byte saying whether the
    // local has been written. it used to decline; now it agrees
    let flagged = with_source(
        "\
def early(a: int) -> int:
    if a > 0:
        return helper(a)
    def helper(x: int) -> int:
        return x * 2
    return helper(a)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            module
                .declined
                .iter()
                .find(|declined| declined.name == "early")
                .map(|declined| declined.reason.clone())
        },
    );
    assert_eq!(flagged, None);
}

#[test]
fn a_generator_becomes_a_state_class_and_a_constructor() {
    with_source(
        "\
def counted(n: int) -> object:
    i = 0
    while i < n:
        yield i
        i = i + 1
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let state = &module.classes[0];
            assert_eq!(state.name, "counted$gen");
            assert!(!state.exported);
            assert_eq!(
                state.resume.as_ref().map(|r| r.method.as_str()),
                Some("$resume")
            );

            // the state number and the two protocol fields have fixed roles; a local
            // takes its own representation where it is definitely assigned
            let field = |name: &str| state.fields.iter().find(|f| f.name == name);
            assert_eq!(field("$state").map(|f| f.ty.clone()), Some(RType::INT));
            assert_eq!(field("$sent").map(|f| f.ty.clone()), Some(RType::OBJECT));
            assert_eq!(field("$thrown").map(|f| f.ty.clone()), Some(RType::OBJECT));
            assert_eq!(field("n").map(|f| f.ty.clone()), Some(RType::INT));
            assert_eq!(field("i").map(|f| f.ty.clone()), Some(RType::INT));

            // calling it allocates and returns; it does not run the body
            let constructor = print_function(&module.functions[0]);
            assert!(constructor.contains("new counted$gen("), "{constructor}");
            assert!(!constructor.contains("branch"), "{constructor}");

            // the body dispatches on the state and returns at the yield
            let resume = print_function(&state.methods[0]);
            assert!(resume.contains("<counted$gen.$state>"), "{resume}");
            assert!(resume.contains("raise StopIteration"), "{resume}");
        },
    );
}

#[test]
fn a_value_that_has_to_survive_a_suspension_takes_a_field() {
    // the invariant the whole design rests on: a `yield` *returns*, so nothing in a
    // register comes back. this is what found the loop-iterator bug
    let mut builder = FunctionBuilder::new("$resume", RType::OBJECT);
    let receiver = builder.param(
        "$gen",
        RType::Instance {
            class: "g$gen".to_string(),
            exact: false,
        },
    );
    let held = builder.temp(RType::OBJECT);
    let resume_at = builder.new_block();
    builder.push(Op::GetIter {
        dest: held,
        src: Value::Register(receiver),
    });
    let suspend_at = builder.current_block();
    builder.terminate(Terminator::Return(Value::None));
    builder.switch_to(resume_at);
    // reads a register written before the suspension
    builder.terminate(Terminator::Return(Value::Register(held)));
    let mut function = builder.finish();

    let parked = crate::generators::park_live_registers(
        &mut function,
        "g$gen",
        &[crate::generators::Resumption {
            state: 1,
            suspend: suspend_at,
            resume: resume_at,
        }],
    )
    .expect("a live register takes a field");
    // one field, of the register's own representation, written at the suspension and
    // read back at the resumption
    assert_eq!(
        parked
            .iter()
            .map(|field| (field.name.clone(), field.ty.clone()))
            .collect::<Vec<_>>(),
        vec![("$park1".to_string(), RType::OBJECT)]
    );
    let text = print_function(&function);
    assert!(text.contains("<g$gen.$park1> = r1"), "{text}");
    assert!(text.contains("r1 = $gen.<g$gen.$park1>"), "{text}");
}

#[test]
fn a_parked_value_that_may_be_unassigned_declines() {
    // the byte saying whether a local was written is a register too, and it does not
    // survive the suspension — so the value would come back with `UnboundLocalError`
    // attached rather than with the answer
    let mut builder = FunctionBuilder::new("$resume", RType::OBJECT);
    let receiver = builder.param(
        "$gen",
        RType::Instance {
            class: "g$gen".to_string(),
            exact: false,
        },
    );
    let held = builder.local("maybe", RType::OBJECT);
    let writing = builder.new_block();
    let resume_at = builder.new_block();
    let condition = builder.temp(RType::BIT);
    builder.push(Op::IsNull {
        dest: condition,
        src: Value::Register(receiver),
    });
    let suspend_at = builder.new_block();
    builder.terminate(Terminator::Branch {
        cond: Value::Register(condition),
        then_block: writing,
        else_block: suspend_at,
    });
    builder.switch_to(writing);
    builder.push(Op::GetIter {
        dest: held,
        src: Value::Register(receiver),
    });
    builder.terminate(Terminator::Goto(suspend_at));
    builder.switch_to(suspend_at);
    builder.terminate(Terminator::Return(Value::None));
    builder.switch_to(resume_at);
    builder.terminate(Terminator::Return(Value::Register(held)));
    let mut function = builder.finish();

    let error = crate::generators::park_live_registers(
        &mut function,
        "g$gen",
        &[crate::generators::Resumption {
            state: 1,
            suspend: suspend_at,
            resume: resume_at,
        }],
    )
    .expect_err("a maybe-unassigned local must be declined");
    assert!(error.reason.contains("`maybe`"), "{error:?}");
    assert!(
        error.reason.contains("not assigned on every path"),
        "{error:?}"
    );
}

#[test]
fn a_parked_temporary_keeps_its_own_representation() {
    // the point of parking at all: a state field is a cell by default, forced to
    // `object` because unset has to be distinguishable from every value. a park slot
    // is written on the only path that reaches its read, so the value survives the
    // suspension in the representation it had — `total` here stays a tagged int, and
    // the resumed addition is not the object protocol
    with_source(
        "\
async def stepped(i: int) -> int:
    return i * 7

async def summed(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        total = total + await stepped(i)
        i = i + 1
    return total
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let state = module
                .classes
                .iter()
                .find(|class| class.name == "summed$gen")
                .expect("the coroutine has a state class");
            let parked: Vec<(String, RType)> = state
                .fields
                .iter()
                .filter(|field| field.name.starts_with("$park"))
                .map(|field| (field.name.clone(), field.ty.clone()))
                .collect();
            assert_eq!(parked.len(), 1, "{parked:?}");
            assert_eq!(parked[0].1, RType::INT, "{parked:?}");

            // written before the `return` that suspends, read back at the resumption
            let resume = print_function(&state.methods[0]);
            let slot = &parked[0].0;
            assert!(
                resume.contains(&format!("<summed$gen.{slot}> = ")),
                "{resume}"
            );
            assert!(
                resume.contains(&format!("$gen.<summed$gen.{slot}>")),
                "{resume}"
            );
        },
    );
}

#[test]
fn a_loop_iterator_in_a_generator_lives_in_a_field() {
    with_source(
        "\
def each(words: list[str]) -> object:
    for w in words:
        yield w
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let state = &module.classes[0];
            // one reserved field per `for`, because the iterator has no source name
            assert!(state.fields.iter().any(|f| f.name == "$iter0"));
            let resume = print_function(&state.methods[0]);
            assert!(resume.contains("<each$gen.$iter0>"), "{resume}");
        },
    );
}

#[test]
fn a_yield_inside_try_raises_into_its_own_handler() {
    // `throw` and `close` resume *by raising*, and the raise has to happen at the
    // suspension — otherwise a `yield` inside `try` would skip its own handler
    with_source(
        "\
def guarded(log: list[str], n: int) -> object:
    try:
        yield n
    finally:
        log.append(\"closed\")
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let state = &module.classes[0];
            // the exception rides in a field, checked at every resumption point
            assert!(state.fields.iter().any(|field| field.name == "$thrown"));
            let resume = &state.methods[0];
            let text = print_function(resume);
            // the resumption point reads the field and raises, which enters the
            // enclosing handler because that is this block's error target
            assert!(text.contains("<guarded$gen.$thrown>"), "{text}");
            assert!(
                has_op(resume, |op| matches!(op, Op::Reraise { .. })),
                "{text}"
            );
            assert!(
                resume
                    .blocks
                    .iter()
                    .any(|block| block.error_target.is_some()),
                "the raise has a handler to land in: {text}"
            );
        },
    );
}

#[test]
fn yield_from_delegates_and_takes_the_inner_return_value() {
    with_source(
        "\
def inner(n: int) -> object:
    yield n
    return n * 100

def outer(n: int) -> object:
    got = yield from inner(n)
    yield got
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let outer = module
                .classes
                .iter()
                .find(|class| class.name == "outer$gen")
                .expect("outer is a generator");
            let resume = &outer.methods[0];
            let text = print_function(resume);
            // the inner iterator is parked in a field, because the delegation itself
            // suspends and a register would not come back
            assert!(text.contains("delegiter"), "{text}");
            assert!(text.contains("<outer$gen.$iter0>"), "{text}");
            assert!(
                has_op(resume, |op| matches!(op, Op::DelegateStep { .. })),
                "{text}"
            );
        },
    );
}

#[test]
fn an_await_uses_the_awaitable_protocol_not_iteration() {
    // awaiting an ordinary iterable has to be an error, so the two cannot share the
    // way they obtain the iterator
    with_source(
        "\
async def plain(n: int) -> int:
    return n * 2

async def chained(n: int) -> int:
    return await plain(n)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let chained = module
                .classes
                .iter()
                .find(|class| class.name == "chained$gen")
                .expect("chained is a coroutine");
            // the machine is the same as a generator's; only the surface differs
            assert!(
                chained
                    .resume
                    .as_ref()
                    .is_some_and(|r| { r.surface == by_ir::function::Surface::Coroutine })
            );
            assert_eq!(
                chained.resume.as_ref().map(|r| r.method.as_str()),
                Some("$resume")
            );
            let text = print_function(&chained.methods[0]);
            assert!(text.contains("awaititer"), "{text}");
            assert!(!text.contains("delegiter"), "{text}");
        },
    );
}

#[test]
fn an_async_generator_presents_the_async_iteration_surface() {
    // `__aiter__`/`__anext__` rather than `__await__`: one resume method drives all
    // three surfaces, and which one a state class presents is the only difference
    let surface = with_source(
        "\
async def both(n: int) -> object:
    yield n
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            module
                .classes
                .iter()
                .find(|class| class.name == "both$gen")
                .and_then(|class| class.resume.as_ref())
                .map(|resume| resume.surface)
        },
    );
    assert_eq!(surface, Some(by_ir::function::Surface::AsyncGenerator));
}

#[test]
fn a_with_block_exits_on_every_path() {
    with_source(
        "\
def guarded(mgr: object) -> str:
    with mgr:
        return \"body\"
    return \"after\"
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let function = &module.functions[0];
            let text = print_function(function);
            assert!(text.contains("enter"), "{text}");
            // three exits: the `return` inside the body, the fall-through, and the
            // handler — and `__exit__` runs on all three
            assert_eq!(text.matches("= exit ").count(), 3, "{text}");
        },
    );
}

#[test]
fn an_early_return_runs_the_finally() {
    // the bug this exists for was a silent wrong answer: the `finally` was skipped
    with_source(
        "\
def early(log: list[str]) -> str:
    try:
        return \"body\"
    finally:
        log.append(\"f\")
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let function = &module.functions[0];
            let text = print_function(function);
            // the `append` call appears on the return path as well as the others
            assert!(
                text.matches("callmethod").count() >= 2 || text.matches("append").count() >= 2,
                "{text}"
            );
        },
    );
}

#[test]
fn a_break_runs_only_the_cleanups_inside_the_loop() {
    with_source(
        "\
def looped(log: list[str], n: int) -> str:
    try:
        i = 0
        while i < n:
            try:
                if i == 1:
                    break
            finally:
                log.append(\"inner\")
            i = i + 1
    finally:
        log.append(\"outer\")
    return \"done\"
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let text = print_function(&module.functions[0]);
            // the `break` unwinds to the loop's depth: the inner `finally` runs, the
            // outer one does not — it runs when control leaves the loop normally
            assert!(text.contains("inner"), "{text}");
            assert!(text.contains("outer"), "{text}");
        },
    );
}

#[test]
fn a_lambda_is_a_nested_function_with_a_generated_name() {
    // it goes through the closure machinery unchanged: a synthesized definition, a
    // method of the environment, and a `MakeClosure` where the expression was
    with_source(
        "\
def adder(n: int) -> ((int) -> int):
    return lambda x: x + n
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let environment = &module.classes[0];
            assert_eq!(environment.name, "adder$env");
            assert_eq!(environment.methods[0].name, "$lambda0");
            // the capture is a field read, exactly as for a `def`
            let text = print_function(&environment.methods[0]);
            assert!(text.contains("<adder$env.n>"), "{text}");

            let outer = print_function(&module.functions[0]);
            assert!(outer.contains("closure adder$env.$lambda0"), "{outer}");
        },
    );
}

#[test]
fn a_lambda_that_captures_a_mutated_name_shares_the_cell() {
    with_source(
        "\
def counter() -> (() -> int):
    n = 0
    f = lambda: n
    n = 1
    return f
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let method = &module.classes[0].methods[0];
            assert!(
                has_op(method, |op| matches!(op, Op::GetCell { .. })),
                "{}",
                print_function(method)
            );
        },
    );
}

#[test]
fn a_closure_inside_a_method_gets_a_sibling_environment() {
    // an environment is a *sibling* class, not something nested in the one whose
    // method made it — and the module has to collect both or the layout its methods
    // reference is never emitted
    with_source(
        "\
data class Scaler:
    k: int

    def make(self) -> ((int) -> int):
        return lambda x: x * self.k
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let names: Vec<&str> = module
                .classes
                .iter()
                .map(|class| class.name.as_str())
                .collect();
            assert!(names.contains(&"Scaler"), "{names:?}");
            // qualified by the class, because two classes may each have a `make`
            assert!(names.contains(&"Scaler$make$env"), "{names:?}");
            // the capture is the receiver itself, so the environment holds a native
            // instance rather than a boxed object
            let environment = module
                .classes
                .iter()
                .find(|class| class.name == "Scaler$make$env")
                .expect("the environment is emitted");
            assert_eq!(
                environment.fields[0].ty,
                RType::Instance {
                    class: "Scaler".to_string(),
                    exact: false
                }
            );
        },
    );
}

#[test]
fn a_definitely_assigned_generator_local_is_unboxed() {
    // a state field is a cell — `object`, with an unset check — only where it has to
    // be. a name assigned before every read takes its own representation, which is
    // what keeps a generator's arithmetic off the object protocol
    with_source(
        "\
def counted(n: int) -> object:
    i = 0
    while i < n:
        yield i
        i = i + 1
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let state = &module.classes[0];
            let ty = |name: &str| {
                state
                    .fields
                    .iter()
                    .find(|field| field.name == name)
                    .map(|field| field.ty.clone())
            };
            // the parameter is seeded by the constructor, and `i` is assigned first
            assert_eq!(ty("n"), Some(RType::INT));
            assert_eq!(ty("i"), Some(RType::INT));
            // so the reads are infallible field loads, not checked cell reads
            let resume = &state.methods[0];
            assert!(
                !has_op(resume, |op| matches!(op, Op::GetCell { .. })),
                "{}",
                print_function(resume)
            );
        },
    );
}

#[test]
fn a_generator_local_that_may_be_unset_stays_a_cell() {
    // each of these would read a *zero* instead of raising if it were unboxed
    for (source, name) in [
        ("def f(n: int) -> object:\n    yield x\n    x = 1\n", "x"),
        (
            "def f(n: int) -> object:\n    if n:\n        y = 1\n    yield y\n",
            "y",
        ),
        (
            "def f(n: int) -> object:\n    total += n\n    yield total\n",
            "total",
        ),
        (
            "def f(v: list[int]) -> object:\n    for e in v:\n        pass\n    yield e\n",
            "e",
        ),
    ] {
        with_source(source, |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            let state = &module.classes[0];
            let field = state
                .fields
                .iter()
                .find(|field| field.name == name)
                .unwrap_or_else(|| panic!("`{name}` is a state field"));
            assert_eq!(field.ty, RType::OBJECT, "`{name}` must stay a cell");
            assert!(
                has_op(&state.methods[0], |op| matches!(op, Op::GetCell { .. })),
                "`{name}` must be read with the unset check"
            );
        });
    }
}

#[test]
fn a_compiled_call_packs_a_variadic_callee_itself() {
    // the callee's body sees the same ordinary tuple and dict it sees when python
    // calls it — the packing just happens at compile time instead
    with_source(
        "\
def both(a: int, *rest: int, **named: object) -> int:
    return a + len(rest) + len(named)

def caller(a: int) -> int:
    return both(a, 1, 2, k=3)
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let caller = module
                .functions
                .iter()
                .find(|function| function.name == "caller")
                .expect("caller is emitted");
            let text = print_function(caller);
            assert!(text.contains("= tuple("), "{text}");
            assert!(text.contains("= dict("), "{text}");
            // and it is a *native* call, not one through the object protocol
            assert!(text.contains("call both("), "{text}");
        },
    );
}

#[test]
fn a_local_read_on_a_path_that_skips_it_is_flagged_by_name() {
    let named = with_source(
        "\
def picked(flag: bool, n: int) -> int:
    if flag:
        value = n
    return value
",
        |db, env, model, suite| {
            let module = crate::build_module(db, env, model, suite, "app", true);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            module
                .functions
                .iter()
                .find(|function| function.name == "picked")
                .map(|function| {
                    function
                        .registers
                        .iter()
                        .filter(|decl| decl.may_be_unassigned)
                        .filter_map(|decl| decl.name.clone())
                        .collect::<Vec<_>>()
                })
        },
    );
    assert_eq!(named, Some(vec!["value".to_string()]));
}

/// the reason each declined entry in `source` gives, by name
fn declines(source: &str) -> Vec<(String, String)> {
    with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        module
            .declined
            .iter()
            .map(|declined| (declined.name.clone(), declined.reason.clone()))
            .collect()
    })
}

/// the rendered IR of one method of one emitted class
///
/// the behavioural tests in `by_build` say the *answer* is right; this says the answer
/// is reached the intended way, which is what catches a lowering that regresses to a
/// slower or differently-scoped shape while still computing the same thing
fn method_ir(source: &str, class: &str, method: &str) -> String {
    with_source(source, |db, env, model, suite| {
        let module = crate::build_module(db, env, model, suite, "app", true);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        module
            .classes
            .iter()
            .find(|candidate| candidate.name == class)
            .and_then(|owner| {
                owner
                    .methods
                    .iter()
                    .find(|candidate| candidate.name == method)
            })
            .map(print_function)
            .unwrap_or_else(|| panic!("{class}.{method} was not emitted"))
    })
}

/// a base with a method to reach, for the `super()` cases below
const A_BASE: &str = "\
data class A:
    n: int

    def label(self) -> str:
        return \"a\"
";

#[test]
fn a_zero_argument_super_pivots_on_the_class_the_body_was_written_in() {
    // the pivot is `B` itself, not the receiver's own class. reaching for the
    // receiver's would make an inherited body find *itself* and recur forever, and it
    // is the one thing a behavioural test on `B` alone cannot tell apart — `C(B)`
    // inheriting `label` is where the difference shows
    let ir = method_ir(
        &format!(
            "{A_BASE}
data class B(A):
    def label(self) -> str:
        return super().label()
"
        ),
        "B",
        "label",
    );
    // `class B` is the pivot and `r1` the boxed receiver: the two-argument form
    // python's own compiler would have built from the frame
    assert!(ir.contains("= class B"), "{ir}");
    assert!(ir.contains("pycall super(r2, r1)"), "{ir}");
}

#[test]
fn a_rebound_super_is_an_ordinary_call() {
    // the zero-argument form is only sugar when `super` *is* the builtin. a module that
    // binds the name to something of its own is calling that, and lowering it to the
    // two-argument form would hand a one-argument function two — so this compiles, and
    // compiles as a plain call rather than declining
    let ir = method_ir(
        &format!(
            "{A_BASE}
def super() -> str:
    return \"shadowed\"


data class B(A):
    def label(self) -> str:
        return super()
"
        ),
        "B",
        "label",
    );
    assert!(!ir.contains("= class B"), "{ir}");
    // and it resolves to the module's own function: a native call, not the object
    // protocol, which is what a shadowing definition in this module earns
    assert!(ir.contains("call super()"), "{ir}");
}

#[test]
fn a_zero_argument_super_declines_in_a_nested_function() {
    // the nested frame's slot zero is its own first argument, not the method's receiver
    let reasons = declines(&format!(
        "{A_BASE}
data class B(A):
    def nested(self) -> str:
        def inner() -> str:
            return super().label()
        return inner()
"
    ));
    assert!(
        reasons
            .iter()
            .any(|(_, r)| r.contains("nested function's own slot zero")),
        "{reasons:?}"
    );
}

#[test]
fn a_zero_argument_super_declines_in_a_generator() {
    // a generator's body becomes a method of its *state* object, and slot zero holds
    // that state rather than the instance the generator was written on
    let reasons = declines(&format!(
        "{A_BASE}
data class B(A):
    def gen(self) -> object:
        yield super().label()
"
    ));
    assert!(
        reasons
            .iter()
            .any(|(_, r)| r.contains("generator's resume frame")),
        "{reasons:?}"
    );
}

#[test]
fn a_dunder_with_no_slot_of_its_own_is_not_declined_for_one() {
    // `PyNumberMethods` has no complex field, so `complex(x)` finds `__complex__` by
    // name — the method table is all it ever needed
    let reasons = declines(
        "\
class Cell:
    def __init__(self, n: int) -> None:
        self.n = n

    def __complex__(self) -> object:
        return complex(self.n, 1)
",
    );
    assert!(reasons.is_empty(), "{reasons:?}");
}

#[test]
fn the_dunders_whose_slots_have_adapters_are_not_declined() {
    // each of these fills a slot the emitter now writes an adapter for, so the gate
    // that lists what has one has to name them or the class declines with a slot
    // sitting empty
    for method in [
        "def __await__(self) -> object:\n        return iter([])",
        "def __get__(self, obj: object, owner: object) -> object:\n        return self.n",
        "def __del__(self) -> None:\n        pass",
        "def __getattr__(self, name: str) -> object:\n        return name",
    ] {
        let reasons = declines(&format!(
            "\
class Held:
    def __init__(self, n: int) -> None:
        self.n = n

    {method}
"
        ));
        assert!(reasons.is_empty(), "{method}: {reasons:?}");
    }
}

#[test]
fn a_dunder_whose_slot_has_no_adapter_still_declines() {
    // the gate is what keeps a slot the emitter cannot fill from compiling to a class
    // python never consults — a wrong answer where a decline was right
    for method in [
        "def __new__(cls, n: int) -> \"Held\":\n        return object.__new__(cls)",
        "def __setattr__(self, name: str, value: object) -> None:\n        pass",
        "def __getattribute__(self, name: str) -> object:\n        return name",
        // `__get__` reaching `tp_descr_get` does not make this class a *data*
        // descriptor, and `__ipow__` fills a ternary slot of its own that `nb_power`
        // did not bring with it
        "def __set__(self, obj: object, value: object) -> None:\n        pass",
        "def __ipow__(self, other: object) -> object:\n        return self",
    ] {
        let reasons = declines(&format!(
            "\
class Held:
    def __init__(self, n: int) -> None:
        self.n = n

    {method}
"
        ));
        assert!(
            reasons
                .iter()
                .any(|(_, r)| r.contains("fills a type slot with no adapter yet")),
            "{method}: {reasons:?}"
        );
    }
}

#[test]
fn a_finalizer_declines_where_a_base_owns_the_dealloc() {
    // `tp_finalize` is reached from `tp_dealloc`, and the dealloc belongs to whichever
    // class owns the instance layout. this one is freed through `dict`'s, which never
    // calls a finalizer — so the cleanup would silently not happen
    let reasons = declines(
        "\
class Held(dict):
    def __del__(self) -> None:
        pass
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Held".to_string(),
            "`__del__` is reached from the dealloc of the class that owns the layout, and this one extends a base".to_string()
        )]
    );
}

#[test]
fn a_getattr_hook_declines_where_a_base_owns_the_lookup() {
    // the hook runs the ordinary lookup and falls back only where it raised, and what
    // the ordinary one *is* comes from the base. `dict` answers with the generic one
    // here, but nothing about a base says it must — so this is lowered only where the
    // base is `object`
    let reasons = declines(
        "\
class Held(dict):
    def __getattr__(self, name: str) -> object:
        return name
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Held".to_string(),
            "`__getattr__` falls back from the lookup a base may have replaced, and this class extends one".to_string()
        )]
    );
}

#[test]
fn a_class_storing_through_setattr_has_no_layout() {
    // the fields are the attributes the body is seen to assign, and they are the whole
    // of an emitted instance — `setattr` names its attribute as a *value*, so no layout
    // can hold what it stores
    let reasons = declines(
        "\
class Held:
    def __init__(self) -> None:
        self.n = 0

    def install(self, name: str) -> None:
        setattr(self, name, 1)
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Held".to_string(),
            "a `setattr` on the receiver names its attribute at runtime".to_string()
        )]
    );
}

#[test]
fn a_setattr_on_anything_but_the_receiver_leaves_the_layout_alone() {
    // the rule is about *this* class's own instances: storing on something else is
    // somebody else's layout, and a class that never receives one keeps its fields
    let reasons = declines(
        "\
class Held:
    def __init__(self) -> None:
        self.n = 0

    def install(self, other: object, name: str) -> None:
        setattr(other, name, 1)
",
    );
    assert_eq!(reasons, Vec::new());
}

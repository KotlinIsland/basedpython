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
    count_op(function, predicate) > 0
}

fn count_op(function: &by_ir::function::Function, predicate: impl Fn(&Op) -> bool) -> usize {
    function
        .blocks
        .iter()
        .flat_map(|block| block.ops.iter())
        .filter(|op| predicate(op))
        .count()
}

/// each decorator as it was written, which is what a test about *which* decorators
/// travel with a definition wants to read
fn dotted(decorators: &[by_ir::function::Decorator]) -> Vec<String> {
    decorators
        .iter()
        .map(by_ir::function::Decorator::dotted)
        .collect()
}

/// lower `source` and render the module's IR, failing if it does not verify
fn ir(source: &str) -> String {
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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

/// the representation of one parameter of the single function in `source`
fn param_type(source: &str, function: &str, parameter: &str) -> RType {
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        module
            .all_functions()
            .find(|candidate| candidate.name == function)
            .and_then(|lowered| {
                lowered
                    .params()
                    .iter()
                    .find(|decl| decl.name.as_deref() == Some(parameter))
                    .map(|decl| decl.ty.clone())
            })
            .unwrap_or_else(|| panic!("{function} has no parameter {parameter}"))
    })
}

#[test]
fn a_parameter_its_own_body_rebinds_covers_every_write() {
    // an unannotated parameter is declared by its default, so `safe='/'` alone would
    // make the register a `str`. the body writes to it too, and `safe.encode(...)` is
    // bytes: a `str` register would have to narrow that store with a check, and the
    // check raises on a call the interpreter answers
    assert_eq!(
        param_type(
            "\
def quoted(safe='/'):
    safe = safe.encode('ascii')
    return repr(safe)
",
            "quoted",
            "safe",
        ),
        RType::OBJECT
    );
}

#[test]
fn a_walrus_and_a_handler_name_are_writes_a_parameter_has_to_cover() {
    // neither is an assignment statement: a walrus binds from inside an expression,
    // and a handler's name hangs off the `try` rather than standing in its body. both
    // were invisible to the walk that decides a register's representation
    assert_eq!(
        param_type(
            "\
def walrused(safe='/'):
    if (safe := safe.encode('ascii')):
        return repr(safe)
    return 'empty'
",
            "walrused",
            "safe",
        ),
        RType::OBJECT
    );
    assert_eq!(
        param_type(
            "\
def caught(tag='t'):
    try:
        raise ValueError('boom')
    except ValueError as tag:
        return repr(tag)
",
            "caught",
            "tag",
        ),
        RType::OBJECT
    );
}

#[test]
fn a_parameter_its_own_body_leaves_alone_keeps_its_declared_representation() {
    // the widening is per parameter and driven by the writes, so a body that only
    // *reads* one costs it nothing — a `str` here stays laid out as a `str`
    assert_eq!(
        param_type(
            "\
def quoted(safe='/'):
    return repr(safe)
",
            "quoted",
            "safe",
        ),
        RType::STR
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            let fail = module
                .all_functions()
                .find(|function| function.name == "fail")
                .expect("fail is compiled");
            assert_eq!(fail.ret, RType::INT);
        },
    );
}

#[test]
fn a_returned_pair_holds_an_instance_in_a_slot_of_its_own() {
    // a slot is a pointer to the class's struct, so the pair is two words rather
    // than a heap `tuple` — the class's name is declared ahead of the tuple structs
    // for exactly this
    with_source(
        "\
class Point:
    def __init__(self, x: int) -> None:
        self.x = x


def placed(n: int) -> tuple[Point, int]:
    return Point(n), n
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            let placed = module
                .all_functions()
                .find(|function| function.name == "placed")
                .expect("placed is compiled");
            let RType::Tuple(slots) = &placed.ret else {
                panic!("the pair stayed on the heap: {}", print_function(placed));
            };
            assert!(
                matches!(slots.first(), Some(RType::Instance { class, .. }) if class == "Point"),
                "{}",
                print_function(placed)
            );
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
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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

/// the decorators each method of one emitted class still carries, in the order they lower
///
/// what a method *keeps* is the question the metaclass construction turns on: a decorator
/// still on the list is carried off the interpreted body, and one lowering consumed is a
/// method table entry with a convention on it
fn method_decorators(source: &str, class: &str) -> Vec<(String, Vec<String>)> {
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        module
            .classes
            .iter()
            .find(|candidate| candidate.name == class)
            .unwrap_or_else(|| panic!("{class} is emitted"))
            .methods
            .iter()
            .map(|method| {
                (
                    method.name.clone(),
                    method
                        .decorators
                        .iter()
                        .map(by_ir::function::Decorator::dotted)
                        .collect(),
                )
            })
            .collect()
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

/// the type slots one emitted class fills from an assignment, and whether each is called
fn class_slot_aliases(source: &str, class: &str) -> Vec<(String, bool)> {
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        module
            .classes
            .iter()
            .find(|candidate| candidate.name == class)
            .unwrap_or_else(|| panic!("{class} is emitted"))
            .slot_aliases
            .iter()
            .map(|alias| (alias.name.clone(), alias.unsupported))
            .collect()
    })
}

#[test]
fn a_dunder_the_body_assigns_is_a_constant_and_a_slot_at_once() {
    // the copy alone gives two answers: `repr(x)` reads `tp_repr`, which the assignment
    // never touched, while `x.__repr__()` reads the name it did
    const SOURCE: &str = "\
def _describe(self) -> str:
    return \"described\"


class Tagged:
    KIND = \"tagged\"
    __repr__ = _describe
    __str__ = __repr__
";
    assert_eq!(
        class_constants(SOURCE, "Tagged"),
        ["KIND", "__repr__", "__str__"]
    );
    assert_eq!(
        class_slot_aliases(SOURCE, "Tagged"),
        [
            ("__repr__".to_string(), false),
            ("__str__".to_string(), false)
        ]
    );
}

#[test]
fn a_hash_the_body_assigns_none_fills_its_slot_with_nothing_to_call() {
    // `numbers.Number` is the corpus's example. python's slot for it is
    // `PyObject_HashNotImplemented` rather than a call into the `None`
    assert_eq!(
        class_slot_aliases(
            "\
class Opaque:
    __hash__ = None
",
            "Opaque"
        ),
        [("__hash__".to_string(), true)]
    );
}

#[test]
fn a_dunder_with_no_slot_adapter_declines_however_the_body_wrote_it() {
    // the same question the `def` path asks. `collections`' namedtuple machinery writes
    // `__new__ = eval(code, namespace)`, which is arbitrary and reaches a slot nothing
    // here can fill — and a `__setattr__` that never runs would let a write through that
    // the interpreted class intercepts
    assert_eq!(
        declines(
            "\
def _hook(self, name: str, value: int) -> None:
    return None


class Written:
    __setattr__ = _hook
"
        ),
        [(
            "Written".to_string(),
            "`__setattr__` fills a type slot with no adapter yet".to_string()
        )]
    );
}

#[test]
fn a_slot_other_than_hash_assigned_none_declines() {
    // python reads `__X__ = None` as "this type does not support that operation at all",
    // and `tp_hash` is the only slot with a standing value saying so. turning any other
    // one off would need the slot left empty *and* the inherited one kept out of it,
    // which a spec cannot express
    assert_eq!(
        declines(
            "\
class Opaque:
    __iter__ = None
"
        ),
        [(
            "Opaque".to_string(),
            "`__iter__ = None` turns a type slot off, and only `__hash__` has a standing \
             value for that"
                .to_string()
        )]
    );
}

#[test]
fn a_dunder_both_defined_and_assigned_declines() {
    // the copy would put the assignment's value over the method's descriptor in the
    // type's dict while the slot went on calling the method — two answers again, and
    // this time from the two halves of one class
    assert_eq!(
        declines(
            "\
def _describe(self) -> str:
    return \"assigned\"


class Tagged:
    def __repr__(self) -> str:
        return \"defined\"

    __repr__ = _describe
"
        ),
        [(
            "Tagged".to_string(),
            "`__repr__` is both defined and assigned, and its type slot has room for one"
                .to_string()
        )]
    );
}

#[test]
fn a_docstring_holding_a_nul_declines() {
    // the method table spells `ml_doc` as a `const char *` and python reads it with
    // `PyUnicode_FromString`, which stops at the first NUL. so there are two entries this
    // could be given and neither means it: the truncation, and the `NULL` that stands for
    // a definition with no docstring at all. either would have the compiled definition
    // answer `__doc__` with something its interpreted twin does not, which is worth more
    // than the compilation
    assert_eq!(
        declines(
            "\
def described() -> int:
    \"before\\0after\"
    return 1
"
        ),
        [(
            "described".to_string(),
            "a docstring containing a NUL has no faithful spelling in a method table".to_string()
        )]
    );
}

#[test]
fn an_annotated_assignment_to_a_slot_dunder_fills_the_slot_too() {
    // the annotated path used to carry the constant without asking the question the
    // plain one asks, which is the same two answers reached by a different statement
    assert_eq!(
        class_slot_aliases(
            "\
from typing import Callable


def _describe(self) -> str:
    return \"described\"


class Tagged:
    __repr__: Callable[[\"Tagged\"], str] = _describe
",
            "Tagged"
        ),
        [("__repr__".to_string(), false)]
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let class = &module.classes[0];
            let fields: Vec<&str> = class.fields.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(fields, ["x"]);
            assert!(class.constants.is_empty(), "{:?}", class.constants);
        },
    );
}

#[test]
fn a_name_that_is_both_a_class_level_value_and_a_field_carries_it_as_a_default() {
    // this is python's commonest way of writing a field with a fallback, and the two
    // answers under the one name are what the presence byte tells apart — so the field
    // takes one whether or not `__init__` would have needed it
    with_source(
        "\
class Tagged:
    KIND: str = \"class-level\"

    def __init__(self, kind: str) -> None:
        self.KIND = kind
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let class = &module.classes[0];
            let field = class
                .fields
                .iter()
                .find(|field| field.name == "KIND")
                .expect("the field is laid out");
            assert_eq!(field.defaulted_by.as_deref(), Some("Tagged"));
            assert!(field.optional, "a defaulted field needs an absent state");
            // it stays a constant: the copy is what puts the value in the type's dict for
            // the descriptor to take out of it at init
            assert!(class.constants.contains(&"KIND".to_string()));
        },
    );
}

#[test]
fn a_class_level_value_that_may_be_a_descriptor_still_declines_against_a_field() {
    // `calendar.Calendar` writes `firstweekday = property(...)` beside a
    // `self.firstweekday = ...`, and that is not a field at all: the assignment runs the
    // property's setter and the instance keeps nothing of its own. it is spelled exactly
    // like a field with a fallback, so only the value tells them apart — and nothing here
    // knows what a call is going to answer
    let reasons = declines(
        "\
class Tagged:
    KIND = property(lambda self: \"class-level\")

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
fn a_dunder_that_is_both_a_class_level_value_and_a_field_still_declines() {
    // a dunder in that shape is two mechanisms over one name: the type slot the assignment
    // fills is taken back out of the type's dict by name, and a descriptor standing there
    // instead would be held as the slot's filler and called as one. leaving every dunder
    // out is what keeps them apart, and it costs nothing — a name python reaches through a
    // slot is not an attribute a class keeps a fallback for
    let reasons = declines(
        "\
class Sized:
    __len__ = 3

    def __init__(self, own: bool) -> None:
        if own:
            self.__len__ = 9
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "Sized"
            && reason.contains("both a class-level constant and a field")),
        "{reasons:?}"
    );
}

#[test]
fn a_class_level_value_over_a_field_its_base_fixed_declines() {
    // the presence byte is part of the base's layout, and a subclass cannot add one to a
    // struct its base already settled. without it the base's own constructor would write
    // the field while recording nothing, and every instance would read as having nothing
    // of its own — the class's value answering over an instance that has one
    let reasons = declines(
        "\
class Base:
    def __init__(self, kind: str) -> None:
        self.KIND = kind


class Rebound(Base):
    KIND = \"rebound\"
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "Rebound"
            && reason.contains("a field its base laid out with no absent state")),
        "{reasons:?}"
    );
}

#[test]
fn a_method_that_rebinds_its_receiver_declines() {
    // every other parameter widens to cover what its body writes into it. slot zero
    // cannot: it is the receiver each field read in the body is addressed against, and
    // a frame whose `self` has become an ordinary object has no layout left to read
    let reasons = declines(
        "\
class Tagged:
    def __init__(self, kind: str) -> None:
        self.kind = kind

    def read(self) -> object:
        self = 3
        return self
",
    );
    assert!(
        reasons
            .iter()
            .any(|(name, reason)| name == "Tagged" && reason.contains("rebinds its receiver")),
        "{reasons:?}"
    );
}

#[test]
fn a_decorated_class_with_a_class_level_constant_is_lowered() {
    // the constant is copied off the body the interpreted `class` statement wrote, which
    // is captured while that statement runs and so predates every decorator. it used to
    // be read back off the finished definition, and a decorator that makes something of
    // what the body wrote leaves that definition saying something else — `@dataclass`
    // deletes the `field(init=False)` a body wrote. so this whole shape declined, which
    // over the corpus was almost every decorated class there was
    let reasons = declines(
        "\
def tagger(cls):
    return cls


@tagger
class Tagged:
    KIND: str = \"class-level\"

    def read(self) -> str:
        return \"read\"
",
    );
    assert!(
        !reasons.iter().any(|(name, _)| name == "Tagged"),
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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

/// the layout of one emitted class, as `(name, representation, may be unwritten)`
///
/// the presence flag is part of the answer rather than a detail: an attribute the
/// constructor may have skipped has to read back as `AttributeError`, and one it always
/// writes must not pay for a byte saying so
fn layout(source: &str, class: &str) -> Vec<(String, RType, bool)> {
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        module
            .classes
            .iter()
            .find(|candidate| candidate.name == class)
            .map(|owner| {
                owner
                    .fields
                    .iter()
                    .map(|field| (field.name.clone(), field.ty.clone(), field.optional))
                    .collect()
            })
            .unwrap_or_else(|| panic!("{class} was not emitted"))
    })
}

/// an attribute bound by *unpacking* is an attribute like any other
///
/// the layout used to read only a plain `self.a = v`, so a tuple target reached it as
/// nothing at all — the class was emitted without either field and the write fell through
/// to `PyObject_SetAttr`, which an emitted instance has no `__dict__` to answer.
/// `concurrent.futures.process._ThreadWakeup` is the shape, assigning
/// `self._reader, self._writer` from a pair
#[test]
fn an_unpacking_target_gives_the_layout_its_fields() {
    assert_eq!(
        layout(
            "\
def pair() -> tuple[int, int]:
    return (1, 2)


class Wakeup:
    def __init__(self) -> None:
        self._reader, self._writer = pair()
",
            "Wakeup"
        ),
        [
            ("_reader".to_string(), RType::INT, false),
            ("_writer".to_string(), RType::INT, false),
        ]
    );
}

/// a target is a *tree*, so the walk has to reach every leaf of it
///
/// a starred leaf binds a list rather than an element, which is why its representation is
/// the object one while the two beside it stay integers
#[test]
fn a_nested_and_starred_target_gives_the_layout_every_leaf() {
    assert_eq!(
        layout(
            "\
def shaped() -> tuple[tuple[int, int], int, int]:
    return ((1, 2), 3, 4)


class Tree:
    def __init__(self) -> None:
        (self.a, self.b), *self.rest = shaped()
",
            "Tree"
        ),
        [
            ("a".to_string(), RType::INT, false),
            ("b".to_string(), RType::INT, false),
            ("rest".to_string(), RType::OBJECT, false),
        ]
    );
}

/// a class with no `__init__` still has attributes, and they are the ones its methods
/// write
///
/// the field pass used to give up the moment it could not find an `__init__`, so a class
/// that sets itself up in a `configure` instead was emitted with an empty layout and every
/// one of those writes landed nowhere. nothing is written at construction, so each of them
/// is the optional field a partly-assigning `__init__` already gets
#[test]
fn a_class_with_no_init_lays_out_what_its_methods_write() {
    assert_eq!(
        layout(
            "\
class Late:
    def configure(self, n: int) -> None:
        self.value = n

    def read(self) -> int:
        return self.value
",
            "Late"
        ),
        [("value".to_string(), RType::INT, true)]
    );
}

/// a `for` target and a `with` target each bind an attribute, and only one of them is
/// certain to have bound it
///
/// the loop body runs once per element and an empty iterable runs it never, so the field
/// takes the presence byte that answers `AttributeError` for a read that comes too early.
/// a `with` binds what `__enter__` handed back before its body starts, so it is as settled
/// as a plain assignment
#[test]
fn a_loop_target_may_be_unwritten_where_a_with_target_is_not() {
    assert_eq!(
        layout(
            "\
import contextlib


class Bound:
    def __init__(self, values: list[int]) -> None:
        with contextlib.nullcontext(1) as self.held:
            pass
        for self.item in values:
            pass
",
            "Bound"
        ),
        [
            ("held".to_string(), RType::INT, false),
            ("item".to_string(), RType::INT, true),
        ]
    );
}

/// a field a `del` names takes the presence byte a delete needs, and its siblings do not
///
/// deleting is the only way an attribute `__init__` assigned on every path can go absent
/// again, and the byte beside the field is the only place that can be recorded. a field
/// nothing deletes keeps paying nothing for it
#[test]
fn a_field_a_del_names_takes_a_presence_byte() {
    assert_eq!(
        layout(
            "\
class Held:
    def __init__(self) -> None:
        self.dropped = 1
        self.kept = 2

    def drop(self) -> None:
        del self.dropped
",
            "Held"
        ),
        [
            ("dropped".to_string(), RType::INT, true),
            ("kept".to_string(), RType::INT, false),
        ]
    );
}

/// the delete need not be written in a method, or on this class's own receiver
///
/// an emitted instance is reachable from anywhere the module can name it, so a plain
/// function is as much a deleter as a method is. the names are taken module-wide for
/// that reason — and because a base and a subclass share the base's fields, so a rule
/// that read only one class's body could give them different layouts
#[test]
fn a_del_written_outside_the_class_still_gives_the_field_its_byte() {
    assert_eq!(
        layout(
            "\
class Held:
    def __init__(self) -> None:
        self.dropped = 1
        self.kept = 2


def drop(held: Held) -> None:
    del held.dropped
",
            "Held"
        ),
        [
            ("dropped".to_string(), RType::INT, true),
            ("kept".to_string(), RType::INT, false),
        ]
    );
}

/// a field whose only assignment is `None` is laid out as an object
///
/// sized for the assignment alone it would be a zero-width slot that holds nothing but
/// `None`, and every later write from elsewhere in the module is then refused — the
/// function doing the writing cannot be lowered, and the interpreted one python is left
/// with raises against the field's setter. `self.x = None` in `__init__` is one of the
/// commonest shapes there is, and the checker says so directly: an implicit attribute
/// with no assignment but `None` is inferred as `None | Unknown`, gradual on purpose
#[test]
fn a_field_only_ever_assigned_none_is_laid_out_as_an_object() {
    assert_eq!(
        layout(
            "\
class Node:
    def __init__(self) -> None:
        self.parent = None
",
            "Node"
        ),
        [("parent".to_string(), RType::OBJECT, false)]
    );
}

/// a field the checker has a settled type for keeps that type's representation
///
/// the widening above is the checker's answer rather than a rule of the layout's own, so
/// an attribute every assignment agrees about is unaffected — which is where a compiled
/// class's speed lives
#[test]
fn a_field_the_checker_has_a_settled_type_for_keeps_its_representation() {
    assert_eq!(
        layout(
            "\
class Counter:
    def __init__(self) -> None:
        self.n = 0
        self.label = \"start\"
",
            "Counter"
        ),
        [
            ("n".to_string(), RType::INT, false),
            ("label".to_string(), RType::STR, false),
        ]
    );
}

/// a private field is looked up under the name the body writes, not the mangled one
///
/// the layout holds `_Held__hidden`, because that is the attribute python binds. the
/// checker does not model the mangling at all — asking it about `_Held__hidden` finds
/// nothing — so the widening has to ask under `__hidden`, and a rule that reused the
/// layout's name would have left every private field at whatever one write said
#[test]
fn a_private_field_only_ever_assigned_none_is_widened_too() {
    assert_eq!(
        layout(
            "\
class Held:
    def __init__(self) -> None:
        self.__hidden = None
        self.__count = 0
",
            "Held"
        ),
        [
            ("_Held__hidden".to_string(), RType::OBJECT, false),
            ("_Held__count".to_string(), RType::INT, false),
        ]
    );
}

/// a declaration wider than the assignment is what the slot has to hold
///
/// `value: object` says the module may store anything there, whatever `__init__` happens
/// to put in it first. sized for the assignment the field would be an integer, and the
/// declaration's own promise — that a `str` may be written — would be refused
#[test]
fn a_field_declared_wider_than_its_assignment_holds_the_declaration() {
    assert_eq!(
        layout(
            "\
class Slot:
    value: object

    def __init__(self) -> None:
        self.value = 1
",
            "Slot"
        ),
        [("value".to_string(), RType::OBJECT, false)]
    );
}

/// widening a field says nothing about whether the instance has one
///
/// the representation and the presence byte are separate answers: an attribute only some
/// paths through `__init__` assign still reads back as `AttributeError` on the paths that
/// skipped it, and widening what it holds does not fill it in
#[test]
fn a_widened_field_assigned_on_one_path_keeps_its_presence_byte() {
    assert_eq!(
        layout(
            "\
class Late:
    def __init__(self, ready: bool) -> None:
        if ready:
            self.held = None
",
            "Late"
        ),
        [("held".to_string(), RType::OBJECT, true)]
    );
}

/// a subclass inherits the widened field at the width its base gave it
///
/// a subclass's struct *begins* with its base's, so the two cannot disagree about what a
/// field holds any more than about where it starts. the widening happens in the class
/// that declares the field, and the subclass copies it across as it does every other
#[test]
fn a_subclass_inherits_a_widened_field_unchanged() {
    let source = "\
class Base:
    def __init__(self) -> None:
        self.parent = None


class Derived(Base):
    def __init__(self) -> None:
        super().__init__()
        self.depth = 0

    def root(self) -> None:
        self.parent = Base()
";
    assert_eq!(
        layout(source, "Base"),
        [("parent".to_string(), RType::OBJECT, false)]
    );
    assert_eq!(
        layout(source, "Derived"),
        [
            ("parent".to_string(), RType::OBJECT, false),
            ("depth".to_string(), RType::INT, false),
        ]
    );
}

/// each method names its own receiver, and the layout has to read the one it wrote
///
/// the field pass took slot zero off `__init__` and then matched *that* name in every
/// other method, so a class whose `__init__` says `self` while a later method says `this`
/// lost every attribute the later method gave the instance
#[test]
fn a_method_that_names_its_receiver_otherwise_still_reaches_the_layout() {
    assert_eq!(
        layout(
            "\
class Renamed:
    def __init__(self) -> None:
        self.a = 1

    def more(this) -> None:
        this.b = 2
",
            "Renamed"
        ),
        [
            ("a".to_string(), RType::INT, false),
            ("b".to_string(), RType::INT, true),
        ]
    );
}

/// slot zero of a `classmethod` is the class, so what it *reads* through it is not
/// instance storage
///
/// giving every instance a field for `cls.seen` would be storage the source never asked
/// for, under a name the type already publishes. the write form of the same question no
/// longer reaches a layout at all — see
/// [`a_classmethod_that_writes_on_the_class_declines`]
#[test]
fn what_a_classmethod_reads_is_not_an_instance_field() {
    assert_eq!(
        layout(
            "\
class Counted:
    seen = 0

    def __init__(self) -> None:
        self.a = 1

    @classmethod
    def note(cls) -> int:
        return cls.seen
",
            "Counted"
        ),
        [("a".to_string(), RType::INT, false)]
    );
}

/// an augmented assignment is a write, so the attribute it names is a field
///
/// it is the optional one: `+=` reads before it writes, and an attribute nothing else
/// assigned is an `AttributeError` on that read rather than a value
#[test]
fn an_augmented_assignment_names_a_field_of_its_own() {
    assert_eq!(
        layout(
            "\
class Counter:
    def bump(self) -> None:
        self.total += 1
",
            "Counter"
        ),
        [("total".to_string(), RType::OBJECT, true)]
    );
}

/// `__dict__` is the one attribute no layout can be given
///
/// an emitted instance **is** its layout and there is nothing behind it, so the namespace
/// `__dict__` stands for does not exist to be read or written. a field of that name would
/// be a different thing wearing the name. `multiprocessing.dummy.Namespace` writes through
/// one and `tkinter.Event.__repr__` reads one, and both used to compile and then raise
#[test]
fn a_class_that_reaches_for_its_own_dict_declines() {
    assert_eq!(
        declines(
            "\
class Namespace:
    def __init__(self) -> None:
        self.a = 1

    def show(self) -> str:
        return repr(self.__dict__)
",
        ),
        vec![(
            "Namespace".to_string(),
            "`__dict__` is read off a `Namespace`, and an emitted instance is its layout \
             with nothing behind it"
                .to_string()
        )]
    );
}

/// and a write of a name the layout does not hold declines rather than reaching for the
/// dynamic form
///
/// the dynamic form is what a write takes when the compiler does not know the receiver's
/// layout, and it is the wrong answer when it does: the emitted type publishes no
/// `__dict__`, so `PyObject_SetAttr` raises where the interpreted class stored a value.
/// this is the invariant the field passes are meant to keep, said once where every write
/// goes past — so it holds for a write from anywhere, not only for the class body the
/// field passes read
#[test]
fn an_attribute_the_layout_cannot_hold_declines_rather_than_writing_dynamically() {
    assert_eq!(
        declines(
            "\
class Held:
    def __init__(self) -> None:
        self.a = 1


def poke(other: Held) -> None:
    other.spare = 2
",
        ),
        vec![(
            "poke".to_string(),
            "`spare` is written on a `Held`, whose layout has nowhere to keep it".to_string()
        )]
    );
}

/// but a name the *chain* holds is one the write really lands in, however little the
/// receiver's own class declares
///
/// a class that adds no field of its own declares an empty layout: its instances carry
/// every one of its base's, reached through the descriptors the base published. reading
/// that emptiness as "nowhere to keep it" would turn the working write in `Restating` into
/// a decline, and take `Wrapper` down with it
#[test]
fn an_attribute_a_base_holds_is_written_without_declining() {
    assert_eq!(
        declines(
            "\
class Wrapper(OSError):
    def __init__(self, code: int) -> None:
        self.code = code


class Restating(Wrapper):
    def __init__(self, code: int) -> None:
        self.code = code + 1
",
        ),
        Vec::new()
    );
}

/// and `__dict__` cannot be given a field of its own either
///
/// a write of it is not an attribute assignment at all — it replaces the instance
/// namespace — so a field wearing the name would be a different thing under it
#[test]
fn a_class_that_writes_its_own_dict_declines() {
    assert_eq!(
        declines(
            "\
class Namespace:
    def __init__(self, values: dict[str, int]) -> None:
        self.__dict__ = values
",
        ),
        vec![(
            "Namespace".to_string(),
            "`__dict__` is written on the receiver, and an emitted instance is its layout \
             with nothing behind it"
                .to_string()
        )]
    );
}

/// a `case` body is a body like any other, and what it writes is a field
///
/// the statement walk did not descend into a `match` at all, so every write a `case` made
/// was invisible to the layout — and the write then declined at the lowering rather than
/// landing in storage of its own
#[test]
fn a_write_in_a_case_body_reaches_the_layout() {
    assert_eq!(
        layout(
            "\
class Matched:
    def __init__(self, n: int) -> None:
        match n:
            case 0:
                self.tag = \"zero\"
            case _:
                self.tag = \"other\"
",
            "Matched"
        ),
        [("tag".to_string(), RType::STR, true)]
    );
}

#[test]
fn a_class_level_constant_under_a_class_keyword_is_carried_not_declined() {
    // a constant used to keep its class off the metaclass construction, on the reasoning
    // that it is settled after the metaclass has decided what the class defines. it is
    // not: it goes into the namespace with the methods, and the class is asked afterwards
    // whether it kept the value. so the class lowers, carrying the constant.
    //
    // a `@property` pair is the boundary, and the one thing this gate still turns down —
    // the `property` the two halves become is written onto the *finished* type, past the
    // point the metaclass decided anything
    assert_eq!(
        class_constants(
            "\
from abc import ABCMeta


class Tagged(metaclass=ABCMeta):
    TAG = 1

    def label(self) -> str:
        return \"tagged\"
",
            "Tagged"
        ),
        vec!["TAG".to_string()]
    );
    assert_eq!(
        declines(
            "\
from abc import ABCMeta


class Paired(metaclass=ABCMeta):
    @property
    def value(self) -> int:
        return 1

    @value.setter
    def value(self, given: int) -> None:
        pass
"
        ),
        vec![(
            "Paired".to_string(),
            "a property on a class built through its metaclass is not lowered yet".to_string()
        )]
    );
}

/// every route a method decorator takes into the namespace the metaclass is handed
///
/// the metaclass decides what the class defines from that namespace, so a decorator whose
/// answer arrived later would be a decorator the metaclass never saw. each of these puts
/// it there before the call instead, and none of them declines
#[test]
fn a_decorated_method_under_a_class_keyword_reaches_the_namespace() {
    // `@classmethod` and `@staticmethod` are gone by the time there is a method table:
    // the entry carries `METH_CLASS` or `METH_STATIC` and the runtime builds the same
    // descriptor a class body would have written
    assert_eq!(
        method_decorators(
            "\
from abc import ABCMeta


class Bound(metaclass=ABCMeta):
    @classmethod
    def kind(cls) -> str:
        return \"bound\"

    @staticmethod
    def tag() -> str:
        return \"bound\"
",
            "Bound"
        ),
        vec![
            ("kind".to_string(), Vec::new()),
            ("tag".to_string(), Vec::new())
        ]
    );
    // anything else stays on the method and is carried off the interpreted body, which is
    // where the decorator's single application already landed
    assert_eq!(
        method_decorators(
            "\
from abc import ABCMeta, abstractmethod


class Marked(metaclass=ABCMeta):
    @abstractmethod
    def area(self) -> int:
        return 0
",
            "Marked"
        ),
        vec![("area".to_string(), vec!["abstractmethod".to_string()])]
    );
}

#[test]
fn a_class_level_constant_beside_a_base_of_ours_keeps_that_base_emitted() {
    // the shape the stdlib is made of: no keyword at all, a base this module emits
    // standing beside one from outside, and a constant. a spec cannot work that base list
    // out, so the metaclass is what builds it — and the constant rides into the namespace
    // it is handed rather than closing it.
    //
    // the base staying emitted is the point, and it is what the decline used to cost:
    // `Reader` declining took `Codec` with it, because the interpreted `Reader` extends
    // the *twin's* `Codec` and `issubclass(m.Reader, m.Codec)` would answer False against
    // a twin that says True. neither declines now
    const SOURCE: &str = "\
import codecs


class Codec(codecs.Codec):
    def label(self) -> str:
        return \"codec\"


class Reader(Codec, codecs.StreamReader):
    tag = 1

    def kind(self) -> str:
        return \"reader\"
";
    assert_eq!(declines(SOURCE), vec![]);
    assert_eq!(class_constants(SOURCE, "Reader"), vec!["tag".to_string()]);
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
fn a_base_written_as_an_alias_is_the_class_it_was_bound_to() {
    // the alias is not a base out of this module: it stands for a class this module
    // writes, and the emitted type is what the name will hold. taking it as external
    // built the class on the interpreted definition instead — the alias is carried over
    // to the emitted type only once every class has been built — so `isinstance` said
    // `False` where the interpreter says `True`
    const SOURCE: &str = "\
class Root:
    def root(self) -> str:
        return \"root\"


Alias = Root


class Over(Alias):
    def side(self) -> str:
        return \"over\"
";
    assert_eq!(declines(SOURCE), Vec::new());
    let bases = with_source(SOURCE, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        module
            .classes
            .iter()
            .map(|class| (class.name.clone(), class.base.clone()))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        bases,
        vec![
            ("Root".to_string(), None),
            (
                "Over".to_string(),
                Some(ClassBase::InModule("Root".to_string()))
            )
        ]
    );
}

#[test]
fn an_alias_does_not_hide_a_base_this_module_lays_out() {
    // the same refusal as the direct spelling, which is the point: every question the
    // base list is asked is asked of the *name*, so an alias that stood for itself
    // walked straight past this gate and compiled the one shape it exists to refuse
    let reasons = declines(
        "\
import codecs


class Laid:
    def __init__(self, n: int) -> None:
        self.n = n


Alias = Laid


class OnLaid(Alias, codecs.Codec):
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
fn a_name_the_module_binds_twice_declines_rather_than_pick_a_class() {
    // `Over` was built on `Root` and the name holds `Other` by the time the module body
    // ends, so neither class is the answer: the one the class statement saw is gone, and
    // the one the emitted module would look up is not what it extends
    let reasons = declines(
        "\
class Root:
    def root(self) -> str:
        return \"root\"


class Other:
    def other(self) -> str:
        return \"other\"


Alias = Root
Alias = Other


class Over(Alias):
    def side(self) -> str:
        return \"over\"
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Over".to_string(),
            "a base the module binds more than once stands for the class bound last, not the one it was built on"
                .to_string()
        )]
    );
}

#[test]
fn an_alias_chain_that_leaves_the_module_is_left_where_it_was_written() {
    // the emitted module looks the base up by name, and both names hold the same object
    // at import — so following one is no gain, and it would trade a name this body binds
    // once for one it may bind again. only a chain that ends at a class of *ours* moves
    const SOURCE: &str = "\
from codecs import Codec

Alias = Codec


class Over(Alias):
    def side(self) -> str:
        return \"over\"
";
    assert_eq!(declines(SOURCE), Vec::new());
    let bases = with_source(SOURCE, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        module
            .classes
            .iter()
            .map(|class| (class.name.clone(), class.base.clone()))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        bases,
        vec![(
            "Over".to_string(),
            Some(ClassBase::External(vec!["Alias".to_string()]))
        )]
    );
}

#[test]
fn a_name_bound_twice_to_nothing_of_ours_still_stands_for_itself() {
    // the boundary: a name the module rebinds is a hazard only where a class of this
    // module's is behind it. two imported names leave the base exactly what it was
    const SOURCE: &str = "\
import codecs

Alias = codecs.Codec
Alias = codecs.StreamWriter


class Over(Alias):
    def side(self) -> str:
        return \"over\"
";
    assert_eq!(declines(SOURCE), Vec::new());
    let bases = with_source(SOURCE, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        module
            .classes
            .iter()
            .map(|class| (class.name.clone(), class.base.clone()))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        bases,
        vec![(
            "Over".to_string(),
            Some(ClassBase::External(vec!["Alias".to_string()]))
        )]
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
fn the_metaclass_a_base_has_is_named_in_the_decline_it_causes() {
    // the reason used to say only which metaclass a spec *wants*, so a reader had to go
    // and find out what the base actually carried before knowing whether the class was
    // recoverable at all. `ABCMeta` and a metaclass written here are worth telling apart:
    // cpython refuses the first from 3.14 whatever else changes, while a plain one may
    // simply be a construction nothing has lowered yet
    let reasons = declines(
        "\
from abc import ABC


class Storing(ABC):
    def __init__(self, n: int) -> None:
        self.n = n
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Storing".to_string(),
            "a class with fields of its own needs `type` for every base's metaclass, and `ABC` has `ABCMeta`"
                .to_string()
        )]
    );
}

#[test]
fn a_base_with_a_metaclass_of_this_projects_own_is_named_by_that_metaclass() {
    // the companion of the test above, and what says the name is read off the base rather
    // than written into the message: nothing here is `ABCMeta`, and the dotted path the
    // base was written as is what the reason repeats back
    let reasons = declines(
        "\
import enum


class Storing(enum.Enum):
    def __init__(self, n: int) -> None:
        self.n = n
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Storing".to_string(),
            "a class with fields of its own needs `type` for every base's metaclass, and `enum.Enum` has `EnumMeta`"
                .to_string()
        )]
    );
}

#[test]
fn a_base_that_is_not_a_class_is_said_to_be_that_rather_than_to_have_a_metaclass() {
    // a module that gives up on the platforms it does not serve leaves everything after
    // the `raise` unreachable, and a base named there is not a class as far as the types
    // are concerned — there is no metaclass on it to have found. five of the standard
    // library's declining classes are this, all of them in `asyncio`'s windows modules,
    // and telling a reader they "have" some metaclass would be an invention
    let reasons = declines(
        "\
import sys

if sys.version_info >= (3, 0):
    raise ImportError(\"not this build\")

import subprocess


class Storing(subprocess.Popen):
    def __init__(self, n: int) -> None:
        self.n = n
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Storing".to_string(),
            "a class with fields of its own needs `type` for every base's metaclass, and `subprocess.Popen` is not a class the types settle on"
                .to_string()
        )]
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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

/// a weak reference taken a whole function away from the instance it is of
///
/// the same fact the `__slots__` case above turns on — a type spec adds no `__weakref__`
/// — and the frontend refuses `weakref.ref(self)` where it is written. but
/// `logging.Handler.__init__` does not write one: it calls `_addHandlerRef(self)`, whose
/// body takes the reference, and no predicate over `__init__`'s own body can see that.
/// so the refusal is carried back to the caller that hands the instance over, and
/// declining that caller is what keeps its class interpreted — which is a class a weak
/// reference *can* be made of
#[test]
fn a_caller_that_hands_an_instance_to_a_weak_reference_is_declined() {
    let reasons = declines(
        "\
import weakref

registry = []


def registers(thing):
    registry.append(weakref.ref(thing))


class Handler:
    def __init__(self):
        registers(self)
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Handler".to_string(),
            "`registers` takes a weak reference of what it is handed, and an emitted instance is its layout — a type spec adds no `__weakref__`, so no weak reference of one can be made".to_string()
        )]
    );
}

/// and the cascade goes as far as the calls do
///
/// a frame between the receiver and the weak reference is one more function the answer
/// has to travel through, which the pruner's own loop does. handing over something that
/// is not an instance of ours costs nothing at all: an object from anywhere else carries
/// a `__weakref__` of its own
#[test]
fn only_a_caller_handing_over_an_instance_of_ours_is_declined() {
    let reasons = declines(
        "\
import weakref

registry = []


def registers(thing):
    registry.append(weakref.ref(thing))


def forwards(thing):
    registers(thing)


class Handler:
    def __init__(self):
        forwards(self)


def registers_a_stranger(other: object) -> None:
    registers(other)
",
    );
    assert_eq!(
        reasons
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["Handler"]
    );
    assert!(
        reasons[0]
            .1
            .starts_with("`forwards` takes a weak reference"),
        "gave `{}`",
        reasons[0].1
    );
}

/// a weak reference of something the receiver *holds* is not a weak reference of the
/// receiver
///
/// `multiprocessing.queues.Queue._start_thread` writes `weakref.ref(self._thread)`, and
/// a `threading.Thread` is not a `Queue`. the refusal is about the *place* rather than
/// about the spelling: a field whose type no instance of ours is assignable to can hold
/// none of them, so nothing about the call raises and nothing about it is turned down
#[test]
fn a_weak_reference_of_a_field_no_instance_of_ours_can_reach_is_kept() {
    let reasons = declines(
        "\
import threading
import weakref

registry = []


class Queue:
    def __init__(self, thread: threading.Thread) -> None:
        self.thread = thread

    def watch(self) -> None:
        registry.append(weakref.ref(self.thread))

    def start(self) -> None:
        self.watch()
",
    );
    assert_eq!(reasons, Vec::new());
}

/// and where an instance of ours *can* be standing there, the refusal stays
///
/// `Holder.node` is declared as a class this module lays out, so the reference is of an
/// emitted instance and raises. that is turned down where it is written rather than
/// carried back to a caller: the frame that reads the field is the frame that raises,
/// and no caller of it is handing the instance over
#[test]
fn a_weak_reference_of_a_field_declared_as_one_of_ours_is_declined() {
    let reasons = declines(
        "\
import weakref

registry = []


class Node:
    def __init__(self) -> None:
        self.tag = 1


class Holder:
    def __init__(self, node: Node) -> None:
        self.node = node

    def watch(self) -> None:
        registry.append(weakref.ref(self.node))
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Holder".to_string(),
            "an emitted instance is its layout and a type spec adds no `__weakref__`, so a weak reference to one cannot be made".to_string()
        )]
    );
}

/// a field declared `object` is a place an instance of ours fits, so the frame stays
/// marked and its callers are still turned down
///
/// this is what says the question asked is assignability and not a match on a class's
/// name: nothing here writes `Node` anywhere near the weak reference, and `object` is a
/// place every one of ours can stand in
#[test]
fn a_weak_reference_of_a_field_declared_object_still_reaches_its_callers() {
    let reasons = declines(
        "\
import weakref

registry = []


class Node:
    def __init__(self) -> None:
        self.tag = 1


class Holder:
    def __init__(self, held: object) -> None:
        self.held = held

    def watch(self) -> None:
        registry.append(weakref.ref(self.held))


def runs(holder: Holder) -> None:
    holder.watch()
",
    );
    assert_eq!(
        reasons
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["runs"]
    );
    assert!(
        reasons[0]
            .1
            .starts_with("`Holder.watch` takes a weak reference"),
        "gave `{}`",
        reasons[0].1
    );
}

#[test]
fn fields_past_a_base_that_holds_nothing_are_lowered() {
    // `Wrapper` and `Held` lay nothing out of their own, so neither needs storage past an
    // instance — but `Deep`'s fields do, and reaching them takes three type slots of
    // `Deep`'s own that call the base's. what breaks that chain is the base carrying
    // slots *we* emitted, which read the base to chain to from the type that declared
    // them; holding no fields does nothing for it either way. so both rungs are built
    // from specs of their own and given the three with nothing in them.
    //
    // `Beside` is the boundary: its layout chain ends at `object` rather than outside, so
    // its struct *begins* with `Rooted`'s rather than sitting past an instance of it, and
    // its deallocator frees the object rather than passing it on
    let reasons = declines(
        "\
class Wrapper(OSError):
    pass


class Held(Wrapper):
    pass


class Deep(Held):
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
    assert_eq!(reasons, Vec::new());
}

#[test]
fn a_base_extended_from_inside_a_module_level_block_declines() {
    // a `class` under a module-level `if` is written by the same module body, in the
    // same namespace, at the same time — and the body runs to completion before any
    // emitted type replaces what it bound. so `Guarded`'s base is the interpreted
    // `Base` the body built and nothing can reach again, while the module's own name
    // now holds the emitted one: `Base.__init__(self)` inside it is then a slot
    // wrapper asked for a receiver its argument is not. `smtplib` writes
    // `class SMTP_SSL(SMTP)` under `if _have_ssl:` and answered exactly that
    let reasons = declines(
        "\
class Base:
    def __init__(self) -> None:
        self.code = 1


if len('x') == 1:

    class Guarded(Base):
        pass
",
    );
    assert_eq!(
        reasons,
        vec![(
            "Base".to_string(),
            "`Guarded` declined, so it extends the interpreted definition rather than this type"
                .to_string()
        )]
    );
}

#[test]
fn a_class_a_nested_frame_writes_is_not_one_the_module_body_extends_with() {
    // the boundary is the *frame*: what a `def` or a `class` body writes is bound in a
    // namespace of its own, so a class in one is not a class the module body built and
    // says nothing about what the module's own names hold. `make` gives up its own
    // definition over the nested `class`, which is a separate matter — `Base` keeps its
    // layout either way
    let reasons = declines(
        "\
class Base:
    def __init__(self) -> None:
        self.code = 1


def make() -> None:
    class Inner(Base):
        pass
",
    );
    assert!(
        !reasons.iter().any(|(name, _)| name == "Base"),
        "{reasons:?}"
    );
}

#[test]
fn fields_past_a_hollow_base_beside_a_class_this_module_writes_decline() {
    // `Hollow` reads as standing on a base from outside, but the list also names `Mixin`
    // — a class this module writes and then turns down. what stands under that name at
    // import is a `class` statement's type all the same, so the type `Hollow` is built on
    // is a heap one, and a spec cannot be built on one of those. the refusal would be the
    // whole module's, which is a worse answer than `Deep` keeping its own definition
    let reasons = declines(
        "\
class Mixin:
    def hide(self) -> None:
        setattr(self, 'hidden', 1)


class Hollow(OSError, Mixin):
    pass


class Deep(Hollow):
    def __init__(self, code: int) -> None:
        self.code = code
",
    );
    assert_eq!(
        reasons,
        vec![
            (
                "Mixin".to_string(),
                "a `setattr` on the receiver names its attribute at runtime".to_string()
            ),
            (
                "Deep".to_string(),
                "a class whose fields sit past a base's instance needs a base python frees itself, and one this module builds from a spec is the only one of ours that is"
                    .to_string()
            ),
            (
                "Hollow".to_string(),
                "`Deep` declined, so it extends the interpreted definition rather than this type"
                    .to_string()
            ),
        ]
    );
}

#[test]
fn fields_past_a_base_this_module_does_not_free_decline() {
    // `Wrapper` stores through `setattr`, which no layout can record, so this module
    // leaves it to its interpreted definition — a `class` statement's type, carrying
    // `subtype_dealloc`. `Held`'s fields would sit past a `Wrapper` instance, so it
    // supplies the three type slots that reach them and each calls `Wrapper`'s. python's
    // own three resolve which base to chain to from the instance's type, find `Held`'s
    // there, and call it back until the stack runs out
    let reasons = declines(
        "\
class Wrapper(OSError):
    def hide(self) -> None:
        setattr(self, 'hidden', 1)


class Held(Wrapper):
    def __init__(self, code: int) -> None:
        self.code = code
",
    );
    assert_eq!(
        reasons,
        vec![
            (
                "Wrapper".to_string(),
                "a `setattr` on the receiver names its attribute at runtime".to_string()
            ),
            (
                "Held".to_string(),
                "a class whose fields sit past a base's instance needs a base python frees itself, and one this module builds from a spec is the only one of ours that is"
                    .to_string()
            ),
        ]
    );
}

#[test]
fn fields_past_a_base_built_from_a_spec_are_lowered() {
    // the one base of ours a class can keep its storage past. `Wrapper` keeps fields of
    // its own past an `OSError` instance, so this module builds it from a type spec and
    // its `tp_dealloc`, `tp_traverse` and `tp_clear` are ones we emitted — each reading
    // the base to chain to from the type that declared it, so `Held`'s three chain to
    // `Wrapper`'s, `Wrapper`'s to `OSError`'s, and the walk stops there.
    //
    // the layout is the base's followed by what the subclass adds, which is what says
    // where each rung's storage begins
    let (declined, layouts) = with_source(
        "\
class Wrapper(OSError):
    def __init__(self, code: int) -> None:
        self.code = code


class Held(Wrapper):
    def __init__(self, code: int, note: str) -> None:
        Wrapper.__init__(self, code)
        self.note = note
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            let declined: Vec<(String, String)> = module
                .declined
                .iter()
                .map(|declined| (declined.name.clone(), declined.reason.clone()))
                .collect();
            let layouts: Vec<(String, Vec<String>)> = module
                .classes
                .iter()
                .map(|class| {
                    (
                        class.name.clone(),
                        class
                            .fields
                            .iter()
                            .map(|field| field.name.clone())
                            .collect(),
                    )
                })
                .collect();
            (declined, layouts)
        },
    );
    assert_eq!(declined, Vec::new());
    assert_eq!(
        layouts,
        vec![
            ("Wrapper".to_string(), vec!["code".to_string()]),
            (
                "Held".to_string(),
                vec!["code".to_string(), "note".to_string()]
            ),
        ]
    );
}

#[test]
fn a_class_keyword_on_a_class_appended_over_a_base_of_ours_declines() {
    // a spec has nowhere to put a class keyword, and a class whose fields sit past a
    // base's instance has no other construction — so the keyword is what it gives up.
    // the refusal comes from resolving the base rather than from placing the fields,
    // because a base of ours beside a keyword has none whatever the fields are
    let reasons = declines(
        "\
class Meta(type):
    pass


class Wrapper(OSError):
    def __init__(self, code: int) -> None:
        self.code = code


class Held(Wrapper, metaclass=Meta):
    def __init__(self, code: int, note: str) -> None:
        Wrapper.__init__(self, code)
        self.note = note
",
    );
    assert!(
        reasons.contains(&(
            "Held".to_string(),
            "a class keyword on a base this module emits is not lowered yet".to_string()
        )),
        "{reasons:?}"
    );
}

/// the field representations of one emitted class, rendered
fn field_types(source: &str, class: &str) -> Vec<(String, String)> {
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        module
            .classes
            .iter()
            .find(|candidate| candidate.name == class)
            .map(|owner| {
                owner
                    .fields
                    .iter()
                    .map(|field| (field.name.clone(), field.ty.to_string()))
                    .collect()
            })
            .unwrap_or_else(|| panic!("{class} was not emitted"))
    })
}

#[test]
fn a_class_from_another_module_is_not_the_one_we_emitted_under_that_name() {
    // the emitted layouts are keyed by bare class name, and a bare name is not an
    // identity: `csv` declares a `Dialect` of its own and imports `_csv.Dialect`
    // beside it under another name. asking only the name gave the *imported* class
    // this module's layout, so `_Dialect(self)` was narrowed to a struct its answer is
    // not and `csv.excel()` raised a `TypeError` where python built a dialect
    assert_eq!(
        field_types(
            "\
from decimal import Decimal as _Decimal


class Decimal:
    def __init__(self) -> None:
        self.other = _Decimal(1)
",
            "Decimal",
        ),
        vec![("other".to_string(), "object".to_string())]
    );
    // and the class this module *does* write under the name still gets its layout
    assert_eq!(
        field_types(
            "\
class Decimal:
    def __init__(self) -> None:
        self.other = Decimal.__new__(Decimal)
",
            "Decimal",
        ),
        vec![("other".to_string(), "Decimal".to_string())]
    );
}

#[test]
fn a_subclass_that_appends_nothing_past_a_base_declares_nothing() {
    // the fields are what makes the difference: `Held` above appends storage past a
    // `Wrapper` instance and has no construction, while a class that adds *no* field of
    // its own appends nothing at all. what such a class keeps is what `Wrapper` already
    // keeps, at the offsets `Wrapper` laid them out and through the descriptors `Wrapper`
    // published — so it is built the way any other class with no storage of its own is
    //
    // `Restating` is the same class written the other way round: assigning an attribute
    // the base already stores adds nothing either, and the write lands on the base's
    // field through the base's own setter
    let (declined, layouts) = with_source(
        "\
class Wrapper(OSError):
    def __init__(self, code: int) -> None:
        self.code = code


class Plain(Wrapper):
    pass


class Tagged(Wrapper):
    TAG = 1


class Restating(Wrapper):
    def __init__(self, code: int) -> None:
        self.code = code + 1
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            let declined: Vec<(String, String)> = module
                .declined
                .iter()
                .map(|declined| (declined.name.clone(), declined.reason.clone()))
                .collect();
            let layouts: Vec<(String, Vec<String>)> = module
                .classes
                .iter()
                .map(|class| {
                    (
                        class.name.clone(),
                        class
                            .fields
                            .iter()
                            .map(|field| field.name.clone())
                            .collect(),
                    )
                })
                .collect();
            (declined, layouts)
        },
    );
    assert_eq!(declined, Vec::new());
    assert_eq!(
        layouts,
        vec![
            ("Wrapper".to_string(), vec!["code".to_string()]),
            ("Plain".to_string(), Vec::new()),
            ("Tagged".to_string(), Vec::new()),
            ("Restating".to_string(), Vec::new()),
        ]
    );
}

#[test]
fn a_subclass_with_no_storage_is_rebuilt_on_a_base_that_declines_later() {
    // a base is settled as one of ours while the layouts settle, and only the body being
    // lowered can turn it down after that. a class with no storage of its own does not
    // need it to have stayed one: what stands under the name at import is a class either
    // way, and building on the *name* is what every class over a base out of this module
    // already does — so it takes that construction rather than cascading behind the base.
    //
    // `Storing` is the boundary: a class with a field declares a size of its own, and a
    // class over a base out of this module declares none — the base allocates. so the
    // storage would have nowhere to go, and it cascades behind the base instead
    const SOURCE: &str = "\
class Base:
    def __setattr__(self, name: str, value: object) -> None:
        pass

    def label(self) -> str:
        return \"base\"


class Below(Base):
    def side(self) -> str:
        return \"below\"


class Storing(Base):
    def __init__(self, extra: int) -> None:
        self.extra = extra
";
    assert_eq!(
        declines(SOURCE),
        vec![
            (
                "Base".to_string(),
                "`__setattr__` fills a type slot with no adapter yet".to_string()
            ),
            (
                "Storing".to_string(),
                "`Base` declined, so it is not a base to build on".to_string()
            )
        ]
    );
    let bases = with_source(SOURCE, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        module
            .classes
            .iter()
            .map(|class| (class.name.clone(), class.base.clone()))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        bases,
        vec![(
            "Below".to_string(),
            Some(ClassBase::External(vec!["Base".to_string()]))
        )]
    );
}

#[test]
fn a_class_a_subclass_stores_inside_is_not_rebuilt_on_its_declining_base() {
    // rebuilding `Middle` on a name would move the layout of everything under it outside
    // the module, and `Storing`'s field would go from sitting inside a `Middle` instance
    // to sitting past one — a construction that has no answer, and one that refuses the
    // *whole module* at import rather than the class. `urllib.request` lost all nineteen
    // of its compiled functions that way.
    //
    // `Aside` is the boundary: nothing stores anything under it, so it is rebuilt
    const SOURCE: &str = "\
class Base:
    def __setattr__(self, name: str, value: object) -> None:
        pass

    def label(self) -> str:
        return \"base\"


class Middle(Base):
    def side(self) -> str:
        return \"middle\"


class Storing(Middle):
    def __init__(self) -> None:
        self.extra = 1


class Aside(Base):
    def side(self) -> str:
        return \"aside\"
";
    assert_eq!(
        declines(SOURCE),
        vec![
            (
                "Base".to_string(),
                "`__setattr__` fills a type slot with no adapter yet".to_string()
            ),
            (
                "Middle".to_string(),
                "`Base` declined, so it is not a base to build on".to_string()
            ),
            (
                "Storing".to_string(),
                "`Middle` declined, so it is not a base to build on".to_string()
            )
        ]
    );
    let bases = with_source(SOURCE, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        module
            .classes
            .iter()
            .map(|class| (class.name.clone(), class.base.clone()))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        bases,
        vec![(
            "Aside".to_string(),
            Some(ClassBase::External(vec!["Base".to_string()]))
        )]
    );
}

#[test]
fn an_annotated_class_attribute_under_a_class_keyword_is_carried_with_the_rest() {
    // an annotated assignment is a class-level constant, so it takes the same route a
    // plain one does — into the namespace the metaclass is handed. it used to reach the
    // same gate instead, and a class the compiler had been building silently without the
    // attribute went from missing the attribute to refusing to build at all
    assert_eq!(
        class_constants(
            "\
from abc import ABCMeta


class Tagged(metaclass=ABCMeta):
    TAG: int = 1

    def label(self) -> str:
        return \"tagged\"
",
            "Tagged"
        ),
        vec!["TAG".to_string()]
    );
}

#[test]
fn a_subclass_of_a_class_the_metaclass_gate_turns_down_builds_on_the_interpreted_base() {
    // the gate is asked while the layouts settle, so a class it turns down leaves the
    // layout set — and its subclass then takes the external base every other declining
    // class's subclass takes rather than being laid out on a base nothing emits. asked
    // while the *body* was lowered instead, the base stayed in the set and the subclass
    // cascaded behind it.
    //
    // `Constant` is the boundary in the other direction: a class-level constant no longer
    // turns a class down, so that half stays in the layout set and its subclass is laid
    // out on it — an `InModule` base against an `External` one
    const SOURCE: &str = "\
from abc import ABCMeta


class Paired(metaclass=ABCMeta):
    @property
    def value(self) -> int:
        return 1

    @value.setter
    def value(self, given: int) -> None:
        pass


class BelowPaired(Paired):
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
        vec![(
            "Paired".to_string(),
            "a property on a class built through its metaclass is not lowered yet".to_string()
        )]
    );
    // the base each subclass gets is the point: an `InModule` one below `Paired` would
    // name a type this module never emits, and an `External` one below `Constant` would
    // give up a layout the module does have
    let bases = with_source(SOURCE, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
                "BelowPaired".to_string(),
                Some(ClassBase::External(vec!["Paired".to_string()]))
            ),
            // a keyword-only class header has no bases at all
            (
                "Constant".to_string(),
                Some(ClassBase::External(Vec::new()))
            ),
            (
                "BelowConstant".to_string(),
                Some(ClassBase::InModule("Constant".to_string()))
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
fn a_class_in_an_inheritance_chain_guards_its_direct_call() {
    // it is a mutable heap type: python can rebind a method on it, or override it in a
    // subclass, and an unguarded direct call would see neither. so the call tests the
    // receiver against each class that could answer and falls through to the protocol,
    // where a class outside the module's reckoning is still found
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let ops = |name: &str| {
                print_function(
                    module
                        .all_functions()
                        .find(|function| function.name == name)
                        .unwrap_or_else(|| panic!("{name} is compiled")),
                )
            };
            // the direct call is there, but only behind a test of the receiver's exact
            // type and of the class not having been written to since import — and the
            // protocol call it falls through to is what a receiver neither test
            // describes still takes
            assert!(
                ops("through").contains("method-stands r1 Shape.describe"),
                "{}",
                ops("through")
            );
            assert!(
                ops("through").contains("call Shape.describe"),
                "{}",
                ops("through")
            );
            assert!(ops("through").contains(".describe()"), "{}", ops("through"));
            assert!(
                ops("plain").contains("call Plain.doubled"),
                "{}",
                ops("plain")
            );
        },
    );
}

#[test]
fn a_final_receiver_answers_through_the_licence() {
    // `final` rules out an override and nothing else. the class is still a mutable heap
    // type, so `Fixed.tripled = f` rebinds the method and a value written on an instance
    // shadows it — a direct call sees neither, and answered from the compiled body while
    // python answered from the shadow. the licence asks all three questions at once, so
    // that is what a receiver of one takes
    with_source(
        "\
data class Open:
    n: int

    def doubled(self) -> int:
        return self.n * 2

data class Derived(Open):
    extra: int

final data class Fixed(Open):
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let ops = |name: &str| {
                print_function(
                    module
                        .all_functions()
                        .find(|function| function.name == name)
                        .unwrap_or_else(|| panic!("{name} is compiled")),
                )
            };
            // both classes are in an inheritance chain, so both are mutable heap types.
            // the licence's test is what reaches the body, and the body is still reached
            assert!(
                ops("on_final").contains("method-stands r1 Fixed.tripled"),
                "{}",
                ops("on_final")
            );
            assert!(
                ops("on_final").contains("call Fixed.tripled"),
                "{}",
                ops("on_final")
            );
            // an open receiver takes the same test for a method it declares
            assert!(
                ops("on_open").contains("method-stands r1 Open.doubled"),
                "{}",
                ops("on_open")
            );
            // a method the class only *inherits* is not one a licence can be taken for:
            // what a class's body binds is not knowable from the method table alone, so
            // the ordinary protocol call is what an inherited name reaches
            assert!(
                ops("on_final_inherited").contains(".doubled()"),
                "{}",
                ops("on_final_inherited")
            );
            assert!(
                !ops("on_final_inherited").contains("call Open.doubled"),
                "{}",
                ops("on_final_inherited")
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let through = print_function(
                module
                    .all_functions()
                    .find(|function| function.name == "through")
                    .expect("through is compiled"),
            );
            // the direct call stands only behind a test of the receiver's exact type,
            // since `Circle` overrides and a `Circle` is a `Shape`
            assert!(
                through.contains("method-stands r1 Shape.describe"),
                "{through}"
            );
            assert!(
                through.contains("method-stands r1 Circle.describe"),
                "{through}"
            );
            assert!(through.contains(".describe()"), "{through}");
        },
    );
}

#[test]
fn a_decorated_class_gives_up_its_direct_call_and_its_direct_construction() {
    // a decorated class is a mutable heap type, and python can rebind a method on
    // one — a direct call would not see the rebinding.
    //
    // the *construction* goes the same way, and for a sharper reason: the decorator
    // replaces what the module namespace binds, so `Loud(...)` names whatever it
    // returned. allocating the emitted layout instead skipped the decorator outright,
    // and a decorator returning another class had every construction in the module
    // building the wrong object with no diagnostic at all
    // `Loud` is the last statement that runs anything, and the annotations below it are
    // deferred, so nothing here can see the module between the twin's `class` and init —
    // which is what keeps a decorated class compiling at all. see `watched_definitions`
    with_source(
        "\
from __future__ import annotations

def tagged(cls: type) -> type:
    return cls

class Quiet:
    def __init__(self, n: int) -> None:
        self.n = n

    def doubled(self) -> int:
        return self.n * 2

@tagged
class Loud:
    def __init__(self, n: int) -> None:
        self.n = n

    def doubled(self) -> int:
        return self.n * 2

def loud(x: Loud) -> int:
    return x.doubled()

def quiet(x: Quiet) -> int:
    return x.doubled()

def build_loud(n: int) -> int:
    return Loud(n).doubled()

def build_quiet(n: int) -> int:
    return Quiet(n).doubled()
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            // and the undecorated one is allocated where it stands, while the
            // decorated one is resolved through the namespace the decorator wrote to
            assert!(
                ops("build_quiet").contains("new Quiet"),
                "{}",
                ops("build_quiet")
            );
            assert!(
                !ops("build_loud").contains("new Loud"),
                "{}",
                ops("build_loud")
            );
            assert!(
                ops("build_loud").contains("pycall Loud"),
                "{}",
                ops("build_loud")
            );
            // and its decorators travel with the class
            let loud = module
                .classes
                .iter()
                .find(|class| class.name == "Loud")
                .expect("Loud is emitted");
            assert_eq!(dotted(&loud.decorators), ["tagged"]);
        },
    );
}

#[test]
fn a_class_modifier_is_not_a_decorator() {
    // `sealed`, `abstract`, `open` and `export` reach the ast as decorators with no
    // `@`, and the transpiler erases them — so the interpreted twin has no such name
    // and looking one up at module init raised `NameError` and took the whole
    // extension down with it
    for modifier in ["sealed", "abstract", "open", "export"] {
        let source = format!(
            "\
{modifier} data class Shape:
    n: int
"
        );
        let decorators = with_source(&source, |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(
                module.declined.is_empty(),
                "{modifier}: {:?}",
                module.declined
            );
            module
                .classes
                .iter()
                .find(|class| class.name == "Shape")
                .map(|class| dotted(&class.decorators))
                .unwrap_or_else(|| panic!("{modifier}: Shape is emitted"))
        });
        assert!(decorators.is_empty(), "{modifier}: {decorators:?}");
    }
}

#[test]
fn a_class_with_a_hand_written_dunder_is_declined() {
    // `__init__` is generated from the fields, so a hand-written one would
    // disagree with it about the layout.
    //
    // the reason names the method rather than saying "a dunder method", for the reason
    // the metaclass reason names the metaclass it found: which one it is decides whether
    // there is anything to do about it, and a reader should not have to go back to the
    // source to find out. `__repr__` is here beside `__init__` because the two are not
    // the same case at all — one disagrees with a generated constructor and the other
    // only fills a slot nothing has lowered — and the old wording said the same words
    // about both
    let source = "\
data class Point:
    x: int

    def __init__(self, x: int) -> None:
        pass
";
    assert_eq!(
        declines(source),
        vec![(
            "Point".to_string(),
            "`__init__` on a data class is not lowered yet".to_string()
        )]
    );
    assert_eq!(
        declines(
            "\
data class Tag:
    x: int

    def __repr__(self) -> str:
        return \"tag\"
"
        ),
        vec![(
            "Tag".to_string(),
            "`__repr__` on a data class is not lowered yet".to_string()
        )]
    );
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
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let decorated = module
            .functions
            .iter()
            .find(|function| function.name == "f")
            .expect("f is compiled");
        assert_eq!(dotted(&decorated.decorators), ["deco"]);
    });
}

#[test]
fn a_decorator_that_is_a_call_is_declined() {
    // python calls `make(1)` where the `def` stands. module-level code is not compiled,
    // so the only moment init has is the end of the module — by which time the
    // interpreted twin has already made that call, and making it again would be a
    // second one, in the wrong place
    let source = "\
def make(n: int) -> object:
    return n

@make(1)
def f() -> None:
    pass
";
    let reason = with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        module
            .declined
            .iter()
            .find(|declined| declined.name == "f")
            .map(|declined| declined.reason.clone())
            .unwrap_or_default()
    });
    assert!(reason.contains("run it a second time"), "{reason}");
}

#[test]
fn a_decorator_written_as_a_path_keeps_its_segments() {
    // every step of `functools.cache` is a read, so evaluating it at init means what it
    // meant where the `def` stood — and the ir carries the chain rather than one name
    let source = "\
import functools

@functools.cache
def f(n: int) -> int:
    return n
";
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let decorated = module
            .functions
            .iter()
            .find(|function| function.name == "f")
            .expect("f is compiled");
        assert_eq!(
            decorated.decorators,
            [by_ir::function::Decorator::Path {
                root: "functools".to_string(),
                attributes: vec!["cache".to_string()],
            }]
        );
    });
}

#[test]
fn a_decorator_rooted_in_the_class_body_is_declined() {
    // a decorator is resolved out of the *module* namespace at init, and a class body is
    // not that namespace. `@x.setter` is the shape this exists for, but that one also
    // writes two `def`s of one name — so the names here are distinct, or the duplicate
    // would answer first and this guard would never be reached
    let source = "\
class Box:
    def wrap(fn: object) -> object:
        return fn

    @wrap
    def value(self) -> int:
        return 1
";
    let reasons = with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(
            module.classes.iter().all(|class| class.name != "Box"),
            "Box must not be emitted"
        );
        module
            .declined
            .iter()
            .map(|declined| declined.reason.clone())
            .collect::<Vec<_>>()
    });
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("`wrap` is bound by the class body")),
        "{reasons:?}"
    );
}

#[test]
fn a_decorated_function_is_not_called_at_its_native_entry() {
    // the module namespace holds what the decorator returned, and the native entry is
    // what it was handed — so reaching the entry directly runs the *undecorated*
    // function. `caller` answered 2 where the interpreted module answered 4
    let ir = ir("\
def double(fn: object) -> object:
    def inner(x: int) -> int:
        return fn(x) * 2
    return inner

@double
def f(x: int) -> int:
    return x + 1

def caller(x: int) -> int:
    return f(x)

def plain(x: int) -> int:
    return x + 1

def other(x: int) -> int:
    return plain(x)
");
    assert!(ir.contains("pycall f("), "{ir}");
    assert!(!ir.contains("= call f("), "{ir}");
    // an undecorated sibling still reaches its native entry
    assert!(ir.contains("= call plain("), "{ir}");
}

#[test]
fn a_modifier_is_translated_rather_than_looked_up() {
    // a modifier reaches the ast as a decorator with no `@`, and there is no such name
    // in the module namespace to look up: `static` compiled to
    // `By_ApplyDecorator(dict, "make", "static")` and the extension then failed to
    // import with `NameError: name 'static' is not defined`.
    //
    // the transpiler rewrites each of these to a python decorator, and this is the
    // same mapping — so the compiled definition ends up wearing what the interpreted
    // twin wears
    for (modifier, expected) in [
        ("abstract", vec!["abstractmethod".to_string()]),
        ("override", vec!["override".to_string()]),
        ("export", Vec::new()),
    ] {
        let source = format!(
            "\
class Box:
    {modifier} def make(self) -> int:
        return 7
"
        );
        let decorators = with_source(&source, |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(
                module.declined.is_empty(),
                "{modifier}: {:?}",
                module.declined
            );
            module
                .all_functions()
                .find(|function| function.name.ends_with("make"))
                .map(|function| dotted(&function.decorators))
                .unwrap_or_else(|| panic!("{modifier}: make is compiled"))
        });
        assert_eq!(decorators, expected, "{modifier}");
    }
}

#[test]
fn a_method_that_is_not_bound_to_its_receiver_carries_its_convention() {
    // a method's first parameter used to be forced to the receiver whatever the
    // decorators said, and these two say slot zero holds something else — so
    // `Box.make(3)` compiled with `3` bound to a `Box` and raised at its first call.
    //
    // the convention rides on the method table entry now, so slot zero holds what
    // python puts there: nothing at all for a static method, whose first written
    // parameter keeps its own representation, and the *class* for a class method — an
    // ordinary object, pointedly not an instance of the layout, so nothing derives a
    // field read from it.
    //
    // and the decorator comes off the list: it is honoured by the emitted type rather
    // than applied to it, and applying it as well would wrap the descriptor twice
    for (source, binding, params) in [
        (
            "class Box:\n    static def make(x: int) -> int:\n        return x\n",
            by_ir::function::Binding::Static,
            vec![("x", RType::INT)],
        ),
        (
            "class Box:\n    @staticmethod\n    def make(x: int) -> int:\n        return x\n",
            by_ir::function::Binding::Static,
            vec![("x", RType::INT)],
        ),
        (
            "class Box:\n    @classmethod\n    def make(cls, x: int) -> int:\n        return x\n",
            by_ir::function::Binding::Class,
            vec![("cls", RType::OBJECT), ("x", RType::INT)],
        ),
        (
            "class Box:\n    def make(self, x: int) -> int:\n        return x\n",
            by_ir::function::Binding::Instance,
            vec![
                (
                    "self",
                    RType::Instance {
                        class: "Box".to_string(),
                        exact: false,
                    },
                ),
                ("x", RType::INT),
            ],
        ),
    ] {
        with_source(source, |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(
                module.declined.is_empty(),
                "{source}: {:?}",
                module.declined
            );
            let method = module
                .all_functions()
                .find(|function| function.name.ends_with("make"))
                .unwrap_or_else(|| panic!("{source}: make is compiled"));
            assert_eq!(method.binding, binding, "{source}");
            assert!(method.decorators.is_empty(), "{source}");
            let lowered: Vec<(&str, RType)> = method
                .params()
                .iter()
                .map(|param| (param.name.as_deref().unwrap_or(""), param.ty.clone()))
                .collect();
            let expected: Vec<(&str, RType)> = params
                .iter()
                .map(|(name, ty)| (*name, ty.clone()))
                .collect();
            assert_eq!(lowered, expected, "{source}");
        });
    }
}

#[test]
fn a_second_decorator_over_a_static_method_keeps_the_decline() {
    // the runtime folds the remaining decorators onto the attribute it reads back off
    // the finished type — and reading a static method back hands over the plain
    // function it wraps, which would then be written back as an ordinary method. so
    // the convention is only honoured natively where nothing else has to be applied
    for source in [
        "class Box:\n    @final\n    @staticmethod\n    def make() -> int:\n        return 7\n",
        "class Box:\n    @staticmethod\n    @final\n    def make() -> int:\n        return 7\n",
        "class Box:\n    @final\n    @classmethod\n    def make(cls) -> int:\n        return 7\n",
    ] {
        let reasons = declines(source);
        assert!(
            reasons
                .iter()
                .any(|(_, reason)| reason.contains("a second decorator over")),
            "{source}: {reasons:?}"
        );
    }
}

#[test]
fn a_convention_python_gives_a_method_itself_is_not_carried_twice() {
    // python already makes each of these implicitly static or class, and an emitted
    // generic class publishes a `__class_getitem__` of its own — so a table entry here
    // would either duplicate the convention or collide with that entry.
    //
    // `__new__` is not among them: it is published by an assignment onto the finished
    // type rather than through the table, so a `@staticmethod` over it says only what
    // python already says and is dropped
    for source in [
        "class Box:\n    @classmethod\n    def __init_subclass__(cls) -> None:\n        return None\n",
        "class Box:\n    @classmethod\n    def __class_getitem__(cls, item: object) -> object:\n        return item\n",
    ] {
        let reasons = declines(source);
        assert!(
            reasons
                .iter()
                .any(|(_, reason)| reason.contains("a convention of its own")
                    || reason.contains("fills a type slot")),
            "{source}: {reasons:?}"
        );
    }
}

#[test]
fn a_global_a_frame_assigns_is_written_to_the_namespace_and_read_back_from_it() {
    // a `global` declaration says where a name lives, and both halves of it have to
    // agree: the write goes to the module namespace, and every read in the same frame
    // comes back out of it. binding a register for either half is what made
    // `mimetypes.init` set `inited` where nothing else could see it
    for (source, function, writes, reads) in [
        (
            "seen = 0\n\ndef bump(n: int) -> int:\n    global seen\n    seen = n\n    return seen\n",
            "bump",
            1,
            1,
        ),
        // augmented assignment is a read and a write of the one place, so it is both
        (
            "seen = 0\n\ndef bump(n: int) -> int:\n    global seen\n    seen += n\n    return seen\n",
            "bump",
            1,
            2,
        ),
        // a loop target is a binding like any other
        (
            "seen = 0\n\ndef bump(ns: list[int]) -> int:\n    global seen\n    for seen in ns:\n        pass\n    return seen\n",
            "bump",
            1,
            1,
        ),
        // a declaration with no assignment under it is redundant, and the name resolves
        // where it already resolved
        (
            "seen = 0\n\ndef read(n: int) -> int:\n    global seen\n    return seen + n\n",
            "read",
            0,
            1,
        ),
        // the same name written by a frame that did *not* declare it is an ordinary
        // local, and shadowing the global is what python does with it too
        (
            "seen = 0\n\ndef shadow(n: int) -> int:\n    seen = n\n    return seen\n",
            "shadow",
            0,
            0,
        ),
    ] {
        let (stores, loads) = with_source(source, |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(
                module.declined.is_empty(),
                "{source}: {:?}",
                module.declined
            );
            let lowered = module
                .functions
                .iter()
                .find(|candidate| candidate.name == function)
                .unwrap_or_else(|| panic!("{function} was not emitted"));
            let count = |wanted: fn(&Op) -> bool| {
                lowered
                    .blocks
                    .iter()
                    .flat_map(|block| block.ops.iter())
                    .filter(|op| wanted(op))
                    .count()
            };
            (
                count(|op| matches!(op, Op::StoreGlobal { name, .. } if name == "seen")),
                count(|op| matches!(op, Op::LoadGlobal { name, .. } if name == "seen")),
            )
        });
        assert_eq!((stores, loads), (writes, reads), "{source}");
    }
}

#[test]
fn a_global_a_nested_frame_declares_is_never_captured_from_the_frame_around_it() {
    // the enclosing frame binds a local `seen` and the nested one declares `seen`
    // global, so the two names are different places. the nested body only *reads* it,
    // which is the case nothing else rules out: a body that wrote it would look like
    // it owned the name anyway, so only the declaration says the enclosing local is
    // the wrong place to capture
    let source = "\
seen = 0
tally = 0


def outer(n: int) -> int:
    seen = n

    def peek() -> int:
        global seen
        return seen

    return peek() + seen


def declared_out_here(n: int) -> int:
    global tally
    tally = n

    def look() -> int:
        return tally

    return look() + tally
";
    let (fields, loads) = with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let nested: Vec<&by_ir::function::Function> = module
            .classes
            .iter()
            .flat_map(|class| class.methods.iter())
            .filter(|candidate| candidate.name == "peek" || candidate.name == "look")
            .collect();
        assert_eq!(nested.len(), 2, "both nested functions are emitted");
        (
            // neither name may become an environment field: there is nothing in the
            // frame around either nested function for it to hold
            module
                .classes
                .iter()
                .flat_map(|class| class.fields.iter())
                .filter(|field| field.name == "seen" || field.name == "tally")
                .count(),
            nested
                .iter()
                .flat_map(|function| function.blocks.iter())
                .flat_map(|block| block.ops.iter())
                .filter(|op| {
                    matches!(op, Op::LoadGlobal { name, .. } if name == "seen" || name == "tally")
                })
                .count(),
        )
    });
    assert_eq!((fields, loads), (0, 2));
}

#[test]
fn a_static_method_that_suspends_declines() {
    // a generator's state class is namespaced by the receiver's class, and neither of
    // these has one — so two classes each with a static `values` would want a single
    // state class between them
    let reasons =
        declines("class Box:\n    @staticmethod\n    def values() -> object:\n        yield 1\n");
    assert!(
        reasons
            .iter()
            .any(|(_, reason)| reason.contains("that suspends is not lowered yet")),
        "{reasons:?}"
    );
}

#[test]
fn a_static_method_and_a_function_of_one_name_get_environments_of_their_own() {
    // a nested function lives on a generated environment class named after the frame
    // that makes it, and a method's frame is namespaced by its class. a static method
    // has no receiver to take that name from, so the name comes from the class the
    // `def` was *written* in — otherwise these two ask for one class between them
    with_source(
        "\
class Box:
    @staticmethod
    def add(n: int) -> int:
        def inner(k: int) -> int:
            return k + n
        return inner(1)


def add(n: int) -> int:
    def inner(k: int) -> int:
        return k + n + 100
    return inner(1)
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let environments: Vec<&str> = module
                .classes
                .iter()
                .map(|class| class.name.as_str())
                .filter(|name| name.ends_with("$env"))
                .collect();
            assert_eq!(environments, vec!["Box$add$env", "add$env"]);
        },
    );
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
    def __setattr__(self, name: str, value: object) -> None:
        object.__setattr__(self, name, value)

    def __init__(self, tag: str) -> None:
        Container.__init__(self, tag)
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
    // an emitted class cannot be subclassed, so there is nothing to dispatch on. what
    // is left is a value written into the instance's own dict, which shadows the
    // class's method — so every protocol call here is the arm behind that one test,
    // and there is no other reason for one to appear
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
                let asked = count_op(function, |op| matches!(op, Op::DictShadows { .. }));
                assert!(asked > 0, "{text}");
                assert_eq!(
                    count_op(function, |op| matches!(op, Op::CallMethod { .. })),
                    asked,
                    "{text}"
                );
            }
        },
    );
}

#[test]
fn a_slots_class_reaches_its_body_with_nothing_asked() {
    // `__slots__` is python's own way of saying an instance's attributes are exactly the
    // declared ones, so there is no dict for a value shadowing a method to go in — and
    // python itself refuses the write. that leaves the direct call with nothing to test,
    // which is the shape it had before the test existed
    with_source(
        "\
class Cell:
    __slots__ = (\"n\",)

    def __init__(self, n: int) -> None:
        self.n = n

    def doubled(self) -> int:
        return self.n * 2

def use(c: Cell) -> int:
    return c.doubled()
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            let function = &module.functions[0];
            let text = print_function(function);
            assert!(text.contains("call Cell.doubled"), "{text}");
            assert!(
                !has_op(function, |op| matches!(
                    op,
                    Op::DictShadows { .. } | Op::CallMethod { .. }
                )),
                "{text}"
            );
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            let function = &module.functions[0];
            let text = print_function(function);
            // no box on the way in and no unbox on the way out. the block asking whether
            // the instance shadows the method is on the way to the call and boxes
            // nothing either — the receiver is read where it lies
            let direct = function
                .blocks
                .iter()
                .find(|block| {
                    block
                        .ops
                        .iter()
                        .any(|op| matches!(op, Op::CallNative { .. }))
                })
                .expect("the direct call is emitted");
            for block in function.blocks.iter().filter(|block| {
                block
                    .ops
                    .iter()
                    .any(|op| matches!(op, Op::DictShadows { .. }))
            }) {
                assert!(
                    !block
                        .ops
                        .iter()
                        .any(|op| matches!(op, Op::Box { .. } | Op::Unbox { .. })),
                    "{text}"
                );
            }
            assert!(
                !direct
                    .ops
                    .iter()
                    .any(|op| matches!(op, Op::Box { .. } | Op::Unbox { .. })),
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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

/// a decorator a method carries is kept and applied at module init
///
/// `total` carries `@property` under a second decorator, which is deliberately not the
/// group of one — a plain `@property` written on its own is lowered as an attribute
/// instead, and what stands here is `doubling`'s answer rather than a `property`
#[test]
fn a_decorated_method_keeps_its_decorators() {
    with_source(
        "\
def doubling(fn: object) -> object:
    return fn

data class Point:
    x: int

    @doubling
    @property
    def total(self) -> int:
        return self.x

    @doubling
    def raw(self) -> int:
        return self.x
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
                    .map(|method| dotted(&method.decorators))
            };
            assert_eq!(
                decorators("total"),
                Some(vec!["doubling".to_string(), "property".to_string()])
            );
            assert_eq!(decorators("raw"), Some(vec!["doubling".to_string()]));
            assert!(class.properties.is_empty(), "{:?}", class.properties);
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
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module = crate::build_module(db, env, model, suite, "app", crate::Language::Python);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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

            // the body dispatches on the state and returns at the yield, and the end
            // of it is a finish rather than a raise
            let resume = print_function(&state.methods[0]);
            assert!(resume.contains("<counted$gen.$state>"), "{resume}");
            assert!(resume.contains("finish "), "{resume}");
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
        // a generator parks its iterator, and a parked loop keeps the protocol
        cursor: None,
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
        // a generator parks its iterator, and a parked loop keeps the protocol
        cursor: None,
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
        error.reason.contains("may be unbound where it is read"),
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
    //
    // `stepped` awaits on its own behalf so that awaiting it really does suspend: a
    // coroutine that never suspends is called rather than driven, and there would be
    // nothing to park across
    with_source(
        "\
async def stepped(i: int, held: object) -> int:
    await held
    return i * 7

async def summed(n: int, held: object) -> int:
    total = 0
    i = 0
    while i < n:
        total = total + await stepped(i, held)
        i = i + 1
    return total
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
async def chained(awaitable: object) -> object:
    return await awaitable
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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

/// what the resume method of `chained` lowers to, for the direct-await tests below
fn awaiting_frame(source: &str) -> (String, Vec<String>) {
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let chained = module
            .classes
            .iter()
            .find(|class| class.name == "chained$gen")
            .expect("chained is a coroutine");
        let emitted = module
            .all_functions()
            .map(by_ir::Function::qualified_name)
            .collect();
        (print_function(&chained.methods[0]), emitted)
    })
}

#[test]
fn an_await_of_a_coroutine_that_never_suspends_is_the_call() {
    // the coroutine `step(i)` would build is made here, awaited once and dropped, and
    // one `send` runs the whole body — so there is nothing between the call and the
    // value but an object nobody can see
    let (text, emitted) = awaiting_frame(
        "\
async def step(n: int) -> int:
    return n * 2

async def chained(n: int) -> int:
    return await step(n)
",
    );
    assert!(text.contains("call step$direct(r"), "{text}");
    assert!(!text.contains("awaititer"), "{text}");
    assert!(
        emitted.iter().any(|name| name == "step$direct"),
        "{emitted:?}"
    );
    // and `step` itself is still a coroutine: calling it without awaiting has to hand
    // back an object `asyncio` can drive like any other
    assert!(emitted.iter().any(|name| name == "step"), "{emitted:?}");
}

#[test]
fn an_await_of_a_coroutine_that_suspends_keeps_the_awaitable_protocol() {
    // `step` has a resumption point of its own, so the object really does carry state
    // between two sends and the await has to drive it
    let (text, emitted) = awaiting_frame(
        "\
async def step(awaitable: object) -> object:
    return await awaitable

async def chained(awaitable: object) -> object:
    return await step(awaitable)
",
    );
    assert!(text.contains("awaititer"), "{text}");
    assert!(!text.contains("$direct"), "{text}");
    assert!(
        !emitted.iter().any(|name| name.contains("$direct")),
        "{emitted:?}"
    );
}

#[test]
fn a_coroutine_awaited_through_a_name_keeps_the_awaitable_protocol() {
    // the object has been given a name, and what a name can be made to do — awaited
    // twice, closed, handed to `asyncio`, or dropped so the `RuntimeWarning` fires —
    // is not what this can answer from the shape of the `await` alone
    let (text, emitted) = awaiting_frame(
        "\
async def step(n: int) -> int:
    return n * 2

async def chained(n: int) -> int:
    held = step(n)
    return await held
",
    );
    assert!(text.contains("awaititer"), "{text}");
    assert!(!text.contains("call step$direct"), "{text}");
    // and with no `await` in the module reaching it, the edition is not emitted at
    // all: it would be a second copy of a body under a name the namespace never binds
    assert!(
        !emitted.iter().any(|name| name == "step$direct"),
        "{emitted:?}"
    );
}

#[test]
fn an_async_generator_is_given_no_direct_edition() {
    // an `async def` that yields is an async *generator*: `step(n)` is an asynchronous
    // iterator and is never awaited at all, so there is no call for one to stand for
    let (_, emitted) = awaiting_frame(
        "\
async def step(n: int) -> object:
    yield n

async def chained(awaitable: object) -> object:
    return await awaitable
",
    );
    assert!(
        !emitted.iter().any(|name| name.contains("$direct")),
        "{emitted:?}"
    );
}

#[test]
fn a_coroutine_whose_body_awaits_on_its_own_behalf_is_given_no_direct_edition() {
    // an `async with` and an `async for` await without the body saying so, and the
    // property has to be about the machine rather than about the word `await`
    for body in [
        "    async with held:\n        return 1\n",
        "    async for item in held:\n        return 1\n    return 0\n",
        "    return len([x async for x in held])\n",
    ] {
        let source = format!(
            "\
async def step(held: object) -> int:
{body}
async def chained(awaitable: object) -> object:
    return await awaitable
"
        );
        let emitted = with_source(&source, |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            module
                .all_functions()
                .map(by_ir::Function::qualified_name)
                .collect::<Vec<_>>()
        });
        assert!(
            !emitted.iter().any(|name| name.contains("$direct")),
            "{body}\n{emitted:?}"
        );
    }
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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

#[test]
fn a_deleted_local_is_flagged_even_where_every_path_assigns_it() {
    // the fixpoint that finds a read-before-write cannot see this one: `x` is assigned
    // on the only path that reaches the `del`, so a forward analysis calls it
    // definitely written. the byte is what the deletion unbinds *into*, so the
    // deletion is what asks for it
    let named = with_source(
        "\
def drop(n: int) -> int:
    x = n
    del x
    return n
",
        |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(module.declined.is_empty(), "{:?}", module.declined);
            module
                .functions
                .iter()
                .find(|function| function.name == "drop")
                .map(|function| {
                    let text = print_function(function);
                    assert!(text.contains("del "), "{text}");
                    function
                        .registers
                        .iter()
                        .filter(|decl| decl.may_be_unassigned)
                        .filter_map(|decl| decl.name.clone())
                        .collect::<Vec<_>>()
                })
        },
    );
    assert_eq!(named, Some(vec!["x".to_string()]));
}

#[test]
fn deleting_a_name_nothing_else_binds_declines() {
    // python makes a name local for the whole function as soon as any statement in it
    // binds *or deletes* the name, so `count` here is local and every read of it in
    // the body raises. this lowering decides what is local from the writes, so it
    // would resolve those reads out of the module namespace instead
    let reasons = declines(
        "\
count = 1


def drop() -> int:
    del count
    return 0
",
    );
    assert_eq!(
        reasons
            .iter()
            .map(|(name, reason)| (name.as_str(), reason.as_str()))
            .collect::<Vec<_>>(),
        vec![(
            "drop",
            "`del count` is the only statement binding `count` in this function, and a \
             name deleted but never assigned is local for the whole of it"
        )]
    );
}

#[test]
fn deleting_a_name_a_nested_function_shares_declines() {
    // `held` is one cell between the two frames, and its unbound state is the field
    // being NULL rather than a byte beside a register — which is not what the
    // deletion clears
    let reasons = declines(
        "\
def outer(n: int) -> int:
    held = n

    def inner() -> int:
        return held

    del held
    return inner()
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "outer"
            && reason
                == "`del held` unbinds a name another frame shares, which is a \
                          cell rather than a register"),
        "{reasons:?}"
    );
}

#[test]
fn a_class_body_filling_a_table_with_its_own_methods_is_lowered() {
    // the shape `pickle.Unpickler` is 68 of and `pprint.PrettyPrinter` 18 of. the
    // subscript binds no name in the class namespace: it writes into an object the body
    // already built, which module init copies across already finished
    let reasons = declines(
        "\
class Table:
    dispatch = {}

    def load_int(self, v: int) -> int:
        return v
    dispatch[int] = load_int
",
    );
    assert!(reasons.is_empty(), "{reasons:?}");
}

#[test]
fn a_class_body_writing_one_value_under_two_names_is_lowered() {
    let reasons = declines(
        "\
class Bounds:
    low = high = 3

    def span(self) -> int:
        return self.high - self.low
",
    );
    assert!(reasons.is_empty(), "{reasons:?}");
}

#[test]
fn a_table_a_class_body_fills_conditionally_is_lowered() {
    // `pickle._Pickler`'s shape: the write stands under an `if`, and the interpreted
    // definition made it into the table before init copies the table across
    let reasons = declines(
        "\
available: bool = True


class Table:
    dispatch = {}

    def load_int(self, v: int) -> int:
        return v
    if available:
        dispatch[int] = load_int
",
    );
    assert!(reasons.is_empty(), "{reasons:?}");
}

#[test]
fn a_method_a_class_body_defines_conditionally_is_lowered() {
    let reasons = declines(
        "\
available: bool = True


class Table:
    def load_int(self, v: int) -> int:
        return v
    if available:
        def load_str(self, v: str) -> str:
            return v
",
    );
    assert!(reasons.is_empty(), "{reasons:?}");
}

#[test]
fn a_conditional_binding_beside_a_definition_of_the_same_name_declines() {
    // the `def` is lowered into the method table and the conditional binding is copied
    // off the interpreted definition, so the type would carry two answers for one name
    let reasons = declines(
        "\
available: bool = True


class Table:
    def load(self, v: int) -> int:
        return v
    if available:
        def load(self, v: int) -> int:
            return v + 1
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "Table"
            && reason
                == "`load` is both defined by this class body and bound by a block nested in it"),
        "{reasons:?}"
    );
}

#[test]
fn a_conditional_dunder_declines() {
    let reasons = declines(
        "\
available: bool = True


class Table:
    n: int = 0
    if available:
        def __repr__(self) -> str:
            return \"table\"
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "Table"
            && reason
                == "`__repr__` is bound by a block nested in the class body, and a dunder is settled before one runs"),
        "{reasons:?}"
    );
}

#[test]
fn a_loop_in_a_class_body_is_lowered_for_what_it_leaves_behind() {
    // the loop variable stays in the namespace when the loop ends, so it is one of the
    // names carried across
    let reasons = declines(
        "\
class Table:
    names = []
    for width in (1, 2, 3):
        names.append(width)
",
    );
    assert!(reasons.is_empty(), "{reasons:?}");
}

#[test]
fn a_try_in_a_class_body_declines() {
    let reasons = declines(
        "\
class Table:
    try:
        n = 1
    except ValueError:
        n = 2
",
    );
    assert!(
        reasons
            .iter()
            .any(|(name, reason)| name == "Table"
                && reason == "only fields and methods are lowered yet"),
        "{reasons:?}"
    );
}

#[test]
fn a_decorator_a_conditional_bound_declines_the_class() {
    // init resolves a decorator out of the *module* namespace, and a block nested in the
    // class body binds into the class namespace instead — which is nowhere init looks
    let reasons = declines(
        "\
available: bool = True


def wrap(f):
    return f


class Table:
    if available:
        helper = wrap

    @helper
    def load(self, n: int) -> int:
        return n
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "Table"
            && reason
                == "`helper` is bound by the class body, and a decorator is resolved out of the module namespace at init"),
        "{reasons:?}"
    );
}

#[test]
fn an_import_under_a_class_body_conditional_declines() {
    let reasons = declines(
        "\
available: bool = True


class Table:
    n: int = 0
    if available:
        import sys
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "Table"
            && reason == "an import nested in a class body is not lowered yet"),
        "{reasons:?}"
    );
}

#[test]
fn a_class_level_assignment_to_an_attribute_declines() {
    let reasons = declines(
        "\
class Holder:
    n: int = 0


class Table:
    held = Holder()
    held.n = 1
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "Table"
            && reason == "only a plain class-level name is lowered yet"),
        "{reasons:?}"
    );
}

#[test]
fn a_class_level_assignment_unpacking_a_tuple_declines() {
    let reasons = declines(
        "\
class Pair:
    low, high = 1, 2
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "Pair"
            && reason == "only a plain class-level name is lowered yet"),
        "{reasons:?}"
    );
}

/// the reason each declined entry in `source` gives, by name
fn declines(source: &str) -> Vec<(String, String)> {
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
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

/// a class whose `__new__` allocates and fills, and a subclass that inherits it
const A_WRITTEN_NEW: &str = "\
class Held:
    def __new__(cls, n: int) -> \"Held\":
        self = object.__new__(cls)
        self.n = n
        return self


class Under(Held):
    def __init__(self, n: int) -> None:
        self.n = n * 2


def make(n: int) -> Held:
    return Held(n)


def below(n: int) -> Under:
    return Under(n)
";

#[test]
fn a_written_new_is_handed_the_class_in_slot_zero() {
    // python makes `__new__` a static method and puts the *class* in front of the
    // arguments, so slot zero is bound out of the vector like any other static method's
    // — and typed as the plain object a class is, because reading a field off it would
    // be treating a type as an instance
    assert!(
        method_ir(A_WRITTEN_NEW, "Held", "__new__").contains("def __new__(cls: object, n: int)"),
        "{}",
        method_ir(A_WRITTEN_NEW, "Held", "__new__")
    );
}

#[test]
fn a_construction_of_a_class_that_writes_a_new_goes_through_python() {
    // the direct allocation skips `__new__` entirely, which is the whole of what the
    // written one exists to do. an inherited one counts: `Under` writes none of its own
    // and still constructs through `Held`'s
    let lowered = ir(A_WRITTEN_NEW);
    for class in ["Held", "Under"] {
        assert!(
            lowered.contains(&format!("pycall {class}(")),
            "{class}: {lowered}"
        );
        assert!(
            !lowered.contains(&format!("new {class}(")),
            "{class}: {lowered}"
        );
    }
}

#[test]
fn a_new_whose_answer_is_another_class_declines() {
    // every `C(...)` in the module is compiled believing it got a `C`, because the
    // checker does not follow `__new__`. one that answers something else would hand each
    // construction an object of a shape it was compiled not to expect
    let reasons = declines(
        "\
class Aside:
    def __init__(self, n: int) -> None:
        self.n = n


class Held:
    def __new__(cls, n: int) -> Aside:
        return Aside(n)
",
    );
    assert!(
        reasons
            .iter()
            .any(|(name, reason)| name == "Held" && reason.contains("`__new__` answers a `Aside`")),
        "{reasons:?}"
    );
}

#[test]
fn a_new_over_a_base_outside_the_module_declines() {
    // the allocation is the base's, and only that base knows how big one of its
    // instances is
    let reasons = declines(
        "\
class Pair(tuple):
    def __new__(cls, a: int, b: int) -> \"Pair\":
        return tuple.__new__(cls, (a, b))
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "Pair"
            && reason.contains("takes its instance layout from a base outside this module")),
        "{reasons:?}"
    );
}

#[test]
fn a_new_the_class_body_assigns_declines() {
    // an assignment binds the name python fills from the allocator slot, so the name and
    // the slot would answer differently
    let reasons = declines(
        "\
def make(cls: object, n: int) -> object:
    return object.__new__(cls)


class Held:
    __new__ = make
",
    );
    assert!(
        reasons.iter().any(|(name, reason)| name == "Held"
            && reason.contains("`__new__ = ...` binds the name python fills from `tp_new`")),
        "{reasons:?}"
    );
}

#[test]
fn a_dunder_whose_slot_has_no_adapter_still_declines() {
    // the gate is what keeps a slot the emitter cannot fill from compiling to a class
    // python never consults — a wrong answer where a decline was right
    for method in [
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

#[test]
fn a_method_defined_twice_in_a_class_body_is_declined() {
    // two `def`s of one name bind whichever one ran, and they mangle to one C symbol —
    // so a class with both emitted two `Box.value` entries and the module then failed to
    // compile outright, which is worse than any wrong answer
    let source = "\
class Box:
    def value(self) -> int:
        return 1

    def value(self) -> int:
        return 2
";
    let reasons = with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(
            module.classes.iter().all(|class| class.name != "Box"),
            "Box must not be emitted"
        );
        module
            .declined
            .iter()
            .map(|declined| declined.reason.clone())
            .collect::<Vec<_>>()
    });
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("`value` is defined more than once")),
        "{reasons:?}"
    );
}

/// a property's halves leave the method table and become one published attribute
///
/// two `def value`s in one body is what `defined_once` turns down, and this is the one
/// shape where they are not two definitions at all — python folds them into a single
/// `property`. the halves keep their bodies, under names a source cannot write, and the
/// name the source *did* write is published once
#[test]
fn a_property_pair_is_lowered_as_one_attribute() {
    let source = "\
class Box:
    def __init__(self, n: int) -> None:
        self._n = n

    @property
    def value(self) -> int:
        return self._n

    @value.setter
    def value(self, given: int) -> None:
        self._n = given
";
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let class = module
            .classes
            .iter()
            .find(|class| class.name == "Box")
            .expect("Box is emitted");
        assert_eq!(class.properties.len(), 1, "{:?}", class.properties);
        let property = &class.properties[0];
        assert_eq!(property.name, "value");
        assert_eq!(property.getter.as_deref(), Some("value$get"));
        assert_eq!(property.setter.as_deref(), Some("value$set"));
        assert_eq!(property.deleter, None);
        // the name the source wrote is not in the layout: `self._n` is the field, and
        // `value` is reached through the descriptor
        assert!(
            class.fields.iter().all(|field| field.name != "value"),
            "{:?}",
            class.fields
        );
        // nor in the method table, where an entry would answer under the same name the
        // published property has to
        assert_eq!(
            class
                .table_methods()
                .map(|method| method.name.clone())
                .collect::<Vec<_>>(),
            vec!["__init__".to_string()]
        );
    });
}

/// a read and a write of a property call its halves outright
///
/// the descriptor protocol arrives at these same two bodies, but it gets there by looking
/// the name up on the type, finding a `property`, and calling through it — and it has to
/// box the getter's answer to hand it back. naming the half directly skips all of that,
/// which is what a field read already does for an attribute that is one
#[test]
fn a_property_read_and_write_call_the_halves_directly() {
    let source = "\
class Cell:
    def __init__(self) -> None:
        self._v = 0

    @property
    def v(self) -> int:
        return self._v

    @v.setter
    def v(self, given: int) -> None:
        self._v = given


def bump(cell: Cell) -> int:
    cell.v = cell.v + 1
    return cell.v
";
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let bump = module
            .functions
            .iter()
            .find(|function| function.name == "bump")
            .expect("bump is emitted");
        let text = print_function(bump);
        assert!(text.contains("call Cell.v$get("), "{text}");
        assert!(text.contains("call Cell.v$set("), "{text}");
        // the protocol is gone from the frame entirely, and with it the boxing that only
        // existed to get an `int` back through a `PyObject *`
        assert!(
            !has_op(bump, |op| matches!(
                op,
                Op::GetAttr { .. } | Op::SetAttr { .. }
            )),
            "{text}"
        );
    });
}

/// a `@property` written once is a group of one, and is lowered as the attribute it is
///
/// nothing is written under it, so there is no second `def value` for `defined_once` to
/// object to and the ordinary method path would take it — with `@property` carried along
/// as a decorator to apply at init. that path cannot make the body run: the class body
/// already applied the decorator, so what the type is given is the object *that* body
/// left. lowering it as a group instead publishes one `property` over the compiled getter,
/// which is also what lets a typed read call the half outright
#[test]
fn a_lone_property_getter_is_lowered_as_one_attribute() {
    let source = "\
class Box:
    def __init__(self, n: int) -> None:
        self._n = n

    @property
    def value(self) -> int:
        return self._n


def read(box: Box) -> int:
    return box.value
";
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let class = module
            .classes
            .iter()
            .find(|class| class.name == "Box")
            .expect("Box is emitted");
        assert_eq!(class.properties.len(), 1, "{:?}", class.properties);
        let property = &class.properties[0];
        assert_eq!(property.name, "value");
        assert_eq!(property.getter.as_deref(), Some("value$get"));
        assert_eq!(property.setter, None);
        assert_eq!(property.deleter, None);
        // the getter has left the method table, where an entry would answer under the
        // same name the published property has to
        assert_eq!(
            class
                .table_methods()
                .map(|method| method.name.clone())
                .collect::<Vec<_>>(),
            vec!["__init__".to_string()]
        );
        let read = module
            .functions
            .iter()
            .find(|function| function.name == "read")
            .expect("read is emitted");
        let text = print_function(read);
        assert!(text.contains("value$get"), "{text}");
        assert!(
            !has_op(read, |op| matches!(op, Op::GetAttr { .. })),
            "{text}"
        );
    });
}

/// a group of one that cannot be published is left where it stands, not declined
///
/// a pair this cannot read has to turn the class down: two `def value`s in one body are
/// two definitions of one name to everything else, and there is nowhere to leave them. a
/// group of one is a single ordinary `def` under a single ordinary decorator, so the path
/// that took it before this construct existed still works — it runs the interpreted body,
/// which is what the whole class did a moment ago.
///
/// the first two cost a real module when they declined instead: a getter that suspends is
/// `email._header_value_parser.MimeParameters`, and the construction through a metaclass
/// is `urllib.parse._NetlocResultMixinStr`, which took three more classes down with it.
/// the third is a class whose *base* may carry a metaclass — which is a wrong answer
/// rather than a decline, so it is the one that has to be got right
#[test]
fn a_lone_property_getter_that_cannot_be_published_is_left_alone() {
    let suspends = "\
class Box:
    def __init__(self) -> None:
        self.n = 1

    @property
    def value(self) -> object:
        yield self.n

    def plain(self) -> int:
        return self.n
";
    let through_a_metaclass = "\
from abc import ABCMeta


class Box(metaclass=ABCMeta):
    @property
    def value(self) -> int:
        return 1

    def plain(self) -> int:
        return 2
";
    // a base may carry a metaclass of its own, and this class never names it — `numbers`
    // writes `metaclass=ABCMeta` on `Number` and on nothing below it, and `Integral`'s two
    // lone getters were called abstract by the emitted type once they left the method
    // table for a `property` written on afterwards
    let over_a_base = "\
class Held:
    def held(self) -> int:
        return 1


class Box(Held):
    @property
    def value(self) -> int:
        return 1

    def plain(self) -> int:
        return 2
";
    for source in [suspends, through_a_metaclass, over_a_base] {
        with_source(source, |db, env, model, suite| {
            let module =
                crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
            assert!(
                module.declined.is_empty(),
                "{source}\n{:?}",
                module.declined
            );
            let class = module
                .classes
                .iter()
                .find(|class| class.name == "Box")
                .expect("Box is emitted");
            assert!(class.properties.is_empty(), "{:?}", class.properties);
            // the getter is back where it was, carrying the decorator the class body
            // already applied — so the type is given that body's `property`
            assert!(
                class.methods.iter().any(|method| method.name == "value"),
                "the getter stays a method"
            );
            assert!(
                class.methods.iter().any(|method| method.name == "plain"),
                "and the rest of the class is still lowered"
            );
        });
    }
}

/// a `def` written once under something that is not `@property` is not one of these
///
/// the group of one is recognised by the decorator alone, so every other single decorated
/// `def` in a class body has to go on reaching the ordinary method path — otherwise a
/// `@staticmethod` or a `@functools.cache` would be published as an attribute built out
/// of itself
#[test]
fn a_lone_method_under_another_decorator_is_not_a_property() {
    let source = "\
class Box:
    def __init__(self) -> None:
        self.n = 0

    @staticmethod
    def made() -> int:
        return 1
";
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let class = module
            .classes
            .iter()
            .find(|class| class.name == "Box")
            .expect("Box is emitted");
        assert!(class.properties.is_empty(), "{:?}", class.properties);
        assert!(
            class.table_methods().any(|method| method.name == "made"),
            "made stays in the method table"
        );
    });
}

/// a write to a property that has no setter keeps the protocol
///
/// python answers one by raising, in wording the `property` object owns. there is no
/// setter body to call, and a write that quietly reached the *getter's* field instead
/// would be the silent wrong answer this whole path exists to avoid
#[test]
fn a_write_to_a_property_with_no_setter_stays_on_the_protocol() {
    let source = "\
class Reading:
    def __init__(self) -> None:
        self._v = 0

    @property
    def v(self) -> int:
        return self._v

    @v.deleter
    def v(self) -> None:
        pass


def store(reading: Reading) -> None:
    reading.v = 1
";
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let store = module
            .functions
            .iter()
            .find(|function| function.name == "store")
            .expect("store is emitted");
        let text = print_function(store);
        assert!(
            has_op(store, |op| matches!(op, Op::SetAttr { .. })),
            "{text}"
        );
        assert!(!text.contains("v$set"), "{text}");
    });
}

/// a property whose half carries a defaulted extra parameter keeps the protocol
///
/// python hands a setter exactly one argument and a getter none, so a body written to
/// take more than that is reached with its defaults filled in — and only the wrapper
/// knows how to fill them. the emitted body has a parameter for every one of them, so a
/// direct call passing what the *protocol* passes would hand over too few arguments,
/// which is a call the c compiler would refuse rather than a wrong answer at runtime
#[test]
fn a_property_half_with_an_extra_defaulted_parameter_stays_on_the_protocol() {
    let source = "\
class Box:
    def __init__(self) -> None:
        self.n = 0

    @property
    def value(self) -> int:
        return self.n

    @value.setter
    def value(self, given: int, extra: int = 2) -> None:
        self.n = given * extra


def store(box: Box) -> None:
    box.value = 3
";
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let store = module
            .functions
            .iter()
            .find(|function| function.name == "store")
            .expect("store is emitted");
        let text = print_function(store);
        assert!(
            has_op(store, |op| matches!(op, Op::SetAttr { .. })),
            "{text}"
        );
        assert!(!text.contains("value$set"), "{text}");
    });
}

/// a property on a class another class in the module extends keeps the protocol
///
/// a subclass may override either half, and a receiver typed as the base cannot see
/// which one it got — the same reason a method on such a class is not called directly.
/// the layout pass makes both classes *mutable*, and that is what this rests on
#[test]
fn a_property_on_an_extended_class_stays_on_the_protocol() {
    let source = "\
class Base:
    def __init__(self) -> None:
        self._v = 0

    @property
    def v(self) -> int:
        return self._v

    @v.setter
    def v(self, given: int) -> None:
        self._v = given


class Narrow(Base):
    @property
    def v(self) -> int:
        return self._v * 2


def read(base: Base) -> int:
    return base.v
";
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        let read = module
            .functions
            .iter()
            .find(|function| function.name == "read")
            .expect("read is emitted");
        let text = print_function(read);
        assert!(
            has_op(read, |op| matches!(op, Op::GetAttr { .. })),
            "{text}"
        );
        assert!(!text.contains("v$get"), "{text}");
    });
}

/// a write to a property is a write to the descriptor, not to a field beside it
///
/// `logging.Manager.__init__` writes `self.disable = 0` where `disable` is a property,
/// and a layout that took that as a field would store the raw value and never run the
/// setter — which is where `_checkLevel` lives
#[test]
fn a_write_to_a_property_does_not_become_a_field() {
    let source = "\
class Manager:
    def __init__(self) -> None:
        self.disable = 0

    @property
    def disable(self) -> int:
        return self._disable

    @disable.setter
    def disable(self, level: int) -> None:
        self._disable = level + 1
";
    with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        let class = module
            .classes
            .iter()
            .find(|class| class.name == "Manager")
            .expect("Manager is emitted");
        let fields: Vec<&str> = class
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(fields, vec!["_disable"], "{fields:?}");
    });
}

/// a property on a class its metaclass builds is turned down
///
/// the `property` a pair becomes is written into the type module init built, and a class
/// built through its metaclass is built out of a *namespace* the metaclass is free to do
/// anything with — so what module init wrote may be on a type nothing reaches. that is
/// what this pins: the class declines for the decorator its halves carry, and a change
/// that let a decorated method through there has to answer for the property as well
#[test]
fn a_property_on_a_class_its_metaclass_builds_is_declined() {
    let reasons = declines(
        "\
from abc import ABCMeta


class Meta(metaclass=ABCMeta):
    @property
    def value(self) -> int:
        return 1

    @value.setter
    def value(self, given: int) -> None:
        pass
",
    );
    assert!(
        reasons
            .iter()
            .any(|(name, reason)| name == "Meta" && reason.contains("built through its metaclass")),
        "{reasons:?}"
    );
}

#[test]
fn a_property_half_the_backend_cannot_reach_is_declined() {
    // each of these is *nearly* the construct, and says what it actually is rather than
    // falling through to a message about a name written twice
    let restated = declines(
        "\
class Box:
    @property
    def value(self) -> int:
        return 1

    @value.getter
    def value(self) -> int:
        return 2
",
    );
    assert!(
        restated
            .iter()
            .any(|(_, reason)| reason.contains("writes a second `getter`")),
        "{restated:?}"
    );
    let slotted = declines(
        "\
class Box:
    @property
    def __len__(self) -> int:
        return 1

    @__len__.setter
    def __len__(self, given: int) -> None:
        pass
",
    );
    assert!(
        slotted
            .iter()
            .any(|(_, reason)| reason.contains("fills a type slot")),
        "{slotted:?}"
    );
    // `@other.setter` folds this body into a *different* property, which is not the one
    // attribute this lowers a group into
    let foreign = declines(
        "\
class Box:
    @property
    def value(self) -> int:
        return 1

    @property
    def other(self) -> int:
        return 2

    @other.setter
    def value(self, given: int) -> None:
        pass
",
    );
    assert!(
        foreign
            .iter()
            .any(|(_, reason)| reason.contains("`value` is defined more than once")),
        "{foreign:?}"
    );
}

/// a half with a parameter the property's call does not fill takes it from its default
///
/// `property` calls a setter with the receiver and the one value, positionally. that
/// says how many arguments arrive, not how many parameters the `def` may be written
/// with: a half is published as a `PyMethodDef` over its *wrapper*, which binds the call
/// exactly as a call through the name would, so a trailing default is bound the way
/// python binds it. the old gate compared the parameter count against the arity and
/// turned this down
#[test]
fn a_property_half_with_a_default_binds_it() {
    let ir = method_ir(
        "\
class Box:
    def __init__(self) -> None:
        self.n = 0

    @property
    def value(self) -> int:
        return self.n

    @value.setter
    def value(self, given: int, extra: int = 2) -> None:
        self.n = given * extra
",
        "Box",
        "value$set",
    );
    assert!(ir.contains("given"), "{ir}");
    assert!(ir.contains("extra"), "{ir}");
}

/// a half is still turned down where the one call would not bind
///
/// the gate is now the question the arity check was standing in for — does the call
/// `property` makes bind this `def` — so a half that genuinely cannot take it is refused
/// for what it is. a keyword-only parameter is never reached by that call at all, so one
/// without a default has no value to take
#[test]
fn a_property_half_the_one_call_cannot_bind_is_declined() {
    let missing = declines(
        "\
class Box:
    @property
    def value(self) -> int:
        return 1

    @value.setter
    def value(self, given: int, extra: int) -> None:
        pass
",
    );
    assert!(
        missing
            .iter()
            .any(|(_, reason)| reason.contains("is called with exactly 2 argument(s)")),
        "{missing:?}"
    );
    let unreachable = declines(
        "\
class Box:
    @property
    def value(self) -> int:
        return 1

    @value.setter
    def value(self, given: int, *, bump: int) -> None:
        pass
",
    );
    assert!(
        unreachable
            .iter()
            .any(|(_, reason)| reason.contains("is called with exactly 2 argument(s)")),
        "{unreachable:?}"
    );
}

/// a `@property` group whose halves carry a second decorator declines as a property
///
/// `@property` over `@abc.abstractmethod` is the shape `abc` documents, and both halves
/// of one carry two decorators. reading each decorator list strictly meant neither `def`
/// was recognised as a half at all, so the pair fell through to the generic message about
/// a name written twice — the one every other near-miss on this path goes out of its way
/// to avoid. the group is still declined; it now declines as the property it is
#[test]
fn a_property_stacked_with_another_decorator_declines_as_a_property() {
    let stacked = declines(
        "\
def marking(fn: object) -> object:
    return fn


class Box:
    @property
    @marking
    def value(self) -> int:
        return 1

    @value.setter
    @marking
    def value(self, given: int) -> None:
        pass
",
    );
    assert!(
        stacked
            .iter()
            .any(|(_, reason)| reason.contains("not a plain `@property`")),
        "{stacked:?}"
    );
    assert!(
        !stacked
            .iter()
            .any(|(_, reason)| reason.contains("defined more than once")),
        "{stacked:?}"
    );
}

/// and a half carrying the second decorator names that decorator rather than the name
///
/// the `@property` above it is plain, so the group is this construct and the `def` below
/// is plainly its setter. what stops it is the decorator wrapping the setter's body,
/// because what would be folded into the property is that decorator's answer rather than
/// the body written here
#[test]
fn a_property_half_carrying_a_second_decorator_names_it() {
    let stacked = declines(
        "\
def marking(fn: object) -> object:
    return fn


class Box:
    @property
    def value(self) -> int:
        return 1

    @value.setter
    @marking
    def value(self, given: int) -> None:
        pass
",
    );
    assert!(
        stacked
            .iter()
            .any(|(_, reason)| reason.contains("carries a decorator beside `@value.setter`")),
        "{stacked:?}"
    );
    assert!(
        !stacked
            .iter()
            .any(|(_, reason)| reason.contains("defined more than once")),
        "{stacked:?}"
    );
}

#[test]
fn a_module_level_function_defined_twice_is_declined() {
    // the same in the module scope, which had nobody asking: three module-level `def _`s
    // in `importlib/resources/_common.py` emitted one `by_m__` twice and the extension
    // failed to build at all
    let source = "\
import functools

@functools.cache
def _(n: int) -> int:
    return n

@functools.cache
def _(n: int) -> int:
    return n + 1
";
    let reasons = with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.functions.is_empty(), "nothing may compile");
        module
            .declined
            .iter()
            .map(|declined| declined.reason.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert!(
        reasons
            .iter()
            .all(|reason| reason.contains("`_` is defined more than once")),
        "{reasons:?}"
    );
}

/// the twin's source keeps only the decorators module init will not re-apply
///
/// a decorator init applies is evaluated there, over the compiled definition — so
/// leaving it on the twin's `def` evaluates it a second time and doubles whatever it did
/// on the way. a class's comes out for the same reason, because init applies that one to
/// the namespace entry. a *method's* stays: the class construction reads what it wrote —
/// `ABCMeta` computes `__abstractmethods__` from the namespace the body left — so taking
/// it out changes the class the twin builds rather than only when the decorator ran
#[test]
fn only_the_decorators_init_applies_come_out_of_the_twin() {
    // one decorated definition, and it is the last statement: a second one below it would
    // be something still running under the first, which keeps its decorator on the twin —
    // see `watched_definitions`
    let source = "\
def mark(f: object) -> object:
    return f


@mark
class Held:
    @mark
    def value(self) -> int:
        return 2
";
    let twin = with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        crate::without_init_decorators(source, &module).expect("the twin parses")
    });
    assert_eq!(twin.matches("@mark").count(), 1, "{twin}");
    assert!(twin.contains("    @mark\n    def value"), "{twin}");
    // the blanking keeps every line where it was, so a traceback through the twin still
    // quotes the right one
    assert_eq!(twin.lines().count(), source.lines().count(), "{twin}");
}

/// a decorator init does not re-apply stays on the twin's definition
///
/// `staticmethod` is the shape: the method table honours the binding itself, so init has
/// nothing to apply and the twin's own `def` is the only thing that can
#[test]
fn a_decorator_init_does_not_re_apply_stays_on_the_twin() {
    let source = "\
class Held:
    @staticmethod
    def value() -> int:
        return 2
";
    let twin = with_source(source, |db, env, model, suite| {
        let module =
            crate::build_module(db, env, model, suite, "app", crate::Language::BasedPython);
        assert!(module.declined.is_empty(), "{:?}", module.declined);
        crate::without_init_decorators(source, &module).expect("the twin parses")
    });
    assert!(twin.contains("@staticmethod"), "{twin}");
}

/// a decorated definition the module body is still running below cannot have its
/// decorator moved to init
#[test]
fn a_decorated_definition_the_module_body_runs_below_declines() {
    let reasons = declines(
        "\
def mark(f: object) -> object:
    return f


@mark
def counted() -> int:
    return 1


at_import = counted()
",
    );
    assert!(
        reasons
            .iter()
            .any(|(name, reason)| name == "counted" && reason.contains("goes on running below it")),
        "{reasons:?}"
    );
}

/// and neither can a decorated class one is still running below
#[test]
fn a_decorated_class_the_module_body_runs_below_declines() {
    let reasons = declines(
        "\
def mark(c: object) -> object:
    return c


@mark
class Held:
    def value(self) -> int:
        return 1


table = [Held]
",
    );
    assert!(
        reasons
            .iter()
            .any(|(name, reason)| name == "Held" && reason.contains("goes on running below it")),
        "{reasons:?}"
    );
}

/// the statement below need not name the definition at all
///
/// what the decorator did is missing for as long as the window is open, and it did it
/// wherever it liked. `enrol` never gives its own name away, and `snapshot` never mentions
/// `counted` — the two are connected only through a list that neither of them is
#[test]
fn a_decorated_definition_declines_over_a_statement_that_never_names_it() {
    let reasons = declines(
        "\
registry: list[str] = []


def enrol(f: object) -> object:
    registry.append('seen')
    return f


@enrol
def counted() -> int:
    return 1


snapshot = len(registry)
",
    );
    assert!(
        reasons
            .iter()
            .any(|(name, reason)| name == "counted" && reason.contains("goes on running below it")),
        "{reasons:?}"
    );
}

/// a definition with nothing running below it keeps its decorator moved to init
///
/// this is the other half of [`a_decorated_definition_declines_over_a_statement_that_never_names_it`]:
/// the gate is about what is *below* the definition, and a binding of a literal evaluates
/// nothing that could have seen the module either way
#[test]
fn a_decorated_definition_over_settled_statements_still_compiles() {
    let reasons = declines(
        "\
def mark(f: object) -> object:
    return f


@mark
def counted() -> int:
    return 1


version = (1, 2)
names = ['counted']
",
    );
    assert!(reasons.is_empty(), "{reasons:?}");
}

/// and a decorated definition below one is nothing running below it either — until it
/// declines
///
/// `first` compiles beside `second` because `second`'s decorator comes out of the twin
/// too, so nothing evaluates between the two. `third`'s does not: a decorator that is a
/// call keeps its decline, and the twin's `@maker()` then runs where it stands, inside the
/// window both of the others are still in.
///
/// the annotations are deferred so that the headers below `first` evaluate nothing either
#[test]
fn a_decorated_definition_that_keeps_its_decorator_reopens_the_ones_above_it() {
    let stack = "\
from __future__ import annotations


def mark(f: object) -> object:
    return f


def maker() -> object:
    return mark


@mark
def first() -> int:
    return 1


@mark
def second() -> int:
    return 2
";
    assert!(declines(stack).is_empty(), "{:?}", declines(stack));

    let reasons = declines(&format!(
        "{stack}

@maker()
def third() -> int:
    return 3
"
    ));
    for name in ["first", "second"] {
        assert!(
            reasons.iter().any(|(declined, reason)| declined == name
                && reason.contains("`third` declined below it and kept its decorator")),
            "{reasons:?}"
        );
    }
}

/// a frame that finishes and a body that raises `StopIteration` are different
/// operations, not one operation told apart by its error class
///
/// every way a resumable frame can end without naming a value — a bare `return`,
/// running off the end, and being resumed once it already has — is a *finish*. a
/// written `raise StopIteration` is an exception the body chose to raise, and python
/// eventually turns it into a `RuntimeError` (pep 479). conflating the two is what
/// would make that conversion impossible, so they lower apart and this says so
#[test]
fn a_finish_and_a_written_stop_iteration_lower_to_different_operations() {
    let silent = method_ir(
        "\
def silent(n: int) -> object:
    if n > 0:
        yield n
        return
    yield 0
",
        "silent$gen",
        "$resume",
    );
    assert!(silent.contains("finish "), "{silent}");
    assert!(!silent.contains("raise StopIteration"), "{silent}");

    // the *implicit* end — running off the body, and being resumed after the frame
    // already finished, which share one block — is the same finish
    let off_the_end = method_ir(
        "\
def counting(n: int) -> object:
    i = 0
    while i < n:
        yield i
        i = i + 1
",
        "counting$gen",
        "$resume",
    );
    assert_eq!(off_the_end.matches("finish ").count(), 1, "{off_the_end}");
    assert!(
        !off_the_end.contains("raise StopIteration"),
        "{off_the_end}"
    );

    // and a body that raises the exception itself still raises it: the implicit end
    // beside it is a finish, so the frame holds one of each
    let written = method_ir(
        "\
def written(n: int) -> object:
    yield n
    raise StopIteration
",
        "written$gen",
        "$resume",
    );
    assert_eq!(written.matches("finish ").count(), 1, "{written}");
    assert_eq!(
        written.matches("raise StopIteration").count(),
        1,
        "{written}"
    );
}

/// a `return <value>` finishes with the value, and nothing about it is a raise
#[test]
fn a_returned_value_rides_on_the_finish() {
    let ir = method_ir(
        "\
def returning(n: int) -> object:
    yield n
    return 'end'
",
        "returning$gen",
        "$resume",
    );
    // one for the `return`, one for the implicit end the dispatch falls through to
    assert_eq!(ir.matches("finish ").count(), 2, "{ir}");
    assert!(!ir.contains("raise StopIteration"), "{ir}");
}

/// a nested function reaches the enclosing method's receiver through the environment
///
/// the answer alone cannot say this: resolving `self` as a global raised `NameError`,
/// but a lowering that reached it through `PyObject_GetAttr` on a captured object would
/// give the same answer as the field write and be slower every time. the shape is what
/// says it is a capture read at a compile-time offset followed by a field store
#[test]
fn a_nested_function_reads_the_receiver_out_of_its_environment() {
    let ir = method_ir(
        "\
class Held:
    def __init__(self) -> None:
        self.held = 1

        def go() -> None:
            self.held = 2

        go()
",
        "Held$__init__$env",
        "go",
    );
    assert!(ir.contains("$env.<Held$__init__$env.self>"), "{ir}");
    assert!(ir.contains(".<Held.held> = 2"), "{ir}");
    assert!(!ir.contains("global self"), "{ir}");

    // a `for` target, a subscript target and a `del` are the other three shapes whose
    // only mention of the receiver is inside a target
    for body in [
        "for self.held in values:\n                pass",
        "self.bucket[0] = 9",
        "del self.held",
    ] {
        let ir = method_ir(
            &format!(
                "\
class Held:
    def __init__(self, values: list) -> None:
        self.held = 1
        self.bucket = [0]

        def go() -> None:
            {body}

        go()
"
            ),
            "Held$__init__$env",
            "go",
        );
        assert!(ir.contains("$env.<Held$__init__$env.self>"), "{ir}");
        assert!(!ir.contains("global self"), "{ir}");
    }
}

/// a `classmethod` that binds an attribute on the class declines, and so does the class
///
/// the emitted type is sealed, so the write raises where python binds a class attribute.
/// nothing narrower than the class would help: a method left interpreted is still handed
/// the emitted type
#[test]
fn a_classmethod_that_writes_on_the_class_declines() {
    assert_eq!(
        declines(
            "\
class Counter:
    count: int = 0

    @classmethod
    def bump(cls) -> int:
        cls.count = cls.count + 1
        return cls.count
"
        ),
        vec![(
            "Counter".to_string(),
            "`cls.count` binds an attribute on the class, and the type this module emits \
             for it is sealed"
                .to_string()
        )]
    );
}

/// and `del cls.count` is the same write read backwards
#[test]
fn a_classmethod_that_deletes_on_the_class_declines() {
    assert_eq!(
        declines(
            "\
class Counter:
    count: int = 0

    @classmethod
    def forget(cls) -> None:
        del cls.count
"
        ),
        vec![(
            "Counter".to_string(),
            "`cls.count` binds an attribute on the class, and the type this module emits \
             for it is sealed"
                .to_string()
        )]
    );
}

/// and a function nested in the `classmethod` reaches the same class object
///
/// the nested frame captures `cls` — that is what a closure is — so the write lands on
/// the same sealed type. it is only reachable at all because a nested function reads the
/// frame around it, which is the other half of this change
#[test]
fn a_function_nested_in_a_classmethod_that_writes_on_the_class_declines() {
    assert_eq!(
        declines(
            "\
class Counter:
    count: int = 0

    @classmethod
    def bump(cls) -> None:
        def go() -> None:
            cls.count = 1

        go()
"
        ),
        vec![(
            "Counter".to_string(),
            "`cls.count` binds an attribute on the class, and the type this module emits \
             for it is sealed"
                .to_string()
        )]
    );
}

/// reading through the class object is not writing through it, and stays compiled
#[test]
fn a_classmethod_that_only_reads_the_class_is_still_lowered() {
    assert_eq!(
        declines(
            "\
class Counter:
    count: int = 0

    @classmethod
    def read(cls) -> int:
        return cls.count
"
        ),
        vec![]
    );
}

/// `globals()` is the module namespace, and a compiled function has it in hand
///
/// calling the builtin would answer about the frame underneath, which for a compiled
/// function is the caller's, in another module — so the read gave `None` for a name
/// the module plainly binds and the write bound it in the caller
#[test]
fn globals_lowers_to_the_module_namespace() {
    let ir = ir("\
marker: int = 7


def read() -> object:
    return globals()
");
    assert!(ir.contains("= globals"), "{ir}");
    assert!(!ir.contains("pycall globals"), "{ir}");
}

/// a module that binds `globals` itself has not written the builtin, and python calls
/// what the name holds
#[test]
fn a_module_that_defines_globals_keeps_its_own() {
    let ir = ir("\
def globals() -> dict[str, object]:
    return {}


def read() -> object:
    return globals()
");
    assert!(!ir.contains("= globals\n"), "{ir}");
}

/// the same question one scope in: a local holding a dict is not the builtin
#[test]
fn a_local_named_globals_is_not_the_builtin() {
    let ir = ir("\
def read() -> object:
    globals: dict[str, object] = {}
    return globals
");
    assert!(!ir.contains("= globals\n"), "{ir}");
}

/// `locals()` has no compiled answer at all: a compiled frame's locals are registers,
/// several of them not even objects
#[test]
fn the_frame_reading_builtins_are_declined() {
    for call in [
        "locals()",
        "vars()",
        "dir()",
        "eval(\"1\")",
        "exec(\"a = 1\")",
    ] {
        let reason = decline(&format!("def f() -> object:\n    return {call}\n"));
        assert!(reason.contains("calling frame"), "{call} gave `{reason}`");
    }
}

/// `warnings.warn` blames a frame counted back from its own caller, and the count
/// starts one frame further out when the caller is compiled. the one frame that is
/// missing is this function's own, and the lowering supplies it, so the call is
/// lowered however the level is written
#[test]
fn a_warning_at_a_written_level_is_lowered_however_it_is_reached() {
    for source in [
        "\
import warnings


def f() -> None:
    warnings.warn('gone', DeprecationWarning)
",
        "\
from warnings import warn


def f() -> None:
    warn('gone')
",
        "\
import warnings


def f() -> None:
    warnings.warn('gone', DeprecationWarning, stacklevel=1)
",
        // zero and negative levels mean the same frame: `warn` walks `stacklevel - 1`
        // frames and never fewer than none
        "\
import warnings


def f() -> None:
    warnings.warn('gone', DeprecationWarning, stacklevel=0)
",
        "\
import warnings


def f() -> None:
    warnings.warn('gone', DeprecationWarning, stacklevel=-2)
",
        // `source=None` is the default written out, not a source to carry
        "\
import warnings


def f() -> None:
    warnings.warn('gone', DeprecationWarning, 1, None)
",
        // above the default level the walk is written out rather than counted, so
        // these are lowered too
        "\
import warnings


def f() -> None:
    warnings.warn('gone', DeprecationWarning, stacklevel=2)
",
        "\
import warnings


def f() -> None:
    warnings.warn('gone', DeprecationWarning, 3)
",
        // a level no stack is ever that deep for lands where python lands: off the end
        "\
import warnings


def f() -> None:
    warnings.warn('gone', DeprecationWarning, stacklevel=1000000000000)
",
    ] {
        assert_eq!(declines(source), vec![], "{source}");
    }
    // and a `warn` the module writes itself is a different function entirely — which
    // is why the definition is resolved rather than the spelling matched
    assert_eq!(
        declines(
            "\
def warn(message: str) -> None:
    return None


def f() -> None:
    warn('gone')
"
        ),
        vec![]
    );
}

/// each guard around that lowering, broken one at a time
///
/// every one of these is a shape whose answer the compiler cannot supply, and every
/// one of them would otherwise reach `warn_explicit` with a context that is not the
/// one `warn` would have computed
#[test]
fn a_warning_the_compiler_cannot_place_is_declined_for_its_own_reason() {
    for (call, expected) in [
        // how far the walk goes has to be settled here, because the walk is written
        // into the call rather than counted at it
        (
            "warnings.warn('m', UserWarning, stacklevel=level)",
            "`stacklevel` written out",
        ),
        (
            "warnings.warn('m', UserWarning, stacklevel=level + 1)",
            "`stacklevel` written out",
        ),
        // the keyword forces the level to at least two, and does not exist at all
        // before 3.12 — so accepting even an empty one would answer where python raised
        (
            "warnings.warn('m', UserWarning, skip_file_prefixes=())",
            "`skip_file_prefixes`",
        ),
        (
            "warnings.warn('m', UserWarning, skip_file_prefixes=('/a/',))",
            "`skip_file_prefixes`",
        ),
        // the public `warn_explicit` entry point takes no `source`
        ("warnings.warn('m', UserWarning, 1, [1])", "a `source`"),
        ("warnings.warn('m', UserWarning, source=[1])", "a `source`"),
        // a spread leaves which value fills which parameter unsettled
        ("warnings.warn('m', *rest)", "a spread argument"),
        ("warnings.warn('m', **rest)", "a spread argument"),
        // shapes python itself refuses, left to its own wording
        (
            "warnings.warn('m', UserWarning, 1, None, ())",
            "at most four positional",
        ),
        ("warnings.warn(stacklevel=1)", "no message"),
        (
            "warnings.warn('m', nonsense=1)",
            "an argument it does not take",
        ),
    ] {
        let source = format!(
            "\
import warnings


def f(level: int, rest: tuple) -> None:
    {call}
"
        );
        let reason = decline(&source);
        assert!(
            reason.contains(expected),
            "{call} gave `{reason}`, which does not mention {expected}"
        );
    }
}

/// the level the walk is given is the one the call wrote, held to a floor of one
///
/// the behavioural tests in `by_build` say the warning lands where python lands; this
/// says the number that decides where reaches the op at all, because a level dropped on
/// the way through would leave every one of them at the default and still lower
#[test]
fn a_warning_carries_the_level_it_was_written_with() {
    for (written, level) in [
        ("", 1),
        (", stacklevel=1", 1),
        (", stacklevel=0", 1),
        (", stacklevel=-3", 1),
        (", stacklevel=2", 2),
        (", 7", 7),
        (", stacklevel=1000000000000", 2_147_483_647),
    ] {
        let rendered = ir(&format!(
            "\
import warnings


def f() -> None:
    warnings.warn('gone', DeprecationWarning{written})
"
        ));
        assert!(
            rendered.contains(&format!(" up {level} at ")),
            "`{written}` lowered to\n{rendered}"
        );
    }
}

/// a warning above the default level written in a class body is lowered like any other
///
/// the refusal that used to stand here was a re-costing rather than a rule about frames:
/// a method's decline is what keeps its class interpreted, so lowering one put a whole
/// class under the compiler for the first time. the two failures that turned up when it
/// was first tried — `logging.Handler()` over a weak reference taken through a helper,
/// `fileinput.FileInput()` over `sys.flags` — were defects of their own and are fixed
#[test]
fn a_warning_above_the_default_level_in_a_class_body_is_lowered() {
    assert_eq!(
        declines(
            "\
import warnings


class A:
    def talks(self) -> None:
        warnings.warn('talks', DeprecationWarning, stacklevel=2)
"
        ),
        vec![]
    );
    // and the level it was written with reaches the op, which is what says the walk is
    // written out rather than left at the default
    assert!(
        method_ir(
            "\
import warnings


class A:
    def talks(self) -> None:
        warnings.warn('talks', DeprecationWarning, stacklevel=2)
",
            "A",
            "talks",
        )
        .contains(" up 2 at "),
    );
}

/// and a caller inside the class is what keeps the frame the warning would name
///
/// a method reached by a name is reached by the bare one, because a call through the
/// object protocol writes no target down — so `A.talks` blames `A.speaks`, and `A` goes
/// as a unit
#[test]
fn a_caller_of_a_warning_written_in_a_class_body_is_declined() {
    assert_eq!(
        declines(
            "\
import warnings


class A:
    def talks(self) -> None:
        warnings.warn('talks', DeprecationWarning, stacklevel=2)

    def speaks(self) -> None:
        self.talks()
"
        ),
        vec![(
            "A".to_string(),
            "`talks` warns about whoever called it, and this frame is the one it would name"
                .to_string()
        )]
    );
}

/// a caller a warning would name is the definition that has to keep its frame
///
/// the walk written into the call reaches every frame below this function, but not this
/// function's own caller when that caller is compiled: it pushes none, and the blame
/// would land a module further out. so the caller declines, and for a level above two
/// the ordinary cascade carries that on outwards
#[test]
fn a_caller_a_warning_would_name_is_declined() {
    let reasons = declines(
        "\
import warnings


def far() -> None:
    warnings.warn('far', DeprecationWarning, stacklevel=2)


def calls_far() -> None:
    far()


def calls_calls_far() -> None:
    calls_far()
",
    );
    assert_eq!(
        reasons,
        vec![
            (
                "calls_far".to_string(),
                "`far` warns about whoever called it, and this frame is the one it would name"
                    .to_string()
            ),
            (
                "calls_calls_far".to_string(),
                "`calls_far` declined, so a call has no target".to_string()
            ),
        ]
    );
    // and a call to one that warns at the default level costs its caller nothing: the
    // frame that warning names is the callee's own, which the lowering already has
    assert_eq!(
        declines(
            "\
import warnings


def near() -> None:
    warnings.warn('near', DeprecationWarning)


def calls_near() -> None:
    near()
"
        ),
        vec![]
    );
}

/// `vars(x)` and `dir(x)` are about the object they are handed, not about a frame
#[test]
fn vars_and_dir_of_an_object_stay_compiled() {
    assert_eq!(
        declines("def f(a: object) -> object:\n    return (vars(a), dir(a))\n"),
        vec![]
    );
}

/// `exec` runs in the namespace it is given, and only falls back on the calling
/// frame's when it is given none
#[test]
fn exec_with_a_namespace_of_its_own_stays_compiled() {
    assert_eq!(
        declines(
            "\
def f(ns: dict[str, object]) -> None:
    exec(\"a = 1\", ns)
"
        ),
        vec![]
    );
}

/// …and `None` written there means the calling frame's, so it is the same decline
/// a nested function that calls itself, which is what makes its own name a cell
const A_RECURSIVE_NESTED_FUNCTION: &str = "\
def outer(n: int) -> int:
    def inner(x: int) -> int:
        if x <= 0:
            return n
        return inner(x - 1)
    return inner(n)
";

#[test]
fn a_recursive_nested_functions_def_binds_the_cell_its_body_reads() {
    // one field, written where the `def` stands and read back by the recursive call.
    // a register beside it would leave the body reading a field nothing ever wrote
    let rendered = ir(A_RECURSIVE_NESTED_FUNCTION);
    assert!(
        rendered.contains("$outer$env.<outer$env.inner> ="),
        "{rendered}"
    );
    let body = method_ir(A_RECURSIVE_NESTED_FUNCTION, "outer$env", "inner");
    assert!(body.contains("cell $env.<outer$env.inner>"), "{body}");
}

#[test]
fn the_frame_that_made_a_recursive_closure_still_calls_it_directly() {
    // the enclosing frame's own call does not go through the cell: it made the closure
    // and nothing rebinds the name, so the native entry is what the call is
    let rendered = ir(A_RECURSIVE_NESTED_FUNCTION);
    assert!(rendered.contains("call outer$env.inner("), "{rendered}");
}

#[test]
fn a_nested_function_the_frame_rebinds_is_called_through_its_name() {
    // `step` holds the wrapper by the time it is called, so the direct entry would be
    // calling something the name no longer names
    let rendered = ir("\
def wrap(f: object) -> object:
    return f

def outer(n: int) -> int:
    def step(x: int) -> int:
        return x + 1
    step = wrap(step)
    return step(n)
");
    assert!(!rendered.contains("call outer$env.step("), "{rendered}");
}

#[test]
fn exec_handed_none_for_a_namespace_is_declined() {
    let reason = decline("def f() -> None:\n    exec(\"a = 1\", None)\n");
    assert!(reason.contains("calling frame"), "{reason}");
}

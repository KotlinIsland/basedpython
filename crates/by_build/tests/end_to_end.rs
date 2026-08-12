//! end-to-end: BIR → C → a loadable extension → imported and called by cpython
//!
//! this is the test the whole vertical slice exists to pass. every layer below it
//! can be green while the stack as a whole is broken, because only cpython can
//! say whether the emitted C really produces an importable module that computes
//! the right answers.
//!
//! the tests skip (rather than fail) when no interpreter or C toolchain is
//! available, so a machine without one does not report a false failure.

#![expect(
    clippy::print_stderr,
    reason = "skip notices belong on the test harness's stderr"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use by_build::{Options, Toolchain, build_module, build_source};
use by_ir::builder::FunctionBuilder;
use by_ir::function::{CallConvention, ModuleIr};
use by_ir::ops::{BinOp, CmpOp, Op, Terminator, Value};
use by_ir::rtype::RType;

mod common;

/// an interpreter and its build settings, or `None` when this machine cannot run
/// the test at all
/// a test whose *source* needs a newer interpreter than this one has nothing to
/// say: neither leg can run it, so there is nothing to compare
fn supports(toolchain: &Toolchain, least: (u8, u8)) -> bool {
    toolchain.version.is_none_or(|version| version >= least)
}

fn environment() -> Option<(String, Toolchain)> {
    let python = match std::env::var("PYTHON") {
        Ok(python) => python,
        Err(_) => ["python3", "python"]
            .into_iter()
            .find(|candidate| {
                Command::new(candidate)
                    .arg("--version")
                    .output()
                    .is_ok_and(|out| out.status.success())
            })?
            .to_string(),
    };
    let toolchain = Toolchain::probe(&python).ok()?;
    Some((python, toolchain))
}

/// build `module` into a fresh directory, or `None` when there is no working C
/// compiler on this machine
fn built(module: &ModuleIr, toolchain: &Toolchain, tag: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("by_e2e_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    match build_module(module, toolchain, &dir) {
        Ok(artifact) => {
            assert!(artifact.extension.exists(), "the extension was written");
            Some(dir)
        }
        Err(error) => {
            let text = error.to_string();
            assert!(
                !text.contains("rejected the generated code"),
                "{tag}: the C compiler rejected the generated code:\n{text}"
            );
            eprintln!("skipping {tag}: no working C toolchain ({error})");
            None
        }
    }
}

/// run a python snippet with the build directory on `sys.path`, returning stdout
fn script(python: &str, dir: &Path, body: &str) -> String {
    common::python_output(python, dir, body)
}

/// import `module` and print the `repr` of `expression`
fn eval(python: &str, dir: &Path, module: &str, expression: &str) -> String {
    script(
        python,
        dir,
        &format!("import {module}\nprint(repr({expression}))\n"),
    )
}

/// `def arith(a: int, b: int) -> int: return (a + b) * a - b`
fn arith_module() -> ModuleIr {
    let mut builder = FunctionBuilder::new("arith", RType::INT);
    let a = builder.param("a", RType::INT);
    let b = builder.param("b", RType::INT);
    let sum = builder.temp(RType::INT);
    let scaled = builder.temp(RType::INT);
    let result = builder.temp(RType::INT);
    builder.push(Op::IntBinary {
        dest: sum,
        op: BinOp::Add,
        lhs: Value::Register(a),
        rhs: Value::Register(b),
    });
    builder.push(Op::IntBinary {
        dest: scaled,
        op: BinOp::Mul,
        lhs: Value::Register(sum),
        rhs: Value::Register(a),
    });
    builder.push(Op::IntBinary {
        dest: result,
        op: BinOp::Sub,
        lhs: Value::Register(scaled),
        rhs: Value::Register(b),
    });
    builder.terminate(Terminator::Return(Value::Register(result)));

    ModuleIr {
        name: "by_e2e_arith".to_string(),
        functions: vec![builder.finish()],
        declined: Vec::new(),
        classes: Vec::new(),
        gradual: Vec::new(),
        promoted: Vec::new(),
        lines: None,
        fallback_source: None,
    }
}

/// `def fib(n: int) -> int` — iterative, exercising loops and comparisons
fn fib_module() -> ModuleIr {
    let mut builder = FunctionBuilder::new("fib", RType::INT);
    let n = builder.param("n", RType::INT);
    let a = builder.local("a", RType::INT);
    let b = builder.local("b", RType::INT);
    let i = builder.local("i", RType::INT);
    let cond = builder.temp(RType::BIT);
    let next = builder.temp(RType::INT);

    builder.assign(a, Value::Int(0));
    builder.assign(b, Value::Int(1));
    builder.assign(i, Value::Int(0));

    let header = builder.new_block();
    let body = builder.new_block();
    let exit = builder.new_block();
    builder.terminate(Terminator::Goto(header));

    builder.switch_to(header);
    builder.push(Op::IntCompare {
        dest: cond,
        op: CmpOp::Lt,
        lhs: Value::Register(i),
        rhs: Value::Register(n),
    });
    builder.terminate(Terminator::Branch {
        cond: Value::Register(cond),
        then_block: body,
        else_block: exit,
    });

    builder.switch_to(body);
    builder.push(Op::IntBinary {
        dest: next,
        op: BinOp::Add,
        lhs: Value::Register(a),
        rhs: Value::Register(b),
    });
    builder.assign(a, Value::Register(b));
    builder.assign(b, Value::Register(next));
    builder.push(Op::IntBinary {
        dest: i,
        op: BinOp::Add,
        lhs: Value::Register(i),
        rhs: Value::Int(1),
    });
    builder.terminate(Terminator::Goto(header));

    builder.switch_to(exit);
    builder.terminate(Terminator::Return(Value::Register(a)));

    ModuleIr {
        name: "by_e2e_fib".to_string(),
        functions: vec![builder.finish()],
        declined: Vec::new(),
        classes: Vec::new(),
        gradual: Vec::new(),
        promoted: Vec::new(),
        lines: None,
        fallback_source: None,
    }
}

#[test]
fn integer_arithmetic_computes_the_same_answers_as_python() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(dir) = built(&arith_module(), &toolchain, "arith") else {
        return;
    };

    for (a, b) in [(3_i64, 4_i64), (0, 0), (-5, 7), (100, -100)] {
        let expected = (a + b) * a - b;
        let actual = eval(
            &python,
            &dir,
            "by_e2e_arith",
            &format!("by_e2e_arith.arith({a}, {b})"),
        );
        assert_eq!(actual, expected.to_string(), "arith({a}, {b})");
    }
}

#[test]
fn arbitrary_precision_survives_the_tagged_representation() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(dir) = built(&arith_module(), &toolchain, "bigint") else {
        return;
    };

    // the product leaves the tagged fast path and must fall through to the boxed
    // one rather than wrapping
    let big = 4_611_686_018_427_387_903_i128; // 2^62 - 1
    let expected = (big + 1) * big - 1;
    let actual = eval(
        &python,
        &dir,
        "by_e2e_arith",
        &format!("by_e2e_arith.arith({big}, 1)"),
    );
    assert_eq!(actual, expected.to_string());
}

#[test]
fn a_loop_with_comparisons_runs_natively() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(dir) = built(&fib_module(), &toolchain, "fib") else {
        return;
    };

    let actual = eval(
        &python,
        &dir,
        "by_e2e_fib",
        "[by_e2e_fib.fib(n) for n in range(12)]",
    );
    assert_eq!(actual, "[0, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 89]");

    // far past the tagged range, so most additions take the boxed path
    let actual = eval(&python, &dir, "by_e2e_fib", "by_e2e_fib.fib(200)");
    assert_eq!(actual, "280571172992510140037611932413038677189525");
}

#[test]
fn a_wrong_argument_type_raises_rather_than_misreading_memory() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(dir) = built(&arith_module(), &toolchain, "typecheck") else {
        return;
    };

    // the wrapper's unbox is the `parameters` soundness position: a caller that
    // lies about the type gets a TypeError, never undefined behaviour
    let out = script(
        &python,
        &dir,
        "import by_e2e_arith\n\
         try:\n    by_e2e_arith.arith('x', 1)\n\
         except TypeError as e:\n    print('TypeError:', e)\n\
         else:\n    print('no error')\n",
    );
    assert_eq!(out, "TypeError: expected int, got str");
}

#[test]
fn a_wrong_argument_count_raises() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(dir) = built(&arith_module(), &toolchain, "arity") else {
        return;
    };

    let out = script(
        &python,
        &dir,
        "import by_e2e_arith\n\
         try:\n    by_e2e_arith.arith(1)\n\
         except TypeError:\n    print('caught')\n\
         else:\n    print('no error')\n",
    );
    assert_eq!(out, "caught");
}

#[test]
fn division_floors_like_python_and_raises_on_zero() {
    let Some((python, toolchain)) = environment() else {
        return;
    };

    let mut builder = FunctionBuilder::new("div", RType::INT);
    let a = builder.param("a", RType::INT);
    let b = builder.param("b", RType::INT);
    let out = builder.temp(RType::INT);
    builder.push(Op::IntBinary {
        dest: out,
        op: BinOp::FloorDiv,
        lhs: Value::Register(a),
        rhs: Value::Register(b),
    });
    builder.terminate(Terminator::Return(Value::Register(out)));
    let module = ModuleIr {
        name: "by_e2e_div".to_string(),
        functions: vec![builder.finish()],
        declined: Vec::new(),
        classes: Vec::new(),
        gradual: Vec::new(),
        promoted: Vec::new(),
        lines: None,
        fallback_source: None,
    };
    let Some(dir) = built(&module, &toolchain, "divzero") else {
        return;
    };

    // python floors rather than truncating, so the mixed-sign cases are the test.
    // the expected values are cpython's own, written out rather than recomputed
    // with the same formula the runtime uses
    for (a, b, expected) in [(7_i64, 2_i64, 3_i64), (-7, 2, -4), (7, -2, -4), (-7, -2, 3)] {
        let actual = eval(
            &python,
            &dir,
            "by_e2e_div",
            &format!("by_e2e_div.div({a}, {b})"),
        );
        assert_eq!(actual, expected.to_string(), "div({a}, {b})");
    }

    let out = script(
        &python,
        &dir,
        "import by_e2e_div\n\
         try:\n    by_e2e_div.div(1, 0)\n\
         except ZeroDivisionError:\n    print('caught')\n\
         else:\n    print('no error')\n",
    );
    assert_eq!(out, "caught");
}

#[test]
fn floats_are_unboxed_and_exclude_int() {
    let Some((python, toolchain)) = environment() else {
        return;
    };

    let mut builder = FunctionBuilder::new("scale", RType::FLOAT);
    builder.convention(CallConvention::NativeInfallible);
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
    let module = ModuleIr {
        name: "by_e2e_float".to_string(),
        functions: vec![builder.finish()],
        declined: Vec::new(),
        classes: Vec::new(),
        gradual: Vec::new(),
        promoted: Vec::new(),
        lines: None,
        fallback_source: None,
    };
    let Some(dir) = built(&module, &toolchain, "float") else {
        return;
    };

    assert_eq!(
        eval(
            &python,
            &dir,
            "by_e2e_float",
            "by_e2e_float.scale(1.5, 4.0)"
        ),
        "6.0"
    );

    // `.by`'s float does not admit int — features/no-number-promotions.md
    let out = script(
        &python,
        &dir,
        "import by_e2e_float\n\
         try:\n    by_e2e_float.scale(2, 4.0)\n\
         except TypeError as e:\n    print(e)\n\
         else:\n    print('accepted an int')\n",
    );
    assert_eq!(out, "expected float, got int");
}

#[test]
fn calls_between_compiled_functions_stay_native() {
    let Some((python, toolchain)) = environment() else {
        return;
    };

    let mut double = FunctionBuilder::new("double", RType::INT);
    let d = double.param("n", RType::INT);
    let doubled = double.temp(RType::INT);
    double.push(Op::IntBinary {
        dest: doubled,
        op: BinOp::Add,
        lhs: Value::Register(d),
        rhs: Value::Register(d),
    });
    double.terminate(Terminator::Return(Value::Register(doubled)));

    let mut quad = FunctionBuilder::new("quad", RType::INT);
    let q = quad.param("n", RType::INT);
    let once = quad.temp(RType::INT);
    let twice = quad.temp(RType::INT);
    quad.push(Op::CallNative {
        owner: None,
        dest: Some(once),
        callee: "double".to_string(),
        args: vec![Value::Register(q)],
    });
    quad.push(Op::CallNative {
        owner: None,
        dest: Some(twice),
        callee: "double".to_string(),
        args: vec![Value::Register(once)],
    });
    quad.terminate(Terminator::Return(Value::Register(twice)));

    let module = ModuleIr {
        name: "by_e2e_call".to_string(),
        functions: vec![double.finish(), quad.finish()],
        declined: Vec::new(),
        classes: Vec::new(),
        gradual: Vec::new(),
        promoted: Vec::new(),
        lines: None,
        fallback_source: None,
    };
    let Some(dir) = built(&module, &toolchain, "call") else {
        return;
    };

    assert_eq!(
        eval(&python, &dir, "by_e2e_call", "by_e2e_call.quad(5)"),
        "20"
    );
}

#[test]
fn repeated_calls_do_not_leak_the_boxed_representation() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(dir) = built(&fib_module(), &toolchain, "refcount") else {
        return;
    };

    // fib(200) allocates a PyLongObject on nearly every iteration. if the
    // ownership discipline is wrong, the live object count climbs without bound
    let out = script(
        &python,
        &dir,
        "import gc, by_e2e_fib\n\
         for _ in range(50): by_e2e_fib.fib(200)\n\
         gc.collect(); before = len(gc.get_objects())\n\
         for _ in range(500): by_e2e_fib.fib(200)\n\
         gc.collect(); after = len(gc.get_objects())\n\
         print('stable' if after <= before + 100 else f'grew {before}->{after}')\n",
    );
    assert_eq!(out, "stable");
}

#[test]
fn a_returned_value_outlives_the_frame_that_made_it() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(dir) = built(&fib_module(), &toolchain, "outlives") else {
        return;
    };

    // the frame releases every register on the way out, so a returned big int
    // survives only because it was retained first
    let out = script(
        &python,
        &dir,
        "import by_e2e_fib\n\
         values = [by_e2e_fib.fib(200) for _ in range(100)]\n\
         print(len({str(v) for v in values}), values[0] == values[-1])\n",
    );
    assert_eq!(out, "1 True");
}

#[test]
fn annotate_writes_a_report_beside_the_generated_c() {
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_e2e_annotate");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def scale(x: float) -> float:
    return x * 2.0

def slow(a: int) -> None:
    try:
        pass
    except* ValueError:
        pass
";
    let Ok(built) = build_source(
        source,
        "by_e2e_annotate",
        &toolchain,
        &dir,
        &Options {
            annotate: true,
            ..Options::default()
        },
    ) else {
        eprintln!("skipping: no working C toolchain");
        return;
    };
    let path = built.artifact.annotation.expect("a report was written");
    assert_eq!(path.parent(), built.artifact.source.parent());
    let report = std::fs::read_to_string(&path).expect("the report is readable");
    assert!(
        report.contains("1 compiled, 1 left interpreted"),
        "{report}"
    );
    assert!(report.contains("- slow: `except*`"), "{report}");
    assert!(report.contains("infallible"), "{report}");
}

#[test]
fn an_accumulated_string_is_grown_rather_than_copied() {
    // `agree` cannot say which build answered, and both builds answer the same
    // string either way — so the only way to know the accumulator is being grown in
    // place is to read what was emitted
    let dir = std::env::temp_dir().join("by_e2e_append");
    let _ = std::fs::remove_dir_all(&dir);
    let built = by_build::emit_source(
        "\
def build(n: int, piece: str) -> str:
    out = \"\"
    i = 0
    while i < n:
        out = out + piece
        i = i + 1
    return out

def kept(a: str, b: str) -> object:
    joined = a + b
    return (joined, a)
",
        "by_e2e_append",
        &dir,
        &Options::default(),
    )
    .expect("the module emits");
    let emitted = std::fs::read_to_string(&built.artifact.source).expect("the C is readable");
    let appends = emitted.matches("By_StrAppend(").count();
    let copies = emitted.matches("By_StrConcat(").count();
    // one per copy of the loop body — `unswitch` duplicates it
    assert!(
        appends >= 1,
        "the accumulator was copied every step:\n{emitted}"
    );
    // `a` is read again after the concatenation in `kept`, so that one still copies
    assert_eq!(copies, 1, "{emitted}");
}

#[test]
fn no_annotation_is_written_unless_it_is_asked_for() {
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_e2e_noannotate");
    let _ = std::fs::remove_dir_all(&dir);
    let Ok(built) = build_source(
        "def f(a: int) -> int:\n    return a\n",
        "by_e2e_noannotate",
        &toolchain,
        &dir,
        &Options::default(),
    ) else {
        eprintln!("skipping: no working C toolchain");
        return;
    };
    assert!(built.artifact.annotation.is_none());
}

#[test]
fn the_fallback_honours_a_supplied_transpiler_config() {
    // a declined function *runs* from the embedded source, so a build that means
    // to insert extra soundness checks has to insert them there too — otherwise
    // the two halves of one module disagree about what they check
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_e2e_fallbackconfig");
    let _ = std::fs::remove_dir_all(&dir);
    // `except*` has no lowering, so this declines and *runs* from the fallback —
    // which is exactly the code the checks have to reach
    let source = "\
def total(a: list[int]) -> int:
    try:
        pass
    except* ValueError:
        pass
    return len(a)
";
    let config = by_transforms::Config {
        soundness: by_transforms::SoundnessPositions::all(),
        ..by_transforms::Config::default()
    };
    let Ok(built) = build_source(
        source,
        "by_e2e_fallbackconfig",
        &toolchain,
        &dir,
        &Options {
            fallback: Some(config),
            ..Options::default()
        },
    ) else {
        eprintln!("skipping: no working C toolchain");
        return;
    };
    assert_eq!(built.declined.len(), 1);
    let c = std::fs::read_to_string(&built.artifact.source).expect("the C is readable");
    assert!(
        c.contains("_soundness_check"),
        "the fallback carries the checks"
    );

    // and the default does not, so the flag is what made the difference
    let plain = std::env::temp_dir().join("by_e2e_fallbackplain");
    let _ = std::fs::remove_dir_all(&plain);
    let built = build_source(
        source,
        "by_e2e_fallbackplain",
        &toolchain,
        &plain,
        &Options::default(),
    )
    .expect("the toolchain already worked");
    let c = std::fs::read_to_string(&built.artifact.source).expect("the C is readable");
    assert!(!c.contains("_soundness_check"), "{c}");
}

/// compile one stdlib module and require only that it *built*
///
/// a decline is fine — that is the design. what is *not* fine is ill-formed ir or c the
/// compiler rejects, because both mean the lowering produced something it cannot stand
/// behind.
///
/// it found two real bugs the moment it was pointed at anything: a `str` key subscript
/// producing ill-formed ir, and an unannotated parameter with a default being given a
/// `double` representation — both of which failed the *build*, which is the property
/// this pins
fn a_stdlib_module_compiles_without_a_hard_failure(name: &str) {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 12)) {
        return;
    }
    let stdlib = Command::new(&python)
        .args([
            "-c",
            "import sysconfig;print(sysconfig.get_paths()['stdlib'])",
        ])
        .output()
        .expect("python answers");
    let stdlib = PathBuf::from(String::from_utf8_lossy(&stdlib.stdout).trim());
    let module = format!(
        "by_corpus_{}",
        name.trim_end_matches(".py").replace('/', "_")
    );
    let dir = std::env::temp_dir().join(format!("by_e2e_corpus_{module}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let Ok(source) = std::fs::read_to_string(stdlib.join(name)) else {
        return;
    };
    match build_source(
        &source,
        &module,
        &toolchain,
        &dir.join(&module),
        &Options::default(),
    ) {
        Ok(built) => {
            // a decline is fine however it was reached — including one the verifier
            // caught, which is the safety net doing its job. what this asserts is only
            // that the module *built*
            assert!(built.artifact.source.exists(), "{name}: no C was emitted");
        }
        Err(error) => {
            let message = error.to_string();
            if message.contains("no working C toolchain") {
                return;
            }
            panic!("{name} failed to build: {message}");
        }
    }
}

/// the corpus, a module to a test
///
/// one test over all of them took longer than any deadlock guard should allow, and a
/// guard loose enough to hold it would no longer catch a hang. a module to a test gives
/// each its own budget, lets the runner overlap them, and names which module broke in
/// the failure itself
macro_rules! corpus {
    ($($test:ident => $path:literal,)*) => {
        $(
            #[test]
            fn $test() {
                a_stdlib_module_compiles_without_a_hard_failure($path);
            }
        )*
    };
}

// small, dependency-light, and between them they exercise classes, generators, closures,
// comprehensions and a lot of numeric code. `symtable` and `warnings` each found a
// signature map that had missed the resumable-return rule — one for methods, one for
// nested functions
//
// the last four are from the *packages*, which the top-level sweep never reached.
// `shutil` is the one that found an unsupplied argument being passed with no coercion at
// all, and it is here because it was the only stdlib module that did not build
//
// `statistics` was dropped: it took as long as the other twelve together, and what it
// covers that they do not is numeric code, which `fractions` and `colorsys` also carry.
// `buildsweep.sh` still walks it, with the other 549
corpus! {
    colorsys_compiles_without_a_hard_failure => "colorsys.py",
    textwrap_compiles_without_a_hard_failure => "textwrap.py",
    queue_compiles_without_a_hard_failure => "queue.py",
    symtable_compiles_without_a_hard_failure => "symtable.py",
    warnings_compiles_without_a_hard_failure => "warnings.py",
    dataclasses_compiles_without_a_hard_failure => "dataclasses.py",
    fractions_compiles_without_a_hard_failure => "fractions.py",
    shutil_compiles_without_a_hard_failure => "shutil.py",
    json_encoder_compiles_without_a_hard_failure => "json/encoder.py",
    json_decoder_compiles_without_a_hard_failure => "json/decoder.py",
    email_utils_compiles_without_a_hard_failure => "email/utils.py",
    urllib_parse_compiles_without_a_hard_failure => "urllib/parse.py",
}

#[test]
fn a_caller_supplied_lowering_is_compiled_the_same_way() {
    if environment().is_some_and(|(_, toolchain)| !supports(&toolchain, (3, 11))) {
        return;
    }
    // `by compile` lowers against a project database so a type imported from a
    // sibling module resolves. this asserts the entry point behind that: a module
    // the caller lowered itself gets the same gates, passes, and fallback
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_e2e_lowered");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
def double(a: int) -> int:
    return a * 2

def slow(a: int) -> None:
    try:
        pass
    except* ValueError:
        pass
";
    let lowered =
        by_irbuild::module_from_source(source, "by_e2e_lowered", by_irbuild::Language::default());
    let Ok(built) = by_build::build_lowered(lowered, source, &toolchain, &dir, &Options::default())
    else {
        eprintln!("skipping: no working C toolchain");
        return;
    };
    // the gates ran
    assert_eq!(built.declined.len(), 1);
    // and so did the fallback, so the declined function still exists
    let out = script(
        &python,
        &dir,
        "import by_e2e_lowered as m\nprint(m.double(21), type(m.slow).__name__)\n",
    );
    assert_eq!(out, "42 function");
}

#[test]
fn require_native_still_bites_on_a_caller_supplied_lowering() {
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_e2e_loweredstrict");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "def slow(a: int) -> None:\n    try:\n        pass\n    except* ValueError:\n        pass\n";
    let lowered = by_irbuild::module_from_source(
        source,
        "by_e2e_loweredstrict",
        by_irbuild::Language::default(),
    );
    let error = by_build::build_lowered(
        lowered,
        source,
        &toolchain,
        &dir,
        &Options {
            require_native: true,
            ..Options::default()
        },
    )
    .expect_err("require-native rejects the decline");
    assert!(error.to_string().contains("require-native"), "{error}");
}

#[test]
fn an_unchanged_module_is_not_recompiled() {
    // the C compiler is the slowest step, and the emitted C is a faithful function
    // of the optimized BIR — so identical C means there is nothing to do
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_e2e_rebuild");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "def double(a: int) -> int:\n    return a * 2\n";
    let Ok(first) = build_source(
        source,
        "by_e2e_rebuild",
        &toolchain,
        &dir,
        &Options::default(),
    ) else {
        eprintln!("skipping: no working C toolchain");
        return;
    };
    let stamp = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .expect("the artifact exists")
    };
    let before = stamp(&first.artifact.extension);

    let second = build_source(
        source,
        "by_e2e_rebuild",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .expect("the toolchain already worked");
    assert_eq!(stamp(&second.artifact.extension), before, "it recompiled");

    // and a real change does rebuild
    let changed = "def double(a: int) -> int:\n    return a * 3\n";
    let third = build_source(
        changed,
        "by_e2e_rebuild",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .expect("the toolchain already worked");
    assert_ne!(
        stamp(&third.artifact.extension),
        before,
        "it skipped a change"
    );
}

#[test]
fn a_stale_extension_is_rebuilt_even_when_the_c_is_unchanged() {
    // the mtime check, not just the content check: a deleted or half-written
    // artifact has to come back
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_e2e_rebuild_stale");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "def double(a: int) -> int:\n    return a * 2\n";
    let Ok(built) = build_source(
        source,
        "by_e2e_rebuild_stale",
        &toolchain,
        &dir,
        &Options::default(),
    ) else {
        eprintln!("skipping: no working C toolchain");
        return;
    };
    std::fs::remove_file(&built.artifact.extension).expect("the artifact exists");
    let again = build_source(
        source,
        "by_e2e_rebuild_stale",
        &toolchain,
        &dir,
        &Options::default(),
    )
    .expect("the toolchain already worked");
    assert!(again.artifact.extension.exists());
}

/// a class on a base from outside the module, holding storage of its own
///
/// `Exception` is deliberate: it is both external and a *GC* type, so it exercises
/// the two obligations that come with appending fields to somebody else's layout
const EXTERNAL_BASE_SOURCE: &str = "\
class Tagged(Exception):
    def __init__(self, note: object) -> None:
        self.note = note
";

/// build `source` into a fresh directory, asserting nothing was left interpreted
///
/// the assertion is the point: a declined class runs from its python definition, where
/// cpython manages the storage and every leak these tests look for is impossible. a
/// test that let the decline through would pass without compiling anything
fn built_from_source(
    source: &str,
    module: &str,
    toolchain: &Toolchain,
    tag: &str,
) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("by_e2e_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    match build_source(source, module, toolchain, &dir, &Options::default()) {
        Ok(built) => {
            assert!(
                built.declined.is_empty(),
                "{tag}: left to the interpreter, so nothing here is under test: {:?}",
                built.declined
            );
            Some(dir)
        }
        Err(error) => {
            let text = error.to_string();
            assert!(
                !text.contains("rejected the generated code"),
                "{tag}: the C compiler rejected the generated code:\n{text}"
            );
            eprintln!("skipping {tag}: no working C toolchain ({error})");
            None
        }
    }
}

#[test]
fn a_class_on_an_external_base_releases_the_fields_it_added() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    // appending storage to another type's layout is PEP 697, which is 3.12
    if !supports(&toolchain, (3, 12)) {
        return;
    }
    let Some(dir) = built_from_source(
        EXTERNAL_BASE_SOURCE,
        "by_e2e_extbase",
        &toolchain,
        "extbase",
    ) else {
        return;
    };

    // the base allocates the instance and cannot know about anything appended after
    // its own data, so a field we do not release in our own dealloc simply leaks
    let out = script(
        &python,
        &dir,
        "import gc, by_e2e_extbase\n\
         def make(): return by_e2e_extbase.Tagged(['note'])\n\
         for _ in range(50): make()\n\
         gc.collect(); before = len(gc.get_objects())\n\
         for _ in range(500): make()\n\
         gc.collect(); after = len(gc.get_objects())\n\
         print('stable' if after <= before + 100 else f'grew {before}->{after}')\n",
    );
    assert_eq!(out, "stable");
}

#[test]
fn a_cycle_through_an_external_bases_added_field_is_collectable() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 12)) {
        return;
    }
    let Some(dir) = built_from_source(
        EXTERNAL_BASE_SOURCE,
        "by_e2e_extcycle",
        &toolchain,
        "extcycle",
    ) else {
        return;
    };

    // `Exception` is a GC type, so ours is too — and a field the collector cannot see
    // holds its cycle alive forever. that is a leak rather than a crash, which is why
    // it is worth a test of its own: nothing else in this suite would notice
    let out = script(
        &python,
        &dir,
        "import gc, by_e2e_extcycle\n\
         def make():\n\
        \x20   t = by_e2e_extcycle.Tagged(None)\n\
        \x20   t.note = t\n\
         for _ in range(50): make()\n\
         gc.collect(); before = len(gc.get_objects())\n\
         for _ in range(500): make()\n\
         gc.collect(); after = len(gc.get_objects())\n\
         print('stable' if after <= before + 100 else f'grew {before}->{after}')\n",
    );
    assert_eq!(out, "stable");
}

/// the source below names every shape that hands a native class between two C types:
/// a class that owns its layout, one appending to an outside base's, a receiver
/// crossing a slot, a construction, and an instance widened to `object`
const POINTER_SOURCE: &str = "\
class Point:
    def __init__(self, x: int) -> None:
        self.x = x

    def bumped(self) -> int:
        return self.x + 1


class Tagged(ValueError):
    def __init__(self, note: str) -> None:
        self.note = note

    def label(self) -> str:
        return self.note


def passed_along(p: Point) -> int:
    other = p
    return other.bumped()


def widened(p: Point) -> object:
    return p


class Giveup(Exception):
    # no field of its own, so it owns its layout — and being raised is what widens an
    # instance register back to an object
    pass


def raised(note: str) -> str:
    try:
        raise Tagged(note)
    except Tagged as e:
        return e.label()


def gave_up() -> str:
    try:
        raise Giveup()
    except Giveup:
        return \"gave up\"
";

#[test]
fn the_emitted_c_names_no_pointer_type_it_does_not_mean() {
    // every one of these was a warning on clang, which `-w` hid, and nothing else in
    // this suite could see. gcc 14 made an incompatible pointer assignment an error by
    // default, and `-w` does not suppress an error — so an instance rendered as the
    // wrong C type stops the build outright rather than compiling to the same address
    let Some((_, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 12)) {
        return;
    }
    let dir = std::env::temp_dir().join("by_e2e_pointers");
    let _ = std::fs::remove_dir_all(&dir);
    let built = by_build::emit_source(POINTER_SOURCE, "by_e2e_pointers", &dir, &Options::default())
        .expect("the module emits");

    let object = dir.join("by_e2e_pointers.o");
    let mut args = by_build::compile_command(&toolchain, &built.artifact.source, &object, &dir);
    // the flag the product carries to keep machine-written code quiet also silences
    // this, so the check has to be made without it
    args.retain(|arg| arg != "-w");
    // the front end is the whole question here, and stopping there keeps the check
    // clear of anything the link needs
    args.push("-c".to_string());
    args.push("-Werror=incompatible-pointer-types".to_string());
    let (program, rest) = args.split_first().expect("the toolchain names a compiler");
    let Ok(result) = Command::new(program).args(rest).output() else {
        eprintln!("skipping: no working C toolchain");
        return;
    };
    let stderr = String::from_utf8_lossy(&result.stderr);
    // an unconditional assertion rather than a skip: a compile that fails for some
    // other reason would otherwise read as this one passing
    assert!(
        result.status.success(),
        "the C compiler rejected the generated code:\n{stderr}"
    );
}

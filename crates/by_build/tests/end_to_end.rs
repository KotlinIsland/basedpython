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
use by_ir::function::{CallConvention, FallbackCode, ModuleIr, ModuleName};
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
        name: by_ir::ModuleName::new("by_e2e_arith"),
        functions: vec![builder.finish()],
        declined: Vec::new(),
        classes: Vec::new(),
        gradual: Vec::new(),
        promoted: Vec::new(),
        lines: None,
        fallback_source: None,
        fallback_code: None,
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
        name: by_ir::ModuleName::new("by_e2e_fib"),
        functions: vec![builder.finish()],
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
        name: by_ir::ModuleName::new("by_e2e_div"),
        functions: vec![builder.finish()],
        declined: Vec::new(),
        classes: Vec::new(),
        gradual: Vec::new(),
        promoted: Vec::new(),
        lines: None,
        fallback_source: None,
        fallback_code: None,
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
        name: by_ir::ModuleName::new("by_e2e_float"),
        functions: vec![builder.finish()],
        declined: Vec::new(),
        classes: Vec::new(),
        gradual: Vec::new(),
        promoted: Vec::new(),
        lines: None,
        fallback_source: None,
        fallback_code: None,
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
        name: by_ir::ModuleName::new("by_e2e_call"),
        functions: vec![double.finish(), quad.finish()],
        declined: Vec::new(),
        classes: Vec::new(),
        gradual: Vec::new(),
        promoted: Vec::new(),
        lines: None,
        fallback_source: None,
        fallback_code: None,
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
        None,
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
        module.as_str(),
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
    let built = by_build::emit_source(
        POINTER_SOURCE,
        "by_e2e_pointers",
        None,
        &dir,
        &Options::default(),
    )
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

/// the source of a package member, whose answers say which member is speaking
///
/// `tag` is the same name in both `dup` modules, which is the whole point: a flat
/// output directory called both files `dup`, so the second silently replaced the
/// first and neither was importable under the name it was compiled as
fn member_source(tag: i32) -> String {
    format!(
        "class Member:\n    def __init__(self) -> None:\n        self.tag: int = {tag}\n\n\ndef tag() -> int:\n    return {tag}\n"
    )
}

/// the artefacts of a package build have to be laid out as the package, because
/// that is the only shape cpython's finder will import them back under: it looks
/// for `pkg/sub/dup<suffix>` for a member and `pkg/sub/__init__<suffix>` for the
/// package itself, and never for a flat file named after the last component
#[test]
fn a_package_is_built_as_a_tree_and_imports_under_its_dotted_names() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    if !supports(&toolchain, (3, 12)) {
        return;
    }
    let dir = std::env::temp_dir().join("by_e2e_package_tree");
    let _ = std::fs::remove_dir_all(&dir);

    let options = Options {
        language: by_irbuild::Language::Python,
        ..Options::default()
    };
    // both packages and both members go into one output directory, the way
    // `by compile -o` builds a whole project
    let members = [
        (ModuleName::package("by_e2e_pkg"), 1),
        (ModuleName::package("by_e2e_pkg.sub"), 2),
        (ModuleName::new("by_e2e_pkg.dup"), 3),
        (ModuleName::new("by_e2e_pkg.sub.dup"), 4),
    ];
    for (name, tag) in &members {
        let Ok(built) = build_source(
            &member_source(*tag),
            name.clone(),
            &toolchain,
            &dir,
            &options,
        ) else {
            eprintln!("skipping: no working C toolchain");
            return;
        };
        assert!(
            built.artifact.extension.exists(),
            "{} was written to {}",
            name.dotted(),
            built.artifact.extension.display()
        );
    }

    // four distinct artefacts: the two `dup` members used to be one file
    assert_eq!(
        members
            .iter()
            .map(|(name, _)| toolchain.extension_path(name))
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4
    );

    let printed = script(
        &python,
        &dir,
        "import sys\n\
         import by_e2e_pkg.sub.dup\n\
         import by_e2e_pkg.dup\n\
         for name in ('by_e2e_pkg', 'by_e2e_pkg.sub', 'by_e2e_pkg.dup', 'by_e2e_pkg.sub.dup'):\n\
         \x20   m = sys.modules[name]\n\
         \x20   print(name, m.__name__, m.tag(), m.Member.__module__, m.__file__)\n",
    );
    let lines: Vec<&str> = printed.lines().collect();
    assert_eq!(lines.len(), 4, "{printed}");
    for (line, (name, tag)) in lines.iter().zip(&members) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // the key it answered to in `sys.modules`, the name the module reports,
        // and the module a class written in it belongs to all have to be the one
        // it was compiled as
        assert_eq!(fields[0], name.dotted(), "{printed}");
        assert_eq!(fields[1], name.dotted(), "{printed}");
        assert_eq!(fields[2], tag.to_string(), "{printed}");
        assert_eq!(fields[3], name.dotted(), "{printed}");
        // and it answered from the extension, not from some interpreted source
        // that happened to be lying beside it
        assert!(fields[4].ends_with(&toolchain.ext_suffix), "{printed}");
    }
}

/// a module that is nothing but its interpreted twin
///
/// which of the two forms of that twin an import runs is what the tests below are
/// about, and a compiled function beside it would only be noise
fn twin_module(name: &str, source: &str, code: Option<FallbackCode>) -> ModuleIr {
    let mut module = ModuleIr::new(name);
    module.fallback_source = Some(source.to_string());
    module.fallback_code = code;
    module
}

/// the twin as source and the same twin compiled, saying *different* things
///
/// nothing else can tell the two apart. an artefact whose code object silently will
/// not read falls back to its source and behaves identically — so a test that gave
/// both forms the same program would pass with the whole compiled path disabled,
/// and the only thing lost would be an import speed nobody asserts on
const TWIN_SOURCE: &str = "WHICH = \"source\"\n";
const TWIN_CODE: &str = "WHICH = \"code\"\n";

/// what an import of a `twin_module` answered, or `None` where it did not import
fn which_twin_ran(python: &str, dir: &Path, name: &str) -> Option<String> {
    let printed = script(
        python,
        dir,
        &format!(
            "try:\n\
             \x20   import {name}\n\
             except BaseException as error:\n\
             \x20   print('!' + type(error).__name__)\n\
             else:\n\
             \x20   print({name}.WHICH)\n"
        ),
    );
    if printed.starts_with('!') {
        return None;
    }
    Some(printed)
}

#[test]
fn the_compiled_twin_is_what_an_import_runs() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(code) = toolchain.marshal(TWIN_CODE) else {
        eprintln!("skipping: the interpreter would not compile the twin");
        return;
    };
    let module = twin_module("by_e2e_twincode", TWIN_SOURCE, Some(code));
    let Some(dir) = built(&module, &toolchain, "twincode") else {
        return;
    };
    assert_eq!(
        which_twin_ran(&python, &dir, "by_e2e_twincode").as_deref(),
        Some("code")
    );
}

#[test]
fn a_twin_compiled_by_another_interpreter_is_left_where_it_is() {
    // marshal promises nothing across versions, and it does not fail softly either:
    // handing cpython 3.14 a code object 3.13 wrote segfaults the process outright.
    // the bytecode magic is cpython's own answer to that — it is what makes an
    // upgraded interpreter regenerate a `.pyc` rather than misread one — and one that
    // does not match has to send the import back to the source
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(mut code) = toolchain.marshal(TWIN_CODE) else {
        eprintln!("skipping: the interpreter would not compile the twin");
        return;
    };
    code.magic += 1;
    let module = twin_module("by_e2e_twinmagic", TWIN_SOURCE, Some(code));
    let Some(dir) = built(&module, &toolchain, "twinmagic") else {
        return;
    };
    assert_eq!(
        which_twin_ran(&python, &dir, "by_e2e_twinmagic").as_deref(),
        Some("source")
    );
}

#[test]
fn a_twin_compiled_at_another_optimization_level_is_left_where_it_is() {
    // the level is part of what the source compiles *to*, not a setting beside it:
    // `-O` takes `assert` out of the bytecode and `-OO` takes docstrings too. this
    // test process runs at level 0, so a code object claiming any other level is one
    // this interpreter would not have produced
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(mut code) = toolchain.marshal(TWIN_CODE) else {
        eprintln!("skipping: the interpreter would not compile the twin");
        return;
    };
    code.optimize = 2;
    let module = twin_module("by_e2e_twinoptimize", TWIN_SOURCE, Some(code));
    let Some(dir) = built(&module, &toolchain, "twinoptimize") else {
        return;
    };
    assert_eq!(
        which_twin_ran(&python, &dir, "by_e2e_twinoptimize").as_deref(),
        Some("source")
    );
}

#[test]
fn a_twin_this_interpreter_should_read_and_cannot_fails_the_import() {
    // the two guards above are mismatches, and a mismatch is ordinary: the source is
    // compiled instead and nothing is wrong. a code object that says it *is* for this
    // interpreter and then will not read is a broken artefact, and falling back to the
    // source there would leave a defect in how these bytes are written costing nothing
    // more visible than an import nobody times
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(mut code) = toolchain.marshal(TWIN_CODE) else {
        eprintln!("skipping: the interpreter would not compile the twin");
        return;
    };
    // no marshal type is written as a NUL, so this is refused before anything is built
    // out of it — a *truncated* code object would be read into one and is not a safe
    // thing to hand an interpreter
    code.marshalled = vec![0u8; 8].into();
    let module = twin_module("by_e2e_twinunreadable", TWIN_SOURCE, Some(code));
    let Some(dir) = built(&module, &toolchain, "twinunreadable") else {
        return;
    };
    assert_eq!(which_twin_ran(&python, &dir, "by_e2e_twinunreadable"), None);
}

#[test]
fn a_twin_that_reads_back_as_something_other_than_code_fails_the_import() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let Some(mut code) = toolchain.marshal(TWIN_CODE) else {
        eprintln!("skipping: the interpreter would not compile the twin");
        return;
    };
    // marshal's `TYPE_INT`: the tag, then the value in four little-endian bytes. it
    // reads back perfectly well and is not a code object, which is the case a bare
    // "did it read" test would hand straight to the evaluator
    code.marshalled = vec![b'i', 42, 0, 0, 0].into();
    let module = twin_module("by_e2e_twinnotcode", TWIN_SOURCE, Some(code));
    let Some(dir) = built(&module, &toolchain, "twinnotcode") else {
        return;
    };
    assert_eq!(which_twin_ran(&python, &dir, "by_e2e_twinnotcode"), None);
}

#[test]
fn running_the_interpreter_with_o_still_means_o_for_the_twin() {
    // the whole-artefact statement of the level guard. the twin has always been
    // compiled by the importing interpreter, so `python -O` took its `assert`
    // statements out; a code object compiled at the build's own level would quietly
    // put them back. the module *body* is where this is visible, because that is the
    // part of a compiled module that always runs interpreted
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_e2e_twinoptrun");
    let _ = std::fs::remove_dir_all(&dir);
    let source = "\
try:
    assert False, \"still here\"
except AssertionError:
    ASSERTED = True
else:
    ASSERTED = False
";
    let Ok(_) = build_source(
        source,
        "by_e2e_twinoptrun",
        &toolchain,
        &dir,
        &Options {
            language: by_irbuild::Language::Python,
            ..Options::default()
        },
    ) else {
        eprintln!("skipping: no working C toolchain");
        return;
    };
    let asserted = |flags: &[&str]| {
        let mut command = Command::new(&python);
        command.args(flags).args([
            "-c",
            &format!(
                "import sys\nsys.path.insert(0, {:?})\n\
                 import by_e2e_twinoptrun as m\nprint(m.ASSERTED)\n",
                dir.display().to_string()
            ),
        ]);
        let out = command.output().expect("the interpreter runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(asserted(&[]), "True");
    assert_eq!(asserted(&["-O"]), "False");
}

#[test]
fn compiling_the_twin_twice_gives_the_same_bytes() {
    // the emitted C has to be a function of the source alone, or a rebuild recompiles a
    // module nothing about which changed — and the C compiler is the slowest step there
    // is. a `frozenset` of strings is the one constant whose written form could turn on
    // something outside the source, because string hashes are seeded per process, so it
    // is what this asks about
    let Some((_, toolchain)) = environment() else {
        return;
    };
    let source =
        "PICKED = \"m\" in {\"a\", \"quite\", \"long\", \"spread\", \"of\", \"words\", \"m\"}\n";
    let (Some(first), Some(second)) = (toolchain.marshal(source), toolchain.marshal(source)) else {
        eprintln!("skipping: the interpreter would not compile the twin");
        return;
    };
    assert_eq!(first, second);
}

/// the header's version branches are decided by the headers the build compiled against,
/// so an artefact loaded by another minor version runs branches for a layout that
/// interpreter does not have — a crash rather than a wrong answer. the running version is
/// read out of `Py_GetVersion`'s banner, which is prose with two numbers on the front, so
/// what that reading does with a real banner and with junk is worth executing rather than
/// reasoning about
#[test]
fn the_running_version_is_read_off_the_banner() {
    let Some((python, toolchain)) = environment() else {
        return;
    };
    let dir = std::env::temp_dir().join("by_e2e_version");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the build directory is made");
    std::fs::write(dir.join(by_rt::BY_H_NAME), by_rt::BY_H).expect("the header is written");

    // banners cpython has really printed, then the shapes a reading could get wrong: a
    // major with no minor, a minor that is not a number, an empty string
    let source = r#"
#include "by.h"

static PyObject *parse(PyObject *self, PyObject *text) {
    int major, minor;
    (void)self;
    By_ParseVersion(PyUnicode_AsUTF8(text), &major, &minor);
    return Py_BuildValue("(ii)", major, minor);
}

static PyObject *running(PyObject *self, PyObject *unused) {
    int major, minor;
    (void)self;
    (void)unused;
    By_ParseVersion(Py_GetVersion(), &major, &minor);
    return Py_BuildValue("(ii)", major, minor);
}

static PyMethodDef methods[] = {{"parse", parse, METH_O, NULL},
                                {"running", running, METH_NOARGS, NULL},
                                {NULL, NULL, 0, NULL}};
static struct PyModuleDef def = {PyModuleDef_HEAD_INIT, "by_e2e_version", NULL, -1,
                                 methods, NULL, NULL, NULL, NULL};
PyMODINIT_FUNC PyInit_by_e2e_version(void) { return PyModule_Create(&def); }
"#;
    let c = dir.join("by_e2e_version.c");
    std::fs::write(&c, source).expect("the probe is written");
    let output = dir.join(format!("by_e2e_version{}", toolchain.ext_suffix));
    let args = by_build::compile_command(&toolchain, &c, &output, &dir);
    let (program, rest) = args
        .split_first()
        .expect("the compiler command is not empty");
    let compiled = Command::new(program)
        .args(rest)
        .output()
        .expect("the compiler runs");
    if !compiled.status.success() {
        eprintln!(
            "skipping: no working C toolchain\n{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        return;
    }

    let answers = script(
        &python,
        &dir,
        "import sys, by_e2e_version as m\n\
         for text in ['3.14.0a1 (main, x) [Clang]', '3.9.7 (default, y)', '3.13.0', '3',\n\
                      '3.x.1', '', '.13', 'python 3.13']:\n\
         \x20   print(m.parse(text))\n\
         print(m.running())\n\
         print((sys.version_info[0], sys.version_info[1]))\n",
    );
    let mut lines = answers.lines();
    for expected in [
        "(3, 14)", "(3, 9)", "(3, 13)",
        // a major alone names no minor, so it names no interpreter
        "(-1, -1)", "(-1, -1)", "(-1, -1)", "(-1, -1)", "(-1, -1)",
    ] {
        assert_eq!(lines.next(), Some(expected), "in:\n{answers}");
    }
    // and the reading of a live banner is the interpreter's own answer about itself
    let running = lines.next().expect("the running version is printed");
    assert_eq!(
        running,
        lines.next().expect("`sys.version_info` is printed")
    );
}

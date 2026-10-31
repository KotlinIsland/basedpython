//! Runtime test for what a subscript in a `.by` module evaluates, on a real interpreter.
//!
//! An ordinary subscript is a call of `__getitem__`, of a metaclass's `__getitem__` or of a
//! class's own `__class_getitem__`, and its slice is a value that reaches that call as written.
//! Only a subscript that specializes a generic class, alias or function holds type arguments, so
//! each program here prints what the call returned and also says which of its subscripts the
//! emitted python keeps byte for byte.
//!
//! The rest of the file is the other side of that line: what a type argument evaluates to when
//! the program reads it back, and a generic function that erases its type arguments, which has
//! nothing to subscript wherever the subscript stands.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile, transpile_analysed_at};

mod common;
mod interpreters;

/// transpile `program` for the interpreter it runs on and run it, for what it printed and what
/// it was transpiled to
fn run(program: &str) -> Option<(String, String)> {
    // a reified type parameter needs python's own type parameter syntax
    let interpreter = common::python(PythonVersion::PY312)?;
    let config = Config {
        min_version: interpreter.version,
        ..Config::default()
    };
    let transpiled = transpile(program, &config)
        .unwrap_or_else(|error| panic!("{program}\nshould transpile: {error}"));
    let output = common::run_source(&mut Command::new(&interpreter.command), &transpiled);
    assert!(
        output.status.success(),
        "failed on {interpreter}:\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stderr),
    );
    Some((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        transpiled,
    ))
}

/// `program` prints `printed`, one line of it per entry
fn prints(program: &str, printed: &[&str]) -> Option<String> {
    let (stdout, transpiled) = run(program)?;
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines, printed,
        "{program}\n--- transpiled ---\n{transpiled}"
    );
    Some(transpiled)
}

/// `program` prints `printed`, and each of `kept`, which it writes itself, is in the output
fn prints_and_keeps(program: &str, printed: &[&str], kept: &[&str]) {
    let Some(transpiled) = prints(program, printed) else {
        return;
    };
    for written in kept {
        assert!(
            transpiled.contains(written),
            "`{written}` should be left as it is written:\n{transpiled}"
        );
    }
}

#[test]
fn a_range_is_indexed() {
    for (program, printed, kept) in [
        ("x = range(3)\nprint(x[1])\n", "1", "print(x[1])"),
        ("y: range = range(3)\nprint(y[1])\n", "1", "print(y[1])"),
        ("a, b = range(1), 2\nprint(a[0])\n", "0", "print(a[0])"),
        (
            "for i in [range(3)]:\n    print(i[1])\n",
            "1",
            "print(i[1])",
        ),
        ("q = range(3)\nprint(q[q[1]])\n", "1", "print(q[q[1]])"),
    ] {
        prints_and_keeps(program, &[printed], &[kept]);
    }
}

#[test]
fn a_class_getitem_of_a_class_that_is_not_generic_is_called_with_the_value() {
    prints_and_keeps(
        "class Vec:\n    def __class_getitem__(cls, n: int) -> int:\n        return n + 1\nprint(Vec[3])\n",
        &["4"],
        &["print(Vec[3])"],
    );
}

const METACLASS: &str = r#"
class M(type):
    def __getitem__(cls, k: int) -> int:
        return k + 1
    def __setitem__(cls, k: int, v: int) -> None:
        print("set", k + 1, v)
    def __delitem__(cls, k: int) -> None:
        print("del", k + 1)
class Reg(metaclass=M):
    @classmethod
    def own(cls) -> int:
        return cls[1]
def through_type(c: type[Reg]) -> int:
    return c[4]
print(Reg[1])
Reg[1] = 2
del Reg[1]
print(Reg.own())
print(through_type(Reg))
x = Reg
print(x[1])
"#;

#[test]
fn a_metaclass_subscript_is_called_with_the_value() {
    prints_and_keeps(
        METACLASS,
        &["2", "set 2 2", "del 2", "2", "5", "2"],
        &[
            "print(Reg[1])",
            "Reg[1] = 2",
            "del Reg[1]",
            "return cls[1]",
            "return c[4]",
            "print(x[1])",
        ],
    );
}

/// nothing in a slice that is a value is read as a type, whichever python would refuse or
/// evaluate differently
#[test]
fn a_slice_that_is_a_value_is_written_as_it_is() {
    let slices = [
        ("n + 1", "2"),
        ("\"a\" + \"b\"", "'ab'"),
        ("f\"{s}\"", "'z'"),
        ("(1, \"a\")", "(1, 'a')"),
        ("[1]", "[1]"),
        ("1.5", "1.5"),
        ("not n", "False"),
        ("{1: 2}", "{1: 2}"),
        ("1 | 2", "3"),
        ("\"a\"", "'a'"),
        ("None", "None"),
        ("1, 2", "(1, 2)"),
        ("1:2", "slice(1, 2, None)"),
        ("-1", "-1"),
        ("1 + 2j", "(1+2j)"),
    ];
    let mut program = String::from(
        "class X:\n    def __class_getitem__(cls, k: object) -> object:\n        return k\nn = 1\ns = \"z\"\n",
    );
    let mut printed = Vec::new();
    let mut kept = Vec::new();
    for (slice, value) in slices {
        let line = format!("print(repr(X[{slice}]))");
        program.push_str(&line);
        program.push('\n');
        printed.push(value);
        kept.push(line);
    }
    let kept: Vec<&str> = kept.iter().map(String::as_str).collect();
    prints_and_keeps(&program, &printed, &kept);
}

/// the type argument a slice holds further in is still one
#[test]
fn a_specialization_in_a_slice_that_is_a_value_is_a_specialization() {
    prints_and_keeps(
        "class X:\n    def __class_getitem__(cls, k: object) -> object:\n        return k\nprint(X[list[1]])\n",
        &["list[typing.Literal[1]]"],
        &[],
    );
}

#[test]
fn a_type_subscripted_through_a_variable_of_a_c_class_is_called() {
    prints_and_keeps(
        indoc::indoc! {"
            import ctypes
            t = ctypes.c_int * 3
            def f(c: type[ctypes.Array[ctypes.c_int]]) -> object:
                return c[1]
            print(type(f(t)).__name__)
        "},
        &["GenericAlias"],
        &["return c[1]"],
    );
}

/// a generic class that answers a subscript for itself is handed the type arguments where the
/// subscript runs as a value, lowered as the checker reads them, and is the type it specializes
/// where a type is read
#[test]
fn a_generic_class_that_answers_a_subscript_itself() {
    let tuple_answer = indoc::indoc! {"
        class U[out T]:
            def __class_getitem__(cls, item):
                return (\"mine\", item)
        def chk[T](x: object) -> bool:
            return x is T
        print(repr(U[1]))
        print(chk[U[int]](1))
        print(chk[U[1]](1))
        print(chk[U[int] | None](None))
        print(chk[list[U[int]]]([]))
    "};
    prints_and_keeps(
        tuple_answer,
        &[
            "('mine', typing.Literal[1])",
            "False",
            "False",
            "True",
            "False",
        ],
        &["print(repr(U[_by_lit(1)]))"],
    );

    let class_answer = indoc::indoc! {"
        class V[T]:
            def __class_getitem__(cls, item):
                return cls
        def chk[T](x: object) -> bool:
            return x is T
        print(chk[V[str]](V[int]()))
    "};
    prints_and_keeps(class_answer, &["False"], &["(V[int]())"]);

    let metaclass_answer = indoc::indoc! {"
        class M(type):
            def __getitem__(cls, k):
                return (k, 1)
        class G[T](metaclass=M):
            pass
        def chk[T](x: object) -> bool:
            return x is T
        print(G[1])
        print(chk[G[int]](1))
    "};
    prints_and_keeps(
        metaclass_answer,
        &["(typing.Literal[1], 1)", "False"],
        &["print(G[_by_lit(1)])"],
    );

    // an annotation is a type too, which nothing evaluates through the class
    let annotated = indoc::indoc! {"
        class U[T]:
            def __class_getitem__(cls, n: int) -> int:
                return n + 1
        x: U[int] = U()
        def f(y: U[int]) -> U[str]:
            return U()
        print(type(f(x)).__name__)
    "};
    prints(annotated, &["U"]);

    let built = indoc::indoc! {"
        class V[T]:
            def __init__(self, t: T) -> None:
                pass
            def __class_getitem__(cls, item):
                return (\"mine\", item)
        print(type(V(1)).__name__)
    "};
    prints_and_keeps(built, &["V"], &["V(1)"]);
}

/// an erased generic function is the function, wherever it is subscripted
#[test]
fn an_erased_generic_function_is_subscripted_wherever_it_stands() {
    let function = "def f[T](x: object = 7) -> object:\n    return x\n";
    for (use_of_it, printed) in [
        ("y = f[int]\nprint(y(1))\n", "1"),
        ("print(f[1]())\n", "7"),
        ("print(list(map(f[int], [2])))\n", "[2]"),
        ("g = f\nprint(g[1](3))\n", "3"),
        ("print(f[1](4))\n", "4"),
        ("print(f[list[1]](5))\n", "5"),
        ("print(f[int?](6))\n", "6"),
        ("print(f[() -> int](8))\n", "8"),
        ("print((f)[int](9))\n", "9"),
        ("print([f[int]][0](10))\n", "10"),
        ("print(f[int].__name__)\n", "f"),
    ] {
        prints(&format!("{function}{use_of_it}"), &[printed]);
    }
    prints(
        indoc::indoc! {"
            class C:
                @staticmethod
                def s[T](x: object) -> object:
                    return x
            print(C.s[int](1))
            y = C.s[int]
            print(y(2))
            print(C().s[int](3))
        "},
        &["1", "2", "3"],
    );
    // nothing it strips is evaluated
    let Some((_, transpiled)) = run(&format!("{function}y = f[list[1]]\n")) else {
        return;
    };
    assert!(transpiled.contains("y = f\n"), "{transpiled}");
}

/// a reified one keeps what it was given
#[test]
fn a_reified_function_is_subscripted_as_itself() {
    prints(
        indoc::indoc! {"
            def r[T](x: object) -> object:
                return T
            y = r[int]
            print(y(1))
            g = r
            print(g[str](3))
        "},
        &["<class 'int'>", "<class 'str'>"],
    );
}

const DEFAULTED: &str = "class O[T = int]:\n    v: T\n    def __init__(self, v: T):\n        self.v = v\ndef chk[T](x: object) -> bool:\n    return x is T\n";

/// a generic class left bare as a type argument is the specialization its defaults solve, as the
/// checker reads it, and not every specialization of the class
#[test]
fn a_class_with_defaults_as_a_type_argument_is_what_they_solve() {
    for (setup, argument) in [
        ("", "O"),
        ("type X = O\n", "X"),
        ("A = O\n", "A"),
        ("", "O | None"),
        ("type X = O | None\n", "X"),
        ("type X = O\ntype Y = X\n", "Y"),
    ] {
        let program = format!(
            "{DEFAULTED}{setup}print(chk[{argument}](O[str](\"a\")))\nprint(chk[{argument}](O[int](1)))\n"
        );
        prints(&program, &["False", "True"]);
    }
    prints(
        &format!(
            "{DEFAULTED}class Out[out T]: pass\nprint(chk[Out[O]](Out[O[str]]()))\nprint(chk[Out[O]](Out[O[int]]()))\n"
        ),
        &["False", "True"],
    );
    // an in-place test has always read it so
    prints(
        &format!("{DEFAULTED}x = O[str](\"a\")\nprint(x is O)\n"),
        &["False"],
    );
}

/// `.v` is what `T` was solved to, in a body behind a test that the bare class would pass
#[test]
fn a_body_behind_a_test_of_a_defaulted_class_reads_what_it_was_solved_to() {
    prints(
        indoc::indoc! {"
            class O[T = int]:
                v: T
                def __init__(self, v: T):
                    self.v = v
            def as_t[T](x: object) -> T | None:
                if x is T:
                    return x
                return None
            y = as_t[O](O[str](\"a\"))
            if y:
                print(y.v + 1)
            print(\"end\")
        "},
        &["end"],
    );
}

/// the name is in scope where the subscript is written, whichever scope bound it
#[test]
fn a_class_with_defaults_defined_in_a_function_is_what_they_solve() {
    let local = "class O[T = int]:\n        v: T\n        def __init__(self, v: T):\n            self.v = v\n";
    let chk = "def chk[T](x: object) -> bool:\n    return x is T\n";
    for (setup, argument) in [
        ("", "O"),
        ("    A = O\n", "A"),
        ("    B = O | None\n", "B"),
        ("    type X = O\n", "X"),
        ("", "O | None"),
        ("", "list[O] | O"),
    ] {
        let program = format!(
            "{chk}def g() -> object:\n    {local}{setup}    return (chk[{argument}](O[str](\"a\")), chk[{argument}](O[int](1)))\nprint(g())\n"
        );
        prints(&program, &["(False, True)"]);
    }
    // the name the class has is another binding where the subscript is, and the one written isn't
    prints(
        &format!(
            "{chk}def g() -> object:\n    {local}    P = O\n    a = O[str](\"a\")\n    b = O[int](1)\n    def inner(O: int) -> object:\n        return (chk[P](a), chk[P](b), O)\n    return inner(1)\nprint(g())\n"
        ),
        &["(False, True, 1)"],
    );
    // through a class, and read from a method
    prints(
        &format!(
            "{chk}class Outer:\n    {local}    def m(self) -> object:\n        return (chk[Outer.O](Outer.O[str](\"a\")), chk[Outer.O](Outer.O[int](1)))\nprint(Outer().m())\nprint(chk[Outer.O](Outer.O[str](\"a\")))\n"
        ),
        &["(False, True)", "False"],
    );
}

/// a class's default that is itself a class of the function
#[test]
fn a_default_naming_a_class_of_the_function_is_spelled_in_it() {
    prints(
        indoc::indoc! {"
            def chk[T](x: object) -> bool:
                return x is T
            def g() -> object:
                class Inner: ...
                class O[T = Inner]:
                    v: T
                    def __init__(self, v: T):
                        self.v = v
                return (chk[O](O[str](\"a\")), chk[O](O[Inner](Inner())))
            print(g())
        "},
        &["(False, True)"],
    );
}

/// below 3.14 the runtime has no `memoryview[...]` to ask for, and the checker reads a bare one
/// as a `memoryview[int]` all the same
#[test]
fn a_class_with_defaults_the_target_cannot_subscript_is_what_they_solve() {
    let Some(interpreter) = common::python(PythonVersion::PY313) else {
        return;
    };
    let program = indoc::indoc! {"
        def chk[T](x: object) -> bool:
            return x is T
        class Out[out T]: ...
        print(chk[Out[memoryview]](Out[memoryview]()))
        print(chk[Out[memoryview]](Out[memoryview[int]]()))
        print(chk[Out[memoryview]](Out[memoryview[bytes]]()))
        print(chk[Out[memoryview[bytes]]](Out[memoryview]()))
    "};
    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    // analysed at the version it is written for, or at one that has the subscript
    for analysed_at in [PythonVersion::PY313, PythonVersion::latest_ty()] {
        let transpiled = transpile_analysed_at(program, &config, analysed_at).unwrap();
        assert!(
            transpiled.contains("_by_subscript(memoryview, int)"),
            "{transpiled}"
        );
        let output = common::run_source(&mut Command::new(&interpreter.command), &transpiled);
        assert!(
            output.status.success(),
            "failed on {interpreter}:\n{}\n--- transpiled ---\n{transpiled}",
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .collect::<Vec<_>>(),
            ["True", "True", "False", "False"],
            "{transpiled}"
        );
    }
}

/// `typing.cast` is the function a first argument is a type of, and no other `cast` is
#[test]
fn only_typing_cast_takes_a_type() {
    let Some(interpreter) = common::python(PythonVersion::PY312) else {
        return;
    };
    let config = Config {
        min_version: interpreter.version,
        ..Config::default()
    };
    let transpiled = transpile_analysed_at(
        "print(memoryview(b\"a\").cast(\"c\").format)\n",
        &config,
        interpreter.version,
    )
    .unwrap();
    let output = common::run_source(&mut Command::new(&interpreter.command), &transpiled);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "c",
        "{}\n{transpiled}",
        String::from_utf8_lossy(&output.stderr),
    );
    prints_and_keeps(
        indoc::indoc! {"
            from typing import cast
            class K:
                def cast(self, s: str) -> str:
                    return s + \"!\"
            print(K().cast(\"x\"))
            print(cast(\"str\", \"y\"))
        "},
        &["x!", "y"],
        &["K().cast(\"x\")"],
    );
    prints_and_keeps(
        "def cast(s: str) -> str:\n    return s + \"!\"\nprint(cast(\"x\"))\n",
        &["x!"],
        &["print(cast(\"x\"))"],
    );
}

/// each way a value is tested against `target`: through a type parameter, in place, and by a
/// checked cast either way it reports. each is asked of `v(0)`, `v(1)` and `v(2)`
fn every_test_form(indent: &str, target: &str) -> String {
    format!(
        "{indent}print(chk[{target}](v(0)), chk[{target}](v(1)), chk[{target}](v(2)))\n\
         {indent}print(v(0) is {target}, v(1) is {target}, v(2) is {target})\n\
         {indent}print((v(0) cast? {target}) is not None, (v(1) cast? {target}) is not None, (v(2) cast? {target}) is not None)\n\
         {indent}print(raises(lambda: v(0) cast! {target}), raises(lambda: v(1) cast! {target}), raises(lambda: v(2) cast! {target}))\n"
    )
}

const TEST_FORMS: &str = indoc::indoc! {"
    def chk[T](x: object) -> bool:
        return x is T
    def raises(f: () -> object) -> bool:
        try:
            f()
        except TypeError:
            return False
        return True
"};

const SOLVED_BY_DEFAULTS: [&str; 4] = [
    "False True False",
    "False True False",
    "False True False",
    "False True False",
];

/// a test written in place reads a bare class as a type argument does: what its defaults solve
#[test]
fn every_test_of_a_class_with_defaults_reads_what_they_solve() {
    let class = "class O[T = int]:\n    v: T\n    def __init__(self, v: T):\n        self.v = v\n";
    for (setup, target) in [
        ("", "O"),
        ("type X = O\n", "X"),
        ("A = O\n", "A"),
        ("B = O | None\n", "B"),
        ("", "O | None"),
    ] {
        prints(
            &format!(
                "{TEST_FORMS}{class}{setup}def v(n: int) -> object:\n    return O[str](\"a\") if n == 0 else O[int](1) if n == 1 else 3\n{}",
                every_test_form("", target)
            ),
            &SOLVED_BY_DEFAULTS,
        );
    }
    // read from a method of the class, which the class statement has not finished binding
    prints(
        &format!(
            "{TEST_FORMS}class O[T = int]:\n    v: T\n    def __init__(self, v: T):\n        self.v = v\n    def m(self, x: object) -> bool:\n        return x is O\nprint(O[int](1).m(O[str](\"a\")), O[int](1).m(O[int](1)))\n"
        ),
        &["False True"],
    );
    // reached through a class
    prints(
        &format!(
            "{TEST_FORMS}class Outer:\n    {}\ndef v(n: int) -> object:\n    return Outer.O[str](\"a\") if n == 0 else Outer.O[int](1) if n == 1 else 3\n{}",
            class.replace('\n', "\n    ").trim_end(),
            every_test_form("", "Outer.O")
        ),
        &SOLVED_BY_DEFAULTS,
    );
    // defined in a function
    for (setup, target) in [
        ("", "O"),
        ("    A = O\n", "A"),
        ("    B = O | None\n", "B"),
        ("", "O | None"),
    ] {
        prints(
            &format!(
                "{TEST_FORMS}def g() -> None:\n    {}\n{setup}    def v(n: int) -> object:\n        return O[str](\"a\") if n == 0 else O[int](1) if n == 1 else 3\n{}g()\n",
                class.replace('\n', "\n    ").trim_end(),
                every_test_form("    ", target)
            ),
            &SOLVED_BY_DEFAULTS,
        );
    }
}

/// a name that is the class itself stays one where nothing reads it as a type argument
#[test]
fn a_class_with_defaults_is_itself_where_it_is_not_a_type_argument() {
    let Some((_, transpiled)) = run(indoc::indoc! {"
        class O[T = int]: ...
        class K(O): ...
        A = O
        print(isinstance(K(), A))
    "}) else {
        return;
    };
    assert!(transpiled.contains("class K(O): ..."), "{transpiled}");
    assert!(transpiled.contains("A = O\n"), "{transpiled}");
}

/// `()` is the empty tuple where a parameter takes one type argument
#[test]
fn the_empty_tuple_is_a_type_argument() {
    prints(
        "class Out[out T]: pass\nprint(type(Out[()]()).__name__)\n",
        &["Out"],
    );
    prints(
        "def f[T]() -> object:\n    return T\nprint(f[()]())\n",
        &["tuple[()]"],
    );
    prints(
        "class R[T]:\n    def t(self) -> object:\n        return T\nprint(R[()]().t())\n",
        &["tuple[()]"],
    );
    prints(
        "def chk[T](x: object) -> bool:\n    return x is T\nprint(chk[()]((1,)))\nprint(chk[()](1))\n",
        &["False", "False"],
    );
    prints(
        "def f[A, B]() -> object:\n    return (A, B)\nprint(f[(), int]())\n",
        &["(tuple[()], <class 'int'>)"],
    );
}

/// and is no type arguments where a parameter takes a run of them
#[test]
fn the_empty_tuple_is_an_empty_run_of_type_arguments() {
    let Some(transpiled) = prints(
        indoc::indoc! {"
            class V[*Ts]: pass
            x = V[()]()
            print(x is V[()])
            print(tuple[()])
        "},
        &["True", "tuple[()]"],
    ) else {
        return;
    };
    assert!(transpiled.contains("x = V[()]()"), "{transpiled}");
}

/// an operation on number literals is the class of its result, as the literal is
#[test]
fn a_type_argument_that_folds_to_a_number_is_its_class() {
    prints(
        indoc::indoc! {"
            def f[T]() -> object:
                return T
            print(f[1 + 2j]())
            print(f[3 - 2j]())
            print(f[1 + 1.5]())
            print(f[1.5]())
            print(f[2j]())
            print(f[1 + 2]())
            print(list[1 + 2j])
        "},
        &[
            "<class 'complex'>",
            "<class 'complex'>",
            "<class 'float'>",
            "<class 'float'>",
            "<class 'complex'>",
            "typing.Literal[3]",
            "list[complex]",
        ],
    );
}

/// a program's own name is its own, and a literal type argument still reaches the helper
#[test]
fn a_program_may_use_the_name_of_the_helper_that_builds_a_literal_type() {
    let function = "def f[T]() -> object:\n    return T\n";
    for (program, printed) in [
        (
            "_by_lit = 5\nprint(f[1](), _by_lit)\n",
            "typing.Literal[1] 5",
        ),
        (
            "def g(_by_lit: int) -> object:\n    return (f[1](), _by_lit)\nprint(g(2))\n",
            "(typing.Literal[1], 2)",
        ),
        (
            "def g() -> object:\n    _by_lit = 3\n    return (f[1](), _by_lit)\nprint(g())\n",
            "(typing.Literal[1], 3)",
        ),
    ] {
        prints(&format!("{function}{program}"), &[printed]);
    }
}

/// the same, for each other helper a type's spelling writes, and read by the helpers it was
/// pasted beside: a cell given a literal is read through `_by_lit` inside the runtime
#[test]
fn a_program_may_use_the_names_of_the_helpers_a_type_is_spelled_with() {
    prints(
        indoc::indoc! {"
            _by_lit = 1
            _by_variance = 2
            _by_out = 3
            _by_specialization = 4
            from collections.abc import Sequence
            class Box[T]:
                pass
            class Own[T]:
                def __class_getitem__(cls, item):
                    return 0
            def chk[T](x: object) -> bool:
                return x is T
            print(chk[Own[int]](1))
            print(chk[Box[out object]](Box[int]()))
            print(chk[Sequence[int]](1))
            print(chk[\"a\"](\"a\"))
            print(_by_lit, _by_variance, _by_out, _by_specialization)
        "},
        &["False", "True", "False", "True", "1 2 3 4"],
    );
}

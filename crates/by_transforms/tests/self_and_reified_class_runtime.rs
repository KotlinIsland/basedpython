//! Runtime test for what a module reads where the source reads something python has no value for:
//! `typing.Self` in a method, a type parameter of a class. Each reads the receiver the method was
//! called through, which the body is free to rebind, and which a metaclass is free to build the
//! class around. A class value is here as well, which a checker reads as a `type` and which may be
//! a `list[int]` python has no class for.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
mod interpreters;

/// what `program`, transpiled for the interpreter it runs on, prints
fn prints(program: &str, printed: &[&str]) {
    let Some(interpreter) = common::python(PythonVersion::PY312) else {
        return;
    };
    let config = Config {
        min_version: interpreter.version,
        ..Config::default()
    };
    let transpiled = transpile(program, &config)
        .unwrap_or_else(|error| panic!("{program}\nshould transpile: {error}"));
    let output = Command::new(&interpreter.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{program}\nfailed on {interpreter}:\n{}",
        stderr.lines().rev().take(4).collect::<Vec<_>>().join("\n"),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .collect::<Vec<_>>(),
        printed,
        "{program}"
    );
}

const SELF_HEAD: &str = "from typing import Self\nclass Out[out T]: ...\ndef chk[T](x: object) -> bool:\n    return x is T\n";

/// `class A` with `body`, and `class B(A)`, asked through each of them
fn self_reads(setup: &str, body: &str) {
    prints(
        &format!(
            "{SELF_HEAD}{setup}class A:\n{body}class B(A): ...\nprint(A().m(Out[A]()), B().m(Out[A]()), B().m(Out[B]()))\n"
        ),
        &["True False True"],
    );
}

#[test]
fn self_is_the_receiver_s_class() {
    self_reads(
        "",
        "    def m(self, v: object) -> bool:\n        return chk[Out[Self]](v)\n",
    );
    // on the line of the header, and after a docstring
    self_reads(
        "",
        "    def m(self, v: object) -> bool: return chk[Out[Self]](v)\n",
    );
    self_reads(
        "",
        "    def m(self, v: object) -> bool:\n        \"\"\"doc\"\"\"\n        return chk[Out[Self]](v)\n",
    );
    // a receiver spelled another way
    self_reads(
        "",
        "    def m(this, v: object) -> bool:\n        return chk[Out[Self]](v)\n",
    );
    self_reads(
        "",
        "    def m(*args) -> bool:\n        return chk[Out[Self]](args[1])\n",
    );
}

#[test]
fn self_is_the_receiver_s_class_whatever_the_builtin_type_is_bound_to() {
    self_reads(
        "",
        "    def m(self, v: object, type: int = 1) -> bool:\n        return chk[Out[Self]](v)\n",
    );
    self_reads(
        "type = 3\n",
        "    def m(self, v: object) -> bool:\n        return chk[Out[Self]](v)\n",
    );
}

#[test]
fn self_is_the_receiver_s_class_whatever_the_body_binds_its_name_to() {
    self_reads(
        "",
        "    def m(self, v: object) -> bool:\n        self = 3\n        return chk[Out[Self]](v)\n",
    );
    self_reads(
        "",
        "    def m(self, v: object) -> bool:\n        def inner(self: int) -> bool:\n            return chk[Out[Self]](v)\n        return inner(1)\n",
    );
    self_reads(
        "",
        "    def m(self, v: object) -> bool:\n        return (lambda self=1: chk[Out[Self]](v))()\n",
    );
    self_reads(
        "",
        "    def m(self, v: object) -> bool:\n        return [chk[Out[Self]](v) for self in [1]][0]\n",
    );
}

#[test]
fn self_in_a_classmethod_is_the_class_it_was_called_through() {
    prints(
        &format!(
            "{SELF_HEAD}class A:\n    @classmethod\n    def m(cls, v: object) -> bool:\n        cls = 3\n        return chk[Out[Self]](v)\nclass B(A): ...\nprint(A.m(Out[A]()), B.m(Out[A]()), B.m(Out[B]()))\n"
        ),
        &["True False True"],
    );
}

/// python hands these the class whatever spells them: a `classmethod` of another name, and the
/// methods it makes one of without a decorator
#[test]
fn self_in_any_classmethod_is_the_class_it_was_called_through() {
    prints(
        &format!(
            "{SELF_HEAD}cm = classmethod\nclass A:\n    @cm\n    def m(cls, v: object) -> bool:\n        return chk[Out[Self]](v)\n    def __class_getitem__(cls, v: object) -> bool:\n        return chk[Out[Self]](v)\nprint(A.m(Out[A]()), A[Out[A]()], A[Out[int]()])\n"
        ),
        &["True True False"],
    );
}

#[test]
fn self_in_a_test_written_in_place_is_the_receiver_s_class() {
    self_reads(
        "",
        "    def m(self, v: object) -> bool:\n        self = 3\n        return v is Out[Self]\n",
    );
}

const OUT_T: &str = "    def k(self) -> object:\n        return T\n";

/// reading a type parameter takes nothing a metaclass has to carry for it
#[test]
fn a_read_of_a_type_parameter_survives_a_class_built_around_its_namespace() {
    prints(
        &format!(
            "from typing import NamedTuple\nclass N[T](NamedTuple):\n    x: T\n{OUT_T}print(N[int](1).k())\n"
        ),
        &["<class 'int'>"],
    );
    prints(
        &format!(
            "class M(type):\n    def __new__(mcs, name, bases, namespace):\n        namespace = dict(namespace)\n        namespace.pop(\"__classcell__\", None)\n        return super().__new__(mcs, name, bases, namespace)\nclass A[T](metaclass=M):\n{OUT_T}print(A[int]().k())\n"
        ),
        &["<class 'int'>"],
    );
    // a class replaced by what decorates it is read as the one it was
    prints(
        &format!(
            "from dataclasses import dataclass\n@dataclass(slots=True)\nclass D[T]:\n    v: int = 1\n{OUT_T}print(D[int]().k())\n"
        ),
        &["<class 'int'>"],
    );
}

/// a parameter of each base is read from what that base was given, under one name or two
#[test]
fn a_read_names_the_parameter_of_the_class_it_is_written_in() {
    prints(
        indoc::indoc! {"
            class Mix[T]:
                def m(self) -> object:
                    return T
            class Other[T]:
                def o(self) -> object:
                    return T
            class Third[U]:
                def t(self) -> object:
                    return U
            class B(Mix[int], Other[str], Third[bytes]): pass
            print(B().m(), B().o(), B().t())
        "},
        &["<class 'int'> <class 'str'> <class 'bytes'>"],
    );
    prints(
        indoc::indoc! {"
            class A[T]:
                def a(self) -> object:
                    return T
            def inner():
                class A2[T](A[int]):
                    def b(self) -> object:
                        return T
                return A2
            print(inner()[str]().a(), inner()[str]().b())
        "},
        &["<class 'int'> <class 'str'>"],
    );
}

#[test]
fn a_class_written_with_keywords_for_its_metaclass_is_specialized() {
    prints(
        &format!(
            "class M(type):\n    def __new__(mcs, name, bases, ns, tag: str):\n        return super().__new__(mcs, name, bases, ns)\n    def __init__(cls, name, bases, ns, tag: str):\n        super().__init__(name, bases, ns)\n        cls.tag = tag\nclass A[T](metaclass=M, tag=\"x\"):\n{OUT_T}print(A[int]().k(), A[int].tag, isinstance(A[int](), A))\n"
        ),
        &["<class 'int'> x True"],
    );
    // and stays an abc of its own, which a class sharing its base's registry wouldn't
    prints(
        &format!(
            "from abc import ABCMeta\nclass M(ABCMeta):\n    def __new__(mcs, name, bases, ns, tag: str):\n        return super().__new__(mcs, name, bases, ns)\n    def __init__(cls, name, bases, ns, tag: str):\n        super().__init__(name, bases, ns)\nclass A[T](metaclass=M, tag=\"x\"):\n{OUT_T}a = A[int]()\nprint(a.k(), isinstance(a, A[int]), isinstance(A[str](), A[int]))\n"
        ),
        &["<class 'int'> True False"],
    );
}

#[test]
fn a_type_argument_that_cannot_be_hashed_is_a_type_argument() {
    prints(
        &format!(
            "from typing import Annotated\nclass Box[T]:\n{OUT_T}x = Box[Annotated[int, []]]()\nprint(x.k() == Annotated[int, []], id(Box[Annotated[int, []]]) == id(Box[Annotated[int, []]]), Box[Annotated[str, []]]().k() == Annotated[int, []])\n"
        ),
        &["True True False"],
    );
    prints(
        indoc::indoc! {"
            from typing import Annotated
            class Inv[T]: pass
            def chk[T](x: object) -> bool:
                return x is T
            print(chk[Inv[out Annotated[int, []]]](Inv[int]()), chk[Inv[out Annotated[int, []]]](Inv[str]()))
        "},
        &["True False"],
    );
}

/// a method takes its type arguments as a function does, whether its body reads them or not
#[test]
fn a_method_is_subscripted_whether_its_body_reads_the_parameter_or_not() {
    prints(
        indoc::indoc! {"
            class K[U]:
                def erases[T](self, x: T) -> T:
                    return x
                def reads[T](self, x: T) -> object:
                    return T
                class def on_class[T](cls, x: T) -> T:
                    return x
            f = K[int]().erases[str]
            print(f(\"a\"), K[int]().reads[str](\"b\"), K.on_class[int](1), K[int]().on_class[int](2))
        "},
        &["a <class 'str'> 1 2"],
    );
}

/// `__init_subclass__` runs as a subclass is defined, which may leave the parameter open
#[test]
fn init_subclass_reads_what_a_subclass_gives_its_base() {
    prints(
        indoc::indoc! {"
            class Box[T]:
                def __init_subclass__(cls, **k):
                    print(\"sub\", T)
            class C[U](Box[U]): pass
            class D(Box[int]): pass
            print(1)
        "},
        &["sub U", "sub <class 'int'>", "1"],
    );
}

/// what the checker reads as a class may be a specialization python has no class for
#[test]
fn a_specialization_stored_as_a_value_is_a_value() {
    prints(
        indoc::indoc! {"
            from collections.abc import Sequence
            class G[T]: pass
            def ident[T](x: T) -> T:
                return x
            def f(a=list[int]):
                return a
            print([list[int]][0])
            for k in (list[int], dict[str, int]):
                print(k)
            print(f())
            print({\"a\": list[int]}[\"a\"])
            print([Sequence[int]][0])
            print(next(list[int] for _ in [1]))
            print(ident(list[int]))
            xs = [G[int]]
            print([type(x()).__name__ for x in xs])
            print([int][0])
        "},
        &[
            "list[int]",
            "list[int]",
            "dict[str, int]",
            "list[int]",
            "list[int]",
            "collections.abc.Sequence[int]",
            "list[int]",
            "list[int]",
            "['G']",
            "<class 'int'>",
        ],
    );
}

/// and what is neither a class nor a specialization of one is still a violation
#[test]
fn a_value_that_is_no_class_is_refused_where_a_class_is_read() {
    prints(
        indoc::indoc! {"
            from typing import cast
            for bad in [3, [1], list]:
                try:
                    for k in cast(list[type[int]], [bad]):
                        print(\"ok\", k)
                except TypeError as error:
                    print(\"refused\")
        "},
        &["refused", "refused", "ok <class 'list'>"],
    );
}

/// the annotation a class-body assignment is given reads what the value does, and later
#[test]
fn a_class_body_assignment_reading_a_later_class_runs() {
    prints(
        indoc::indoc! {"
            class Out[T]: pass
            class K:
                a = Out[K]()
            class J:
                b = Out[Later]()
            class Later: pass
            print(type(K.a).__name__, type(J.b).__name__)
        "},
        &["Out Out"],
    );
}

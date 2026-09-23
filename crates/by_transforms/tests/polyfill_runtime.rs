//! Runtime divergence tests for the **pep 695 polyfill** target.
//!
//! Every other runtime test transpiles at `PY313`, where type parameters are
//! native and the polyfill never runs. That left the default output target
//! (`--min-version 3.10`) — the one a user gets without passing a flag —
//! untested at runtime, and three separate lowerings reached it emitting python
//! that raises on import while `by check` reported nothing:
//!
//! - a callable arrow (`(T) -> R`) rendered `Callable[[T], R]`, keeping the
//!   pre-polyfill parameter names while the polyfill bound `_T` / `_R`
//! - a `type` alias over a variadic emitted `type_params=(_T, Unpack[_Ts])`,
//!   which `TypeAliasType` rejects
//! - a keyword subscript or a `*Ts` inside an alias value or a type-parameter
//!   bound reached the output in its `.by` spelling
//!
//! Asserting on the lowered *text* is what let all three through, so these
//! tests execute it instead.

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod interpreters;
use interpreters::Interpreter;

/// exercises the polyfill's typevar rename across every position that renders
/// replacement text rather than patching source bytes
const RENAMES: &str = r#"
def apply[T, R](fn: (T) -> R, t: T) -> R:
    return fn(t)

class Holder[T, R]:
    def apply(self, fn: (T) -> R, t: T) -> R:
        return fn(t)

class Deco[**P, R]:
    def __init__(self, fn: (**P) -> R) -> None:
        self.fn = fn

    def call(self, *args: P.args, **kwargs: P.kwargs) -> R:
        return self.fn(*args, **kwargs)

assert apply(str, 1) == "1", "arrow parameter and return rename together"
assert Holder[int, str]().apply(str, 2) == "2", "a method's arrow renames too"
assert Deco(str).call(3) == "3", "a parameters-spec arrow renames too"

# the annotation has to survive introspection, not merely import: at 3.9 the
# module is emitted with `from __future__ import annotations`, so a stale name
# raises here rather than at import
import typing
assert set(typing.get_type_hints(apply)) == {"fn", "t", "return"}, "annotations resolve"

print("ok")
"#;

/// the alias / bound positions, where the polyfill re-renders a whole statement
/// and has to splice in what the other passes rewrote inside it
const ALIASES: &str = r#"
class Pair[A, B]:
    def __init__(self) -> None:
        self.tag = "pair"

type Named = Pair[int, B=str]
type Starred[T, *Ts] = tuple[T, *Ts]

def bounded[T: Pair[int, B=str]](t: T) -> str:
    return t.tag

class Bounded[T: Pair[int, B=str]]:
    def tag(self, t: T) -> str:
        return t.tag

assert bounded(Pair()) == "pair", "a keyword subscript in a bound lowers"
assert Bounded[Pair]().tag(Pair()) == "pair", "and in a class's bound"
assert Starred.__type_params__ != (), "the variadic reached `type_params`"

print("ok")
"#;

/// a bound or default naming another type parameter. the `TypeVar` call evaluates both, so an
/// unmangled name here is a `NameError` at import — which a text assertion on the lowered output
/// cannot see, because the text it asserts on is what raises
const DEPENDENT_BOUNDS: &str = r#"
def pick[T, R: T](t: T, r: R) -> R:
    return r

def fallback[T, R = T](t: T, r: R) -> R:
    return r

class Owner[T]:
    def narrow[U: T](self, u: U) -> U:
        return u

assert pick(object(), 1) == 1, "a bound naming an earlier parameter imports"
assert fallback(object(), 1) == 1, "so does a default naming one"
assert Owner().narrow(2) == 2, "and a bound naming an enclosing list's parameter"

# the bound has to resolve to the very TypeVar the earlier parameter emitted, not to some
# other object that merely happens to be in scope
import typing
hints = typing.get_type_hints(pick)
assert hints["r"].__bound__ == hints["t"], "the bound resolves to the earlier parameter"

print("ok")
"#;

/// `some T` declares a type parameter named after its parameter, which python writes nowhere
/// in a type-parameter list, so it is a `TypeVar` at every version. the signature reads the
/// hole under the parameter's name and the body reads the parameter under it
const SOME_PARAMETERS: &str = r#"
def inc(n: some int) -> int:
    return n + 1

def echo(n: some int) -> n:
    return n

def pair[T](x: T, n: some int) -> n:
    def inner() -> None:
        assert n == 3, "the body reads the parameter, not the type parameter"
    inner()
    return n

assert inc(1) == 2, "the parameter list survives"
assert echo(2) == 2, "the return type names the hole"
assert pair("a", 3) == 3, "a hole beside a written type parameter"

import typing
hints = typing.get_type_hints(echo)
assert hints["return"] == hints["n"], "the return type is the parameter's type parameter"

print("ok")
"#;

/// every place a type is evaluated where it is written, holding a union. `A | B` calls
/// `type.__or__`, which python only grew in 3.10, so below it each has to be spelled
/// `Union[A, B]` — whether the author wrote the union with `|`, `or` or `?`, and at any
/// depth in the type
const EVALUATED_UNIONS: &str = r#"
import typing
from typing import cast

Optionals = list[int?]
Values = dict[str, int | None]
Either = list[int or str]
Pairs = list[(int, str | None)]
Handlers = list[(int | None) -> str]
Bound = typing.TypeVar("Bound", bound=int | str)

class Items(list[int | str]):
    pass

def bounded[T: int | str](t: T) -> T:
    return t

class Holder[T: list[int?]]:
    pass

def annotated(a: list[int?], b: int or str) -> int?:
    return None

assert Optionals == list[typing.Optional[int]], "an optional inside a value subscript"
assert Values == dict[str, typing.Optional[int]], "a union inside a value subscript"
assert Either == list[typing.Union[int, str]], "a keyword union"
assert Pairs == list[tuple[int, typing.Optional[str]]], "a union in a tuple type"
assert Handlers == list[typing.Callable[[typing.Optional[int]], str]], "a union in an arrow"
assert Bound.__bound__ == typing.Union[int, str], "a typing construct's type argument"
assert Items.__orig_bases__ == (list[typing.Union[int, str]],), "a class base"
assert cast(int | str, 1) == 1 and cast(list[int?], [2]) == [2], "a cast target"
assert bounded(3) == 3, "a polyfilled bound"
assert Holder[list[int]]() is not None, "a polyfilled class bound"
hints = typing.get_type_hints(annotated)
assert hints["a"] == list[typing.Optional[int]], "an optional the lowering spelled"
assert hints["b"] == typing.Union[int, str], "a keyword union the lowering spelled"
assert isinstance(None, int?) and not isinstance("a", int?), "an optional as a classinfo"
assert isinstance(None, int? | str) and isinstance(None, (bytes, float?)), "nested in a classinfo"
assert isinstance("a", (int | str)?) and issubclass(bool, int?), "around a union, and in issubclass"
assert isinstance(1, (bytes, (float, int | str))), "in a tuple nested in the classinfo"
from builtins import isinstance as is_instance
assert is_instance(1, int | str), "an isinstance reached by another name"

print("ok")
"#;

/// the evaluated unions that need `typing_extensions` below 3.13: an alias is a
/// `TypeAliasType`, and a type parameter with a default a `TypeVar` that takes one
const EVALUATED_UNIONS_EXTENSIONS: &str = r#"
import typing

type Alias = list[int?] | str

def defaulted[T = int | None]() -> None:
    pass

assert Alias.__value__ == typing.Union[list[typing.Optional[int]], str], "an alias value"
print("ok")
"#;

/// a module that binds the names of the constructors the polyfill calls. its own
/// `from typing import TypeVar` takes no `default=` below 3.13, and its `Generic` is the
/// module's to bind
const OWN_CONSTRUCTORS: &str = r#"
from typing import TypeVar

U = TypeVar("U")

class Generic:
    pass

def defaulted[T = int](t: T) -> T:
    return t

class Box[T = str]:
    pass

class Deco[**P]:
    pass

assert defaulted(1) == 1, "a defaulted function"
assert Box.__parameters__[0].__default__ == str, "a defaulted class"
assert len(Deco.__parameters__) == 1, "a parameter specification"
assert U.__name__ == "U" and Generic.__module__ == "__main__", "the module's own names"
print("ok")
"#;

/// a module that binds the names of the type variables the polyfill declares its type
/// parameters as. each declaration is written into the scope the generic stands in, so one
/// under a name the module binds would overwrite it
const OWN_TYPE_VARIABLES: &str = r#"
import typing
from typing import TypeVar

_T = TypeVar("_T", bound=int)

def legacy(x: _T) -> _T:
    return x

def modern[T](x: T) -> T:
    return x

class Holder:
    _U = "a class attribute"

    def method[U](self, x: U) -> U:
        return x

if len("a") == 2:
    def skipped[S](x: S) -> S:
        return x

def later[S](x: S) -> S:
    return x

assert _T.__bound__ === int, "the module's own type variable keeps its bound"
assert typing.get_type_hints(legacy)["x"] === _T, "and a function declared over it reads it"
assert typing.get_type_hints(modern)["x"] !== _T, "a type parameter is a type variable of its own"
assert Holder._U == "a class attribute", "a class body keeps its own name"
assert typing.get_type_hints(later)["x"].__bound__ === None, "a declaration in an `if` not taken"
print("ok")
"#;

/// a default on a parameter specification or a variadic. python rejects a parameter without
/// a default after one with a default, so dropping either default fails the class at import
const DEFAULTED_PACKS: &str = r#"
class Box[T = str, **P = [int]]:
    pass

class Row[T = str, *Ts = *tuple[int, str]]:
    pass

class Shaped[T = str, P: (*: *, **: *) = (int, bytes)]:
    pass

def call[T = int, **P = [str]](x: T) -> T:
    return x

def defaults(cls: type) -> list[object]:
    return [parameter.__default__ for parameter in getattr(cls, "__parameters__")]

assert defaults(Box) == [str, [int]], "a parameter specification's default"
assert repr(defaults(Row)[1]) in ("typing_extensions.Unpack[tuple[int, str]]", "*tuple[int, str]"), "a variadic's"
assert defaults(Shaped) == [str, [int, bytes]], "a parameter list, as a list"
assert call(1) == 1, "a defaulted function"
print("ok")
"#;

/// a method's type variable, and a nested function's, is declared at module scope, where
/// `typing.get_type_hints` resolves the annotations that name it: in the function's module
/// namespace, which a class body or an enclosing function's locals are not part of
const METHOD_TYPE_VARIABLES: &str = r#"
import typing

class Box:
    def put[T](self, x: T) -> T:
        return x

def outer():
    def inner[U](y: U) -> U:
        return y
    return inner

hints = typing.get_type_hints(Box.put)
assert hints["x"] === hints["return"], "a method's annotations resolve"
nested = typing.get_type_hints(outer())
assert nested["y"] === nested["return"], "and a nested function's"
assert Box().put(1) == 1
print("ok")
"#;

/// a method whose bound reads a name of its class. the declaration reads it through the class,
/// so it is at module scope with the others: `typing.get_type_hints` finds it, and so does a
/// function nested in the method, whose annotations are evaluated when the method runs on a
/// target that does not defer them
const CLASS_LEVEL_BOUNDS: &str = r#"
import typing

class C:
    class Inner:
        pass

    def m[T: Inner, U: T](self, x: T, y: U) -> T:
        def nested[V](v: V, t: T) -> V:
            return v
        return nested(x, y)

hints = typing.get_type_hints(C.m)
assert hints["x"] === hints["return"] and hints["y"].__bound__ === hints["x"]
assert typing.get_type_hints(C.m)["x"].__bound__.__forward_arg__ == "C.Inner"
inner = C.Inner()
assert C().m(inner, inner) === inner
print("ok")
"#;

/// a type moved out of its generic to module scope — an inline protocol, an anonymous named
/// tuple — names the type variable the polyfill declared for the generic's parameter. an earlier
/// generic declaring a parameter of the same name differently takes the first name, so a guess
/// at it reads that one instead. where the type parameters stay native syntax, the parameter
/// exists only in its generic's scope, so the hoisted type reads it off the generic
const HOISTED_TYPES: &str = r#"
import typing

class Bounded[T: int]:
    pass

class Holder[T]:
    def takes(self, p: protocol(a: T)) -> None:
        pass

    def gives(self, t: T) -> (x: T, y: int):
        return (t, 1)

parameter = getattr(Holder, "__parameters__")[0]
hoisted = typing.get_type_hints(Holder.takes)["p"]
assert typing.get_type_hints(hoisted)["a"] === parameter, "an inline protocol names its generic's"
named = typing.get_type_hints(Holder.gives)["return"]
assert typing.get_type_hints(named)["x"] === parameter, "and so does an anonymous named tuple"
print("ok")
"#;

/// a module that binds, as values of its own, the names of the typing constructs the lowerings
/// write. each lowering reads the construct it means under a name the module does not spell,
/// and the module keeps its own
const OWN_TYPING_NAMES: &str = r#"
import typing

Union = Callable = Literal = Any = Protocol = NamedTuple = "mine"
cast = overload = Final = ClassVar = final = override = "mine"
abstractmethod = Annotated = TypeIs = NewType = TypeVar = Generic = "mine"

class Base:
    def name(self) -> str:
        return "base"

final class Derived(Base):
    override def name(self) -> str:
        return "derived"

class Counter:
    class var count: int = 0

protocol Named:
    def name(self) -> str: ...

let limit: int = 3

def is_int(x: object) -> x is int:
    return type(x) === int

def first[T](xs: list[T]) -> T:
    return xs[0]

def call(f: (int) -> str) -> str:
    return f(1)

def pick(x: "a" | "b") -> dynamic:
    return x

def opt(x: int?) -> list[int?]:
    return [x]

def shape(p: protocol(a: int)) -> (x: int, y: str):
    return (p.a, "s")

newtype UserId = int

class HasA:
    a: int = 3

assert Derived().name() == "derived" and Counter.count == 0 and limit == 3
assert first([1]) == 1 and call(str) == "1" and pick("a") == "a" and opt(None) == [None]
assert shape(HasA()).x == 3 and UserId(3) == 3 and is_int(1) and (5 cast! int) == 5
assert typing.get_type_hints(opt)["x"] == typing.Optional[int], "a union the lowering spelled"
assert typing.get_type_hints(pick)["x"] == typing.Literal["a", "b"], "a literal type"
assert typing.get_type_hints(call)["f"] == typing.Callable[[int], str], "an arrow"
assert Union == Callable == Literal == Any == Protocol == NamedTuple == "mine"
assert cast == overload == Final == ClassVar == final == override == "mine"
assert abstractmethod == Annotated == TypeIs == NewType == TypeVar == Generic == "mine"
print("ok")
"#;

/// run `program`, transpiled for `target`, on the oldest interpreter found that runs what it is
/// transpiled for, and say which one ran it. `extensions` asks for one with
/// `typing_extensions`. none is a skip, said so
fn run_on_oldest(program: &str, target: PythonVersion, extensions: bool) {
    let (probe, needs) = if extensions {
        ("import typing_extensions", "with `typing_extensions`")
    } else {
        ("", "")
    };
    if let Some(interpreter) = interpreters::oldest(target, probe, needs) {
        run_at(&interpreter, program, target);
    }
}

/// the target the polyfill lowers for on `interpreter`: 3.10, the one a user gets without
/// asking, or the interpreter's own version when that is older
fn polyfill_target(interpreter: &Interpreter) -> PythonVersion {
    interpreter.version.min(PythonVersion::PY310)
}

/// run `program`, lowered by the polyfill, on the oldest interpreter found with
/// `typing_extensions`, which the polyfill's output imports below 3.13
fn run_polyfilled_with_extensions(program: &str) {
    if let Some(interpreter) = interpreters::oldest(
        PythonVersion::PY39,
        "import typing_extensions",
        "with `typing_extensions`",
    ) {
        run_at(&interpreter, program, polyfill_target(&interpreter));
    }
}

fn run_at(interpreter: &Interpreter, program: &str, min_version: PythonVersion) {
    let config = Config {
        min_version,
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");
    let output = Command::new(&interpreter.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "program transpiled for {min_version} failed on {interpreter}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    interpreters::ran(interpreter, min_version);
}

#[test]
fn polyfilled_type_parameters_rename_everywhere() {
    run_on_oldest(RENAMES, PythonVersion::PY310, false);
}

#[test]
fn polyfilled_aliases_and_bounds_run() {
    run_polyfilled_with_extensions(ALIASES);
}

#[test]
fn polyfilled_dependent_bounds_run() {
    run_polyfilled_with_extensions(DEPENDENT_BOUNDS);
}

#[test]
fn some_parameters_run_at_every_version() {
    run_on_oldest(SOME_PARAMETERS, PythonVersion::PY310, false);
    run_on_oldest(SOME_PARAMETERS, PythonVersion::PY313, false);
}

/// an interpreter without `type.__or__` is the only one that can tell a union spelled for
/// it from one that is not, so each one found runs the unions transpiled for 3.9
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn evaluated_unions_run_below_python_310() {
    let found = interpreters::interpreters();
    let older: Vec<&Interpreter> = found
        .iter()
        .filter(|interpreter| {
            (PythonVersion::PY39..PythonVersion::PY310).contains(&interpreter.version)
        })
        .collect();
    if older.is_empty() {
        eprintln!("skipping: no interpreter of python 3.9 found");
    }
    for interpreter in older {
        run_at(interpreter, EVALUATED_UNIONS, PythonVersion::PY39);
        if interpreter.typing_extensions {
            run_at(
                interpreter,
                EVALUATED_UNIONS_EXTENSIONS,
                PythonVersion::PY39,
            );
        } else {
            eprintln!(
                "skipping the aliases and defaults: {interpreter} has no `typing_extensions`"
            );
        }
    }
}

/// the `TypeVar` a default needs is `typing_extensions`' below 3.13, so every interpreter older
/// than that is one that can tell it from the module's own
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn a_module_binding_the_constructors_runs() {
    let found = interpreters::interpreters();
    let older: Vec<&Interpreter> = found
        .iter()
        .filter(|interpreter| {
            interpreter.typing_extensions
                && (PythonVersion::PY39..PythonVersion::PY313).contains(&interpreter.version)
        })
        .collect();
    if older.is_empty() {
        eprintln!("skipping: no interpreter of python 3.9 to 3.12 with `typing_extensions` found");
    }
    for interpreter in older {
        run_at(interpreter, OWN_CONSTRUCTORS, PythonVersion::PY39);
    }
}

#[test]
fn a_module_binding_the_type_variables_runs() {
    run_on_oldest(OWN_TYPE_VARIABLES, PythonVersion::PY39, false);
}

#[test]
fn defaulted_parameter_specifications_and_variadics_run() {
    run_polyfilled_with_extensions(DEFAULTED_PACKS);
    run_on_oldest(DEFAULTED_PACKS, PythonVersion::PY313, false);
}

#[test]
fn a_hoisted_type_names_its_generics_type_variable() {
    run_on_oldest(HOISTED_TYPES, PythonVersion::PY39, false);
    run_on_oldest(HOISTED_TYPES, PythonVersion::PY313, false);
}

#[test]
fn a_module_binding_the_typing_names_runs() {
    run_on_oldest(OWN_TYPING_NAMES, PythonVersion::PY39, true);
    run_on_oldest(OWN_TYPING_NAMES, PythonVersion::PY313, false);
}

#[test]
fn a_method_type_variable_resolves_in_type_hints() {
    run_on_oldest(METHOD_TYPE_VARIABLES, PythonVersion::PY39, false);
}

/// 3.10 and 3.11 evaluate annotations when the function is defined, which a nested function
/// is each time the method runs
#[test]
fn a_bound_reading_a_class_level_name_resolves() {
    run_on_oldest(CLASS_LEVEL_BOUNDS, PythonVersion::PY39, false);
    run_on_oldest(CLASS_LEVEL_BOUNDS, PythonVersion::PY310, false);
}

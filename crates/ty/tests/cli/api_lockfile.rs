use crate::CliTest;
use insta_cmd::{assert_cmd_snapshot, get_cargo_bin};
use std::process::Command;

fn api_lockfile_command(case: &CliTest) -> Command {
    let mut command = Command::new(get_cargo_bin("by"));
    command
        .current_dir(case.root())
        .arg("generate-api-file")
        .env_clear();
    command
}

#[test]
fn generates_lockfile_for_basic_module() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "module.py",
        r"
class SomeClass:
    def get_value(self, value: str) -> str:
        return value.upper()

CONST: int = 42

def public_func(x: int, y: str = 'hi') -> bool:
    return True

def _private() -> None:
    pass
",
    )?;

    assert_cmd_snapshot!(
        {
            let mut cmd = api_lockfile_command(&case);
            cmd.arg("--stdout");
            cmd
        },
        @"
    success: true
    exit_code: 0
    ----- stdout -----
    #api-lock:v=1
    #tool:by=0.0.8
    #python:default
    #modules:1
    module.CONST:v=builtins.int
    module.SomeClass.get_value:d(self:module.SomeClass,value:builtins.str)->builtins.str
    module.SomeClass:c[]
    module.public_func:d(x:builtins.int,y:builtins.str=)->builtins.bool

    ----- stderr -----
    "
    );

    Ok(())
}

#[test]
fn renders_inheritance_and_reexports() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "base.py",
            r"
class Animal:
    def speak(self) -> str:
        return ''
",
        ),
        (
            "derived.py",
            r"
from base import Animal

class Dog(Animal):
    def speak(self) -> str:
        return 'woof'
",
        ),
    ])?;

    assert_cmd_snapshot!(
        {
            let mut cmd = api_lockfile_command(&case);
            cmd.arg("--stdout");
            cmd
        },
        @"
    success: true
    exit_code: 0
    ----- stdout -----
    #api-lock:v=1
    #tool:by=0.0.8
    #python:default
    #modules:2
    base.Animal.speak:d(self:base.Animal)->builtins.str
    base.Animal:c[]
    derived.Animal:r=base.Animal
    derived.Dog.speak:d(self:derived.Dog)->builtins.str
    derived.Dog:c[base.Animal]

    ----- stderr -----
    "
    );

    Ok(())
}

#[test]
fn renders_complex_signatures() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "sigs.py",
        r"
def f(a: int, b: str = '', /, c: float = 0.0, *args: bytes, d: bool = False, **kwargs: int) -> None:
    pass
",
    )?;

    assert_cmd_snapshot!(
        {
            let mut cmd = api_lockfile_command(&case);
            cmd.arg("--stdout");
            cmd
        },
        @"
    success: true
    exit_code: 0
    ----- stdout -----
    #api-lock:v=1
    #tool:by=0.0.8
    #python:default
    #modules:1
    sigs.f:d(a:builtins.int,b:builtins.str=,/,c:builtins.float | builtins.int=,*args:builtins.bytes,d:builtins.bool=,**kwargs:builtins.int)->None

    ----- stderr -----
    "
    );

    Ok(())
}

#[test]
fn renders_generic_class_variance() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "g.py",
        r"
class A[T]:
    def f(self) -> T: ...

class B[T]:
    def f(self, x: T) -> None: ...

class C[T]:
    x: T
    def get(self) -> T: ...
    def set(self, v: T) -> None: ...

class D[T, U]:
    def f(self) -> T: ...
    def g(self, x: U) -> None: ...
",
    )?;

    assert_cmd_snapshot!(
        {
            let mut cmd = api_lockfile_command(&case);
            cmd.arg("--stdout");
            cmd
        },
        @"
    success: true
    exit_code: 0
    ----- stdout -----
    #api-lock:v=1
    #tool:by=0.0.8
    #python:default
    #modules:1
    g.A.f:d(self:Self)->T
    g.A:c<+T>[]
    g.B.f:d(self:Self,x:T)->None
    g.B:c<-T>[]
    g.C.get:d(self:Self)->T
    g.C.set:d(self:Self,v:T)->None
    g.C.x:v=T
    g.C:c<T>[]
    g.D.f:d(self:Self)->T
    g.D.g:d(self:Self,x:U)->None
    g.D:c<+T,-U>[]

    ----- stderr -----
    "
    );

    Ok(())
}

/// a frozen dataclass is read-only in its fields, so it locks as covariant while an
/// otherwise identical mutable dataclass locks as invariant
#[test]
fn renders_frozen_dataclass_variance() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "fr.py",
        r"
from dataclasses import dataclass

@dataclass(frozen=True)
class Frozen[T]:
    x: T

@dataclass
class Mutable[T]:
    x: T
",
    )?;

    assert_cmd_snapshot!(
        {
            let mut cmd = api_lockfile_command(&case);
            cmd.arg("--stdout");
            cmd
        },
        @"
    success: true
    exit_code: 0
    ----- stdout -----
    #api-lock:v=1
    #tool:by=0.0.8
    #python:default
    #modules:1
    fr.Frozen.x:v=T
    fr.Frozen:c<+T>[]{dataclass}
    fr.Mutable.x:v=T
    fr.Mutable:c<T>[]{dataclass}
    fr.dataclass:r=dataclasses.dataclass

    ----- stderr -----
    "
    );

    Ok(())
}

#[test]
fn renders_explicit_typevar_variance() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "ex.py",
        r"
from typing import TypeVar, Generic

T_co = TypeVar('T_co', covariant=True)
T_contra = TypeVar('T_contra', contravariant=True)
T = TypeVar('T')

class Box(Generic[T_co]):
    def get(self) -> T_co: ...

class Sink(Generic[T_contra]):
    def put(self, v: T_contra) -> None: ...

class Cell(Generic[T]):
    x: T
",
    )?;

    assert_cmd_snapshot!(
        {
            let mut cmd = api_lockfile_command(&case);
            cmd.arg("--stdout");
            cmd
        },
        @"
    success: true
    exit_code: 0
    ----- stdout -----
    #api-lock:v=1
    #tool:by=0.0.8
    #python:default
    #modules:1
    ex.Box.get:d(self:Self)->T_co
    ex.Box:c<+T_co>[typing.Generic[T_co]]
    ex.Cell.x:v=T
    ex.Cell:c<T>[typing.Generic[T]]
    ex.Generic:v=typing.Generic
    ex.Sink.put:d(self:Self,v:T_contra)->None
    ex.Sink:c<-T_contra>[typing.Generic[T_contra]]
    ex.T:v=TypeVar
    ex.T_co:v=TypeVar
    ex.T_contra:v=TypeVar
    ex.TypeVar:r=typing.TypeVar

    ----- stderr -----
    "
    );

    Ok(())
}

#[test]
fn renders_decorators_on_methods() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "deco.py",
        r"
from typing import final, overload
from abc import abstractmethod

class C:
    @classmethod
    def cm(cls) -> int: ...
    @staticmethod
    def sm(x: int) -> int: ...
    @final
    def fin(self) -> int: ...
    @abstractmethod
    def abs(self) -> int: ...
    @overload
    def ov(self, x: int) -> int: ...
    @overload
    def ov(self, x: str) -> str: ...
    def ov(self, x): ...
    async def coro(self) -> int: ...
",
    )?;

    assert_cmd_snapshot!({
        let mut cmd = api_lockfile_command(&case);
        cmd.arg("--stdout");
        cmd
    });

    Ok(())
}

#[test]
fn renders_qualifiers_on_variables() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "q.py",
        r"
from typing import Final, ClassVar

X: Final[int] = 1
Y: Final = 2

class C:
    a: ClassVar[int] = 0
    b: Final[str] = 'x'
    c: int = 3
",
    )?;

    assert_cmd_snapshot!({
        let mut cmd = api_lockfile_command(&case);
        cmd.arg("--stdout");
        cmd
    });

    Ok(())
}

#[test]
fn renders_class_kind_flags() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "kind.py",
        r"
from typing import Protocol, TypedDict, NamedTuple, final
from dataclasses import dataclass
from enum import Enum

@final
class F: ...

@dataclass
class D:
    x: int

class P(Protocol):
    def m(self) -> int: ...

class TD(TypedDict):
    name: str
    age: int

class NT(NamedTuple):
    x: int
    y: int

class Colors(Enum):
    RED = 1
    BLUE = 2
",
    )?;

    assert_cmd_snapshot!({
        let mut cmd = api_lockfile_command(&case);
        cmd.arg("--stdout");
        cmd
    });

    Ok(())
}

#[test]
fn renders_instance_attributes() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "ia.py",
        r"
class C:
    def __init__(self, x: int) -> None:
        self.x: int = x
        self.y = 'hi'
",
    )?;

    assert_cmd_snapshot!({
        let mut cmd = api_lockfile_command(&case);
        cmd.arg("--stdout");
        cmd
    });

    Ok(())
}

#[test]
fn renders_generic_type_alias() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "ta.py",
        r"
type Plain = int | str
type Wrapper[T] = list[T]
",
    )?;

    assert_cmd_snapshot!({
        let mut cmd = api_lockfile_command(&case);
        cmd.arg("--stdout");
        cmd
    });

    Ok(())
}

#[test]
fn honors_dunder_all() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "al.py",
        r"
__all__ = ['public_a', '_underscore_public']

def public_a() -> None: ...
def public_b() -> None: ...
def _underscore_public() -> None: ...
",
    )?;

    assert_cmd_snapshot!({
        let mut cmd = api_lockfile_command(&case);
        cmd.arg("--stdout");
        cmd
    });

    Ok(())
}

#[test]
fn renders_property_accessors() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "p.py",
        r"
class C:
    @property
    def ro(self) -> int: ...
    @property
    def rw(self) -> int: ...
    @rw.setter
    def rw(self, v: int) -> None: ...
",
    )?;

    assert_cmd_snapshot!({
        let mut cmd = api_lockfile_command(&case);
        cmd.arg("--stdout");
        cmd
    });

    Ok(())
}

#[test]
fn writes_to_file_by_default() -> anyhow::Result<()> {
    let case = CliTest::with_file("m.py", "X: int = 1\n")?;

    let status = api_lockfile_command(&case).status()?;
    assert!(status.success());

    let out_path = case.root().join("api.lock");
    let contents = std::fs::read_to_string(&out_path)?;
    // header carries tool version, python target and module count; assert
    // the format-version line, the module-count line and the body
    assert!(
        contents.starts_with("#api-lock:v=1\n"),
        "missing format header: {contents}"
    );
    assert!(
        contents.contains("#modules:1\n"),
        "missing module-count header: {contents}"
    );
    assert!(
        contents.ends_with("m.X:v=builtins.int\n"),
        "missing body line: {contents}"
    );

    Ok(())
}

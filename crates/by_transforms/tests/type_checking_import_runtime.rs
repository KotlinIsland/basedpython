//! Runtime test for a class imported only under `if TYPE_CHECKING:`, or bound only further
//! down the module.
//!
//! The checker resolves such a name, and the running program does not have it where the
//! lowered code reads it — under `TYPE_CHECKING` that is how an import cycle is broken. A
//! lowering that writes a type into code that runs (a specialization the checker solved, a
//! soundness check's target, a cast's type) must not write that name, or the module raises
//! `NameError` with `by check` clean, unless it looks the name up as it runs.

mod common;
mod interpreters;

use std::fs;
use std::path::Path;
use std::process::Command;

use by_transforms::{Config, PythonVersion, SoundnessPositions, transpile, transpile_typed};
use ruff_db::files::system_path_to_file;
use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
use ty_project::{ProjectMetadata, TestDb};

const PROGRAM: &str = r#"
from typing import TYPE_CHECKING
from decimal import Decimal as RuntimeDecimal

if TYPE_CHECKING:
    from decimal import Decimal


class Ledger:
    entries: set[Decimal]

    def __init__(self):
        self.entries = set()
        kept: set[Decimal] = set()
        assert kept == set()


def first(values: list[Decimal]) -> Decimal:
    for value in values:
        return value
    return RuntimeDecimal(0)


def widen(value: Decimal) -> Decimal | int:
    return value cast Decimal | int


totals: set[Decimal] = set()
ledger = Ledger()
ledger.entries = set()
assert ledger.entries == set() and totals == set()
assert first([RuntimeDecimal(1)]) == RuntimeDecimal(1)
assert widen(RuntimeDecimal(2)) == RuntimeDecimal(2)
print("ok")
"#;

/// the same reads where the name is bound only under `TYPE_CHECKING` imported under another
/// name, only on a path the program does not take, or only further down the module
const ELSEWHERE_PROGRAM: &str = r#"
import os
from typing import TYPE_CHECKING as CHECKING
from decimal import Decimal as RuntimeDecimal

if CHECKING:
    from decimal import Decimal

if os.environ.get("BASEDPYTHON_NEVER_SET"):
    from fractions import Fraction


def first(values: list[Decimal]) -> Decimal:
    for value in values:
        return value
    return RuntimeDecimal(0)


def first_fraction(values: list[Fraction]) -> object:
    for value in values:
        return value
    return None


early: set[Later] = set()
seen: set[Fraction] = set()


class Later: ...


class Ledger:
    entries: set[Decimal]
    fractions: set[Fraction]

    def __init__(self):
        self.entries = set()
        self.fractions = set()


assert first([RuntimeDecimal(1)]) == RuntimeDecimal(1)
assert first_fraction([]) is None
assert early == set() and seen == set()
assert Ledger().entries == set() and Ledger().fractions == set()
print("ok")
"#;

/// the same reads where the name is bound past a condition that reads `TYPE_CHECKING` in some
/// other way than as the whole test of an `if`, or where its value is chosen by the constant
const CONDITION_PROGRAM: &str = r#"
import typing
from typing import TYPE_CHECKING
from decimal import Decimal as RuntimeDecimal
from fractions import Fraction as RuntimeFraction

match TYPE_CHECKING:
    case True:
        from decimal import Decimal as InMatch
    case _:
        pass

if TYPE_CHECKING == True:
    from decimal import Decimal as InComparison

if bool(TYPE_CHECKING):
    from decimal import Decimal as InCall

if bool(typing.TYPE_CHECKING):
    from decimal import Decimal as InAttribute

if checking := TYPE_CHECKING:
    from decimal import Decimal as InWalrus

if checking:
    from decimal import Decimal as InWalrusAlias

while TYPE_CHECKING:
    from decimal import Decimal as InWhile
    break

RUNNING = not TYPE_CHECKING
if not RUNNING:
    from decimal import Decimal as InNegatedAlias

combined = False
combined |= TYPE_CHECKING
if combined:
    from decimal import Decimal as InAugmented

Fraction = RuntimeFraction if TYPE_CHECKING else None

_ = (InConditional := RuntimeDecimal) if TYPE_CHECKING else None


def parse(text: str) -> dynamic:
    return RuntimeFraction(text)


class Ledger:
    in_match: set[InMatch]
    in_comparison: set[InComparison]
    in_call: set[InCall]
    in_attribute: set[InAttribute]
    in_walrus: set[InWalrus]
    in_walrus_alias: set[InWalrusAlias]
    in_augmented: set[InAugmented]
    in_while: set[InWhile]
    in_negated_alias: set[InNegatedAlias]
    in_conditional: set[InConditional]

    def __init__(self):
        self.in_match = set()
        self.in_comparison = set()
        self.in_call = set()
        self.in_attribute = set()
        self.in_walrus = set()
        self.in_walrus_alias = set()
        self.in_augmented = set()
        self.in_while = set()
        self.in_negated_alias = set()
        self.in_conditional = set()
        in_ternary: Fraction = parse("1")
        assert in_ternary == RuntimeFraction(1)


price: Fraction = parse("1/2")
assert price == RuntimeFraction(1, 2)
ledger = Ledger()
assert ledger.in_match == ledger.in_negated_alias == set()
print("ok")
"#;

/// a check in a function on a class the module declares further down: the function can run
/// before the class exists, when the value passes unchecked, and after, when it is checked
const LATER_PROGRAM: &str = r#"
def loose(value: object) -> dynamic:
    return value


def first(values: list[dynamic]) -> Later:
    return values[0]


def make(value: object) -> Kept:
    return loose(value)


assert first([1]) == 1


class Later:
    def __init__(self, size: int):
        self.size = size


private class Kept: ...


class Holder:
    later: list[Later]

    def __init__(self, later: list[dynamic]):
        self.later = later

    def total(self) -> int:
        total = 0
        for later in self.later:
            total += later.size
        return total


def refused(run) -> bool:
    try:
        run()
    except TypeError:
        return True
    return False


assert Holder([Later(1), Later(2)]).total() == 3
assert refused(lambda: Holder([1]).total())
assert refused(lambda: first([1]))
assert first([Later(3)]).size == 3
assert refused(lambda: make(1))
assert make(Kept()) is not None
print("ok")
"#;

#[test]
fn a_class_imported_only_for_the_checker_is_never_evaluated() {
    run(PROGRAM);
}

#[test]
fn a_name_bound_elsewhere_than_where_it_is_read_is_never_evaluated() {
    run(ELSEWHERE_PROGRAM);
}

#[test]
fn a_name_bound_past_a_condition_reading_type_checking_is_never_evaluated() {
    run(CONDITION_PROGRAM);
}

#[test]
fn a_class_declared_further_down_is_checked_once_it_is_bound() {
    run(LATER_PROGRAM);
}

/// a module importing a name another module binds one way for the checker and another way as
/// it runs reads what the program binds, which need not be the class the checker resolved
#[test]
fn a_name_imported_from_a_type_checking_block_elsewhere_is_never_evaluated() {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    let files = [
        (
            "/kinds.by",
            "\
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from decimal import Decimal
else:
    Decimal = float


def parse(text: str) -> dynamic:
    import decimal

    return decimal.Decimal(text)
",
        ),
        (
            "/main.by",
            "\
from kinds import Decimal, parse


def total() -> Decimal:
    return parse(\"1.5\")


assert str(total()) == \"1.5\"
print(\"ok\")
",
        ),
    ];
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("type_checking_import_elsewhere");
    // a stale directory from an earlier run would mask a transpile failure
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create case dir");
    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/"),
    ));
    db.init_program().expect("program init failed");
    for (path, source) in files {
        db.write_file(path, source).expect("write file failed");
    }
    let config = Config {
        min_version: python.version,
        soundness: SoundnessPositions::all(),
        ..Config::default()
    };
    for (path, _) in files {
        let file = system_path_to_file(&db, path).expect("file not in db");
        let transpiled =
            transpile_typed(&db, file, &config, None).expect("transpile should succeed");
        let relative = path.trim_start_matches('/').replace(".by", ".py");
        fs::write(dir.join(relative), transpiled).expect("write module");
    }
    let output = Command::new(&python.command)
        .arg("main.py")
        .current_dir(&dir)
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "transpiled program failed on {python}:\n--- stderr ---\n{}\n--- main.py ---\n{}",
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(dir.join("main.py")).unwrap_or_default(),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

fn run(program: &str) {
    let Some(python) = common::python(PythonVersion::PY39) else {
        return;
    };
    let config = Config {
        min_version: python.version,
        soundness: SoundnessPositions::all(),
        ..Config::default()
    };
    let transpiled = transpile(program, &config).expect("transpile should succeed");
    let output = Command::new(&python.command)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");
    assert!(
        output.status.success(),
        "transpiled program failed on {python}:\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

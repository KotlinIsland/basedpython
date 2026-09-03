//! marks the stdlib members whose result is meant to be thrown away
//!
//! basedpython reports a discarded call result (`unused-return-value`), which
//! for almost every function is the mistake it looks like. a handful of stdlib
//! members are the exception: they answer a question *and* do something, and
//! the doing is usually the whole point — `entries.pop()` shortens a list,
//! `f.write(...)` writes, `reveal_type(x)` is a question to the checker. this
//! patch writes `@ignorable_return_value` onto exactly those declarations
//!
//! the table below is where a new exemption goes. the bar is that discarding
//! the result must be *idiomatic*, not merely common: `path.read_text()`
//! discarded is a bug, `path.write_text(...)` discarded is how it is written

use std::path::Path;

use ruff_python_ast::{Expr, ModModule, Stmt, StmtFunctionDef};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

/// the marker, which every basedpython file resolves without an import
const MARKER: &str = "ignorable_return_value";

/// stdlib members a caller may ignore the result of, as
/// `(module, [dotted member path])`
///
/// a member named here is marked wherever the module declares it — every
/// overload, and every branch of a `sys.version_info` gate
const IGNORABLE: &[(&str, &[&str])] = &[
    // the checker's own questions: both hand back what they were given so they
    // can wrap an expression, and a statement of one is the ordinary way to
    // write them
    ("typing", &["reveal_type", "assert_type"]),
    ("typing_extensions", &["reveal_type", "assert_type"]),
    // `pop` removes, and what it hands back is the item it removed, which a
    // caller shortening a container has no use for. `setdefault` is written for
    // the insertion as often as for the lookup.
    //
    // the mutable ABCs are named in both of their homes: `collections-abc-home`
    // moves them out of `typing`, which has not happened yet on a fresh sync
    // and has already happened when the patches are re-applied to the committed
    // stubs
    ("builtins", &["list.pop", "dict.pop", "bytearray.pop"]),
    ("collections", &["deque.pop", "deque.popleft"]),
    (
        "typing",
        &[
            "MutableSequence.pop",
            "MutableSet.pop",
            "MutableMapping.pop",
            "MutableMapping.setdefault",
        ],
    ),
    (
        "_collections_abc",
        &[
            "MutableSequence.pop",
            "MutableSet.pop",
            "MutableMapping.pop",
            "MutableMapping.setdefault",
        ],
    ),
    // a write answers with how much it wrote, which matters only to a caller
    // handling a partial write; a seek answers with where it landed, which is
    // where it was told to go. the concrete file objects inherit these
    ("typing", &["IO.write", "IO.seek", "IO.truncate"]),
    (
        "_io",
        &[
            "_IOBase.seek",
            "_IOBase.truncate",
            "_RawIOBase.write",
            "_BufferedIOBase.write",
            "_TextIOBase.write",
            "BufferedWriter.write",
            "BufferedWriter.seek",
            "TextIOWrapper.seek",
        ],
    ),
    ("os", &["write", "system", "lseek"]),
    // the movers answer with the destination the caller already handed them, the same
    // shape as the `shutil` copies below
    (
        "pathlib",
        &[
            "Path.write_text",
            "Path.write_bytes",
            "Path.rename",
            "Path.replace",
            "Path.copy",
            "Path.copy_into",
            "Path.move",
            "Path.move_into",
        ],
    ),
    // `check_call` raises on failure, so the exit status it answers with is
    // always zero; `call` and `run` are routinely used for the side effect
    ("subprocess", &["call", "check_call", "run", "Popen.wait"]),
    // an acquire in a `try` / `finally` is not asking whether it succeeded: a
    // blocking acquire only returns when it did. `threading.Lock` is an alias
    // for the `_thread` lock, which is a different class per version
    (
        "threading",
        &[
            "Event.wait",
            "Condition.wait",
            "Semaphore.acquire",
            "_RLock.acquire",
        ],
    ),
    (
        "_thread",
        &["lock.acquire", "LockType.acquire", "RLock.acquire"],
    ),
    // these answer with the destination they were told to write to
    ("shutil", &["copy", "copy2", "copyfile", "move"]),
    // the argument is registered by the call; the `Action` it answers with is
    // for the rare caller that wants to reconfigure it afterwards
    ("argparse", &["_ActionsContainer.add_argument"]),
    ("gc", &["collect"]),
];

pub struct IgnorableReturnValue;

impl Patch for IgnorableReturnValue {
    fn name(&self) -> &'static str {
        "ignorable-return-value"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        TARGET_SYMBOLS
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let Some(module) = crate::module_qualname(module_path) else {
            return Vec::new();
        };
        let members: Vec<&str> = IGNORABLE
            .iter()
            .filter(|(name, _)| *name == module)
            .flat_map(|(_, members)| members.iter().copied())
            .collect();
        if members.is_empty() {
            return Vec::new();
        }
        let mut edits = Vec::new();
        let mut found = Vec::new();
        walk(
            &parsed.syntax().body,
            &[],
            &members,
            source,
            &mut edits,
            &mut found,
        );
        edits
    }
}

/// every member of [`IGNORABLE`] as `module.member`, so an upstream rename of
/// one is flagged rather than silently marking nothing
const TARGET_SYMBOLS: &[&str] = &[
    "typing.reveal_type",
    "typing.assert_type",
    "typing_extensions.reveal_type",
    "typing_extensions.assert_type",
    "builtins.list.pop",
    "builtins.dict.pop",
    "builtins.bytearray.pop",
    "collections.deque.pop",
    "collections.deque.popleft",
    "typing.MutableSequence.pop",
    "typing.MutableSet.pop",
    "typing.MutableMapping.pop",
    "typing.MutableMapping.setdefault",
    "_collections_abc.MutableSequence.pop",
    "_collections_abc.MutableSet.pop",
    "_collections_abc.MutableMapping.pop",
    "_collections_abc.MutableMapping.setdefault",
    "typing.IO.write",
    "typing.IO.seek",
    "typing.IO.truncate",
    "_io._IOBase.seek",
    "_io._IOBase.truncate",
    "_io._RawIOBase.write",
    "_io._BufferedIOBase.write",
    "_io._TextIOBase.write",
    "_io.BufferedWriter.write",
    "_io.BufferedWriter.seek",
    "_io.TextIOWrapper.seek",
    "os.write",
    "os.system",
    "os.lseek",
    "pathlib.Path.write_text",
    "pathlib.Path.write_bytes",
    "pathlib.Path.rename",
    "pathlib.Path.replace",
    "pathlib.Path.copy",
    "pathlib.Path.copy_into",
    "pathlib.Path.move",
    "pathlib.Path.move_into",
    "subprocess.call",
    "subprocess.check_call",
    "subprocess.run",
    "subprocess.Popen.wait",
    "threading.Event.wait",
    "threading.Condition.wait",
    "threading.Semaphore.acquire",
    "threading._RLock.acquire",
    "_thread.lock.acquire",
    "_thread.LockType.acquire",
    "_thread.RLock.acquire",
    "shutil.copy",
    "shutil.copy2",
    "shutil.copyfile",
    "shutil.move",
    "argparse._ActionsContainer.add_argument",
    "gc.collect",
];

/// walk `body`, where `path` is the chain of class names enclosing it.
///
/// a version gate or a `try` around a declaration does not change what the
/// member is called, so those bodies are walked at the same path.
///
/// `found` collects every listed member the module declares, marked or not. the
/// rewrite has no use for it; the test that an entry still names something does
fn walk(
    body: &[Stmt],
    path: &[&str],
    members: &[&str],
    source: &str,
    edits: &mut Vec<Edit>,
    found: &mut Vec<String>,
) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(function) => {
                let mut qualified = path.to_vec();
                qualified.push(function.name.as_str());
                let qualified = qualified.join(".");
                if members.contains(&qualified.as_str()) {
                    found.push(qualified);
                    if !already_marked(function) {
                        edits.push(mark(function, source));
                    }
                }
            }
            Stmt::ClassDef(class) => {
                let mut nested = path.to_vec();
                nested.push(class.name.as_str());
                walk(&class.body, &nested, members, source, edits, found);
            }
            Stmt::If(node) => {
                walk(&node.body, path, members, source, edits, found);
                for clause in &node.elif_else_clauses {
                    walk(&clause.body, path, members, source, edits, found);
                }
            }
            Stmt::Try(node) => {
                walk(&node.body, path, members, source, edits, found);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(handler) = handler;
                    walk(&handler.body, path, members, source, edits, found);
                }
                walk(&node.orelse, path, members, source, edits, found);
                walk(&node.finalbody, path, members, source, edits, found);
            }
            Stmt::With(node) => walk(&node.body, path, members, source, edits, found),
            _ => {}
        }
    }
}

/// every listed member the vendored stub for `module` declares.
#[cfg(test)]
fn declared_members(module: &str, members: &[&str]) -> Vec<String> {
    let stdlib = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("ty_vendored/vendor/typeshed/stdlib");
    let flat = stdlib.join(format!("{module}.byi"));
    let package = stdlib.join(module).join("__init__.byi");
    let path = if flat.is_file() { flat } else { package };
    let Ok(source) = std::fs::read_to_string(&path) else {
        panic!("no vendored stub for `{module}` at {}", path.display());
    };
    let parsed = ruff_python_parser::parse_unchecked_source(
        &source,
        ruff_python_ast::PySourceType::BasedPythonStub,
    );
    let mut edits = Vec::new();
    let mut found = Vec::new();
    walk(
        &parsed.syntax().body,
        &[],
        members,
        &source,
        &mut edits,
        &mut found,
    );
    found
}

fn already_marked(function: &StmtFunctionDef) -> bool {
    function.decorator_list.iter().any(
        |decorator| matches!(&decorator.expression, Expr::Name(name) if name.id.as_str() == MARKER),
    )
}

/// insert the marker on its own line above the declaration, at the declaration's
/// indentation.
///
/// the insertion point is the start of the first decorator's line rather than
/// of the `def` line, so a member that already carries `@overload` keeps the
/// decorators in the order it had them
fn mark(function: &StmtFunctionDef, source: &str) -> Edit {
    let first = function
        .decorator_list
        .first()
        .map_or_else(|| function.range().start(), Ranged::start);
    let bytes = source.as_bytes();
    let mut line_start = first.to_usize();
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    let indent = &source[line_start..first.to_usize()];
    Edit {
        start: line_start,
        end: line_start,
        replacement: format!("{indent}@{MARKER}\n"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use super::IgnorableReturnValue;
    use crate::{Patch, apply_edits};

    #[track_caller]
    fn run(module: &str, source: &str) -> String {
        let parsed = parse_unchecked_source(source, PySourceType::BasedPython);
        let edits =
            IgnorableReturnValue.rewrite(Path::new(&format!("{module}.byi")), &parsed, source);
        apply_edits(source, edits)
    }

    #[test]
    fn a_named_free_function_is_marked() {
        assert_eq!(
            run("os", "def write(fd: int, data: bytes, /) -> int: ...\n"),
            "@ignorable_return_value\ndef write(fd: int, data: bytes, /) -> int: ...\n"
        );
    }

    #[test]
    fn a_named_method_is_marked_at_its_own_indentation() {
        assert_eq!(
            run(
                "builtins",
                "class list[Element]:\n    def pop(self, index: int = -1, /) -> Element: ...\n"
            ),
            "class list[Element]:\n    @ignorable_return_value\n    def pop(self, index: int = -1, /) -> Element: ...\n"
        );
    }

    #[test]
    fn every_overload_and_every_version_branch_is_marked() {
        assert_eq!(
            run(
                "subprocess",
                "import sys\n\
                 if sys.version_info >= (3, 11):\n    \
                     def run(args: str) -> int: ...\n    \
                     def run(args: list[str]) -> int: ...\n"
            ),
            "import sys\n\
             if sys.version_info >= (3, 11):\n    \
                 @ignorable_return_value\n    \
                 def run(args: str) -> int: ...\n    \
                 @ignorable_return_value\n    \
                 def run(args: list[str]) -> int: ...\n"
        );
    }

    #[test]
    fn the_marker_goes_above_the_decorators_already_there() {
        assert_eq!(
            run("gc", "@deprecated(\"x\")\ndef collect() -> int: ...\n"),
            "@ignorable_return_value\n@deprecated(\"x\")\ndef collect() -> int: ...\n"
        );
    }

    #[test]
    fn a_second_run_changes_nothing() {
        let once = run("gc", "def collect() -> int: ...\n");
        assert_eq!(run("gc", &once), once);
    }

    /// an entry that names nothing marks nothing, and nothing else would say so:
    /// the rewrite is a no-op either way. so ask the committed stubs directly.
    ///
    /// a member is looked for across every module that lists it, because the
    /// mutable ABCs are listed in both of their homes on purpose — `typing`
    /// before `collections-abc-home` moves them and `_collections_abc` after
    #[test]
    fn every_listed_member_still_names_something() {
        let mut declared: Vec<String> = Vec::new();
        for (module, members) in super::IGNORABLE {
            declared.extend(super::declared_members(module, members));
        }
        for (module, members) in super::IGNORABLE {
            for member in *members {
                assert!(
                    declared.iter().any(|found| found == member),
                    "`{module}.{member}` is not declared by any stub that lists it — \
                     upstream renamed or removed it, and the entry marks nothing"
                );
            }
        }
    }

    /// the same name on another class, or in another module, is a different
    /// member
    #[test]
    fn an_unnamed_member_is_left_alone() {
        assert_eq!(
            run("builtins", "class tuple:\n    def pop(self) -> int: ...\n"),
            "class tuple:\n    def pop(self) -> int: ...\n"
        );
        assert_eq!(
            run("json", "def collect() -> int: ...\n"),
            "def collect() -> int: ...\n"
        );
    }
}

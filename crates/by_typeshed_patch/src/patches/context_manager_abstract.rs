//! make the context-manager entry methods abstract (ports python/typeshed#15584)
//!
//! `AbstractContextManager.__enter__` and `AbstractAsyncContextManager.__aenter__`
//! have default implementations that return `self`, but that can't be spelled in
//! the type system, so upstream marks them abstract. every concrete `contextlib`
//! subclass then needs an explicit entry method returning the type it declares as
//! the context manager's yield type (the first argument of its
//! `AbstractContextManager[X, ...]` base), which this patch derives and inserts
//!
//! scoped to `contextlib`, matching the upstream change (no other stdlib class
//! subclasses these bases without its own entry method)

use std::path::Path;

use ruff_python_ast::{Expr, ModModule, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_python_parser::Parsed;
use ruff_text_size::{Ranged, TextRange};

use crate::{Edit, Patch};

pub struct ContextManagerAbstractEnter;

/// (abstract base class, its entry method, whether the method is `async`)
const ABSTRACT_BASES: &[(&str, &str, bool)] = &[
    ("AbstractContextManager", "__enter__", false),
    ("AbstractAsyncContextManager", "__aenter__", true),
];

impl Patch for ContextManagerAbstractEnter {
    fn name(&self) -> &'static str {
        "context-manager-abstract-enter"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[
            "contextlib.AbstractContextManager",
            "contextlib.AbstractAsyncContextManager",
        ]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        if crate::module_qualname(module_path).as_deref() != Some("contextlib") {
            return Vec::new();
        }
        let mut edits = Vec::new();
        walk_classes(
            &parsed.syntax().body,
            &mut |class| match class.name.as_str() {
                "AbstractContextManager" => {
                    make_entry_abstract(class, "__enter__", source, &mut edits);
                }
                "AbstractAsyncContextManager" => {
                    make_entry_abstract(class, "__aenter__", source, &mut edits);
                }
                _ => insert_concrete_entry(class, source, &mut edits),
            },
        );
        edits
    }
}

/// prefix `abstract ` to the given entry method of an abstract base, unless it is
/// already abstract (idempotent on a second pass)
fn make_entry_abstract(class: &StmtClassDef, entry: &str, source: &str, edits: &mut Vec<Edit>) {
    let Some(func) = find_method(class, entry) else {
        return;
    };
    if func
        .decorator_list
        .iter()
        .any(|d| decorator_name(&d.expression) == Some("abstract"))
    {
        return;
    }
    // insert at the first non-whitespace column of the `def` / `async def` line
    let (line_start, indent) = line_start_and_indent(func.range(), source);
    let keyword = line_start + indent.len();
    edits.push(Edit {
        start: keyword,
        end: keyword,
        replacement: "abstract ".to_string(),
    });
}

/// give a concrete subclass an explicit entry method when it inherits an abstract
/// one. the return type is the first argument of the class's
/// `Abstract[Async]ContextManager[X, ...]` base
fn insert_concrete_entry(class: &StmtClassDef, source: &str, edits: &mut Vec<Edit>) {
    for &(base, entry, is_async) in ABSTRACT_BASES {
        if class_defines(class, entry) {
            continue;
        }
        let Some(yield_type) = context_manager_yield_type(class, base, source) else {
            continue;
        };
        // anchor the insertion on the class's exit method, which every affected
        // subclass declares; skip if it is missing (nothing sensible to anchor to)
        let exit = if is_async { "__aexit__" } else { "__exit__" };
        let Some(anchor) = find_method(class, exit) else {
            continue;
        };
        let (line_start, indent) = line_start_and_indent(anchor.range(), source);
        let def = if is_async {
            format!("{indent}async def {entry}(self) -> {yield_type}\n")
        } else {
            format!("{indent}def {entry}(self) -> {yield_type}\n")
        };
        edits.push(Edit {
            start: line_start,
            end: line_start,
            replacement: def,
        });
    }
}

/// the first type argument of a `<base>[X, ...]` base expression, as source text
fn context_manager_yield_type<'a>(
    class: &StmtClassDef,
    base: &str,
    source: &'a str,
) -> Option<&'a str> {
    for base_expr in class.bases() {
        if let Expr::Subscript(sub) = base_expr
            && decorator_name(&sub.value) == Some(base)
        {
            let first = match sub.slice.as_ref() {
                Expr::Tuple(tuple) => tuple.elts.first()?,
                other => other,
            };
            return Some(&source[first.range()]);
        }
    }
    None
}

fn class_defines(class: &StmtClassDef, name: &str) -> bool {
    class
        .body
        .iter()
        .any(|s| matches!(s, Stmt::FunctionDef(f) if f.name.as_str() == name))
}

fn find_method<'a>(class: &'a StmtClassDef, name: &str) -> Option<&'a StmtFunctionDef> {
    class.body.iter().find_map(|s| match s {
        Stmt::FunctionDef(f) if f.name.as_str() == name => Some(f),
        _ => None,
    })
}

/// leading `Name`/`Attribute` identifier of an expression (`Foo`, `mod.Foo`)
fn decorator_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attr) => Some(attr.attr.as_str()),
        _ => None,
    }
}

/// (byte offset of the start of the line `range` begins on, its leading indent)
fn line_start_and_indent(range: TextRange, source: &str) -> (usize, &str) {
    let bytes = source.as_bytes();
    let mut line_start = range.start().to_usize();
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    let mut keyword = line_start;
    while keyword < bytes.len() && matches!(bytes[keyword], b' ' | b'\t') {
        keyword += 1;
    }
    (line_start, &source[line_start..keyword])
}

fn walk_classes<'a>(body: &'a [Stmt], f: &mut impl FnMut(&'a StmtClassDef)) {
    for stmt in body {
        match stmt {
            Stmt::ClassDef(class) => {
                f(class);
                walk_classes(&class.body, f);
            }
            Stmt::If(node) => {
                walk_classes(&node.body, f);
                for clause in &node.elif_else_clauses {
                    walk_classes(&clause.body, f);
                }
            }
            Stmt::Try(node) => {
                walk_classes(&node.body, f);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk_classes(&h.body, f);
                }
                walk_classes(&node.orelse, f);
                walk_classes(&node.finalbody, f);
            }
            Stmt::With(node) => walk_classes(&node.body, f),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = ContextManagerAbstractEnter.rewrite(Path::new("contextlib.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn makes_abstract_base_enter_abstract() {
        let src = "\
protocol AbstractContextManager[out Element, out ExitT](ABC):
    def __enter__(self) -> Element:
        \"\"\"doc\"\"\"

    abstract def __exit__(self, /) -> ExitT
";
        let expected = "\
protocol AbstractContextManager[out Element, out ExitT](ABC):
    abstract def __enter__(self) -> Element:
        \"\"\"doc\"\"\"

    abstract def __exit__(self, /) -> ExitT
";
        assert_eq!(run(src), expected);
        // idempotent: the entry is already abstract on a second pass
        assert_eq!(run(&run(src)), run(src));
    }

    #[test]
    fn makes_async_base_aenter_abstract() {
        let src = "\
protocol AbstractAsyncContextManager[out Element, out ExitT](ABC):
    async def __aenter__(self) -> Element:
        \"\"\"doc\"\"\"

    abstract async def __aexit__(self, /) -> ExitT
";
        let expected = "\
protocol AbstractAsyncContextManager[out Element, out ExitT](ABC):
    abstract async def __aenter__(self) -> Element:
        \"\"\"doc\"\"\"

    abstract async def __aexit__(self, /) -> ExitT
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn inserts_entry_into_concrete_subclass() {
        let src = "\
class closing[in out SupportsCloseT: _SupportsClose](AbstractContextManager[SupportsCloseT, None]):
    init(self, thing: SupportsCloseT)
    def __exit__(self, *exc_info: Unused) -> None
";
        let expected = "\
class closing[in out SupportsCloseT: _SupportsClose](AbstractContextManager[SupportsCloseT, None]):
    init(self, thing: SupportsCloseT)
    def __enter__(self) -> SupportsCloseT
    def __exit__(self, *exc_info: Unused) -> None
";
        assert_eq!(run(src), expected);
        // idempotent: the subclass now defines __enter__
        assert_eq!(run(expected), expected);
    }

    #[test]
    fn inserts_async_entry_and_concrete_return_type() {
        let src = "\
class suppress(AbstractContextManager[None, bool]):
    init(self, *exceptions: type[BaseException])
    def __exit__(self, /) -> bool

class aclosing[in out SupportsAcloseT: _SupportsAclose](AbstractAsyncContextManager[SupportsAcloseT, None]):
    init(self, thing: SupportsAcloseT)
    async def __aexit__(self, *exc_info: Unused) -> None
";
        let expected = "\
class suppress(AbstractContextManager[None, bool]):
    init(self, *exceptions: type[BaseException])
    def __enter__(self) -> None
    def __exit__(self, /) -> bool

class aclosing[in out SupportsAcloseT: _SupportsAclose](AbstractAsyncContextManager[SupportsAcloseT, None]):
    init(self, thing: SupportsAcloseT)
    async def __aenter__(self) -> SupportsAcloseT
    async def __aexit__(self, *exc_info: Unused) -> None
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn skips_subclass_that_defines_entry() {
        let src = "\
class nullcontext[in out Element](AbstractContextManager[Element, None], AbstractAsyncContextManager[Element, None]):
    def __enter__(self) -> Element
    def __exit__(self, *exctype: Unused) -> None
    async def __aenter__(self) -> Element
    async def __aexit__(self, *exctype: Unused) -> None
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn skips_non_contextlib() {
        let parsed = parse_unchecked_source(
            "class AbstractContextManager[out Element]:\n    def __enter__(self) -> Element\n",
            PySourceType::BasedPythonStub,
        );
        let edits = ContextManagerAbstractEnter.rewrite(Path::new("typing.byi"), &parsed, "x");
        assert!(edits.is_empty());
    }
}

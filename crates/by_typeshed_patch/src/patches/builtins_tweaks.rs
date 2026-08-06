//! small, symbol-local semantic tweaks to `builtins`
//!
//! - [`FrozendictCovariant`] makes `frozendict` fully covariant (`in out` → `out`)
//! - [`TypeDictProxyCovariant`] makes `type.__dict__`'s value projection
//!   covariant (`out dynamic`)
//! - [`HashableKeyBound`] bounds the key/element typevar of `dict`, `set`,
//!   `frozendict` and `frozenset` by `Hashable`
//! - [`BorrowingBuiltins`] marks the parameters of the builtins that cannot
//!   retain their argument as `local`
//!
//! each patch is scoped to a single named symbol and is idempotent

use std::fmt::Write as _;
use std::path::Path;

use ruff_python_ast::{Expr, ModModule, Parameter, Stmt, StmtClassDef, TypeParam};
use ruff_python_parser::Parsed;
use ruff_text_size::{Ranged, TextRange};

use crate::{Edit, Patch};

fn in_builtins(module_path: &Path) -> bool {
    crate::module_qualname(module_path).as_deref() == Some("builtins")
}

/// visit every class in the module, descending through version guards and
/// nested scopes
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

/// replace the source text of `range` when a substring transform changes it
fn substitute(range: TextRange, source: &str, from: &str, to: &str) -> Option<Edit> {
    let slice = &source[range];
    if !slice.contains(from) {
        return None;
    }
    Some(Edit {
        start: range.start().to_usize(),
        end: range.end().to_usize(),
        replacement: slice.replace(from, to),
    })
}

// ---------------------------------------------------------------------------

pub struct FrozendictCovariant;

impl Patch for FrozendictCovariant {
    fn name(&self) -> &'static str {
        "frozendict-covariant"
    }
    fn target_symbols(&self) -> &'static [&'static str] {
        &["builtins.frozendict"]
    }
    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        if !in_builtins(module_path) {
            return Vec::new();
        }
        let mut edits = Vec::new();
        walk_classes(&parsed.syntax().body, &mut |class| {
            if class.name.as_str() == "frozendict"
                && let Some(type_params) = &class.type_params
                && let Some(edit) = substitute(type_params.range(), source, "in out ", "out ")
            {
                edits.push(edit);
            }
        });
        edits
    }
}

// ---------------------------------------------------------------------------

pub struct TypeDictProxyCovariant;

impl Patch for TypeDictProxyCovariant {
    fn name(&self) -> &'static str {
        "type-dict-proxy-covariant"
    }
    fn target_symbols(&self) -> &'static [&'static str] {
        &["builtins.type.__dict__"]
    }
    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        if !in_builtins(module_path) {
            return Vec::new();
        }
        let mut edits = Vec::new();
        walk_classes(&parsed.syntax().body, &mut |class| {
            if class.name.as_str() != "type" {
                return;
            }
            for member in &class.body {
                // `__dict__: Final[types.MappingProxyType[str, dynamic]]` becomes
                // the read-only `final let __dict__: types.MappingProxyType[str,
                // out dynamic]`, dropping the now-redundant explanatory comment
                if let Stmt::AnnAssign(assign) = member
                    && assign
                        .target
                        .as_name_expr()
                        .is_some_and(|n| n.id == "__dict__")
                    && let Expr::Subscript(final_sub) = assign.annotation.as_ref()
                    && subscript_head(&final_sub.value) == Some("Final")
                {
                    let inner = final_sub.slice.as_ref();
                    let inner_text = project_mapping_value(inner, source);
                    let target_start = assign.target.range().start().to_usize();
                    edits.push(Edit {
                        start: target_start,
                        end: target_start,
                        replacement: "final let ".to_string(),
                    });
                    edits.push(Edit {
                        start: assign.annotation.range().start().to_usize(),
                        end: assign.annotation.range().end().to_usize(),
                        replacement: inner_text,
                    });
                    if let Some(comment_edit) = delete_leading_comments(assign.range(), source) {
                        edits.push(comment_edit);
                    }
                }
            }
        });
        edits
    }
}

/// render `inner` (a `types.MappingProxyType[str, VALUE]` expression) with its
/// value projected to `out VALUE`
fn project_mapping_value(inner: &Expr, source: &str) -> String {
    let text = &source[inner.range()];
    let Some(value_range) = mapping_proxy_value(inner) else {
        return text.to_string();
    };
    if source[value_range].starts_with("out ") || source[value_range].starts_with("in ") {
        return text.to_string();
    }
    let base = inner.range().start().to_usize();
    let vs = value_range.start().to_usize() - base;
    let ve = value_range.end().to_usize() - base;
    format!("{}out {}{}", &text[..vs], &text[vs..ve], &text[ve..])
}

/// deletion edit for the run of comment lines directly above `range`'s statement
/// (keeping the statement itself), or `None` if there are none
fn delete_leading_comments(range: TextRange, source: &str) -> Option<Edit> {
    let bytes = source.as_bytes();
    let mut stmt_line = range.start().to_usize();
    while stmt_line > 0 && bytes[stmt_line - 1] != b'\n' {
        stmt_line -= 1;
    }
    let mut start = stmt_line;
    while start > 0 {
        let line_end = start - 1;
        let mut line_start = line_end;
        while line_start > 0 && bytes[line_start - 1] != b'\n' {
            line_start -= 1;
        }
        if source[line_start..line_end].trim_start().starts_with('#') {
            start = line_start;
        } else {
            break;
        }
    }
    (start < stmt_line).then(|| Edit {
        start,
        end: stmt_line,
        replacement: String::new(),
    })
}

/// range of the value argument of a `...MappingProxyType[str, VALUE]` subscript
/// found anywhere within `annotation`, if it is not already projected
fn mapping_proxy_value(annotation: &Expr) -> Option<TextRange> {
    fn find(expr: &Expr) -> Option<TextRange> {
        if let Expr::Subscript(sub) = expr
            && subscript_head(&sub.value) == Some("MappingProxyType")
            && let Expr::Tuple(tuple) = &*sub.slice
            && let [_key, value] = tuple.elts.as_slice()
        {
            return Some(value.range());
        }
        // descend into `Final[...]` and other wrappers
        match expr {
            Expr::Subscript(sub) => find(&sub.slice).or_else(|| find(&sub.value)),
            _ => None,
        }
    }
    find(annotation)
}

fn subscript_head(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attr) => Some(attr.attr.as_str()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------

/// bounds the key/element typevar of the hashable-keyed builtin containers by
/// `Hashable`, matching their runtime constraint (a `dict`/`set` key or a
/// `frozendict`/`frozenset` element must be hashable). `Hashable` is one of
/// basedpython's implicit typing names, so no import is needed.
///
/// only the *first* typevar of each class is bounded (the key / element); the
/// value typevar of the mapping types is left unbounded. an unbounded typevar
/// used as a key still satisfies the bound because its own upper bound is
/// `object`, which is hashable
pub struct HashableKeyBound;

/// the container classes whose first typevar is the hashable key / element
const HASHABLE_KEYED: &[&str] = &["dict", "set", "frozendict", "frozenset"];

impl Patch for HashableKeyBound {
    fn name(&self) -> &'static str {
        "hashable-key-bound"
    }
    fn target_symbols(&self) -> &'static [&'static str] {
        &[
            "builtins.dict",
            "builtins.set",
            "builtins.frozendict",
            "builtins.frozenset",
        ]
    }
    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, _source: &str) -> Vec<Edit> {
        if !in_builtins(module_path) {
            return Vec::new();
        }
        let mut edits = Vec::new();
        walk_classes(&parsed.syntax().body, &mut |class| {
            if !HASHABLE_KEYED.contains(&class.name.as_str()) {
                return;
            }
            let Some(type_params) = &class.type_params else {
                return;
            };
            // the key / element is always the first typevar; leave it alone if it
            // already carries a bound (idempotent on the converted form)
            if let Some(TypeParam::TypeVar(tv)) = type_params.type_params.first()
                && tv.bound.is_none()
            {
                let at = tv.name.range().end().to_usize();
                edits.push(Edit {
                    start: at,
                    end: at,
                    replacement: ": Hashable".to_string(),
                });
            }
        });
        edits
    }
}

/// Mark the parameters of the builtins that provably cannot retain their
/// argument as [`local`](https://docs.basedpython.org/features/local-lifetimes).
///
/// The escape rule is that a `local` handed to a non-`local` parameter escapes,
/// since that callee might keep it. Without this, an ordinary `len(xs)` on a
/// borrow was reported — which made `local` unusable for the read-only borrow
/// it exists to express.
///
/// Every entry here returns a fresh scalar and hands no part of the argument
/// back. `min`, `max` and `sorted` are deliberately absent: they return an
/// *element*, whose lifetime is a separate question from the container's.
pub struct BorrowingBuiltins;

/// `(function, index of the parameter that is only read)`
const BORROWED: &[(&str, usize)] = &[
    ("all", 0),
    ("any", 0),
    ("ascii", 0),
    ("hash", 0),
    ("isinstance", 0),
    ("issubclass", 0),
    ("len", 0),
    ("repr", 0),
    ("sum", 0),
];

/// visit every function in the module, descending through version guards and
/// overload groups
fn walk_functions<'a>(body: &'a [Stmt], f: &mut impl FnMut(&'a ruff_python_ast::StmtFunctionDef)) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(function) => f(function),
            Stmt::If(node) => {
                walk_functions(&node.body, f);
                for clause in &node.elif_else_clauses {
                    walk_functions(&clause.body, f);
                }
            }
            _ => {}
        }
    }
}

impl Patch for BorrowingBuiltins {
    fn name(&self) -> &'static str {
        "borrowing-builtins"
    }
    fn target_symbols(&self) -> &'static [&'static str] {
        &[
            "builtins.all",
            "builtins.any",
            "builtins.ascii",
            "builtins.hash",
            "builtins.isinstance",
            "builtins.issubclass",
            "builtins.len",
            "builtins.repr",
            "builtins.sum",
        ]
    }
    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        if !in_builtins(module_path) {
            return Vec::new();
        }
        let mut edits = Vec::new();
        walk_functions(&parsed.syntax().body, &mut |function| {
            let Some((_, index)) = BORROWED
                .iter()
                .find(|(name, _)| *name == function.name.as_str())
            else {
                return;
            };
            // the borrowed parameter is positional-only in every entry, and each
            // overload of an overloaded builtin gets its own edit
            let Some(parameter) = function
                .parameters
                .posonlyargs
                .get(*index)
                .map(|with_default| &with_default.parameter)
            else {
                return;
            };
            if already_local(source, parameter) {
                return;
            }
            let at = parameter.name.range().start().to_usize();
            edits.push(Edit {
                start: at,
                end: at,
                replacement: "local ".to_string(),
            });
        });
        edits
    }
}

/// whether the parameter's name is already preceded by the `local` keyword —
/// the modifier is not recorded on the AST node, so it is read back off the
/// source, and this is what makes the patch idempotent
fn already_local(source: &str, parameter: &Parameter) -> bool {
    source[..parameter.name.range().start().to_usize()]
        .trim_end()
        .ends_with("local")
}

// ---------------------------------------------------------------------------

pub struct ObjectFormatSpec;

/// the comment written above the narrowed signature, so a reader does not take
/// the divergence from upstream typeshed for a mistake
const OBJECT_FORMAT_NOTE: &str = "\
# the default implementation raises `TypeError` for any non-empty spec: it
# has nothing to interpret one with. a class that wants a format spec has
# to override this
";

impl Patch for ObjectFormatSpec {
    fn name(&self) -> &'static str {
        "object-format-empty-spec"
    }
    fn target_symbols(&self) -> &'static [&'static str] {
        &["builtins.object.__format__"]
    }
    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        if !in_builtins(module_path) {
            return Vec::new();
        }
        let mut edits = Vec::new();
        walk_classes(&parsed.syntax().body, &mut |class| {
            if class.name.as_str() != "object" {
                return;
            }
            for member in &class.body {
                let Stmt::FunctionDef(function) = member else {
                    continue;
                };
                if function.name.as_str() != "__format__" {
                    continue;
                }
                let Some(annotation) = function
                    .parameters
                    .posonlyargs
                    .iter()
                    .find(|with_default| with_default.parameter.name.as_str() == "format_spec")
                    .and_then(|with_default| with_default.parameter.annotation.as_ref())
                else {
                    continue;
                };
                // already narrowed, so a re-run is a no-op and the note is not
                // written twice
                if &source[annotation.range()] != "str" {
                    continue;
                }
                edits.push(Edit {
                    start: annotation.range().start().to_usize(),
                    end: annotation.range().end().to_usize(),
                    replacement: "\"\"".to_string(),
                });
                let (line, indent) = line_start(source, function.range().start().to_usize());
                edits.push(Edit {
                    start: line,
                    end: line,
                    replacement: OBJECT_FORMAT_NOTE.lines().fold(
                        String::new(),
                        |mut note, comment| {
                            let _ = writeln!(note, "{indent}{comment}");
                            note
                        },
                    ),
                });
            }
        });
        edits
    }
}

/// the offset the line containing `at` starts at, and the indentation it opens
/// with
fn line_start(source: &str, at: usize) -> (usize, &str) {
    let bytes = source.as_bytes();
    let mut start = at;
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    (start, &source[start..at])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(patch: &dyn Patch, src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = patch.rewrite(Path::new("builtins.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn borrowed_builtins_take_local() {
        let src = "def len(obj: Sized, /) -> int: ...\n";
        let expected = "def len(local obj: Sized, /) -> int: ...\n";
        assert_eq!(run(&BorrowingBuiltins, src), expected);
        // idempotent: the modifier is read back off the source, not the AST
        assert_eq!(run(&BorrowingBuiltins, expected), expected);
    }

    #[test]
    fn every_overload_of_a_borrowed_builtin_is_marked() {
        let src = "\
@overload
def sum(iterable: Iterable[int], /, start: int = 0) -> int: ...
@overload
def sum[T](iterable: Iterable[T], /) -> T: ...
";
        let out = run(&BorrowingBuiltins, src);
        assert_eq!(out.matches("local iterable").count(), 2, "{out}");
    }

    #[test]
    fn an_unlisted_builtin_is_untouched() {
        // `sorted` returns an *element*, whose lifetime is a separate question
        let src = "def sorted(iterable: Iterable[T], /) -> list[T]: ...\n";
        assert_eq!(run(&BorrowingBuiltins, src), src);
    }

    #[test]
    fn a_non_builtins_module_is_untouched() {
        let src = "def len(obj: Sized, /) -> int: ...\n";
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = BorrowingBuiltins.rewrite(Path::new("typing.byi"), &parsed, src);
        assert!(edits.is_empty());
    }

    #[test]
    fn frozendict_becomes_covariant() {
        let src = "class frozendict[in out Key, in out Value](Mapping[Key, Value]): ...\n";
        let expected = "class frozendict[out Key, out Value](Mapping[Key, Value]): ...\n";
        assert_eq!(run(&FrozendictCovariant, src), expected);
        // idempotent
        assert_eq!(run(&FrozendictCovariant, expected), expected);
    }

    #[test]
    fn type_dict_becomes_final_let_and_drops_comment() {
        let src = "\
class type:
    # type.__dict__ is read-only at runtime, but that can't be expressed currently.
    # See https://github.com/python/typeshed/issues/11033 for a discussion.
    __dict__: Final[types.MappingProxyType[str, dynamic]]
";
        let expected = "\
class type:
    final let __dict__: types.MappingProxyType[str, out dynamic]
";
        assert_eq!(run(&TypeDictProxyCovariant, src), expected);
        // idempotent: the rewritten form is no longer a `Final[...]` annotation
        assert_eq!(run(&TypeDictProxyCovariant, expected), expected);
    }

    #[test]
    fn skips_non_builtins() {
        let parsed = parse_unchecked_source(
            "class frozendict[in out Key, in out Value]: ...\n",
            PySourceType::BasedPythonStub,
        );
        let edits = FrozendictCovariant.rewrite(Path::new("types.byi"), &parsed, "irrelevant");
        assert!(edits.is_empty());
    }

    #[test]
    fn hashable_key_bounds_dict_and_set() {
        let src = "\
class dict[in out Key, in out Value](MutableMapping[Key, Value]): ...
class set[in out Element](MutableSet[Element]): ...
class frozendict[out Key, out Value](Mapping[Key, Value]): ...
class frozenset[out Element](AbstractSet[Element]): ...
";
        let expected = "\
class dict[in out Key: Hashable, in out Value](MutableMapping[Key, Value]): ...
class set[in out Element: Hashable](MutableSet[Element]): ...
class frozendict[out Key: Hashable, out Value](Mapping[Key, Value]): ...
class frozenset[out Element: Hashable](AbstractSet[Element]): ...
";
        assert_eq!(run(&HashableKeyBound, src), expected);
        // idempotent: the bounded form is left untouched
        assert_eq!(run(&HashableKeyBound, expected), expected);
    }

    #[test]
    fn object_format_takes_only_the_empty_spec() {
        let src = "\
class object:
    def __format__(self, format_spec: str, /) -> str
";
        let expected = "\
class object:
    # the default implementation raises `TypeError` for any non-empty spec: it
    # has nothing to interpret one with. a class that wants a format spec has
    # to override this
    def __format__(self, format_spec: \"\", /) -> str
";
        assert_eq!(run(&ObjectFormatSpec, src), expected);
        // idempotent: the narrowed annotation is no longer `str`, so neither
        // the signature nor the note is rewritten
        assert_eq!(run(&ObjectFormatSpec, expected), expected);
    }

    #[test]
    fn object_format_leaves_overriding_classes_alone() {
        let src = "\
class int:
    override def __format__(self, format_spec: str, /) -> str
";
        assert_eq!(run(&ObjectFormatSpec, src), src);
    }

    #[test]
    fn hashable_key_bound_skips_non_builtins() {
        let parsed = parse_unchecked_source(
            "class dict[in out Key, in out Value]: ...\n",
            PySourceType::BasedPythonStub,
        );
        let edits = HashableKeyBound.rewrite(Path::new("types.byi"), &parsed, "irrelevant");
        assert!(edits.is_empty());
    }
}

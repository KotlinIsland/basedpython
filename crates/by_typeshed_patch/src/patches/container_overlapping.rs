//! membership (`in`) uses `Overlapping` on the covariant container element
//!
//! `Container` is covariant in its element (`out Element`), so a membership test
//! consumes that covariant typevar in an input position. basedpython types the
//! parameter as `Overlapping[Element]`: a value is accepted iff it is not
//! disjoint from `Element`, so for an `xs: Container[int]` both `1 in xs` and
//! `o in xs` (an `o: object`) are allowed, while `"a" in xs` is rejected. a bare
//! `object()` is inferred `final object` — exactly `object`, so disjoint from
//! `int` — and is rejected like any other disjoint operand
//!
//! `Container.__contains__` is the abstract membership requirement. every other
//! container that already declares `__contains__` keeps its own declaration
//! (abstract on `AbstractSet`, concrete mixins on `Sequence`/`Mapping`/the views,
//! concrete C implementations on `tuple`/`list`/`set`/`frozenset`) — the patch
//! only rewrites each parameter from `object` to `Overlapping[<element>]`, so the
//! abstract/concrete structure is unchanged and every membership test applies the
//! overlap check. `Mapping` and `dict` additionally consume their covariant key
//! in `__getitem__`/`get`, likewise typed `Overlapping[Key]`.
//!
//! this is sound because `Overlapping[Key]` relates as `Key` for subtyping and
//! override compatibility (a subclass may override with `Key` or the bare upper
//! bound); only the call site applies the overlap admissibility check
//!
//! this patch matches the nice type-parameter names (`Element`, `Key`) rather
//! than the legacy `_T_co`/`_KT_co` form, so it is registered as a
//! post-conversion patch (pass 3), where each patch gets its own re-parse

use std::path::Path;

use ruff_python_ast::{Expr, ModModule, Parameters, Stmt, StmtClassDef};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

/// stale upstream comments that describe the old (contravariant `Container[Any]`)
/// model; the `Overlapping` rewrite makes them wrong, so they are removed
const STALE_COMMENTS: &[&str] = &[
    "# This is generic more on vibes than anything else",
    "# Note: need to use Container[Any] instead of Container[_T_co] to ensure covariance.",
    "# Implement Sized (but don't have it as a base class).",
];

pub struct ContainerMembershipOverlapping;

impl Patch for ContainerMembershipOverlapping {
    fn name(&self) -> &'static str {
        "container-membership-overlapping"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[
            "typing.Container",
            "typing.Collection",
            "typing.Mapping",
            "typing.Sequence",
            "typing.AbstractSet",
            "typing.KeysView",
            "typing.ValuesView",
            "typing.ItemsView",
            "builtins.tuple",
            "builtins.list",
            "builtins.set",
            "builtins.frozenset",
            "builtins.dict",
            "builtins.frozendict",
        ]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        match module_qualname(module_path).as_deref() {
            Some("typing") => rewrite_typing(parsed, source),
            Some("builtins") => rewrite_builtins(parsed),
            _ => Vec::new(),
        }
    }
}

fn rewrite_typing(parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
    let mut edits = Vec::new();
    remove_stale_comments(source, &mut edits);
    for stmt in &parsed.syntax().body {
        let Stmt::ClassDef(class) = stmt else {
            continue;
        };
        // rewrite each declared `__contains__` parameter to `Overlapping[<element>]`,
        // keeping the method exactly as abstract/concrete as it already is. the
        // element is the type membership is tested against for that class
        match class.name.as_str() {
            "Container" => rewrite_container(class, &mut edits),
            "Collection" => rewrite_collection_base(class, &mut edits),
            "Sequence" | "AbstractSet" => rewrite_contains_param(class, "Element", &mut edits),
            "KeysView" => rewrite_contains_param(class, "Key", &mut edits),
            "ValuesView" => rewrite_contains_param(class, "Value", &mut edits),
            "ItemsView" => rewrite_contains_param(class, "tuple[Key, Value]", &mut edits),
            // `Mapping` also consumes its covariant `Key` in `__getitem__`/`get`
            "Mapping" => {
                rewrite_contains_param(class, "Key", &mut edits);
                rewrite_key_params_to_overlapping(&class.body, &mut edits);
            }
            _ => {}
        }
    }
    edits
}

/// delete every line holding one of the [`STALE_COMMENTS`]
fn remove_stale_comments(source: &str, edits: &mut Vec<Edit>) {
    for comment in STALE_COMMENTS {
        if let Some(pos) = source.find(comment) {
            let (start, end) = full_line_span(source, pos, pos + comment.len());
            edits.push(Edit {
                start,
                end,
                replacement: String::new(),
            });
        }
    }
}

/// rewrite the `key` parameter of every `__getitem__`/`get` method in `stmts` to
/// `Overlapping[Key]`, descending into `sys.version_info` guards
fn rewrite_key_params_to_overlapping(stmts: &[Stmt], edits: &mut Vec<Edit>) {
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(func) if matches!(func.name.as_str(), "__getitem__" | "get") => {
                if let Some(range) = named_positional_annotation(&func.parameters, "key") {
                    edits.push(replace_range(range, "Overlapping[Key]"));
                }
            }
            Stmt::If(if_stmt) => {
                rewrite_key_params_to_overlapping(&if_stmt.body, edits);
                for clause in &if_stmt.elif_else_clauses {
                    rewrite_key_params_to_overlapping(&clause.body, edits);
                }
            }
            _ => {}
        }
    }
}

/// the concrete builtin containers keep their own `__contains__` (it is a real,
/// concrete C implementation), with the parameter rewritten to `Overlapping`.
/// `str`/`bytes`/`bytearray` are deliberately left alone: their `__contains__`
/// takes a genuinely different type (substring / buffer), not the element.
///
/// classes are also visited inside module-level `sys.version_info` guards, where
/// `frozendict` lives
fn rewrite_builtins(parsed: &Parsed<ModModule>) -> Vec<Edit> {
    let mut edits = Vec::new();
    rewrite_builtin_classes(&parsed.syntax().body, &mut edits);
    edits
}

fn rewrite_builtin_classes(stmts: &[Stmt], edits: &mut Vec<Edit>) {
    for stmt in stmts {
        match stmt {
            Stmt::ClassDef(class) => match class.name.as_str() {
                "tuple" | "list" | "set" | "frozenset" => {
                    rewrite_contains_param(class, "Element", edits);
                }
                // `dict`/`frozendict` consume their key in `__getitem__`/`get`;
                // they inherit `__contains__` from `Mapping`
                "dict" | "frozendict" => {
                    rewrite_key_params_to_overlapping(&class.body, edits);
                }
                _ => {}
            },
            Stmt::If(if_stmt) => {
                rewrite_builtin_classes(&if_stmt.body, edits);
                for clause in &if_stmt.elif_else_clauses {
                    rewrite_builtin_classes(&clause.body, edits);
                }
            }
            _ => {}
        }
    }
}

/// `class Container[in ContainerT = Any]` → `class Container[out Element]`, and
/// `abstract def __contains__(self, x: <T>, /)` → `x: Overlapping[Element]`.
///
/// the method stays `abstract`: `Container.__contains__` is the single abstract
/// membership requirement, and every container inherits it (the overlap check is
/// applied structurally through `Container[Element]`, not via a redeclaration)
fn rewrite_container(class: &StmtClassDef, edits: &mut Vec<Edit>) {
    if let Some(type_params) = class.type_params.as_ref() {
        edits.push(replace_range(type_params.range(), "[out Element]"));
    }
    for stmt in &class.body {
        let Stmt::FunctionDef(method) = stmt else {
            continue;
        };
        if method.name.as_str() != "__contains__" {
            continue;
        }
        if let Some(range) = first_positional_annotation(&method.parameters) {
            edits.push(replace_range(range, "Overlapping[Element]"));
        }
    }
}

/// `class Collection[out Element](Iterable[Element], Container[Any], ...)` →
/// `Container[Element]`, so the inherited membership check sees the real element
fn rewrite_collection_base(class: &StmtClassDef, edits: &mut Vec<Edit>) {
    let Some(arguments) = class.arguments.as_ref() else {
        return;
    };
    for base in &arguments.args {
        let Expr::Subscript(subscript) = base else {
            continue;
        };
        let Expr::Name(name) = subscript.value.as_ref() else {
            continue;
        };
        if name.id.as_str() == "Container" {
            edits.push(replace_range(subscript.slice.range(), "Element"));
        }
    }
}

/// rewrite the first non-`self` positional parameter of every `__contains__`
/// method declared directly on `class` to `Overlapping[<element>]`, leaving the
/// method's `abstract`/concrete status and body untouched
fn rewrite_contains_param(class: &StmtClassDef, element: &str, edits: &mut Vec<Edit>) {
    for stmt in &class.body {
        if let Stmt::FunctionDef(func) = stmt
            && func.name.as_str() == "__contains__"
            && let Some(range) = first_positional_annotation(&func.parameters)
        {
            edits.push(replace_range(range, &format!("Overlapping[{element}]")));
        }
    }
}

/// byte range of the annotation of the first non-`self` positional parameter
fn first_positional_annotation(parameters: &Parameters) -> Option<ruff_text_size::TextRange> {
    parameters
        .posonlyargs
        .iter()
        .chain(parameters.args.iter())
        .filter(|p| p.parameter.name.as_str() != "self")
        .find_map(|p| p.parameter.annotation.as_ref().map(|a| a.range()))
}

/// byte range of the annotation of the positional parameter named `name`
fn named_positional_annotation(
    parameters: &Parameters,
    name: &str,
) -> Option<ruff_text_size::TextRange> {
    parameters
        .posonlyargs
        .iter()
        .chain(parameters.args.iter())
        .find(|p| p.parameter.name.as_str() == name)
        .and_then(|p| p.parameter.annotation.as_ref().map(|a| a.range()))
}

fn replace_range(range: ruff_text_size::TextRange, replacement: &str) -> Edit {
    Edit {
        start: range.start().to_usize(),
        end: range.end().to_usize(),
        replacement: replacement.to_string(),
    }
}

/// expand `[node_start, node_end)` to cover the full source line(s): back to the
/// start of the first line and forward through the trailing newline
fn full_line_span(source: &str, node_start: usize, node_end: usize) -> (usize, usize) {
    let start = source[..node_start]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let end = source[node_end..]
        .find('\n')
        .map_or(source.len(), |newline| node_end + newline + 1);
    (start, end)
}

/// dotted module name for a typeshed file path relative to `stdlib/`
fn module_qualname(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let mut parts: Vec<&str> = path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    if stem != "__init__" {
        parts.push(stem);
    }
    Some(parts.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(path: &str, src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = ContainerMembershipOverlapping.rewrite(Path::new(path), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn rewrites_container_and_drops_inherited_overrides() {
        let src = "\
class Container[in ContainerT = Any](Protocol):
    # This is generic more on vibes than anything else
    abstract def __contains__(self, x: ContainerT, /) -> bool

class Collection[out Element](Iterable[Element], Container[Any], Protocol):
    # Note: need to use Container[Any] instead of Container[_T_co] to ensure covariance.
    # Implement Sized (but don't have it as a base class).
    abstract def __len__(self) -> int

class Sequence[out Element](Collection[Element]):
    def __contains__(self, value: object, /) -> bool
    def index(self, value: Any, /) -> int

class Mapping[out Key, out Value](Collection[Key]):
    abstract def __getitem__(self, key: Key, /) -> Value
    def get(self, key: object, /) -> Value | None
    def __contains__(self, key: object, /) -> bool
    def __eq__(self, other: object, /) -> bool
";
        let expected = "\
class Container[out Element](Protocol):
    abstract def __contains__(self, x: Overlapping[Element], /) -> bool

class Collection[out Element](Iterable[Element], Container[Element], Protocol):
    abstract def __len__(self) -> int

class Sequence[out Element](Collection[Element]):
    def __contains__(self, value: Overlapping[Element], /) -> bool
    def index(self, value: Any, /) -> int

class Mapping[out Key, out Value](Collection[Key]):
    abstract def __getitem__(self, key: Overlapping[Key], /) -> Value
    def get(self, key: Overlapping[Key], /) -> Value | None
    def __contains__(self, key: Overlapping[Key], /) -> bool
    def __eq__(self, other: object, /) -> bool
";
        assert_eq!(run("typing.byi", src), expected);
    }

    #[test]
    fn rewrites_concrete_builtin_contains_but_keeps_str() {
        let src = "\
class tuple[out Element](Sequence[Element]):
    def __contains__(self, key: object, /) -> bool:
        \"\"\"Return bool(key in self).\"\"\"

class str(Sequence[str]):
    def __contains__(self, key: str, /) -> bool:  # type: ignore[override]
        \"\"\"substring\"\"\"

class set[in out Element](MutableSet[Element]):
    def __contains__(self, o: object, /) -> bool: ...
";
        let expected = "\
class tuple[out Element](Sequence[Element]):
    def __contains__(self, key: Overlapping[Element], /) -> bool:
        \"\"\"Return bool(key in self).\"\"\"

class str(Sequence[str]):
    def __contains__(self, key: str, /) -> bool:  # type: ignore[override]
        \"\"\"substring\"\"\"

class set[in out Element](MutableSet[Element]):
    def __contains__(self, o: Overlapping[Element], /) -> bool: ...
";
        assert_eq!(run("builtins.byi", src), expected);
    }

    #[test]
    fn rewrites_dict_key_params_including_version_guards() {
        let src = "\
class dict[in out Key, in out Value](MutableMapping[Key, Value]):
    def __getitem__(self, key: Key, /) -> Value: ...
    def get(self, key: object, default: None = None, /) -> Value | None: ...
    if sys.version_info >= (3, 12):
        def get(self, key: Key, default: Value, /) -> Value: ...
";
        let expected = "\
class dict[in out Key, in out Value](MutableMapping[Key, Value]):
    def __getitem__(self, key: Overlapping[Key], /) -> Value: ...
    def get(self, key: Overlapping[Key], default: None = None, /) -> Value | None: ...
    if sys.version_info >= (3, 12):
        def get(self, key: Overlapping[Key], default: Value, /) -> Value: ...
";
        assert_eq!(run("builtins.byi", src), expected);
    }

    #[test]
    fn rewrites_frozendict_nested_in_version_guard() {
        let src = "\
if sys.version_info >= (3, 15):
    class frozendict[in out Key, in out Value](Mapping[Key, Value]):
        def get(self, key: Key, default: None = None, /) -> Value | None: ...
        def __getitem__(self, key: Key, /) -> Value: ...
";
        let expected = "\
if sys.version_info >= (3, 15):
    class frozendict[in out Key, in out Value](Mapping[Key, Value]):
        def get(self, key: Overlapping[Key], default: None = None, /) -> Value | None: ...
        def __getitem__(self, key: Overlapping[Key], /) -> Value: ...
";
        assert_eq!(run("builtins.byi", src), expected);
    }

    #[test]
    fn skips_other_modules() {
        let src = "class Container[in ContainerT = Any](Protocol):\n    abstract def __contains__(self, x: ContainerT, /) -> bool\n";
        assert_eq!(run("builtins.byi", src), src);
    }
}

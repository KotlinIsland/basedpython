//! output-position widening for the invariant builtin containers
//!
//! an invariant generic container cannot be assigned to a wider specialization:
//! `list[int]` is not a `list[int | None]`, because a caller holding the wider
//! type could insert a `None` the original never expected. but a method that
//! returns a *fresh* container of the same class — `list.copy`, `list.__add__`,
//! `list.__mul__`, the set algebra, `dict.copy`, ... — hands back a brand new
//! object the caller solely owns, so widening its element type at the call site
//! is sound
//!
//! this patch encodes that by giving each such method a `Never`-defaulted type
//! parameter and unioning it into every invariant position of the return type:
//!
//! ```by
//! def copy[Widen... = Never](self) -> list[Element | Widen...]
//! ```
//!
//! with an expected type the parameter solves to the widening
//! (`b: list[int | None] = a.copy()`); with no expected type it defaults to
//! `Never` and `Element | Never` collapses back to `Element`, so ordinary
//! inference is unchanged (`reveal_type(a.copy())` is still `list[int]`)
//!
//! unlike the legacy-form semantic patches this runs in the post-pep 695 pass:
//! it keys off the explicit variance keywords to widen only invariant positions,
//! leaving covariant containers (`frozenset`, `tuple`) alone — their copies
//! already widen for free

use std::collections::HashSet;
use std::path::Path;

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_ast::{Expr, ModModule, Stmt, StmtClassDef, StmtFunctionDef, TypeParam, Variance};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch, module_qualname};

/// module that owns the container definitions we widen
const MODULE: &str = "builtins";

/// the mutable, invariant builtin containers. `frozenset` and `tuple` are
/// immutable and therefore covariant, so their copies already widen without help
const TARGET_CLASSES: &[&str] = &["list", "set", "dict"];

/// `Never` is version-guarded in `typing` (3.11+) but unconditional in
/// `typing_extensions`; builtins loads for every version, so we source it there
const NEVER_IMPORT_FROM: &str = "typing_extensions";

pub struct OutputWidening;

impl Patch for OutputWidening {
    fn name(&self) -> &'static str {
        "output-widening"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &["builtins.list", "builtins.set", "builtins.dict"]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, _source: &str) -> Vec<Edit> {
        if module_qualname(module_path).as_deref() != Some(MODULE) {
            return Vec::new();
        }

        let body = &parsed.syntax().body;
        let mut edits = Vec::new();
        for stmt in body {
            if let Stmt::ClassDef(class) = stmt
                && TARGET_CLASSES.contains(&class.name.as_str())
            {
                widen_class(class, &mut edits);
            }
        }

        // a widened method references `Never` in its default; make sure the name
        // resolves. done last so it is skipped when nothing was widened
        if !edits.is_empty()
            && let Some(import) = ensure_never_import(body)
        {
            edits.push(import);
        }
        edits
    }
}

/// widen every fresh-container-returning method of one container class
fn widen_class(class: &StmtClassDef, edits: &mut Vec<Edit>) {
    let Some(type_params) = &class.type_params else {
        return;
    };
    // the class's type parameters in order, paired with whether each is invariant
    let params: Vec<ClassParam> = type_params
        .iter()
        .map(|tp| ClassParam {
            name: tp.name().as_str(),
            invariant: matches!(tp, TypeParam::TypeVar(t) if t.variance == Some(Variance::Invariant)),
        })
        .collect();

    let mut methods = Vec::new();
    collect_methods(&class.body, &mut methods);
    for func in methods {
        widen_method(func, class.name.as_str(), &params, edits);
    }
}

/// collect the class's methods, descending through `if`/`else` version guards
/// but not into nested classes or a method's own body — those own their scopes
fn collect_methods<'a>(body: &'a [Stmt], out: &mut Vec<&'a StmtFunctionDef>) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(func) => out.push(func),
            Stmt::If(if_stmt) => {
                collect_methods(&if_stmt.body, out);
                for clause in &if_stmt.elif_else_clauses {
                    collect_methods(&clause.body, out);
                }
            }
            _ => {}
        }
    }
}

struct ClassParam<'a> {
    name: &'a str,
    invariant: bool,
}

/// widen a single method if it returns a fresh instance of its own class, i.e.
/// `ClassName[...]` with one subscript position per class type parameter
fn widen_method(
    func: &StmtFunctionDef,
    class_name: &str,
    params: &[ClassParam],
    edits: &mut Vec<Edit>,
) {
    // idempotent: a previous run already added the widening parameter(s)
    if let Some(type_params) = &func.type_params
        && type_params
            .iter()
            .any(|p| p.name().as_str().starts_with(WIDEN_PREFIX))
    {
        return;
    }

    let Some(Expr::Subscript(sub)) = func.returns.as_deref() else {
        return;
    };
    let Expr::Name(head) = sub.value.as_ref() else {
        return;
    };
    if head.id.as_str() != class_name {
        return;
    }

    let positions: Vec<&Expr> = match sub.slice.as_ref() {
        Expr::Tuple(tuple) => tuple.elts.iter().collect(),
        single => vec![single],
    };
    // only a return that mirrors the class's own arity is a fresh same-class
    // container; anything else (e.g. `dict.keys() -> KeysView[Key]`) is skipped
    // by the head check above, and defensive arity keeps position/param aligned
    if positions.len() != params.len() {
        return;
    }

    let mut used: HashSet<String> = params.iter().map(|p| p.name.to_string()).collect();
    if let Some(type_params) = &func.type_params {
        used.extend(type_params.iter().map(|p| p.name().to_string()));
    }

    let mut new_params: Vec<String> = Vec::new();
    let mut position_inserts: Vec<Edit> = Vec::new();
    for (position, param) in positions.iter().zip(params) {
        // widen only invariant positions that actually carry the class parameter
        if !param.invariant || !references(position, param.name) {
            continue;
        }
        let widen = unique_name(&format!("{WIDEN_PREFIX}{}", param.name), &mut used);
        let at = position.range().end().to_usize();
        position_inserts.push(Edit {
            start: at,
            end: at,
            replacement: format!(" | {widen}"),
        });
        new_params.push(format!("{widen} = Never"));
    }

    if new_params.is_empty() {
        return;
    }
    edits.extend(position_inserts);
    edits.push(type_param_edit(func, &new_params));
}

/// prefix identifying the synthesized widening parameters (drives idempotency)
const WIDEN_PREFIX: &str = "Widen";

/// insert the widening parameters into the method's type-parameter list,
/// creating the list when the method has none
fn type_param_edit(func: &StmtFunctionDef, new_params: &[String]) -> Edit {
    match &func.type_params {
        // append before the closing `]`; existing params carry no default, so
        // the defaulted widening params correctly sort last
        Some(type_params) => {
            let close = type_params.range().end().to_usize() - 1;
            Edit {
                start: close,
                end: close,
                replacement: format!(", {}", new_params.join(", ")),
            }
        }
        // fresh `[...]` right after the method name
        None => {
            let at = func.name.range().end().to_usize();
            Edit {
                start: at,
                end: at,
                replacement: format!("[{}]", new_params.join(", ")),
            }
        }
    }
}

/// add `Never` to the module's `typing_extensions` import, or `None` if it is
/// already imported. falls back to a fresh import line if the module has no
/// `typing_extensions` import to extend
fn ensure_never_import(body: &[Stmt]) -> Option<Edit> {
    let mut fallback_anchor = None;
    for stmt in body {
        if let Stmt::ImportFrom(import) = stmt
            && import
                .module
                .as_ref()
                .is_some_and(|m| m == NEVER_IMPORT_FROM)
        {
            if import
                .names
                .iter()
                .any(|alias| alias.name.as_str() == "Never")
            {
                return None;
            }
            let first = import.names.first()?;
            let at = first.name.range().start().to_usize();
            return Some(Edit {
                start: at,
                end: at,
                replacement: "Never, ".to_string(),
            });
        }
        if fallback_anchor.is_none() && matches!(stmt, Stmt::ImportFrom(_) | Stmt::Import(_)) {
            fallback_anchor = Some(stmt.range().start().to_usize());
        }
    }
    fallback_anchor.map(|at| Edit {
        start: at,
        end: at,
        replacement: format!("from {NEVER_IMPORT_FROM} import Never\n"),
    })
}

/// whether `name` appears as a bare name anywhere in `expr`
fn references(expr: &Expr, name: &str) -> bool {
    let mut finder = NameFinder { name, found: false };
    finder.visit_expr(expr);
    finder.found
}

struct NameFinder<'a> {
    name: &'a str,
    found: bool,
}

impl<'a> SourceOrderVisitor<'a> for NameFinder<'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr
            && name.id.as_str() == self.name
        {
            self.found = true;
        }
        if !self.found {
            walk_expr(self, expr);
        }
    }
}

/// `candidate`, suffixed with the smallest integer that avoids a collision with
/// `used`; records the chosen name in `used`
fn unique_name(candidate: &str, used: &mut HashSet<String>) -> String {
    let mut chosen = candidate.to_string();
    let mut suffix = 2;
    while used.contains(&chosen) {
        chosen = format!("{candidate}{suffix}");
        suffix += 1;
    }
    used.insert(chosen.clone());
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(path: &str, src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = OutputWidening.rewrite(Path::new(path), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn widens_pure_output_copy() {
        let src = "\
from typing_extensions import Self
class list[in out Element]:
    def copy(self) -> list[Element]: ...
    def append(self, object: Element, /) -> None: ...
";
        let expected = "\
from typing_extensions import Never, Self
class list[in out Element]:
    def copy[WidenElement = Never](self) -> list[Element | WidenElement]: ...
    def append(self, object: Element, /) -> None: ...
";
        assert_eq!(run("builtins.byi", src), expected);
    }

    #[test]
    fn extends_existing_type_params() {
        let src = "\
from typing_extensions import Never
class list[in out Element]:
    def __add__[Other](self, value: list[Other], /) -> list[Other | Element]: ...
";
        let expected = "\
from typing_extensions import Never
class list[in out Element]:
    def __add__[Other, WidenElement = Never](self, value: list[Other], /) -> list[Other | Element | WidenElement]: ...
";
        assert_eq!(run("builtins.byi", src), expected);
    }

    #[test]
    fn widens_each_invariant_position_of_a_multi_param_class() {
        let src = "\
from typing_extensions import Never
class dict[in out Key, in out Value]:
    def copy(self) -> dict[Key, Value]: ...
";
        let expected = "\
from typing_extensions import Never
class dict[in out Key, in out Value]:
    def copy[WidenKey = Never, WidenValue = Never](self) -> dict[Key | WidenKey, Value | WidenValue]: ...
";
        assert_eq!(run("builtins.byi", src), expected);
    }

    #[test]
    fn leaves_covariant_containers_untouched() {
        let src = "\
class frozenset[out Element]:
    def copy(self) -> frozenset[Element]: ...
";
        assert_eq!(run("builtins.byi", src), src);
    }

    #[test]
    fn ignores_returns_that_are_not_a_fresh_same_class_container() {
        let src = "\
class list[in out Element]:
    def pop(self, index: SupportsIndex = -1, /) -> Element: ...
    def __iter__(self) -> Iterator[Element]: ...
    def __iadd__(self, value: Iterable[Element], /) -> Self: ...
    def clear(self) -> None: ...
";
        assert_eq!(run("builtins.byi", src), src);
    }

    #[test]
    fn descends_into_version_guards_but_not_nested_classes() {
        let src = "\
from typing_extensions import Never
class dict[in out Key, in out Value]:
    if sys.version_info >= (3, 9):
        def __or__[T1, T2](self, value: dict[T1, T2], /) -> dict[Key | T1, Value | T2]: ...
    class _NestedView[in out Element]:
        def copy(self) -> _NestedView[Element]: ...
";
        let expected = "\
from typing_extensions import Never
class dict[in out Key, in out Value]:
    if sys.version_info >= (3, 9):
        def __or__[T1, T2, WidenKey = Never, WidenValue = Never](self, value: dict[T1, T2], /) -> dict[Key | T1 | WidenKey, Value | T2 | WidenValue]: ...
    class _NestedView[in out Element]:
        def copy(self) -> _NestedView[Element]: ...
";
        assert_eq!(run("builtins.byi", src), expected);
    }

    #[test]
    fn idempotent_when_already_widened() {
        let src = "\
from typing_extensions import Never
class list[in out Element]:
    def copy[WidenElement = Never](self) -> list[Element | WidenElement]: ...
";
        assert_eq!(run("builtins.byi", src), src);
    }

    #[test]
    fn skips_non_builtins_modules() {
        let src = "\
class list[in out Element]:
    def copy(self) -> list[Element]: ...
";
        assert_eq!(run("collections.byi", src), src);
    }

    #[test]
    fn skips_untargeted_classes() {
        let src = "\
class MyList[in out Element]:
    def copy(self) -> MyList[Element]: ...
";
        assert_eq!(run("builtins.byi", src), src);
    }
}

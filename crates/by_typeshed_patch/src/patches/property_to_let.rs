//! read-only `@property` → `let NAME: T`
//!
//! most typeshed properties are not computed — they expose a read-only value
//! (`int.imag`, `int.numerator`, ...). a valueless `let NAME: T` declares
//! exactly that: a read-only attribute, identical to a read-only property but
//! without the descriptor machinery
//!
//! deliberately conservative — only a getter that is
//!
//! - decorated with `@property` and nothing else (no `@abstractmethod`,
//!   `@overload`, `@deprecated`, ...)
//! - `self`-only (a genuine property getter takes no further arguments)
//! - annotated with a return type
//! - the sole getter for its name (not an overloaded property)
//! - not paired with a `@NAME.setter` / `@NAME.deleter` (read/write properties
//!   are left alone — they are properties for a reason)
//! - not a genuinely *computed* descriptor rather than a read-only value:
//!   `type.__mro__`/`__bases__` are data descriptors whose descriptor-ness ty
//!   uses to model metaclass-override uncertainty, and `ParamSpec.args`/`.kwargs`
//!   return the special `ParamSpecArgs`/`ParamSpecKwargs` markers
//!
//! is converted; anything else is left untouched

use std::collections::{HashMap, HashSet};
use std::path::Path;

use ruff_python_ast::{Decorator, Expr, ModModule, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

pub struct PropertyToLet;

impl Patch for PropertyToLet {
    fn name(&self) -> &'static str {
        "property-to-let"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let mut edits = Vec::new();
        walk_classes(&parsed.syntax().body, &mut |class| {
            convert_class(class, source, &mut edits);
        });
        edits
    }
}

fn convert_class(class: &StmtClassDef, source: &str, edits: &mut Vec<Edit>) {
    // names with a setter/deleter (read/write) and per-name property-getter counts
    let mut read_write: HashSet<&str> = HashSet::new();
    let mut getter_count: HashMap<&str, u32> = HashMap::new();
    for member in &class.body {
        if let Stmt::FunctionDef(func) = member {
            for decorator in &func.decorator_list {
                if let Some((name, kind)) = accessor_decorator(&decorator.expression)
                    && kind != "property"
                {
                    read_write.insert(name);
                }
            }
            if is_sole_property(&func.decorator_list) {
                *getter_count.entry(func.name.as_str()).or_default() += 1;
            }
        }
    }

    for member in &class.body {
        let Stmt::FunctionDef(func) = member else {
            continue;
        };
        let name = func.name.as_str();
        if !is_sole_property(&func.decorator_list)
            || read_write.contains(name)
            || getter_count.get(name) != Some(&1)
            || !is_self_only(func)
        {
            continue;
        }
        let Some(returns) = &func.returns else {
            continue;
        };
        if is_computed_descriptor(name, returns) {
            if let Some(comment) = descriptor_rationale_comment(func, source) {
                edits.push(comment);
            }
            continue;
        }
        let ret = &source[returns.range()];
        edits.push(Edit {
            start: func.range().start().to_usize(),
            end: func.range().end().to_usize(),
            replacement: format!("let {name}: {ret}"),
        });
    }
}

/// `@property`, `@NAME.setter`, `@NAME.deleter` → `(NAME_or_"property", kind)`
fn accessor_decorator(expr: &Expr) -> Option<(&str, &str)> {
    match expr {
        Expr::Name(name) if name.id == "property" => Some(("property", "property")),
        Expr::Attribute(attr) if matches!(attr.attr.as_str(), "setter" | "deleter") => attr
            .value
            .as_name_expr()
            .map(|n| (n.id.as_str(), attr.attr.as_str())),
        _ => None,
    }
}

/// whether the decorator list is exactly `@property` and nothing else
fn is_sole_property(decorators: &[Decorator]) -> bool {
    matches!(decorators, [only] if matches!(&only.expression, Expr::Name(n) if n.id == "property"))
}

const DESCRIPTOR_RATIONALE: &str =
    "# kept as a property: a data descriptor, not a read-only value — ty relies on the descriptor";

/// an insertion edit adding [`DESCRIPTOR_RATIONALE`] above `func`, or `None` if
/// the line above already carries it (idempotency)
fn descriptor_rationale_comment(func: &StmtFunctionDef, source: &str) -> Option<Edit> {
    let bytes = source.as_bytes();
    // start of the property's own line (`func` range starts at `@property`)
    let mut line_start = func.range().start().to_usize();
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    // already annotated?
    let preceding = source[..line_start].trim_end_matches(['\n', ' ', '\t']);
    if preceding
        .rsplit('\n')
        .next()
        .is_some_and(|last| last.contains("kept as a property"))
    {
        return None;
    }
    let indent = &source[line_start..func.range().start().to_usize()];
    Some(Edit {
        start: func.range().start().to_usize(),
        end: func.range().start().to_usize(),
        replacement: format!("{DESCRIPTOR_RATIONALE}\n{indent}"),
    })
}

/// a property that ty models as a genuine descriptor rather than a read-only
/// value, so it must keep the `@property` form:
///
/// - `type.__mro__` / `type.__bases__` — data descriptors whose descriptor-ness
///   drives metaclass-override uncertainty
/// - `ParamSpec.args` / `.kwargs` — return the `ParamSpecArgs`/`ParamSpecKwargs`
///   markers, which are only valid via the descriptor
fn is_computed_descriptor(name: &str, returns: &Expr) -> bool {
    matches!(name, "__mro__" | "__bases__")
        || matches!(returns, Expr::Name(n) if matches!(n.id.as_str(), "ParamSpecArgs" | "ParamSpecKwargs"))
}

/// whether the signature is `(self)` with no further parameters
fn is_self_only(func: &StmtFunctionDef) -> bool {
    let params = &func.parameters;
    params.posonlyargs.len() + params.args.len() == 1
        && params.kwonlyargs.is_empty()
        && params.vararg.is_none()
        && params.kwarg.is_none()
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
        let edits = PropertyToLet.rewrite(Path::new("m.byi"), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn converts_read_only_property() {
        let src = "\
class C:
    @property
    def imag(self) -> Literal[0]: ...
";
        let expected = "\
class C:
    let imag: Literal[0]
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn converts_property_with_docstring_body() {
        let src = "\
class C:
    @property
    def imag(self) -> int:
        \"\"\"the imaginary part\"\"\"
";
        let expected = "\
class C:
    let imag: int
";
        assert_eq!(run(src), expected);
    }

    #[test]
    fn leaves_read_write_property() {
        let src = "\
class C:
    @property
    def x(self) -> int: ...
    @x.setter
    def x(self, value: int) -> None: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_abstract_property() {
        let src = "\
class C:
    @abstractmethod
    @property
    def x(self) -> int: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_property_with_extra_params() {
        // not a real getter shape
        let src = "\
class C:
    @property
    def x(self, y: int) -> int: ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_property_without_return_annotation() {
        let src = "\
class C:
    @property
    def x(self): ...
";
        assert_eq!(run(src), src);
    }

    #[test]
    fn leaves_computed_descriptors_but_annotates_them() {
        let src = "\
class type:
    @property
    def __mro__(self) -> tuple[type, ...]: ...
class ParamSpec:
    @property
    def args(self) -> ParamSpecArgs: ...
";
        let expected = "\
class type:
    # kept as a property: a data descriptor, not a read-only value — ty relies on the descriptor
    @property
    def __mro__(self) -> tuple[type, ...]: ...
class ParamSpec:
    # kept as a property: a data descriptor, not a read-only value — ty relies on the descriptor
    @property
    def args(self) -> ParamSpecArgs: ...
";
        assert_eq!(run(src), expected);
        // idempotent: re-running does not stack another comment
        assert_eq!(run(expected), expected);
    }

    #[test]
    fn leaves_overloaded_property_getter() {
        let src = "\
class C:
    @property
    def x(self) -> int: ...
    @property
    def x(self) -> str: ...
";
        assert_eq!(run(src), src);
    }
}

//! deletes typeshed symbols basedpython has no use for
//!
//! - `builtins.function` — a `@type_check_only` stand-in for `types.FunctionType`
//!   that upstream keeps only to satisfy other checkers; ty models functions
//!   through `KnownClass::FunctionType`, so the shim is dead weight
//! - `typing.AwaitableGenerator` — upstream marks it obsolete in favour of
//!   `_typeshed._type_checker_internals.AwaitableGenerator`
//!
//! `builtins.ellipsis` (an `ellipsis = EllipsisType` backwards-compat alias) is
//! deliberately *not* deleted: ty models the `...` literal through
//! `types.EllipsisType`, but third-party stubs still annotate parameters with the
//! `ellipsis` type by name — pydantic's `Field` overloads have `default: ellipsis`
//! — and dropping the alias turns those annotations into `Unknown`, poisoning
//! overload resolution
//!
//! each entry deletes the whole (possibly decorated) statement plus any comment
//! lines directly attached above it

use std::path::Path;

use ruff_python_ast::{ModModule, Stmt};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch, delete_with_leading_comments};

/// `(module qualname, symbol name)` pairs to remove
const DEAD: &[(&str, &str)] = &[("builtins", "function"), ("typing", "AwaitableGenerator")];

pub struct DeleteDeadSymbols;

impl Patch for DeleteDeadSymbols {
    fn name(&self) -> &'static str {
        "delete-dead-symbols"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &["builtins.function", "typing.AwaitableGenerator"]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let Some(module) = crate::module_qualname(module_path) else {
            return Vec::new();
        };
        let targets: Vec<&str> = DEAD
            .iter()
            .filter(|(m, _)| *m == module)
            .map(|(_, name)| *name)
            .collect();
        if targets.is_empty() {
            return Vec::new();
        }

        let mut edits = Vec::new();
        for stmt in &parsed.syntax().body {
            if let Some(name) = stmt_name(stmt)
                && targets.contains(&name)
            {
                edits.push(delete_with_leading_comments(stmt.range(), source));
            }
        }
        edits
    }
}

/// the defined name of a top-level statement, if it declares one
fn stmt_name(stmt: &Stmt) -> Option<&str> {
    match stmt {
        Stmt::ClassDef(class) => Some(class.name.as_str()),
        Stmt::FunctionDef(func) => Some(func.name.as_str()),
        Stmt::Assign(assign) => match assign.targets.as_slice() {
            [ruff_python_ast::Expr::Name(name)] => Some(name.id.as_str()),
            _ => None,
        },
        Stmt::AnnAssign(assign) => assign.target.as_name_expr().map(|n| n.id.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(path: &str, src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = DeleteDeadSymbols.rewrite(Path::new(path), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn deletes_function_with_decorators_and_comments() {
        let src = "\
class int: ...
# https://example/issues/7580
# Obsolete, use types.FunctionType instead.
@final
@type_check_only
class function:
    __name__: str
class str: ...
";
        let expected = "\
class int: ...
class str: ...
";
        assert_eq!(run("builtins.byi", src), expected);
    }

    #[test]
    fn deletes_awaitable_generator_in_typing_only() {
        let src = "\
# Obsolete, use elsewhere instead.
@type_check_only
class AwaitableGenerator[out E, in S, out R, in out O](Awaitable[R])
class Other: ...
";
        let expected = "class Other: ...\n";
        assert_eq!(run("typing.byi", src), expected);
    }

    #[test]
    fn keeps_ellipsis_alias() {
        // third-party stubs (e.g. pydantic's `Field`) annotate with the
        // `ellipsis` type by name, so the alias must survive
        let src = "\
class int: ...
# Backwards compatibility hack for the ellipsis type in 3.9 and earlier.
ellipsis = EllipsisType
class str: ...
";
        assert_eq!(run("builtins.byi", src), src);
    }

    #[test]
    fn does_not_touch_other_modules() {
        let src = "class function:\n    x: int\n";
        assert_eq!(run("types.byi", src), src);
    }

    #[test]
    fn keeps_blank_separated_comment() {
        let src = "\
# a section header

@type_check_only
class function: ...
";
        let expected = "# a section header\n\n";
        assert_eq!(run("builtins.byi", src), expected);
    }
}

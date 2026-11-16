//! drop the `dynamic` base of `types.NotImplementedType`
//!
//! typeshed derives the class from `Any` so that `return NotImplemented` type-checks in an
//! operator dunder whatever the dunder declares it returns. that makes `NotImplemented`
//! assignable to every type, everywhere. basedpython types the operator protocol instead: a
//! dunder that returns `NotImplemented` for an operand it can't handle says so in its return
//! type, and the operator falls back past it, so the class is an ordinary final class
//!
//! idempotent: a class with no `dynamic` base is left alone

use std::path::Path;

use ruff_python_ast::{ModModule, Stmt};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::{Edit, Patch};

pub struct NotImplementedTypeStatic;

impl Patch for NotImplementedTypeStatic {
    fn name(&self) -> &'static str {
        "not-implemented-type-static"
    }
    fn target_symbols(&self) -> &'static [&'static str] {
        &["types.NotImplementedType"]
    }
    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, _source: &str) -> Vec<Edit> {
        if crate::module_qualname(module_path).as_deref() != Some("types") {
            return Vec::new();
        }
        parsed
            .syntax()
            .body
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::ClassDef(class) if class.name.as_str() == "NotImplementedType" => {
                    let arguments = class.arguments.as_deref()?;
                    let [base] = &*arguments.args else {
                        return None;
                    };
                    if !arguments.keywords.is_empty()
                        || base.as_name_expr().is_none_or(|name| name.id != "dynamic")
                    {
                        return None;
                    }
                    Some(Edit {
                        start: arguments.range().start().to_usize(),
                        end: arguments.range().end().to_usize(),
                        replacement: String::new(),
                    })
                }
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use super::NotImplementedTypeStatic;
    use crate::{Patch, apply_edits};

    fn rewrite(source: &str) -> String {
        let parsed = parse_unchecked_source(source, PySourceType::BasedPythonStub);
        let edits = NotImplementedTypeStatic.rewrite(Path::new("types.byi"), &parsed, source);
        apply_edits(source, edits)
    }

    #[test]
    fn drops_the_dynamic_base() {
        assert_eq!(
            rewrite("final class NotImplementedType(dynamic):\n    \"\"\"doc\"\"\"\n"),
            "final class NotImplementedType:\n    \"\"\"doc\"\"\"\n"
        );
    }

    #[test]
    fn leaves_a_static_class_alone() {
        let source = "final class NotImplementedType:\n    \"\"\"doc\"\"\"\n";
        assert_eq!(rewrite(source), source);
    }
}

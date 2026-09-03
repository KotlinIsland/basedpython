//! `T | None` → `T?`
//!
//! the rule itself lives in `by_transforms`' `optional_type` reverse transform,
//! which is what a fresh sync's reverse-transpile runs. the committed tree is
//! that pass's own output, so it is never reached again — this patch replays
//! the one rewrite over it, and deliberately holds no second copy of the rule:
//! everything it knows comes from
//! [`by_transforms::optional_marker_edits`]
//!
//! that entry point answers the one question a purely syntactic patch could not:
//! `?` over a bare *type variable* is the wrapped optional, so a stub's
//! `Value | None` has to stay a union

use std::path::Path;

use ruff_python_ast::ModModule;
use ruff_python_parser::Parsed;

use crate::{Edit, Patch};

pub struct OptionalMarker;

impl Patch for OptionalMarker {
    fn name(&self) -> &'static str {
        "optional-marker"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, _module_path: &Path, _parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        by_transforms::optional_marker_edits(source)
            .into_iter()
            .map(|(range, replacement)| Edit {
                start: range.start().to_usize(),
                end: range.end().to_usize(),
                replacement,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_edits;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    fn patch(source: &str) -> String {
        let parsed = parse_unchecked_source(source, PySourceType::BasedPythonStub);
        apply_edits(
            source,
            OptionalMarker.rewrite(Path::new("m.byi"), &parsed, source),
        )
    }

    #[test]
    fn marks_an_optional_parameter_and_return() {
        assert_eq!(
            patch("def f(x: str | None = None) -> bytes | None: ...\n"),
            "def f(x: str? = None) -> bytes?: ...\n"
        );
    }

    #[test]
    fn leaves_a_type_parameter_as_a_union() {
        let source = "class Box[T]:\n    def get(self) -> T | None: ...\n";
        assert_eq!(patch(source), source);
    }

    #[test]
    fn is_idempotent() {
        let once = patch("x: int | None\n");
        assert_eq!(once, "x: int?\n");
        assert_eq!(patch(&once), once);
    }
}

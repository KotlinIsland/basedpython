//! AST pass: strips def-site `out` / `in` / `in out` keyword from
//! `class C[out T]` type-parameter declarations. variance info preserved
//! on the AST node and consumed by ty's type checker directly — this
//! only deletes surface bytes so output is valid Python.
//!
//! also strips the lower end of a basedpython bound range (`class C[T: int..object]`), which
//! ty enforces from the AST node; python bounds only have an upper end
//!
//! also strips the basedpython `/` and bare `*` type-parameter separators
//! (`class C[A, /, B, *, D]`). their positional-only / keyword-only meaning is
//! carried on the AST node and enforced by ty; python's own type-parameter
//! grammar has no separators, so the surface bytes are deleted the same way
//!
//! use-site variance stripped upstream by [`use_site_variance::strip`]. when
//! [`generics`](super::generics) polyfills the type-params header into
//! `Generic[_T]`, its wider replacement wins via `ast_driver`'s first-wins
//! dedup and this pass's narrow deletion becomes a no-op for that stmt

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Stmt, TypeParam, TypeParams};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

pub(crate) struct VarianceStripPass<'src> {
    source: &'src str,
}

impl<'src> VarianceStripPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for VarianceStripPass<'_> {
    fn run(&self, stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut state = State {
            source: self.source,
            edits: Vec::new(),
        };
        for stmt in stmts {
            state.visit_stmt(stmt);
        }
        for (range, repl) in state.edits {
            ctx.text_edits.push((range, repl));
        }
    }
}

struct State<'src> {
    source: &'src str,
    edits: Vec<(TextRange, String)>,
}

impl State<'_> {
    /// basedpython: delete a `/` or bare `*` separator token along with one adjacent comma, so the
    /// remaining type-parameter list is valid python. the comma after the separator is preferred;
    /// a trailing separator (`[A, /]`) takes the comma before it instead.
    fn strip_separator(&mut self, tp: &TypeParams, sep: TextRange) {
        let after = &self.source[usize::from(sep.end())..usize::from(tp.range().end())];
        if let Some(comma) = after.find(',') {
            let mut end = usize::from(sep.end()) + comma + 1;
            // swallow the run of spaces after the comma so `A, /, B` collapses cleanly to `A, B`
            while self.source[end..].starts_with(' ') {
                end += 1;
            }
            self.edits.push((
                TextRange::new(sep.start(), TextSize::try_from(end).unwrap_or(sep.end())),
                String::new(),
            ));
            return;
        }

        // no following comma — the separator is the last token in the list, so remove the comma
        // that precedes it
        let before = &self.source[usize::from(tp.range().start())..usize::from(sep.start())];
        if let Some(comma) = before.rfind(',') {
            let start = usize::from(tp.range().start()) + comma;
            self.edits.push((
                TextRange::new(TextSize::try_from(start).unwrap_or(sep.start()), sep.end()),
                String::new(),
            ));
        }
    }
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let type_params = match stmt {
            Stmt::ClassDef(c) => c.type_params.as_deref(),
            Stmt::FunctionDef(f) => f.type_params.as_deref(),
            _ => None,
        };

        if let Some(tp) = type_params {
            for param in &tp.type_params {
                if let TypeParam::TypeVar(tv) = param {
                    if tv.variance.is_some() {
                        let prefix = TextRange::new(tv.range().start(), tv.name.range().start());
                        self.edits.push((prefix, String::new()));
                    }
                    // `T: int..object` keeps only its upper end, so delete `int..`
                    if let Some(lower) = &tv.lower_bound
                        && let Some(bound) = &tv.bound
                    {
                        self.edits.push((
                            TextRange::new(lower.range().start(), bound.range().start()),
                            String::new(),
                        ));
                    }
                }
            }
            if let Some(slash) = tp.separators.slash_range {
                self.strip_separator(tp, slash);
            }
            if let Some(star) = tp.separators.star_range {
                self.strip_separator(tp, star);
            }
        }

        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, PythonVersion, transpile};
    use indoc::indoc;

    fn check_py312(input: &str, expected: &str) {
        let config = Config {
            min_version: PythonVersion::PY312,
            ..Config::test_default()
        };
        assert_eq!(transpile(input, &config).unwrap(), expected);
    }

    #[test]
    fn strips_out_keyword_on_class() {
        check_py312("class C[out T]: ...\n", "class C[T]: ...\n");
    }

    #[test]
    fn strips_in_keyword_on_class() {
        check_py312("class C[in T]: ...\n", "class C[T]: ...\n");
    }

    #[test]
    fn strips_in_out_keyword_on_class() {
        check_py312("class C[in out T]: ...\n", "class C[T]: ...\n");
    }

    #[test]
    fn strips_on_function() {
        check_py312(
            indoc! {"
                def f[out T](x: T) -> T:
                    return x
            "},
            indoc! {"
                def f[T](x: T) -> T:
                    return x
            "},
        );
    }

    #[test]
    fn invariant_typevar_untouched() {
        check_py312("class C[T]: ...\n", "class C[T]: ...\n");
    }

    #[test]
    fn strips_lower_bound_end() {
        check_py312(
            "class C[T: int..object]: ...\n",
            "class C[T: object]: ...\n",
        );
    }

    #[test]
    fn strips_lower_bound_end_with_default() {
        // a typevar default is 3.13, so this goes through the `Generic[...]` polyfill instead of
        // the surface-byte deletion; the lower end has to be dropped on that path too
        check_py312(
            "class C[T: int..object = str]: ...\n",
            indoc! {"
                from typing import Generic
                from typing_extensions import TypeVar
                _T = TypeVar(\"_T\", bound=object, default=str)
                class C(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn strips_lower_bound_end_under_generic_polyfill() {
        let config = Config {
            min_version: PythonVersion::PY311,
            ..Config::test_default()
        };
        assert_eq!(
            transpile("class C[T: int..object]: ...\n", &config).unwrap(),
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=object)
                class C(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn strips_lower_bound_end_with_variance() {
        check_py312(
            "class C[out T: int..object]: ...\n",
            "class C[T: object]: ...\n",
        );
    }

    #[test]
    fn strips_lower_bound_end_on_function() {
        check_py312(
            indoc! {"
                def f[T: int..object](x: T) -> T:
                    return x
            "},
            indoc! {"
                def f[T: object](x: T) -> T:
                    return x
            "},
        );
    }

    #[test]
    fn strips_lower_bound_end_for_several_params() {
        check_py312(
            "class C[T: int..object, U: str..object]: ...\n",
            "class C[T: object, U: object]: ...\n",
        );
    }

    #[test]
    fn plain_bound_untouched() {
        check_py312("class C[T: object]: ...\n", "class C[T: object]: ...\n");
    }

    #[test]
    fn strips_both_separators() {
        check_py312("class C[A, /, B, *, D]: ...\n", "class C[A, B, D]: ...\n");
    }

    #[test]
    fn strips_adjacent_separators() {
        check_py312("class C[A, /, *, B]: ...\n", "class C[A, B]: ...\n");
    }

    #[test]
    fn strips_trailing_slash() {
        check_py312("class C[A, /]: ...\n", "class C[A]: ...\n");
    }

    #[test]
    fn strips_leading_star() {
        check_py312("class C[*, A]: ...\n", "class C[A]: ...\n");
    }

    #[test]
    fn strips_separators_with_variance() {
        check_py312("class C[out A, /, in B]: ...\n", "class C[A, B]: ...\n");
    }

    #[test]
    fn strips_separators_on_function() {
        check_py312(
            indoc! {"
                def f[A, /, B](a: A, b: B) -> A:
                    return a
            "},
            indoc! {"
                def f[A, B](a: A, b: B) -> A:
                    return a
            "},
        );
    }
}

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::{Expr, PythonVersion, Stmt};
use ruff_text_size::Ranged;

use crate::Config;
use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::transforms::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions};
use crate::transforms::{literal_types, optional_type};
use crate::type_info::TypeInfo;

/// The element type an unpacked tuple element wraps, whichever way the target
/// spells the unpack.
fn strip_unpack(element: &str) -> Option<&str> {
    element.strip_prefix('*').or_else(|| {
        element
            .strip_prefix("Unpack[")
            .and_then(|rest| rest.strip_suffix(']'))
    })
}

/// Rewrites tuple literal types in type positions.
///
/// `a: (int, str)` → `a: tuple[int, str]`
///
/// Fires in syntactic annotation positions (AnnAssign.annotation, parameter
/// annotations, FunctionDef.returns) and in type applications (subscripts
/// whose base is a known type, e.g. `list[(int, str)]`). Pure runtime
/// subscripts on unresolved names are left alone.
pub(crate) struct TupleLiteralType<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    min_version: PythonVersion,
    /// set when a lowering spelled an `Unpack`, so the pass can ask for the import
    pub(crate) needs_unpack_import: std::cell::Cell<bool>,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> TupleLiteralType<'src> {
    pub(crate) fn new(
        source: &'src str,
        types: &'src dyn TypeInfo,
        min_version: PythonVersion,
    ) -> Self {
        Self {
            source,
            types,
            min_version,
            needs_unpack_import: std::cell::Cell::new(false),
            edits: Vec::new(),
        }
    }

    /// How an unpacked element of a tuple type is spelled for the target. The
    /// star form is PEP 646, python 3.11; below that it is a *syntax* error
    /// rather than something the `__future__` import could defer, so a tuple
    /// type has to say `Unpack[...]` instead.
    fn unpack(&self, inner: &str) -> String {
        if self.min_version >= PythonVersion::PY311 {
            format!("*{inner}")
        } else {
            self.needs_unpack_import.set(true);
            format!("Unpack[{inner}]")
        }
    }

    fn src(&self, range: ruff_text_size::TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// Source text for `expr`, with literal-type rewrites applied if needed.
    fn fallback_src(&self, expr: &Expr) -> String {
        literal_types::rewrite_type_expr(self.source, self.types, expr)
            .unwrap_or_else(|| self.src(expr.range()).to_owned())
    }

    /// Returns a rewritten annotation string if any transformation is needed,
    /// or `None` if the expression requires no change.
    fn transform_annotation(&self, expr: &Expr) -> Option<String> {
        match expr {
            // basedpython anonymous named tuple types are owned by the
            // dedicated anon-NT transform; this transform must not touch
            // them (its naive `tuple[...]` rewrite would emit invalid output
            // like `tuple[name: str, age: int]`).
            Expr::Tuple(t) if t.is_anon_named_tuple => None,

            // Parenthesized tuple: `(int, str)` → `tuple[int, str]`.
            // Empty tuple `()` lowers to `tuple[()]` — the only python form
            // for the empty-tuple type. leaving `()` raw is invalid as an
            // annotation
            Expr::Tuple(t) if t.parenthesized => {
                if t.elts.is_empty() {
                    return Some("tuple[()]".to_owned());
                }
                // `*: T` and a bare unpack `*A` share one AST shape, so which
                // one a `Starred` element is comes from the enclosing tuple:
                // only the parameter-shape parser produces `*: T`
                let parameter_shape = t.has_parameter_shape();
                let lowered: Vec<String> = t
                    .elts
                    .iter()
                    .map(|e| self.lower_tuple_element(e, parameter_shape))
                    .filter(|s| !s.is_empty())
                    .collect();
                if lowered.is_empty() {
                    return Some("tuple[()]".to_owned());
                }
                // pure variadic `(*: T)` → `tuple[T, ...]` directly
                // rather than the wrapped `tuple[*tuple[T, ...]]` form
                if parameter_shape
                    && lowered.len() == 1
                    && let Some(rest) = strip_unpack(&lowered[0])
                {
                    return Some(rest.to_owned());
                }
                Some(format!("tuple[{}]", lowered.join(", ")))
            }

            // `A | B` — propagate into both arms
            Expr::BinOp(b) => {
                let left = self.transform_annotation(&b.left);
                let right = self.transform_annotation(&b.right);
                if left.is_some() || right.is_some() {
                    let l = left.unwrap_or_else(|| self.fallback_src(&b.left));
                    let r = right.unwrap_or_else(|| self.fallback_src(&b.right));
                    Some(format!("{l} | {r}"))
                } else {
                    None
                }
            }

            // `X[...]` — propagate into slice only
            Expr::Subscript(s) => {
                // basedpython: `Callable[(int, str), R]` uses a tuple as a
                // parameter list, not a tuple type. Lower to `Callable[[int,
                // str], R]` (list form) so the runtime / type-checker reads
                // it as parameters
                let is_callable_subscript = matches!(s.value.as_ref(),
                    Expr::Name(n) if n.id.as_str() == "Callable"
                ) || matches!(s.value.as_ref(),
                    Expr::Attribute(a) if a.attr.id.as_str() == "Callable"
                );
                if is_callable_subscript
                    && let Expr::Tuple(slice_tuple) = s.slice.as_ref()
                    && !slice_tuple.parenthesized
                    && slice_tuple.elts.len() == 2
                    && let (Some(params_expr), Some(returns_expr)) =
                        (slice_tuple.elts.first(), slice_tuple.elts.get(1))
                    && let Expr::Tuple(params_tuple) = params_expr
                    && params_tuple.parenthesized
                {
                    let params_str = params_tuple
                        .elts
                        .iter()
                        .map(|e| self.lower_callable_param(e))
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let returns_str = self
                        .transform_annotation(returns_expr)
                        .unwrap_or_else(|| self.src(returns_expr.range()).to_owned());
                    let value_str = self.src(s.value.range());
                    return Some(format!("{value_str}[[{params_str}], {returns_str}]"));
                }

                // The slice of a subscript may be a non-parenthesized tuple
                // (e.g. `dict[str, int]` parses the slice as an unparenthesized
                // Tuple). Propagate into each element individually rather than
                // wrapping the whole slice in `tuple[...]`.
                let slice_rewrite = match s.slice.as_ref() {
                    Expr::Tuple(t) if !t.parenthesized => {
                        let rewrites: Vec<Option<String>> = t
                            .elts
                            .iter()
                            .map(|e| self.transform_annotation(e))
                            .collect();
                        if rewrites.iter().any(std::option::Option::is_some) {
                            let parts = rewrites
                                .into_iter()
                                .zip(t.elts.iter())
                                .map(|(r, e)| r.unwrap_or_else(|| self.fallback_src(e)))
                                .collect::<Vec<_>>()
                                .join(", ");
                            Some(parts)
                        } else {
                            None
                        }
                    }
                    // `X[()]` — the empty parenthesized tuple inside a
                    // subscript is the canonical spelling for an empty tuple
                    // type argument (`tuple[()]` is "empty tuple"). don't
                    // recurse — that would re-lower `()` to `tuple[()]` and
                    // emit `X[tuple[()]]`
                    Expr::Tuple(t) if t.parenthesized && t.elts.is_empty() => None,
                    slice => self.transform_annotation(slice),
                };
                if let Some(slice_str) = slice_rewrite {
                    let value_str = self.src(s.value.range());
                    Some(format!("{value_str}[{slice_str}]"))
                } else {
                    None
                }
            }

            _ => None,
        }
    }

    /// Lowers one element of a parameter-shape tuple to a Python type
    /// expression suitable for the parameter list of `Callable[[...], R]`:
    ///
    /// - bare `int`        → `int`
    /// - `name: T`         → `T` (Callable param list is positional-only)
    /// - `*: T` / `*name: T`   → dropped (Callable list has no variadic)
    /// - `**: T` / `**name: T` → dropped
    fn lower_callable_param(&self, elt: &Expr) -> String {
        match elt {
            Expr::Named(named) => {
                if matches!(named.target.as_ref(), Expr::Starred(_)) {
                    return String::new();
                }
                self.transform_annotation(&named.value)
                    .unwrap_or_else(|| self.src(named.value.range()).to_owned())
            }
            Expr::Starred(_) => String::new(),
            _ => self
                .transform_annotation(elt)
                .unwrap_or_else(|| self.src(elt.range()).to_owned()),
        }
    }

    /// Lowers one element of a parameter-shape tuple to a Python type
    /// expression suitable for `tuple[...]`:
    ///
    /// - bare `int`        → `int`
    /// - `name: T`         → `T` (names dropped in tuple type)
    /// - `*: T` / `*name: T` → `*tuple[T, ...]`
    /// - `**: T` / `**name: T` → dropped (kwargs catch-all has no tuple equivalent)
    ///
    /// `parameter_shape` says the enclosing tuple came from the parameter-shape
    /// parser, which is the only thing that tells a `*: T` variadic apart from a
    /// bare `*A` unpack — they parse to the same `Starred`. An unpack lowers to
    /// itself, since python spells it the same way.
    fn lower_tuple_element(&self, elt: &Expr, parameter_shape: bool) -> String {
        match elt {
            // `*name: T` or `**name: T` → Named wrapping a Starred target
            Expr::Named(named) => {
                if let Expr::Starred(starred) = named.target.as_ref() {
                    if matches!(starred.value.as_ref(), Expr::Starred(_)) {
                        return String::new();
                    }
                    let value_src = self
                        .transform_annotation(&named.value)
                        .unwrap_or_else(|| self.src(named.value.range()).to_owned());
                    return self.unpack(&format!("tuple[{value_src}, ...]"));
                }
                self.transform_annotation(&named.value)
                    .unwrap_or_else(|| self.src(named.value.range()).to_owned())
            }
            // `*: T` (anonymous variadic) → `*tuple[T, ...]`
            // `**: T` (kwargs catch-all)  → dropped
            Expr::Starred(s) => {
                if matches!(s.value.as_ref(), Expr::Starred(_)) {
                    return String::new();
                }
                let value_src = self
                    .transform_annotation(&s.value)
                    .unwrap_or_else(|| self.src(s.value.range()).to_owned());
                if parameter_shape {
                    self.unpack(&format!("tuple[{value_src}, ...]"))
                } else {
                    self.unpack(&value_src)
                }
            }
            // a plain element type — also lower a nested `?` (`(int, str?)`),
            // which `transform_annotation` doesn't handle. element-scoped, so the
            // tuple's whole-expression edit subsumes the optional pass's edit
            _ => self
                .transform_annotation(elt)
                .or_else(|| optional_type::rewrite_type_expr(self.source, elt, self.min_version))
                .unwrap_or_else(|| self.src(elt.range()).to_owned()),
        }
    }
}

impl TypeExprVisitor for TupleLiteralType<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        // skip ParamSpec-targeted subscripts (`A[(int, str)]` where `class
        // A[P: Parameters]`): the tuple slice there is a parameter list and
        // is lowered separately by `generics.rs` to `[int, str]`
        if let Expr::Subscript(s) = expr
            && self.types.class_first_typevar_is_paramspec(&s.value)
        {
            return Recurse::Stop;
        }
        // `transform_annotation` is a deep recursive rewriter that produces
        // a single replacement string for the whole expression. emit the
        // edit at the expression's range and tell the walker to stop —
        // descending further would have it re-fire on the same subtree
        if let Some(rewritten) = self.transform_annotation(expr) {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                rewritten,
                expr.range(),
            )));
        }
        Recurse::Stop
    }
}

pub(crate) struct TupleLiteralTypePass<'src> {
    source: &'src str,
    config: Config,
}

impl<'src> TupleLiteralTypePass<'src> {
    pub(crate) fn new(source: &'src str, config: Config) -> Self {
        Self { source, config }
    }
}

impl TypeAwarePass for TupleLiteralTypePass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = TupleLiteralType::new(self.source, types, self.config.min_version);
        walk_type_positions(stmts, Some(types), &mut inner);
        let mut wraps_literal = false;
        for fix in inner.edits {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                if repl.contains("Literal[") {
                    wraps_literal = true;
                }
                ctx.text_edits.push((range, repl));
            }
        }
        if inner.needs_unpack_import.get() {
            ctx.required_imports
                .push("from typing import Unpack".to_owned());
        }
        // when our embedded literal-type lowering produced `Literal[...]` text,
        // request the import. the standalone literal_types pass doesn't see
        // the bare literal anymore because we've replaced its parent annotation
        if wraps_literal && !literal_types::literal_already_imported(types) {
            ctx.required_imports
                .push("from typing import Literal".to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, PythonVersion, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    /// the same, against a target that has PEP 646
    fn check_at(min_version: PythonVersion, input: &str, expected: &str) {
        let config = Config {
            min_version,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn simple_tuple_annotation() {
        check("a: (int, str)\n", "a: tuple[int, str]\n");
    }

    #[test]
    fn single_element_tuple() {
        check("a: (int,)\n", "a: tuple[int]\n");
    }

    /// a bare `*A` splices `A` in — python spells that the same way from 3.11,
    /// unlike the `*: T` variadic it shares an AST shape with
    #[test]
    fn unpacked_element() {
        check_at(
            PythonVersion::PY311,
            "a: (int, *A)\n",
            "a: tuple[int, *A]\n",
        );
    }

    /// a star in a subscript is 3.11 grammar, so a `__future__` import cannot
    /// defer it the way it defers an annotation's *evaluation* — below 3.11 the
    /// unpack has to be spelled out
    #[test]
    fn unpacked_element_before_pep_646() {
        check(
            "a: (int, *A)\n",
            indoc! {"
                from typing_extensions import Unpack
                a: tuple[int, Unpack[A]]
            "},
        );
    }

    #[test]
    fn lone_unpacked_element_without_a_comma() {
        check_at(PythonVersion::PY311, "a: (*A)\n", "a: tuple[*A]\n");
    }

    #[test]
    fn lone_unpacked_element_with_a_comma() {
        check_at(PythonVersion::PY311, "a: (*A,)\n", "a: tuple[*A]\n");
    }

    #[test]
    fn nested_tuple() {
        check(
            "a: (int, (str, float))\n",
            "a: tuple[int, tuple[str, float]]\n",
        );
    }

    #[test]
    fn tuple_in_union() {
        check("a: (int, str) | None\n", "a: tuple[int, str] | None\n");
    }

    /// a `?` on a tuple element lowers inside the `tuple[...]` rendering —
    /// `transform_annotation` doesn't itself handle optionals, so the element
    /// lowering routes through `optional_type`
    #[test]
    fn tuple_element_optional() {
        check("a: (int, str?)\n", "a: tuple[int, str | None]\n");
    }

    #[test]
    fn tuple_in_subscript_slice() {
        check("a: list[(int, str)]\n", "a: list[tuple[int, str]]\n");
    }

    #[test]
    fn subscript_non_parenthesized_tuple_propagated() {
        // dict[str, (int, float)] — the `str, (int, float)` is an unparenthesized
        // tuple in the slice; only the parenthesized inner tuple should be rewritten
        check(
            "a: dict[str, (int, float)]\n",
            "a: dict[str, tuple[int, float]]\n",
        );
    }

    #[test]
    fn function_parameter_annotation() {
        check(
            indoc! {"
                def f(x: (int, str)) -> (bool, float):
                    pass
            "},
            indoc! {"
                def f(x: tuple[int, str]) -> tuple[bool, float]:
                    pass
            "},
        );
    }

    #[test]
    fn value_context_not_rewritten() {
        // Assignment value should NOT be rewritten
        check("a: int = (int, str)\n", "a: int = (int, str)\n");
    }

    #[test]
    fn plain_subscript_unchanged() {
        check("a: list[int]\n", "a: list[int]\n");
    }

    #[test]
    fn non_annotation_tuple_unchanged() {
        // a value-position tuple display is not rewritten into a type
        // expression, and builtin collection literals are not reified
        check("x = (1, 2)\n", "x = (1, 2)\n");
    }

    #[test]
    fn tuple_union_with_literal() {
        check(
            "a: (int, str) | 1\n",
            indoc! {"
                from typing import Literal
                a: tuple[int, str] | Literal[1]
            "},
        );
    }

    #[test]
    fn literal_union_with_tuple() {
        check(
            "a: 1 | (int, str)\n",
            indoc! {"
                from typing import Literal
                a: Literal[1] | tuple[int, str]
            "},
        );
    }

    #[test]
    fn subscript_slice_with_tuple_and_literal() {
        check(
            "a: dict[(int, str), 1]\n",
            indoc! {"
                from typing import Literal
                a: dict[tuple[int, str], Literal[1]]
            "},
        );
    }

    #[test]
    fn python_tuple_unchanged() {
        unchanged("a: (int, str)\n");
    }

    #[test]
    fn variadic_tuple_annotation() {
        // `*: T` in a tuple type expands to `*tuple[T, ...]` so the tuple
        // can hold zero+ values of T after the leading positional fields
        check_at(
            PythonVersion::PY311,
            "b: (int, *: str)\n",
            "b: tuple[int, *tuple[str, ...]]\n",
        );
        check(
            "b: (int, *: str)\n",
            indoc! {"
                from typing_extensions import Unpack
                b: tuple[int, Unpack[tuple[str, ...]]]
            "},
        );
    }

    #[test]
    fn named_variadic_tuple_annotation() {
        // `*name: T` in a tuple type behaves the same as `*: T` — the name
        // is metadata for callable-parameter use and has no effect on tuple
        // type semantics
        check_at(
            PythonVersion::PY311,
            "b: (int, *args: str)\n",
            "b: tuple[int, *tuple[str, ...]]\n",
        );
    }

    #[test]
    fn mixed_marker_tuple_annotation() {
        // `(int, /, name: str)` — `/` is a positional-only marker that has
        // no tuple-type meaning, so it's dropped. named field's type
        // becomes a positional in the tuple
        check("c: (int, /, name: str)\n", "c: tuple[int, str]\n");
    }

    #[test]
    fn kwargs_dropped_in_tuple_annotation() {
        // `**name: T` (kwargs catch-all) has no positional tuple equivalent
        // and is dropped entirely
        check("d: (int, **kw: str)\n", "d: tuple[int]\n");
    }

    #[test]
    fn callable_with_tuple_params() {
        // tuple-as-parameters: `Callable[(int, str), R]` → `Callable[[int,
        // str], R]` since Callable expects a list of types
        check(
            "from typing import Callable\na: Callable[(int, str), int]\n",
            "from typing import Callable\na: Callable[[int, str], int]\n",
        );
    }

    #[test]
    fn callable_with_variadic_tuple_params() {
        // `*: T` in callable params is dropped — Callable's list form has
        // no variadic slot
        check(
            "from typing import Callable\nb: Callable[(int, *: str), int]\n",
            "from typing import Callable\nb: Callable[[int], int]\n",
        );
    }

    #[test]
    fn callable_with_marked_tuple_params() {
        // `/` and named fields collapse to bare positional types in the
        // Callable list
        check(
            "from typing import Callable\nc: Callable[(int, /, name: str), int]\n",
            "from typing import Callable\nc: Callable[[int, str], int]\n",
        );
    }

    #[test]
    fn empty_tuple_type_preserved() {
        // `tuple[()]` is the only spelling of the empty-tuple type;
        // rewriting `()` to `tuple[]` would emit invalid syntax.
        check("a: tuple[()] = ()\n", "a: tuple[()] = ()\n");
    }

    #[test]
    fn type_application_in_call_arg() {
        // `reveal_type(list[(int, str)])` — the subscript is a type application;
        // the tuple literal inside its slice should still lower to `tuple[...]`
        check(
            "reveal_type(list[(int, str)])\n",
            "reveal_type(list[tuple[int, str]])\n",
        );
    }

    #[test]
    fn type_application_in_assignment_value() {
        // `x = list[(int, str)]` — value-position type application
        check("x = list[(int, str)]\n", "x = list[tuple[int, str]]\n");
    }

    #[test]
    fn runtime_subscript_not_rewritten() {
        // `x[1, 2]` where x is a runtime value (unresolved name) must stay as-is
        check(
            "def f(x):\n    x[(int, str)]\n",
            "def f(x):\n    x[(int, str)]\n",
        );
    }

    #[test]
    fn empty_parenthesized_tuple_in_annotation_lowers() {
        // `a: ()` is shorthand for the empty-tuple type. without rewriting it
        // we'd leave `()` raw in python output, which is invalid as an annotation
        check("a: () = ()\n", "a: tuple[()] = ()\n");
    }
}

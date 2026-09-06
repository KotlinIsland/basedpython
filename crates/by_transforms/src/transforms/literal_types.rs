//! Rewrites literal expressions in type-expression context to `Literal[...]`.
//!
//! `a: "asdf" | 5`              → `a: Literal["asdf", 5]`
//! `a: 1 | 2 | int`             → `a: Literal[1, 2] | int`
//! `a: 5`                       → `a: Literal[5]`
//! `X[1 | 2]` where X is a type → `X[Literal[1, 2]]`
//!
//! A float or complex literal has no `Literal[...]` spelling python's own rules
//! admit, so what it becomes is [`FloatLiteralLowering`]'s to say: the nominal
//! `float` / `complex` by default, or `Literal[1.5]` where the project would
//! rather keep the precision than keep the output checkable.
//!
//! Type-expression context is determined structurally (annotations, function
//! return types) or via `TypeInfo` lookup (subscript slices where the value
//! resolves to a class, type alias, or imported/unknown name).

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::{Expr, Operator, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::config::FloatLiteralLowering;
use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::transforms::type_expr_walker::{
    Recurse, TypeExprVisitor, TypePos, walk_type_positions_skipping,
};
use crate::type_info::{TypeInfo, trailing_name};

pub(crate) struct LiteralType<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    float_literals: FloatLiteralLowering,
    pub(crate) edits: Vec<Fix>,
    pub(crate) needs_literal_import: bool,
}

impl<'src> LiteralType<'src> {
    pub(crate) fn new(
        source: &'src str,
        types: &'src dyn TypeInfo,
        float_literals: FloatLiteralLowering,
    ) -> Self {
        Self {
            source,
            types,
            float_literals,
            edits: Vec::new(),
            needs_literal_import: false,
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// Whether a `Subscript.value` resolves to something whose subscript slice
    /// is a type-argument position.
    fn is_type_subscript(&self, value: &Expr) -> bool {
        trailing_name(value).is_some() && self.types.subscript_is_type_context(value)
    }

    /// Is `value` a reference to the named typing special form?
    fn is_typing_name(&self, value: &Expr, name: &str) -> bool {
        trailing_name(value) == Some(name) && self.types.subscript_is_type_context(value)
    }

    fn is_annotated_name(&self, value: &Expr) -> bool {
        self.is_typing_name(value, "Annotated")
    }

    fn is_literal_name(&self, value: &Expr) -> bool {
        self.is_typing_name(value, "Literal")
    }

    pub(crate) fn emit_type_edits(&mut self, expr: &Expr, _at_root: bool) {
        // bare `None` is idiomatic for `NoneType` in any type position —
        // never wrap with `Literal[None]`. union-arm `None`s adjacent to a
        // literal group still get folded in via `emit_union_group_edits`
        if matches!(expr, Expr::NoneLiteral(_)) {
            return;
        }
        if is_literal_expr(expr, self.float_literals) {
            self.needs_literal_import = true;
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                format!("Literal[{}]", self.src(expr.range())),
                expr.range(),
            )));
            return;
        }
        if let Some(nominal) = nominal_float_type(expr, self.float_literals) {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                nominal.to_owned(),
                expr.range(),
            )));
            return;
        }
        if let Expr::BinOp(b) = expr {
            if matches!(b.op, Operator::BitOr) {
                self.emit_union_group_edits(expr);
                return;
            }
            // intersection `A & 1` — recurse into each operand so a bare literal
            // arm is wrapped individually (intersections don't group literals
            // the way `|` unions do)
            if matches!(b.op, Operator::BitAnd) {
                self.emit_type_edits(&b.left, false);
                self.emit_type_edits(&b.right, false);
                return;
            }
        }
        // keyword `and` / `or` spellings of intersection / union — recurse into
        // each arm so a literal arm still gets wrapped
        if let Expr::BoolOp(b) = expr {
            for value in &b.values {
                self.emit_type_edits(value, false);
            }
            return;
        }
        if let Expr::Subscript(s) = expr {
            if self.is_literal_name(&s.value) {
                return;
            }
            if self.is_annotated_name(&s.value) {
                if let Expr::Tuple(t) = s.slice.as_ref() {
                    if !t.parenthesized && !t.elts.is_empty() {
                        self.emit_type_edits(&t.elts[0], false);
                    }
                }
                return;
            }
            if !self.is_type_subscript(&s.value) {
                return;
            }
            match s.slice.as_ref() {
                Expr::Tuple(t) if !t.parenthesized => {
                    for e in &t.elts {
                        // bare strings inside a generic subscript are PEP 484
                        // forward references; don't promote them to `Literal`
                        if matches!(e, Expr::StringLiteral(_)) {
                            continue;
                        }
                        self.emit_type_edits(e, false);
                    }
                }
                Expr::StringLiteral(_) => {}
                slice => self.emit_type_edits(slice, false),
            }
        }
    }

    /// Emit one edit per contiguous literal group within a union expression.
    /// Each edit covers only `first_literal.start..last_literal.end`, so
    /// non-literal name nodes between groups are left at their original ranges.
    fn emit_union_group_edits(&mut self, union_expr: &Expr) {
        let float_literals = self.float_literals;
        let parts = flatten_union(union_expr);
        if !parts.iter().any(|p| {
            is_literal_expr(p, float_literals) || nominal_float_type(p, float_literals).is_some()
        }) {
            return;
        }

        let mut group_start: Option<TextSize> = None;
        let mut group_end = TextSize::from(0);
        let mut group_list: Vec<String> = Vec::new();
        let mut pending_none_start: Option<TextSize> = None;

        macro_rules! flush_group {
            () => {
                if let Some(start) = group_start.take() {
                    let lit_str = std::mem::take(&mut group_list).join(", ");
                    self.needs_literal_import = true;
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("Literal[{lit_str}]"),
                        TextRange::new(start, group_end),
                    )));
                }
            };
        }

        for p in &parts {
            if matches!(p, Expr::NoneLiteral(_)) {
                if group_start.is_some() {
                    // None following a literal: extend the group
                    group_list.push("None".to_owned());
                    group_end = p.range().end();
                } else {
                    pending_none_start = Some(p.range().start());
                }
            } else if is_literal_expr(p, float_literals) {
                if group_start.is_none() {
                    if let Some(pn) = pending_none_start.take() {
                        group_start = Some(pn);
                        group_list.push("None".to_owned());
                    } else {
                        group_start = Some(p.range().start());
                    }
                }
                group_list.push(self.src(p.range()).to_owned());
                group_end = p.range().end();
            } else {
                // non-literal: flush current group, discard pending None
                pending_none_start = None;
                flush_group!();
                // recurse into non-literal sub-expressions
                self.emit_type_edits(p, false);
            }
        }
        // trailing None stays as-is; flush final group
        flush_group!();
    }
}

pub(crate) struct LiteralTypePass<'src> {
    source: &'src str,
    float_literals: FloatLiteralLowering,
}

impl<'src> LiteralTypePass<'src> {
    pub(crate) fn new(source: &'src str, float_literals: FloatLiteralLowering) -> Self {
        Self {
            source,
            float_literals,
        }
    }
}

impl TypeAwarePass for LiteralTypePass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = LiteralType::new(self.source, types, self.float_literals);
        walk_type_positions_skipping(stmts, Some(types), &ctx.claimed_type_op_ranges, &mut inner);
        if inner.needs_literal_import && !literal_already_imported(types) {
            ctx.required_imports
                .push("from typing import Literal".to_owned());
        }
        for fix in inner.edits {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                ctx.text_edits.push((range, repl));
            }
        }
    }
}

impl TypeExprVisitor for LiteralType<'_> {
    fn visit(&mut self, expr: &Expr, pos: TypePos) -> Recurse {
        // `emit_type_edits` is a deep recursive rewriter that walks the
        // expression's interior itself (BinOp union grouping, Subscript
        // slice descent, Annotated first-arg-only). emit edits, then tell
        // the walker to stop — letting it descend would double-process
        let at_root = matches!(pos, TypePos::Root);
        self.emit_type_edits(expr, at_root);
        Recurse::Stop
    }
}

fn is_literal_expr(expr: &Expr, float_literals: FloatLiteralLowering) -> bool {
    // a float or complex literal is a basedpython-only literal type, and PEP 586
    // does not admit one into `Literal[...]`. wrapping it anyway is what
    // `FloatLiteralLowering::Literal` asks for; otherwise it is not a literal as
    // far as this pass is concerned and `nominal_float_type` names it instead
    let literal_floats = float_literals == FloatLiteralLowering::Literal;
    match expr {
        Expr::NumberLiteral(n) => {
            literal_floats || matches!(n.value, ruff_python_ast::Number::Int(_))
        }
        Expr::StringLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::BytesLiteral(_) => true,
        Expr::UnaryOp(u) => {
            matches!(u.op, UnaryOp::USub | UnaryOp::UAdd)
                && matches!(
                    u.operand.as_ref(),
                    Expr::NumberLiteral(n)
                        if literal_floats || matches!(n.value, ruff_python_ast::Number::Int(_))
                )
        }
        _ => false,
    }
}

/// the builtin a float or complex literal type is one of, when the project
/// spells such a literal with its nominal type rather than with `Literal[...]`.
///
/// a signed literal is the same type as the literal it negates, so `-1.5` is a
/// `float` just as `1.5` is
fn nominal_float_type(expr: &Expr, float_literals: FloatLiteralLowering) -> Option<&'static str> {
    if float_literals != FloatLiteralLowering::Nominal {
        return None;
    }
    let number = match expr {
        Expr::NumberLiteral(n) => &n.value,
        Expr::UnaryOp(u) if matches!(u.op, UnaryOp::USub | UnaryOp::UAdd) => {
            match u.operand.as_ref() {
                Expr::NumberLiteral(n) => &n.value,
                _ => return None,
            }
        }
        _ => return None,
    };
    match number {
        ruff_python_ast::Number::Float(_) => Some("float"),
        ruff_python_ast::Number::Complex { .. } => Some("complex"),
        ruff_python_ast::Number::Int(_) => None,
    }
}

fn flatten_union(expr: &Expr) -> Vec<&Expr> {
    let mut parts = Vec::new();
    flatten_into(expr, &mut parts);
    parts
}

fn flatten_into<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::BinOp(b) = expr {
        if matches!(b.op, Operator::BitOr) {
            flatten_into(&b.left, out);
            flatten_into(&b.right, out);
            return;
        }
    }
    out.push(expr);
}

/// Whether `Literal` is already bound at module level, so lib.rs can avoid
/// prepending a duplicate import.
pub(crate) fn literal_already_imported(types: &dyn TypeInfo) -> bool {
    types.is_bound_globally("Literal")
}

#[cfg(test)]
mod tests {
    use crate::config::FloatLiteralLowering;
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    /// `check`, for a project that keeps a float literal inside `Literal[...]`
    /// rather than widening it to the type it is one of
    fn check_literal_floats(input: &str, expected: &str) {
        let config = Config {
            float_literals: FloatLiteralLowering::Literal,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    // -------------------------------------------------------------------------
    // Float and complex literal types, which PEP 586 has no `Literal[...]` for.
    // -------------------------------------------------------------------------

    #[test]
    fn a_float_literal_is_the_type_it_is_one_of() {
        check("a: 3.5\n", "a: float\n");
        check("a: 2j\n", "a: complex\n");
        check("a: -1.5\n", "a: float\n");
    }

    /// left bare, `int | 3.5` is a `TypeError` the moment the annotation is
    /// evaluated — `type.__or__` has nothing to do with a float
    #[test]
    fn a_float_arm_of_a_union_is_lowered_too() {
        check("a: int | 3.5\n", "a: int | float\n");
        check("a: int | 2j\n", "a: int | complex\n");
        check("a: list[3.5]\n", "a: list[float]\n");
    }

    #[test]
    fn a_project_may_keep_the_float_literal_instead() {
        check_literal_floats(
            "a: 3.5\n",
            indoc! {"
                from typing import Literal
                a: Literal[3.5]
            "},
        );
        check_literal_floats(
            "a: int | 3.5\n",
            indoc! {"
                from typing import Literal
                a: int | Literal[3.5]
            "},
        );
    }

    /// an int literal has a `Literal[...]` spelling of its own, so neither
    /// setting moves it
    #[test]
    fn an_int_literal_is_unaffected_by_the_setting() {
        check(
            "a: int | 5\n",
            indoc! {"
                from typing import Literal
                a: int | Literal[5]
            "},
        );
        check_literal_floats(
            "a: int | 5\n",
            indoc! {"
                from typing import Literal
                a: int | Literal[5]
            "},
        );
    }

    // -------------------------------------------------------------------------
    // Basic literal unions — the core feature.
    // -------------------------------------------------------------------------

    #[test]
    fn simple_int_union() {
        check(
            "a: 1 | 2\n",
            indoc! {"
                from typing import Literal
                a: Literal[1, 2]
            "},
        );
    }

    #[test]
    fn string_union() {
        check(
            "a: \"foo\" | \"bar\"\n",
            indoc! {"
                from typing import Literal
                a: Literal[\"foo\", \"bar\"]
            "},
        );
    }

    #[test]
    fn mixed_literal_types_from_roadmap() {
        check(
            "a: \"asdf\" | 5 = \"asdf\"\n",
            indoc! {"
                from typing import Literal
                a: Literal[\"asdf\", 5] = \"asdf\"
            "},
        );
    }

    #[test]
    fn three_way_int_union() {
        check(
            "a: 1 | 2 | 3\n",
            indoc! {"
                from typing import Literal
                a: Literal[1, 2, 3]
            "},
        );
    }

    #[test]
    fn bool_union() {
        check(
            "a: True | False\n",
            indoc! {"
                from typing import Literal
                a: Literal[True, False]
            "},
        );
    }

    #[test]
    fn negative_int_literals() {
        check(
            "a: -1 | -2\n",
            indoc! {"
                from typing import Literal
                a: Literal[-1, -2]
            "},
        );
    }

    // -------------------------------------------------------------------------
    // Literals mixed with non-literal types.
    // -------------------------------------------------------------------------

    #[test]
    fn literal_on_right_of_type() {
        check(
            "a: int | 1\n",
            indoc! {"
                from typing import Literal
                a: int | Literal[1]
            "},
        );
    }

    #[test]
    fn literal_on_left_of_type() {
        check(
            "a: 1 | int\n",
            indoc! {"
                from typing import Literal
                a: Literal[1] | int
            "},
        );
    }

    #[test]
    fn literals_split_by_type_stay_split() {
        check(
            "a: 1 | int | 2\n",
            indoc! {"
                from typing import Literal
                a: Literal[1] | int | Literal[2]
            "},
        );
    }

    #[test]
    fn adjacent_literals_merge() {
        check(
            "a: 1 | 2 | int\n",
            indoc! {"
                from typing import Literal
                a: Literal[1, 2] | int
            "},
        );
    }

    // -------------------------------------------------------------------------
    // `None` handling.
    // -------------------------------------------------------------------------

    #[test]
    fn none_with_literal_combines() {
        check(
            "a: None | 1\n",
            indoc! {"
                from typing import Literal
                a: Literal[None, 1]
            "},
        );
    }

    #[test]
    fn bare_none_annotation_unchanged() {
        check("a: None\n", "a: None\n");
    }

    #[test]
    fn none_in_generic_arg_unchanged() {
        // `None` in any position is the idiomatic spelling for `NoneType`;
        // `Literal[None]` wrapping mutates the user's source without semantic gain
        check(
            "from typing import Generator\ng: Generator[int, None, None]\n",
            "from typing import Generator\ng: Generator[int, None, None]\n",
        );
    }

    #[test]
    fn none_in_list_arg_unchanged() {
        check("j: list[None]\n", "j: list[None]\n");
    }

    // -------------------------------------------------------------------------
    // Bare (non-union) literal annotations.
    // -------------------------------------------------------------------------

    #[test]
    fn bare_int_annotation() {
        check(
            "a: 5\n",
            indoc! {"
                from typing import Literal
                a: Literal[5]
            "},
        );
    }

    #[test]
    fn bare_bool_annotation() {
        check(
            "a: True\n",
            indoc! {"
                from typing import Literal
                a: Literal[True]
            "},
        );
    }

    #[test]
    fn bare_string_annotation() {
        check(
            "a: \"Foo\"\n",
            indoc! {"
                from typing import Literal
                a: Literal[\"Foo\"]
            "},
        );
    }

    // -------------------------------------------------------------------------
    // Function signatures.
    // -------------------------------------------------------------------------

    #[test]
    fn function_parameter() {
        check(
            indoc! {"
                def f(x: 1 | 2):
                    pass
            "},
            indoc! {"
                from typing import Literal
                def f(x: Literal[1, 2]):
                    pass
            "},
        );
    }

    #[test]
    fn function_return_type() {
        check(
            indoc! {"
                def f() -> 1 | 2:
                    pass
            "},
            indoc! {"
                from typing import Literal
                def f() -> Literal[1, 2]:
                    pass
            "},
        );
    }

    // -------------------------------------------------------------------------
    // Value-position preservation — `|` there is bitwise-or.
    // -------------------------------------------------------------------------

    #[test]
    fn value_context_unchanged() {
        check("x = 1 | 2\n", "x = 1 | 2\n");
    }

    #[test]
    fn value_in_annotated_assign_unchanged() {
        check(
            "a: 1 | 2 = 1 | 2\n",
            indoc! {"
                from typing import Literal
                a: Literal[1, 2] = 1 | 2
            "},
        );
    }

    // -------------------------------------------------------------------------
    // Existing `Literal[...]` — don't double-wrap or touch.
    // -------------------------------------------------------------------------

    #[test]
    fn already_literal_unchanged() {
        check(
            indoc! {"
                from typing import Literal
                a: Literal[1, 2]
            "},
            indoc! {"
                from typing import Literal
                a: Literal[1, 2]
            "},
        );
    }

    #[test]
    fn existing_literal_import_not_duplicated() {
        check(
            indoc! {"
                from typing import Literal
                a: 1 | 2
            "},
            indoc! {"
                from typing import Literal
                a: Literal[1, 2]
            "},
        );
    }

    // -------------------------------------------------------------------------
    // Propagation into subscript slices.
    // -------------------------------------------------------------------------

    #[test]
    fn inside_list_generic() {
        check(
            "a: list[1 | 2]\n",
            indoc! {"
                from typing import Literal
                a: list[Literal[1, 2]]
            "},
        );
    }

    #[test]
    fn inside_dict_generic() {
        check(
            "a: dict[str, 1 | 2]\n",
            indoc! {"
                from typing import Literal
                a: dict[str, Literal[1, 2]]
            "},
        );
    }

    #[test]
    fn subscript_propagation_type_alias() {
        // `X` is a type alias → slice is a type context, propagate.
        // Using min_version 3.12 so the `type X[T] = ...` stays as-is and
        // doesn't interact with the PEP-695 polyfill in this fixture.
        //
        // value-position `x[1 | 2]` (where `x` is an instance) is *not*
        // promoted — promotion fires only for syntactic type contexts and
        // for subscripts on values that ty knows are types
        let config = crate::Config {
            min_version: crate::config::PythonVersion::PY312,
            ..crate::Config::test_default()
        };
        let input = indoc! {"
            type X[T] = list[T]
            x = X[1 | 2]()
            b = x[1 | 2]
        "};
        let expected = indoc! {"
            from typing import Literal
            type X[T] = list[T]
            x = X[Literal[1, 2]]()
            b = x[1 | 2]
        "};
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn annotated_metadata_not_propagated() {
        check(
            "a: Annotated[int, 1 | 2]\n",
            indoc! {"
                from typing import Annotated
                a: Annotated[int, 1 | 2]
            "},
        );
    }

    // -------------------------------------------------------------------------
    // Type alias values (RHS of `type X = ...`).
    //
    // The output depends on the minimum configured Python version: at 3.12+
    // the `type` statement is native, so we just rewrite the value in place.
    // Below 3.12 the generics polyfill turns the whole statement into a
    // `TypeAliasType(...)` call; the literal rewrite has to land inside.
    // -------------------------------------------------------------------------

    #[test]
    fn type_alias_value_rewritten_312() {
        let config = crate::Config {
            min_version: crate::config::PythonVersion::PY312,
            ..crate::Config::test_default()
        };
        assert_eq!(
            transpile("type X = 1 | 2\n", &config).unwrap(),
            indoc! {"
                from typing import Literal
                type X = Literal[1, 2]
            "},
        );
    }

    #[test]
    fn type_alias_value_rewritten_310() {
        check(
            "type X = 1 | 2\n",
            indoc! {"
                from typing import Literal
                from typing_extensions import TypeAliasType
                X = TypeAliasType(\"X\", Literal[1, 2])
            "},
        );
    }

    // -------------------------------------------------------------------------
    // Class-body annotations.
    // -------------------------------------------------------------------------

    #[test]
    fn class_attribute_annotation() {
        check(
            indoc! {"
                class Foo:
                    x: 1 | 2
            "},
            indoc! {"
                from typing import Literal
                class Foo:
                    x: Literal[1, 2]
            "},
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged("a: 1 | 2\n");
    }

    #[test]
    fn forward_ref_in_generic_class_subscript_not_promoted() {
        // `Foo["Later"]` is a PEP 484 forward reference, not a Literal;
        // promoting it to `Foo[Literal["Later"]]` would change the program.
        check(
            indoc! {"
                class Foo: pass
                class Bar(Foo[\"Later\"]): pass
            "},
            indoc! {"
                class Foo: pass
                class Bar(Foo[\"Later\"]): pass
            "},
        );
    }

    #[test]
    fn forward_ref_inside_dict_arg_not_promoted() {
        check(
            indoc! {"
                class Foo: pass
                a: dict[str, \"Later\"]
            "},
            indoc! {"
                class Foo: pass
                a: dict[str, \"Later\"]
            "},
        );
    }
}

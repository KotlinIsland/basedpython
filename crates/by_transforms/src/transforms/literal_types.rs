//! Rewrites literal expressions in type-expression context to `Literal[...]`.
//!
//! `a: "asdf" | 5`              → `a: Literal["asdf", 5]`
//! `a: 1 | 2 | int`             → `a: Literal[1, 2] | int`
//! `a: 5`                       → `a: Literal[5]`
//! `X[1 | 2]` where X is a type → `X[Literal[1, 2]]`
//!
//! Type-expression context is determined structurally (annotations, function
//! return types) or via `TypeInfo` lookup (subscript slices where the value
//! resolves to a class, type alias, or imported/unknown name).

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::{Expr, ExprSubscript, Operator, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::transforms::type_expr_walker::{
    Recurse, TypeExprVisitor, TypePos, walk_type_positions_skipping,
};
use crate::type_info::TypeInfo;

pub(crate) struct LiteralType<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
    pub(crate) needs_literal_import: bool,
}

impl<'src> LiteralType<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
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
        match value {
            Expr::Name(n) => self.types.subscript_is_type_context(n),
            Expr::Attribute(a) => match a.value.as_ref() {
                Expr::Name(base) => self.types.attr_base_is_type_context(base),
                _ => false,
            },
            _ => false,
        }
    }

    /// Is `value` a reference to the named typing special form?
    fn is_typing_name(&self, value: &Expr, name: &str) -> bool {
        match value {
            Expr::Name(n) => n.id.as_str() == name && self.types.subscript_is_type_context(n),
            Expr::Attribute(a) => {
                a.attr.id.as_str() == name
                    && matches!(a.value.as_ref(), Expr::Name(base) if self.types.attr_base_is_type_context(base))
            }
            _ => false,
        }
    }

    fn is_annotated_name(&self, value: &Expr) -> bool {
        self.is_typing_name(value, "Annotated")
    }

    fn is_literal_name(&self, value: &Expr) -> bool {
        self.is_typing_name(value, "Literal")
    }

    /// Transform a type expression. Returns `Some(rewrite)` if any literal
    /// was promoted to `Literal[...]`, else `None`.
    ///
    /// `at_root` distinguishes a bare-annotation position from an interior
    /// position. bare `None` is left alone in every position — `None` is the
    /// idiomatic spelling for `NoneType` and a `Literal[None]` wrapper here is
    /// noise that mutates the user's source form unnecessarily. union-arm
    /// `None`s still join an adjacent literal group via the path in
    /// `transform_union`
    fn transform_type_expr(&mut self, expr: &Expr, _at_root: bool) -> Option<String> {
        if matches!(expr, Expr::NoneLiteral(_)) {
            return None;
        }

        if is_literal_expr(expr) {
            self.needs_literal_import = true;
            return Some(format!("Literal[{}]", self.src(expr.range())));
        }

        if let Expr::BinOp(b) = expr {
            if matches!(b.op, Operator::BitOr) {
                return self.transform_union(expr);
            }
        }

        if let Expr::Subscript(s) = expr {
            return self.transform_subscript(s);
        }

        None
    }

    fn transform_union(&mut self, expr: &Expr) -> Option<String> {
        enum Group {
            Literals(Vec<String>),
            Other(String),
        }

        let parts = flatten_union(expr);
        let mut groups: Vec<Group> = Vec::new();
        let mut changed = false;
        // `None` between two non-literal arms (e.g. `int | None`) stays bare.
        // We only know whether to attach a `None` to a Literal group once we
        // see what follows it, so hold it pending until we see a non-None
        // literal (attach forward) or anything else (flush as bare `None`).
        let mut pending_none = false;

        for p in parts {
            if matches!(p, Expr::NoneLiteral(_)) {
                if let Some(Group::Literals(list)) = groups.last_mut() {
                    list.push("None".to_owned());
                } else {
                    pending_none = true;
                }
            } else if is_literal_expr(p) {
                let s = self.src(p.range()).to_owned();
                if pending_none {
                    pending_none = false;
                    match groups.last_mut() {
                        Some(Group::Literals(list)) => {
                            list.push("None".to_owned());
                            list.push(s);
                        }
                        _ => groups.push(Group::Literals(vec!["None".to_owned(), s])),
                    }
                } else if let Some(Group::Literals(list)) = groups.last_mut() {
                    list.push(s);
                } else {
                    groups.push(Group::Literals(vec![s]));
                }
                changed = true;
            } else {
                if pending_none {
                    pending_none = false;
                    groups.push(Group::Other("None".to_owned()));
                }
                let rewritten = self.transform_type_expr(p, false);
                if rewritten.is_some() {
                    changed = true;
                }
                let s = rewritten.unwrap_or_else(|| self.src(p.range()).to_owned());
                groups.push(Group::Other(s));
            }
        }
        if pending_none {
            groups.push(Group::Other("None".to_owned()));
        }

        if !changed {
            return None;
        }
        if groups.iter().any(|g| matches!(g, Group::Literals(_))) {
            self.needs_literal_import = true;
        }

        let out: Vec<String> = groups
            .into_iter()
            .map(|g| match g {
                Group::Literals(list) => format!("Literal[{}]", list.join(", ")),
                Group::Other(s) => s,
            })
            .collect();
        Some(out.join(" | "))
    }

    fn transform_subscript(&mut self, s: &ExprSubscript) -> Option<String> {
        // `Literal[...]` is already in literal context — its slice doesn't
        // need re-wrapping.
        if self.is_literal_name(&s.value) {
            return None;
        }
        if self.is_annotated_name(&s.value) {
            return self.transform_annotated_subscript(s);
        }
        if !self.is_type_subscript(&s.value) {
            return None;
        }

        let slice = s.slice.as_ref();
        // Unparenthesized tuple → multiple type args (e.g. `dict[str, int]`).
        if let Expr::Tuple(t) = slice {
            if !t.parenthesized {
                let rewrites: Vec<Option<String>> = t
                    .elts
                    .iter()
                    .map(|e| {
                        if matches!(e, Expr::StringLiteral(_)) {
                            None
                        } else {
                            self.transform_type_expr(e, false)
                        }
                    })
                    .collect();
                if !rewrites.iter().any(std::option::Option::is_some) {
                    return None;
                }
                let parts: Vec<String> = rewrites
                    .into_iter()
                    .zip(t.elts.iter())
                    .map(|(r, e)| r.unwrap_or_else(|| self.src(e.range()).to_owned()))
                    .collect();
                let value_src = self.src(s.value.range());
                return Some(format!("{value_src}[{}]", parts.join(", ")));
            }
        }

        // A bare string literal in a generic-class subscript slot is a PEP
        // 484 forward reference — leave it alone instead of wrapping in
        // `Literal[...]`. (`Literal["X"]` and `Annotated["X", ...]` are
        // already handled by the early returns above.)
        if matches!(slice, Expr::StringLiteral(_)) {
            return None;
        }

        let rewrite = self.transform_type_expr(slice, false)?;
        let value_src = self.src(s.value.range());
        Some(format!("{value_src}[{rewrite}]"))
    }

    /// `Annotated[T, meta...]` — only the first arg is a type position; the
    /// rest is arbitrary metadata and must not be rewritten.
    fn transform_annotated_subscript(&mut self, s: &ExprSubscript) -> Option<String> {
        let Expr::Tuple(t) = s.slice.as_ref() else {
            return None;
        };
        if t.parenthesized || t.elts.is_empty() {
            return None;
        }
        let first_rewrite = self.transform_type_expr(&t.elts[0], false)?;
        let mut parts = vec![first_rewrite];
        for e in &t.elts[1..] {
            parts.push(self.src(e.range()).to_owned());
        }
        let value_src = self.src(s.value.range());
        Some(format!("{value_src}[{}]", parts.join(", ")))
    }

    /// Emit minimal edits for literal type rewrites. Unlike `transform_type_expr`
    /// which returns a full string replacement, this method emits one edit per
    /// contiguous literal group, leaving non-literal parts (e.g. class self-refs
    /// handled by `auto_quote`) at their own ranges so they don't get subsumed.
    pub(crate) fn emit_type_edits(&mut self, expr: &Expr, _at_root: bool) {
        // bare `None` is idiomatic for `NoneType` in any type position —
        // never wrap with `Literal[None]`. union-arm `None`s adjacent to a
        // literal group still get folded in via `emit_union_group_edits`
        if matches!(expr, Expr::NoneLiteral(_)) {
            return;
        }
        if is_literal_expr(expr) {
            self.needs_literal_import = true;
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                format!("Literal[{}]", self.src(expr.range())),
                expr.range(),
            )));
            return;
        }
        if let Expr::BinOp(b) = expr {
            if matches!(b.op, Operator::BitOr) {
                self.emit_union_group_edits(expr);
                return;
            }
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
        let parts = flatten_union(union_expr);
        if !parts.iter().any(|p| is_literal_expr(p)) {
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
            } else if is_literal_expr(p) {
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
}

impl<'src> LiteralTypePass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for LiteralTypePass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = LiteralType::new(self.source, types);
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

fn is_literal_expr(expr: &Expr) -> bool {
    match expr {
        // float/complex literals are basedpython-only literal types — Python's
        // `Literal[...]` rejects them per PEP 586. leave bare so the output is
        // valid Python (`a: 1.5` becomes `a: 1.5` would also be invalid in
        // python; we don't currently rewrite to `float`, but skipping the
        // `Literal[]` wrap is at least no worse than wrapping)
        Expr::NumberLiteral(n) => {
            matches!(n.value, ruff_python_ast::Number::Int(_))
        }
        Expr::StringLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::BytesLiteral(_) => true,
        Expr::UnaryOp(u) => {
            matches!(u.op, UnaryOp::USub | UnaryOp::UAdd)
                && matches!(
                    u.operand.as_ref(),
                    Expr::NumberLiteral(n) if matches!(n.value, ruff_python_ast::Number::Int(_))
                )
        }
        _ => false,
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

/// Stateless rewrite of a type expression, for use by other transforms that
/// need to splice rewritten type text into their own output (e.g.
/// `generics.rs` when wrapping a type alias body in `TypeAliasType(...)`).
///
/// Doesn't update any "needs Literal import" flag — call
/// `LiteralType::visit_stmt` separately for that. The rewrite returned here
/// uses the original source text for sub-expressions, so callers must not
/// then apply incompatible edits (like name renames) on top of overlapping
/// ranges.
pub(crate) fn rewrite_type_expr(source: &str, types: &dyn TypeInfo, expr: &Expr) -> Option<String> {
    let mut t = LiteralType::new(source, types);
    t.transform_type_expr(expr, true)
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
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

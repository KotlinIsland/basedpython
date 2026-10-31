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
use ruff_python_ast::helpers::any_over_expr;
use ruff_python_ast::{Expr, Operator, PythonVersion, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::RuntimeSpelling;

use crate::config::{Config, FloatLiteralLowering};
use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::transforms::optional_type::collect_edits;
use crate::transforms::repeated_underscore::WrittenNames;
use crate::transforms::type_expr_walker::{
    Recurse, RootKind, TypeExprVisitor, TypePos, walk_type_positions_skipping,
};
use crate::type_info::{TypeInfo, trailing_name};

pub(crate) struct LiteralType<'src> {
    source: &'src str,
    /// the name `typing.Literal` is written under
    literal: String,
    written: WrittenNames<'src>,
    types: &'src dyn TypeInfo,
    float_literals: FloatLiteralLowering,
    /// whether the literal is written in a type application that runs as a value. the program
    /// may read one of those back as the type argument it is, so it is written as an inferred
    /// one is, through the helper that builds a literal type
    in_application: bool,
    /// whether what the literals of a union in a type application are written as can be joined
    /// with `|`, which on a target without `type.__or__` they can't, and they are written as
    /// the one `Literal[...]` there
    union_operator_runs: bool,
    pub(crate) edits: Vec<Fix>,
    pub(crate) needs_literal_import: bool,
    /// every string the edits write as a literal type
    literal_strings: Vec<TextRange>,
}

impl<'src> LiteralType<'src> {
    pub(crate) fn new(
        source: &'src str,
        written: WrittenNames<'src>,
        types: &'src dyn TypeInfo,
        float_literals: FloatLiteralLowering,
    ) -> Self {
        Self {
            source,
            literal: written.imported("typing", "Literal"),
            written,
            types,
            float_literals,
            in_application: false,
            union_operator_runs: true,
            edits: Vec::new(),
            needs_literal_import: false,
            literal_strings: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// a float or complex literal type holds no value the runtime can tell apart from its
    /// class's (`0.0 == -0.0`), so a type argument records the class whatever an annotation
    /// is asked to keep
    fn float_lowering(&self) -> FloatLiteralLowering {
        if self.in_application {
            FloatLiteralLowering::Nominal
        } else {
            self.float_literals
        }
    }

    /// the literal type the literal `expr` is written as in a type application
    fn application_literal(&self, expr: &Expr) -> String {
        let value = match expr {
            Expr::StringLiteral(string) => RuntimeSpelling::string(string.value.to_str()),
            Expr::BytesLiteral(bytes) => {
                RuntimeSpelling::bytes(&bytes.value.bytes().collect::<Vec<_>>())
            }
            _ => RuntimeSpelling::text(self.src(expr.range())),
        };
        self.written.spelled(&RuntimeSpelling::literal(&value))
    }

    /// Whether a `Subscript.value` resolves to something whose subscript slice
    /// is a type-argument position.
    fn is_type_subscript(&self, value: &Expr) -> bool {
        (trailing_name(value).is_some() && self.types.subscript_is_type_context(value))
            || self.types.subscript_specializes_reified_function(value)
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
        if is_literal_expr(expr, self.float_lowering()) {
            if expr.is_string_literal_expr() {
                self.literal_strings.push(expr.range());
            }
            if self.in_application {
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    self.application_literal(expr),
                    expr.range(),
                )));
                return;
            }
            self.needs_literal_import = true;
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                format!("{}[{}]", self.literal, self.src(expr.range())),
                expr.range(),
            )));
            return;
        }
        // an enum member names a value where a type is read, as a literal does
        if self.in_application && expr.is_attribute_expr() && self.types.is_plain_value(expr) {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                self.application_literal(expr),
                expr.range(),
            )));
            return;
        }
        if let Some(nominal) = nominal_float_type(expr, self.float_lowering()) {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                self.written.builtin(nominal),
                expr.range(),
            )));
            return;
        }
        // `1?` and `not 1` hold a type, and only their own marker is another pass's to lower
        if let Expr::UnaryOp(u) = expr
            && matches!(u.op, UnaryOp::Optional | UnaryOp::Not)
        {
            self.emit_type_edits(&u.operand, false);
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
        // the parameter list of a `Callable[[A, B], R]`
        if let Expr::List(list) = expr {
            for element in &list.elts {
                self.emit_type_edits(element, false);
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
            // a string here is a literal type, as it is anywhere else in a `.by` type
            // expression — `list["a"]` is `list[Literal["a"]]` — while python would read
            // it as a forward reference to a name `a`
            // a slice the value is not specialized by is a value, an enum member's name
            let specializes = self.types.subscript_specializes(&s.value);
            match s.slice.as_ref() {
                Expr::Tuple(t) if !t.parenthesized => {
                    for e in &t.elts {
                        if specializes || !matches!(e, Expr::StringLiteral(_)) {
                            self.emit_type_argument_edits(e);
                        }
                    }
                }
                Expr::StringLiteral(_) if !specializes => {}
                slice => self.emit_type_argument_edits(slice),
            }
        }
    }

    /// a type argument holding a forward reference is written as a string, which nothing but
    /// a checker reads, and there a literal stays the literal type a checker knows
    fn emit_type_argument_edits(&mut self, argument: &Expr) {
        let in_application = self.in_application;
        self.in_application &= !any_over_expr(
            argument,
            |node| matches!(node, Expr::Name(name) if self.types.is_forward_reference(name) == Some(true)),
        );
        self.emit_type_edits(argument, false);
        self.in_application = in_application;
    }

    /// Emit one edit per contiguous literal group within a union expression.
    /// Each edit covers only `first_literal.start..last_literal.end`, so
    /// non-literal name nodes between groups are left at their original ranges.
    fn emit_union_group_edits(&mut self, union_expr: &Expr) {
        let float_literals = self.float_lowering();
        let parts = flatten_union(union_expr);
        if !parts.iter().any(|p| {
            is_literal_expr(p, float_literals) || nominal_float_type(p, float_literals).is_some()
        }) {
            // no arm is a literal, but an arm may hold one: `list["a"] | int`
            for part in parts {
                self.emit_type_edits(part, false);
            }
            return;
        }

        let through_helper = self.in_application && self.union_operator_runs;
        let mut group_start: Option<TextSize> = None;
        let mut group_end = TextSize::from(0);
        let mut group_list: Vec<String> = Vec::new();
        let mut pending_none_start: Option<TextSize> = None;

        macro_rules! flush_group {
            () => {
                if let Some(start) = group_start.take() {
                    let replacement = if through_helper {
                        std::mem::take(&mut group_list).join(" | ")
                    } else {
                        self.needs_literal_import = true;
                        let lit_str = std::mem::take(&mut group_list).join(", ");
                        format!("{}[{lit_str}]", self.literal)
                    };
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        replacement,
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
                if p.is_string_literal_expr() {
                    self.literal_strings.push(p.range());
                }
                group_list.push(if through_helper {
                    self.application_literal(p)
                } else {
                    self.src(p.range()).to_owned()
                });
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

/// where a type a lowering writes whole is read, which decides how a literal in it is written
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiteralReading {
    Annotation,
    Application,
}

/// `expr`, a type expression a lowering is writing whole, with the literals it holds written as
/// [`LiteralType`] would have and the `?`s around them lowered, and whether that needs
/// `typing.Literal`. `None` when it holds no literal
pub(crate) fn lower_literals(
    source: &str,
    written: WrittenNames,
    types: &dyn TypeInfo,
    expr: &Expr,
    reading: LiteralReading,
    min_version: PythonVersion,
) -> Option<(String, bool)> {
    let mut inner = LiteralType::new(source, written, types, FloatLiteralLowering::default());
    inner.in_application = reading == LiteralReading::Application;
    inner.emit_type_edits(expr, false);
    let mut edits: Vec<_> = inner
        .edits
        .iter()
        .flat_map(|fix| fix.edits().iter().cloned())
        .collect();
    if edits.is_empty() {
        return None;
    }
    edits.extend(
        collect_edits(source, written, expr, min_version)
            .into_iter()
            .map(|(range, lowered)| Edit::range_replacement(lowered, range)),
    );
    // an insertion at an offset goes ahead of a replacement starting there
    edits.sort_by_key(|edit| (edit.start(), edit.end()));
    let mut lowered = String::new();
    let mut at = expr.start();
    for edit in edits {
        lowered.push_str(&source[TextRange::new(at, edit.start())]);
        lowered.push_str(edit.content().unwrap_or_default());
        at = edit.end();
    }
    lowered.push_str(&source[TextRange::new(at, expr.end())]);
    Some((lowered, inner.needs_literal_import))
}

/// every string in `stmts` that the lowering writes as a literal type, because the checker reads
/// a string there as one — which python, reading the same text, takes for a forward reference
///
/// the reverse direction unquotes exactly these, so that a forward reference python wrote comes
/// back from a round trip as the reference it was
pub(crate) fn literal_type_strings(
    source: &str,
    stmts: &[Stmt],
    types: &dyn TypeInfo,
) -> Vec<TextRange> {
    let mut inner = LiteralType::new(
        source,
        WrittenNames::new(source),
        types,
        FloatLiteralLowering::default(),
    );
    walk_type_positions_skipping(stmts, Some(types), &[], &mut inner);
    inner.literal_strings
}

pub(crate) struct LiteralTypePass<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
    float_literals: FloatLiteralLowering,
    union_operator_runs: bool,
}

impl<'src> LiteralTypePass<'src> {
    pub(crate) fn new(source: &'src str, written: WrittenNames<'src>, config: &Config) -> Self {
        Self {
            source,
            written,
            float_literals: config.float_literals,
            union_operator_runs: config.is_stub || config.min_version >= PythonVersion::PY310,
        }
    }
}

impl TypeAwarePass for LiteralTypePass<'_> {
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::LiteralType)
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = LiteralType::new(self.source, self.written, types, self.float_literals);
        inner.union_operator_runs = self.union_operator_runs;
        walk_type_positions_skipping(stmts, Some(types), &ctx.claimed_type_op_ranges, &mut inner);
        if inner.needs_literal_import {
            ctx.required_imports
                .push(self.written.import_from("typing", &["Literal"]));
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

    fn enter_root(&mut self, kind: RootKind) {
        self.in_application = kind == RootKind::Application;
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

/// whether `expr` is a union of literal arms alone, which [`LiteralType`] writes as the one
/// `Literal[...]`, with no union operator to spell for a target that hasn't one
pub(crate) fn is_one_literal_type(expr: &Expr, float_literals: FloatLiteralLowering) -> bool {
    match expr {
        Expr::BinOp(b) if b.op == Operator::BitOr => {
            is_one_literal_type(&b.left, float_literals)
                && is_one_literal_type(&b.right, float_literals)
        }
        arm => is_literal_expr(arm, float_literals),
    }
}

/// how a float or complex literal type written as `text` is spelled in the output, or
/// `None` when `expr` is no such literal: what [`LiteralType`] writes for one the source wrote
pub(crate) fn float_literal_spelling(
    expr: &Expr,
    text: &str,
    written: WrittenNames,
    literal: &str,
    float_literals: FloatLiteralLowering,
) -> Option<String> {
    if let Some(nominal) = nominal_float_type(expr, float_literals) {
        return Some(written.builtin(nominal));
    }
    let is_float =
        |number: &ruff_python_ast::Number| !matches!(number, ruff_python_ast::Number::Int(_));
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
    is_float(number).then(|| format!("{literal}[{text}]"))
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
        let out = transpile(input, &config).unwrap();
        assert!(
            out.ends_with(indoc! {"
                type X[T] = list[T]
                x = X[_by_lit(1) | _by_lit(2)]()
                b = x[1 | 2]
            "}),
            "{out}"
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

    /// a string in a `.by` type expression is a literal type wherever it is, a subscript's
    /// slice included: the checker reads `Foo["Later"]` as `Foo[Literal["Later"]]`, and python
    /// would read the bare string as a forward reference to a name `Later`
    #[test]
    fn a_string_in_a_generic_subscript_is_a_literal_type() {
        check(
            indoc! {"
                class Foo[T]: pass
                class Bar(Foo[\"Later\"]): pass
            "},
            indoc! {"
                from typing import Literal, TypeVar, Generic
                _T = TypeVar(\"_T\")
                class Foo(Generic[_T]): pass
                class Bar(Foo[Literal[\"Later\"]]): pass
            "},
        );
    }

    #[test]
    fn a_string_argument_of_a_generic_annotation_is_a_literal_type() {
        check(
            "a: dict[str, \"Later\"]\n",
            "from typing import Literal\na: dict[str, Literal[\"Later\"]]\n",
        );
        check(
            "a: list[\"x\" | \"y\"]\n",
            "from typing import Literal\na: list[Literal[\"x\", \"y\"]]\n",
        );
    }

    #[test]
    fn a_string_in_a_callable_s_parameter_list_is_a_literal_type() {
        check(
            "from typing import Callable\na: Callable[[\"x\", int], \"y\"]\n",
            "from typing import Callable, Literal\na: Callable[[Literal[\"x\"], int], Literal[\"y\"]]\n",
        );
    }

    /// a union with no literal arm can still hold one inside an arm
    #[test]
    fn a_literal_under_an_optional_is_lowered() {
        for (written, lowered) in [
            ("1?", "Literal[1] | None"),
            ("\"a\"?", "Literal[\"a\"] | None"),
            ("(1 | 2)?", "(Literal[1, 2]) | None"),
            ("list[1?]", "list[Literal[1] | None]"),
            ("(1?, \"a\")", "tuple[Literal[1] | None, Literal[\"a\"]]"),
            ("not 1", "Not[Literal[1]]"),
            ("M[a=1?]", "M[[Literal[1] | None]]"),
            ("M[a=list[1]?]", "M[[list[Literal[1]] | None]]"),
            ("M[a=1??]", "M[[Optional[Literal[1] | None]]]"),
        ] {
            let source = format!("class M[**K]:\n    pass\n\na: {written}\n");
            let out = transpile(&source, &Config::test_default()).unwrap();
            assert!(
                out.contains(&format!("a: {lowered}\n")),
                "{written}:\n{out}"
            );
        }
    }

    #[test]
    fn a_literal_inside_an_arm_of_a_union_is_lowered() {
        check(
            "a: list[\"x\"] | int\n",
            "from typing import Literal\na: list[Literal[\"x\"]] | int\n",
        );
        check(
            "a: list[1] | int\n",
            "from typing import Literal\na: list[Literal[1]] | int\n",
        );
    }

    /// subscripting an enum class looks a member up by name at run time: the string is the
    /// name, not a type
    #[test]
    fn a_runtime_lookup_on_a_class_keeps_its_string() {
        let source = indoc! {"
            from enum import Enum
            class Colour(Enum):
                Red = 1
            red = Colour[\"Red\"]
        "};
        check(source, source);
    }

    /// nor is a subscript the checker resolves to no type at all
    #[test]
    fn an_unresolved_subscript_on_a_class_keeps_its_string() {
        let out = transpile(
            "enum class Shape:\n    case Circle(radius: float)\n    case Point\n\nkind = Shape[\"Circle\"]\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("kind = Shape[\"Circle\"]"), "{out}");
    }

    /// the value of a `TypeAlias` is a type expression
    #[test]
    fn a_string_type_alias_value_is_a_literal_type() {
        check(
            "from typing import TypeAlias\nX: TypeAlias = \"Later\"\n",
            "from typing import TypeAlias, Literal\nX: TypeAlias = Literal[\"Later\"]\n",
        );
    }

    /// `Annotated`'s metadata and `Literal`'s own arguments are not type expressions
    #[test]
    fn a_string_that_is_no_type_expression_stays_a_string() {
        check(
            "from typing import Annotated\na: Annotated[\"x\", \"meta\"]\n",
            "from typing import Annotated, Literal\na: Annotated[Literal[\"x\"], \"meta\"]\n",
        );
    }

    /// a union the runtime would evaluate has no operator to run below 3.10, and a union of
    /// literals needs none
    #[test]
    fn a_union_of_literals_is_one_literal_type_below_the_union_operator() {
        let at_39 = |source: &str| {
            let config = Config {
                min_version: crate::PythonVersion::PY39,
                ..Config::test_default()
            };
            crate::transpile(source, &config).unwrap()
        };
        for (source, written) in [
            ("a = list[1 | 2]\n", "a = list[Literal[1, 2]]\n"),
            ("a = list[1 | None]\n", "a = list[Literal[1, None]]\n"),
            (
                "a = list[\"a\" | \"b\"]\n",
                "a = list[Literal[\"a\", \"b\"]]\n",
            ),
            (
                "class K(list[1 | 2]): ...\n",
                "class K(list[Literal[1, 2]]): ...\n",
            ),
            (
                "from typing import cast\na = cast(1 | 2, 1)\n",
                "cast(Literal[1, 2], 1)",
            ),
            (
                "a = list[1 | 2 | int]\n",
                "a = list[Union[Literal[1], Literal[2], int]]\n",
            ),
            (
                "a = list[1.5 | 2]\n",
                "a = list[Union[float, Literal[2]]]\n",
            ),
        ] {
            let out = at_39(source);
            assert!(out.contains(written), "{source}\n{out}");
        }
    }
}

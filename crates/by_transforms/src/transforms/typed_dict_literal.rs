//! Lowers basedpython typed-dict literal type expressions
//! `{"key": T, ...}` to a synthesized `typing.TypedDict` subclass.
//!
//! Each unique shape (sorted-key + field-type-source pairs, plus any
//! `**: T` extra-items type) is hoisted to a single class definition and
//! reused across all occurrences. Dict literal types are closed by default
//! (`closed=True`); a `**: T` entry switches that to `extra_items=T`.
//!
//! Example:
//! ```by
//! a: {"name": str, "age": int}
//! b: {"name": str, **: str}
//! ```
//!
//! Lowers to:
//! ```python
//! from typing_extensions import TypedDict
//!
//! class _TypedDict_<hash1>(TypedDict, closed=True):
//!     name: str
//!     age: int
//!
//! class _TypedDict_<hash2>(TypedDict, extra_items=str):
//!     name: str
//!
//! a: _TypedDict_<hash1>
//! b: _TypedDict_<hash2>
//! ```

use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use indexmap::IndexMap;
use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::{Expr, ModModule, Operator, Stmt};
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_text_size::Ranged;

use super::type_expr_walker::{Recurse, TypeExprVisitor, TypePos, walk_type_positions};

use super::ast_driver::{AstPass, PassContext};

/// Sorted list of `(field_name, type_source)` pairs identifying a unique
/// shape. Sorted so that `{"a": int, "b": str}` and `{"b": str, "a": int}`
/// resolve to the same class. `extra_items` carries the type for a
/// basedpython `**: T` marker; when `None` the dict is closed (no extra keys
/// allowed).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
struct Shape {
    fields: Vec<(String, String)>,
    extra_items: Option<String>,
}

impl Shape {
    fn class_name(&self) -> String {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        #[expect(clippy::cast_possible_truncation)]
        let truncated = hasher.finish() as u32;
        format!("_TypedDict_{truncated:08x}")
    }

    fn class_def(&self, name: &str) -> String {
        let bases = match &self.extra_items {
            Some(ty) => format!("TypedDict, extra_items={ty}"),
            None => "TypedDict, closed=True".to_owned(),
        };
        let mut out = format!("class {name}({bases}):\n");
        if self.fields.is_empty() {
            out.push_str("    pass\n");
            return out;
        }
        for (field_name, field_type) in &self.fields {
            // Field names that aren't valid Python identifiers (e.g. keys
            // with spaces or hyphens) cannot be expressed as class-body
            // attribute annotations. Such typed dicts are still valid
            // (TypedDict's functional form supports them), but we leave
            // them un-rewritten and let the user fall back to that form.
            let _ = writeln!(out, "    {field_name}: {field_type}");
        }
        out
    }
}

pub(crate) struct TypedDictLiteral<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
    pub(crate) errors: Vec<String>,
    /// Insertion-ordered map of shape → synthesized class name so emitted
    /// classes follow dependency order (inner-before-outer)
    shapes: IndexMap<Shape, String>,
    /// Source range of every typed-dict literal we've assigned a class to.
    /// Used when rendering an outer dict's body to substitute nested dict
    /// spans with their class names.
    range_to_class: Vec<(ruff_text_size::TextRange, String)>,
    pub(crate) needs_import: bool,
    /// Whether any field rendering used basedpython literal-type promotion.
    /// Lib's preamble step turns this into the `from typing import Literal`
    /// import.
    pub(crate) needs_literal_import: bool,
    /// set when a nested `T??` field needs the runtime `Optional[...]` wrapper
    pub(crate) needs_optional_runtime: bool,
}

impl<'src> TypedDictLiteral<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
            errors: Vec::new(),
            shapes: IndexMap::new(),
            range_to_class: Vec::new(),
            needs_import: false,
            needs_literal_import: false,
            needs_optional_runtime: false,
        }
    }

    pub(crate) fn class_defs(&self) -> String {
        let mut out = String::new();
        for (shape, name) in &self.shapes {
            out.push_str(&shape.class_def(name));
            out.push('\n');
        }
        out
    }

    /// Render `range`'s source, substituting any nested typed-dict literal
    /// span with its synthesized class name.
    fn render_subbed(&self, range: ruff_text_size::TextRange) -> String {
        use ruff_text_size::TextSize;
        let mut subs: Vec<(ruff_text_size::TextRange, &str)> = self
            .range_to_class
            .iter()
            .filter(|(r, _)| r.start() >= range.start() && r.end() <= range.end())
            .map(|(r, n)| (*r, n.as_str()))
            .collect();
        subs.sort_by_key(|(r, _)| r.start());

        let mut out = String::new();
        let mut cursor: TextSize = range.start();
        for (sub_range, replacement) in subs {
            if sub_range.start() < cursor {
                continue;
            }
            out.push_str(&self.source[usize::from(cursor)..usize::from(sub_range.start())]);
            out.push_str(replacement);
            cursor = sub_range.end();
        }
        out.push_str(&self.source[usize::from(cursor)..usize::from(range.end())]);
        out
    }

    /// Try to extract a shape from a dict literal. Returns `None` when the
    /// dict has any non-string-literal key, any unpacked `**other` item (other
    /// than the basedpython `**: T` extra-items marker), or any field name
    /// that isn't a valid Python identifier — those cases are left for the
    /// user to spell with the functional `TypedDict()` form instead.
    fn extract_shape(&mut self, dict: &ruff_python_ast::ExprDict) -> Option<Shape> {
        // an empty dict literal `{}` is a valid annotation: a TypedDict with no
        // fields. it still needs to be hoisted so the surface annotation lands
        // on a real class name instead of leaving `{}` raw in python output
        let mut fields = Vec::with_capacity(dict.items.len());
        let mut extra_items: Option<String> = None;
        for item in &dict.items {
            let Some(key_expr) = item.key.as_ref() else {
                // `**: T` is encoded as `key = None, value = Starred(Starred(T))`.
                // any other key-less item is a regular `**other` unpacking,
                // which we don't rewrite
                let Expr::Starred(outer) = &item.value else {
                    return None;
                };
                let Expr::Starred(inner) = outer.value.as_ref() else {
                    return None;
                };
                if extra_items.is_some() {
                    return None;
                }
                extra_items = Some(self.render_field_type(&inner.value));
                continue;
            };
            let Expr::StringLiteral(s) = key_expr else {
                self.errors.push(format!(
                    "typed-dict literal field key must be a string literal, got `{}`",
                    &self.source[usize::from(key_expr.range().start())
                        ..usize::from(key_expr.range().end())]
                ));
                return None;
            };
            let name = s.value.to_str().to_owned();
            if !is_identifier(&name) {
                self.errors.push(format!(
                    "typed-dict literal field name `{name}` is not a valid Python identifier"
                ));
                return None;
            }
            if fields.iter().any(|(n, _): &(String, String)| n == &name) {
                self.errors
                    .push(format!("duplicate typed-dict literal field `{name}`"));
                return None;
            }
            let type_source = self.render_field_type(&item.value);
            fields.push((name, type_source));
        }
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Some(Shape {
            fields,
            extra_items,
        })
    }

    /// Render a field's type expression as Python source, applying
    /// basedpython type-form lowerings (literal-type promotion, parenthesized
    /// tuple types, intersection) recursively. Falls back to source text for
    /// untouched expressions.
    fn render_field_type(&mut self, expr: &Expr) -> String {
        // nested typed-dict literals get substituted with their synthesized
        // class names via `range_to_class`
        if let Some(name) = self
            .range_to_class
            .iter()
            .find(|(r, _)| *r == expr.range())
            .map(|(_, n)| n.clone())
        {
            return name;
        }
        match expr {
            Expr::BinOp(b) if matches!(b.op, Operator::BitOr) => self.render_union(expr),
            Expr::BinOp(b) if matches!(b.op, Operator::BitAnd) => {
                let parts = flatten_bitand(expr);
                let rendered: Vec<String> =
                    parts.iter().map(|p| self.render_field_type(p)).collect();
                format!("Intersection[{}]", rendered.join(", "))
            }
            Expr::Tuple(t) if t.parenthesized && !t.elts.is_empty() => {
                let parts: Vec<String> = t.elts.iter().map(|e| self.render_field_type(e)).collect();
                format!("tuple[{}]", parts.join(", "))
            }
            // `T?` → `T | None` (nested `T??` → `Optional[T | None]`) so an
            // optional field type composes instead of leaking `?`
            Expr::UnaryOp(u) if matches!(u.op, ruff_python_ast::UnaryOp::Optional) => {
                let mut depth: usize = 1;
                let mut inner: &Expr = u.operand.as_ref();
                while let Expr::UnaryOp(u2) = inner {
                    if u2.op != ruff_python_ast::UnaryOp::Optional {
                        break;
                    }
                    depth += 1;
                    inner = u2.operand.as_ref();
                }
                let inner_str = self.render_field_type(inner);
                if depth >= 2 {
                    self.needs_optional_runtime = true;
                }
                format!(
                    "{}{inner_str} | None{}",
                    "Optional[".repeat(depth - 1),
                    "]".repeat(depth - 1)
                )
            }
            _ if is_literal_value(expr) => {
                self.needs_literal_import = true;
                format!("Literal[{}]", self.render_subbed(expr.range()))
            }
            _ => self.render_subbed(expr.range()),
        }
    }

    /// Render a `|` union, grouping consecutive bare literal arms into a
    /// single `Literal[…]` subscript.
    fn render_union(&mut self, expr: &Expr) -> String {
        let parts = flatten_bitor(expr);
        let mut out_groups: Vec<String> = Vec::new();
        let mut pending_list: Vec<String> = Vec::new();
        let flush = |pending: &mut Vec<String>, out: &mut Vec<String>| {
            if !pending.is_empty() {
                out.push(format!("Literal[{}]", pending.join(", ")));
                pending.clear();
            }
        };
        for p in parts {
            if is_literal_value(p) {
                self.needs_literal_import = true;
                pending_list.push(self.render_subbed(p.range()));
            } else {
                flush(&mut pending_list, &mut out_groups);
                out_groups.push(self.render_field_type(p));
            }
        }
        flush(&mut pending_list, &mut out_groups);
        out_groups.join(" | ")
    }

    fn class_name_for(&mut self, shape: Shape, source_range: ruff_text_size::TextRange) -> String {
        let name = if let Some(existing) = self.shapes.get(&shape) {
            existing.clone()
        } else {
            let n = shape.class_name();
            self.shapes.insert(shape, n.clone());
            n
        };
        self.range_to_class.push((source_range, name.clone()));
        name
    }

    fn rewrite_dict_in_annotation(&mut self, dict: &ruff_python_ast::ExprDict) {
        let Some(shape) = self.extract_shape(dict) else {
            return;
        };
        let class_name = self.class_name_for(shape, dict.range());
        self.needs_import = true;
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            class_name,
            dict.range(),
        )));
    }

    /// Recursively descend into an annotation, rewriting any contained dict
    /// literals. Walks structurally relevant containers (`BinOp`, Subscript,
    /// parenthesized Tuple, `BoolOp`) so that `dict[str, {"k": int}]`,
    /// `{"k": int} | None`, etc. all lower correctly.
    fn visit_annotation(&mut self, expr: &Expr) {
        match expr {
            Expr::Dict(d) => {
                // Walk children first so nested dicts register before the
                // outer renders its field types.
                for item in &d.items {
                    self.visit_annotation(&item.value);
                }
                self.rewrite_dict_in_annotation(d);
            }
            Expr::BinOp(b) => {
                self.visit_annotation(&b.left);
                self.visit_annotation(&b.right);
            }
            Expr::Subscript(s) => {
                self.visit_annotation(&s.slice);
            }
            Expr::Tuple(t) => {
                for elt in &t.elts {
                    self.visit_annotation(elt);
                }
            }
            Expr::List(l) => {
                for elt in &l.elts {
                    self.visit_annotation(elt);
                }
            }
            // `**: T` marker is `Starred(Starred(T))`; recurse so nested dict
            // literals inside an extra-items type are registered too
            Expr::Starred(s) => {
                self.visit_annotation(&s.value);
            }
            _ => {}
        }
    }
}

impl TypeExprVisitor for TypedDictLiteral<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        // `visit_annotation` is a deep recursive rewriter (it knows about
        // BinOp, Subscript, Tuple, List, Starred, Dict). emit edits, stop
        // the walker from descending further to avoid double-processing
        self.visit_annotation(expr);
        Recurse::Stop
    }
}

/// True when `expr` is a basedpython "bare literal" eligible for promotion to
/// `Literal[…]` in a type expression: int, str/bytes, bool, hex/neg-int, or
/// `None`
fn is_literal_value(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::NumberLiteral(_)
            | Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
    ) || matches!(
        expr,
        Expr::UnaryOp(u) if matches!(u.op, ruff_python_ast::UnaryOp::USub)
            && matches!(u.operand.as_ref(), Expr::NumberLiteral(_))
    )
}

fn flatten_bitor(expr: &Expr) -> Vec<&Expr> {
    fn walk<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
        match expr {
            Expr::BinOp(b) if matches!(b.op, Operator::BitOr) => {
                walk(&b.left, out);
                walk(&b.right, out);
            }
            _ => out.push(expr),
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

fn flatten_bitand(expr: &Expr) -> Vec<&Expr> {
    fn walk<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
        match expr {
            Expr::BinOp(b) if matches!(b.op, Operator::BitAnd) => {
                walk(&b.left, out);
                walk(&b.right, out);
            }
            _ => out.push(expr),
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

impl AstPass for TypedDictLiteralPass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut inner = TypedDictLiteral::new(self.source);
        let body: &[Stmt] = &module.body;
        walk_type_positions(body, None, &mut inner);
        if inner.needs_literal_import {
            ctx.required_imports
                .push("from typing import Literal".to_owned());
        }
        if inner.needs_optional_runtime {
            ctx.required_imports
                .push(super::wrapped_runtime::OPTIONAL_RUNTIME.to_owned());
        }
        if inner.needs_import {
            // synthesized classes use `closed=True` / `extra_items=T` (PEP 728),
            // which the stdlib `typing.TypedDict` rejects on every released
            // python. source from `typing_extensions` so the generated class
            // body actually executes at runtime
            ctx.required_imports
                .push("from typing_extensions import TypedDict".to_owned());
            // synthesized class defs are raw multi-line python source. push
            // them into required_imports as a single non-`from` line so
            // merge_from_imports leaves them untouched and they end up in
            // the preamble verbatim
            let defs = inner.class_defs();
            if !defs.is_empty() {
                ctx.required_imports
                    .push(defs.trim_end_matches('\n').to_owned());
            }
        }
        for fix in inner.edits {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                ctx.text_edits.push((range, repl));
            }
        }
        ctx.errors.extend(inner.errors);
    }
}

/// AST-pass wrapper around [`TypedDictLiteral`] that surfaces its
/// `needs_literal_import` flag back to the driver so the literal-types
/// import is emitted alongside the synthesized class definitions
pub(crate) struct TypedDictLiteralPass<'src> {
    source: &'src str,
}

impl<'src> TypedDictLiteralPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
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

    #[test]
    fn simple_annotation() {
        let out = transpile(
            "a: {\"name\": str, \"age\": int}\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("from typing_extensions import TypedDict"),
            "got: {out}"
        );
        assert!(
            out.contains("(TypedDict, closed=True):"),
            "dict literal types should be closed by default, got: {out}"
        );
        assert!(out.contains("    name: str\n"), "got: {out}");
        assert!(out.contains("    age: int\n"), "got: {out}");
        assert!(out.contains("a: _TypedDict_"), "got: {out}");
    }

    #[test]
    fn extra_items_marker() {
        let out = transpile("b: {\"key\": int, **: str}\n", &Config::test_default()).unwrap();
        assert!(
            out.contains("from typing_extensions import TypedDict"),
            "got: {out}"
        );
        assert!(
            out.contains("(TypedDict, extra_items=str):"),
            "`**: T` should lower to extra_items=T, got: {out}"
        );
        assert!(out.contains("    key: int\n"), "got: {out}");
        assert!(out.contains("b: _TypedDict_"), "got: {out}");
        // `extra_items=` form must not also set `closed=True`
        assert!(
            !out.contains("closed=True"),
            "extra_items form should not also emit closed=True, got: {out}"
        );
    }

    #[test]
    fn extra_items_distinct_shape() {
        // Same fields but different extra_items must produce distinct classes
        let out = transpile(
            indoc! {"
                a: {\"key\": int}
                b: {\"key\": int, **: str}
            "},
            &Config::test_default(),
        )
        .unwrap();
        let count = out.matches("class _TypedDict_").count();
        assert_eq!(count, 2, "got: {out}");
        assert!(out.contains("closed=True"), "got: {out}");
        assert!(out.contains("extra_items=str"), "got: {out}");
    }

    #[test]
    fn extra_items_only() {
        let out = transpile("c: {**: int}\n", &Config::test_default()).unwrap();
        assert!(out.contains("(TypedDict, extra_items=int):"), "got: {out}");
        assert!(
            out.contains("    pass\n"),
            "empty body needs pass, got: {out}"
        );
    }

    #[test]
    fn function_param_and_return() {
        let out = transpile(
            indoc! {"
                def f(x: {\"name\": str}) -> {\"name\": str}:
                    return x
            "},
            &Config::test_default(),
        )
        .unwrap();
        // Same shape — single class
        let count = out.matches("class _TypedDict_").count();
        assert_eq!(count, 1, "got: {out}");
        assert!(out.contains("def f(x: _TypedDict_"), "got: {out}");
        assert!(out.contains("-> _TypedDict_"), "got: {out}");
    }

    #[test]
    fn shape_dedup() {
        let out = transpile(
            indoc! {"
                a: {\"name\": str, \"age\": int}
                b: {\"name\": str, \"age\": int}
            "},
            &Config::test_default(),
        )
        .unwrap();
        let count = out.matches("class _TypedDict_").count();
        assert_eq!(count, 1, "got: {out}");
    }

    #[test]
    fn key_order_dedup() {
        // Different key insertion order, same shape after sort
        let out = transpile(
            indoc! {"
                a: {\"name\": str, \"age\": int}
                b: {\"age\": int, \"name\": str}
            "},
            &Config::test_default(),
        )
        .unwrap();
        let count = out.matches("class _TypedDict_").count();
        assert_eq!(count, 1, "got: {out}");
    }

    #[test]
    fn nested_typed_dict_in_field() {
        let out = transpile(
            "a: {\"point\": {\"x\": int, \"y\": int}, \"tag\": str}\n",
            &Config::test_default(),
        )
        .unwrap();
        let count = out.matches("class _TypedDict_").count();
        assert_eq!(count, 2, "got: {out}");
        // Outer body must reference inner class name, not raw `{...}` source
        assert!(
            !out.contains("    point: {\"x\": int"),
            "outer body still has unlowered inner dict, got: {out}"
        );
    }

    #[test]
    fn typed_dict_in_subscript() {
        let out = transpile("a: list[{\"name\": str}]\n", &Config::test_default()).unwrap();
        assert!(out.contains("class _TypedDict_"), "got: {out}");
        assert!(out.contains("a: list[_TypedDict_"), "got: {out}");
    }

    #[test]
    fn typed_dict_in_union() {
        let out = transpile("a: {\"name\": str} | None\n", &Config::test_default()).unwrap();
        assert!(out.contains("class _TypedDict_"), "got: {out}");
        assert!(out.contains("a: _TypedDict_"), "got: {out}");
        assert!(out.contains("| None"), "got: {out}");
    }

    #[test]
    fn value_position_unchanged() {
        // Dict literal in value position is a regular dict, not a TypedDict
        check(
            "a: dict[str, int] = {\"k\": 1}\n",
            "a: dict[str, int] = {\"k\": 1}\n",
        );
    }

    #[test]
    fn non_string_keys_rejected() {
        // Non-string-literal keys can't form a TypedDict shape; reject at
        // transpile time rather than leaving an invalid annotation in the
        // output
        let err = transpile("a: {1: int}\n", &Config::test_default()).unwrap_err();
        assert!(err.contains("must be a string literal"), "got: {err}");
    }

    #[test]
    fn non_identifier_keys_rejected() {
        // Field names that aren't valid Python identifiers can't be
        // expressed as class-body attribute annotations — reject
        let err = transpile("a: {\"has space\": int}\n", &Config::test_default()).unwrap_err();
        assert!(err.contains("not a valid Python identifier"), "got: {err}");
    }

    #[test]
    fn empty_dict_lowers_to_empty_typed_dict() {
        // `a: {}` is shorthand for an empty TypedDict. leaving `{}` raw in
        // python output would be invalid as an annotation
        let out = transpile("a: {}\n", &Config::test_default()).unwrap();
        assert!(out.contains("_TypedDict_"), "got: {out}");
        assert!(
            out.contains("(TypedDict, closed=True):"),
            "expected closed-typed-dict base, got: {out}"
        );
        // body is just `pass`; no field lines
        assert!(out.contains("    pass"), "expected pass body, got: {out}");
    }

    #[test]
    fn python_passthrough_unchanged() {
        unchanged("a: {\"k\": int}\n");
    }

    #[test]
    fn class_def_order_inner_before_outer() {
        let out = transpile(
            "x: {\"point\": {\"x\": int, \"y\": int}, \"tag\": str}\n",
            &Config::test_default(),
        )
        .unwrap();
        let inner_pos = out
            .find("class _TypedDict_")
            .expect("expected at least one class");
        let next = out[inner_pos + 1..]
            .find("class _TypedDict_")
            .map(|p| inner_pos + 1 + p)
            .expect("expected two classes");
        let inner_name = out[inner_pos..]
            .lines()
            .next()
            .unwrap()
            .strip_prefix("class ")
            .and_then(|s| s.split('(').next())
            .unwrap();
        // The second class definition must reference the first by name.
        assert!(
            out[next..].contains(inner_name),
            "second class def must reference first by name, got: {out}"
        );
    }

    #[test]
    fn duplicate_field_rejected() {
        let result = transpile("c: {\"k\": int, \"k\": str}\n", &Config::test_default());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[test]
    fn non_identifier_key_rejected() {
        let result = transpile("b: {\"foo-bar\": int}\n", &Config::test_default());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("not a valid Python identifier"), "got: {err}");
    }

    #[test]
    fn non_string_key_rejected() {
        let result = transpile("a: {1: int}\n", &Config::test_default());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("must be a string literal"), "got: {err}");
    }
}

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
//!     name: "str"
//!     age: "int"
//!
//! class _TypedDict_<hash2>(TypedDict, extra_items="str"):
//!     name: "str"
//!
//! a: _TypedDict_<hash1>
//! b: _TypedDict_<hash2>
//! ```
//!
//! field types are emitted as forward references — see `Shape::class_def`

use std::fmt::Write as _;
use std::hash::Hash;

use ruff_python_ast::{Expr, ModModule, Operator};
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_text_size::Ranged;

use super::ast_driver::{AstPass, PassContext};
use super::quotes::WrittenQuotes;
use super::repeated_underscore::WrittenNames;
use super::synthesized_types::{Namer, SynthesizedKind};

/// Sorted list of `(field_name, type_source)` pairs identifying a unique
/// shape. Sorted so that `{"a": int, "b": str}` and `{"b": str, "a": int}`
/// resolve to the same class. `extra_items` carries the type for a
/// basedpython `**: T` marker; when `None` the dict is closed (no extra keys
/// allowed).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub(crate) struct Shape {
    fields: Vec<(String, String)>,
    extra_items: Option<String>,
}

impl Shape {
    /// the name of the class declaring this shape, one the module has no other use of
    pub(crate) fn class_name(&self, written: WrittenNames) -> String {
        written.synthesized_class("_TypedDict_", self)
    }

    /// the class declaring this shape as `name`, `typing_extensions.TypedDict` written as
    /// `typed_dict`
    ///
    /// every field type is written as a string. the hoisted classes are emitted in the
    /// preamble, ahead of everything the module defines, and a shape can name a class declared
    /// later. that does not resolve where the class lands, and `TypedDict` evaluates its
    /// class-body annotations eagerly, so every field is a forward reference
    pub(crate) fn class_def(&self, name: &str, typed_dict: &str, quotes: &WrittenQuotes) -> String {
        let bases = match &self.extra_items {
            Some(ty) => format!("{typed_dict}, extra_items={}", quotes.quote(ty)),
            None => format!("{typed_dict}, closed=True"),
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
            let _ = writeln!(out, "    {field_name}: {}", quotes.quote(field_type));
        }
        out
    }
}

/// The shape of a dict literal type, or `None` when the dict has any
/// non-string-literal key, any unpacked item that is neither the basedpython
/// `**: T` extra-items marker nor a `**Kwargs` pack splice, or any field name
/// that isn't a valid Python identifier — those cases are left for the user to
/// spell with the functional `TypedDict()` form instead.
pub(crate) fn shape(namer: &mut Namer, dict: &ruff_python_ast::ExprDict) -> Option<Shape> {
    // an empty dict literal `{}` is a valid annotation: a TypedDict with no
    // fields. it still needs to be hoisted so the surface annotation lands
    // on a real class name instead of leaving `{}` raw in python output
    let mut fields = Vec::with_capacity(dict.items.len());
    let mut extra_items: Option<String> = None;
    for item in &dict.items {
        let Some(key_expr) = item.key.as_ref() else {
            // `**: T` is encoded as `key = None, value = Starred(Starred(T))`
            let Expr::Starred(outer) = &item.value else {
                // `{**Kwargs}` splices a keyword-variadic pack. its fields are
                // only known once the enclosing class is specialized, and python
                // erases type arguments anyway, so the shape lowers to a
                // `TypedDict` that admits the pack's keys as extra items
                if matches!(&item.value, Expr::Name(_)) && extra_items.is_none() {
                    extra_items = Some(namer.written.builtin("object"));
                    continue;
                }
                return None;
            };
            let Expr::Starred(inner) = outer.value.as_ref() else {
                return None;
            };
            if extra_items.is_some() {
                return None;
            }
            extra_items = Some(field_type(namer, &inner.value));
            continue;
        };
        let Expr::StringLiteral(s) = key_expr else {
            namer.errors.push(format!(
                "typed-dict literal field key must be a string literal, got `{}`",
                namer.src(key_expr.range())
            ));
            return None;
        };
        let name = s.value.to_str().to_owned();
        if !is_identifier(&name) {
            namer.errors.push(format!(
                "typed-dict literal field name `{name}` is not a valid Python identifier"
            ));
            return None;
        }
        if fields.iter().any(|(n, _): &(String, String)| n == &name) {
            namer
                .errors
                .push(format!("duplicate typed-dict literal field `{name}`"));
            return None;
        }
        let type_source = field_type(namer, &item.value);
        fields.push((name, type_source));
    }
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    Some(Shape {
        fields,
        extra_items,
    })
}

/// A field's type as python source, reading the type parameters of the generics around it by
/// the names they have where the class is declared.
fn field_type(namer: &mut Namer, expr: &Expr) -> String {
    let rendered = render_field_type(namer, expr);
    namer.read_type_parameters(expr.range(), &rendered)
}

/// Render a field's type expression as Python source, applying
/// basedpython type-form lowerings (literal-type promotion, parenthesized
/// tuple types, intersection) recursively, and writing every type the walk
/// already named — a nested typed dict, an anonymous named tuple, an inline
/// protocol — as its class.
fn render_field_type(namer: &mut Namer, expr: &Expr) -> String {
    if let Some(name) = namer.substitution_for(expr.range()) {
        return name;
    }
    match expr {
        Expr::BinOp(b) if matches!(b.op, Operator::BitOr) => render_union(namer, expr),
        Expr::BinOp(b) if matches!(b.op, Operator::BitAnd) => {
            let parts = flatten_bitand(expr);
            let rendered: Vec<String> = parts.iter().map(|p| render_field_type(namer, p)).collect();
            namer.imports.insert(("ty_extensions", "Intersection"));
            format!(
                "{}[{}]",
                namer.written.imported("ty_extensions", "Intersection"),
                rendered.join(", ")
            )
        }
        Expr::Tuple(t) if t.parenthesized && !t.elts.is_empty() => {
            let parts: Vec<String> = t.elts.iter().map(|e| render_field_type(namer, e)).collect();
            format!("{}[{}]", namer.written.builtin("tuple"), parts.join(", "))
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
            let inner_str = render_field_type(namer, inner);
            if depth >= 2 {
                namer.needs_optional_runtime = true;
            }
            format!(
                "{}{inner_str} | None{}",
                "Optional[".repeat(depth - 1),
                "]".repeat(depth - 1)
            )
        }
        _ if is_literal_value(expr) => {
            namer.imports.insert(("typing", "Literal"));
            format!(
                "{}[{}]",
                namer.written.imported("typing", "Literal"),
                namer.src(expr.range())
            )
        }
        _ => namer.lower(expr),
    }
}

/// Render a `|` union, grouping consecutive bare literal arms into a
/// single `Literal[…]` subscript.
fn render_union(namer: &mut Namer, expr: &Expr) -> String {
    let parts = flatten_bitor(expr);
    let mut out_groups: Vec<String> = Vec::new();
    let mut pending_list: Vec<String> = Vec::new();
    let literal = namer.written.imported("typing", "Literal");
    let flush = |pending: &mut Vec<String>, out: &mut Vec<String>| {
        if !pending.is_empty() {
            out.push(format!("{literal}[{}]", pending.join(", ")));
            pending.clear();
        }
    };
    for p in parts {
        if is_literal_value(p) {
            namer.imports.insert(("typing", "Literal"));
            pending_list.push(namer.src(p.range()).to_owned());
        } else {
            flush(&mut pending_list, &mut out_groups);
            out_groups.push(render_field_type(namer, p));
        }
    }
    flush(&mut pending_list, &mut out_groups);
    out_groups.join(" | ")
}

/// True when `expr` is a basedpython "bare literal" eligible for promotion to
/// `Literal[…]` in a type expression: int, str/bytes, bool, hex/neg-int, or
/// `None`
fn is_literal_value(expr: &Expr) -> bool {
    // a float or complex is no `Literal`, and the shared lowerer prints it the way the
    // project spells one
    matches!(
        expr,
        Expr::NumberLiteral(ruff_python_ast::ExprNumberLiteral {
            value: ruff_python_ast::Number::Int(_),
            ..
        }) | Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
    ) || matches!(
        expr,
        Expr::UnaryOp(u) if matches!(u.op, ruff_python_ast::UnaryOp::USub)
            && matches!(
                u.operand.as_ref(),
                Expr::NumberLiteral(ruff_python_ast::ExprNumberLiteral {
                    value: ruff_python_ast::Number::Int(_),
                    ..
                })
            )
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

impl AstPass for TypedDictLiteralPass {
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::TypedDictLiteral)
    }

    /// a typed dict type it replaces moves into a class, whose field types the synthesized
    /// type namer prints
    fn subsumes(&self) -> &'static [super::ast_driver::Lowering] {
        super::synthesized_types::CLASS_BODY
    }

    fn run(&self, _module: &mut ModModule, ctx: &mut PassContext) {
        // the class each one names, and the declarations, come from the namer's one walk over
        // the module as parsed, which named every kind of synthesized type together
        for (range, name) in ctx.synthesized.of_kind(SynthesizedKind::TypedDict) {
            ctx.text_edits.push((range, name));
        }
    }
}

/// replaces each typed dict literal type with the `TypedDict` class it names
pub(crate) struct TypedDictLiteralPass;

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

    /// the class is declared at module scope, so a field naming a type parameter reads it by
    /// the name the generic declares it under there: the polyfill's `_T`, or the parameter
    /// read off the generic it belongs to where the list stays native
    #[test]
    fn a_type_parameter_field_reads_the_declared_name() {
        let source = indoc! {"
            class K[T]:
                def get(self) -> {\"a\": T}: ...
            type F[U] = {\"b\": U}
        "};
        let out = transpile(source, &Config::test_default()).unwrap();
        assert!(
            out.contains("    a: \"_T\"\n") && out.contains("    b: \"_U\"\n"),
            "got:\n{out}"
        );
        let config = Config {
            min_version: crate::PythonVersion::PY313,
            ..Config::test_default()
        };
        let out = transpile(source, &config).unwrap();
        assert!(
            out.contains("    a: \"K.__type_params__[0]\"\n"),
            "got:\n{out}"
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
        assert!(out.contains("    name: \"str\"\n"), "got: {out}");
        assert!(out.contains("    age: \"int\"\n"), "got: {out}");
        assert!(out.contains("a: _TypedDict_"), "got: {out}");
    }

    /// a field's type is read when the class's hints are, so the name it reads is imported
    #[test]
    fn an_intersection_field_imports_intersection() {
        let out = transpile(
            indoc! {"
                class A: ...
                class B: ...
                a: {\"x\": A & B}
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("    x: \"Intersection[A, B]\"\n")
                && out.contains("from ty_extensions import Intersection"),
            "got: {out}"
        );
    }

    #[test]
    fn extra_items_marker() {
        let out = transpile("b: {\"key\": int, **: str}\n", &Config::test_default()).unwrap();
        assert!(
            out.contains("from typing_extensions import TypedDict"),
            "got: {out}"
        );
        assert!(
            out.contains("(TypedDict, extra_items=\"str\"):"),
            "`**: T` should lower to extra_items=T, got: {out}"
        );
        assert!(out.contains("    key: \"int\"\n"), "got: {out}");
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
        assert!(out.contains("extra_items=\"str\""), "got: {out}");
    }

    #[test]
    fn extra_items_only() {
        let out = transpile("c: {**: int}\n", &Config::test_default()).unwrap();
        assert!(
            out.contains("(TypedDict, extra_items=\"int\"):"),
            "got: {out}"
        );
        assert!(
            out.contains("    pass\n"),
            "empty body needs pass, got: {out}"
        );
    }

    #[test]
    fn keyword_pack_splice() {
        // `{**Kwargs}` fields are only known once the class is specialized, so
        // the erased shape admits them as extra items
        let out = transpile(
            indoc! {"
                class A[**Kwargs]:
                    def get(self) -> {\"tag\": int, **Kwargs}:
                        raise NotImplementedError
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("(TypedDict, extra_items=\"object\"):"),
            "got: {out}"
        );
        assert!(out.contains("    tag: \"int\"\n"), "got: {out}");
        assert!(out.contains("-> _TypedDict_"), "got: {out}");
        assert!(!out.contains("**Kwargs}"), "got: {out}");
    }

    #[test]
    fn forward_reference_field() {
        // hoisted classes land ahead of the module body, so a field naming a
        // class defined later must not be evaluated at class-creation time
        let out = transpile(
            indoc! {"
                a: {\"foo\": Foo}

                class Foo: ...
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("    foo: \"Foo\"\n"), "got: {out}");
    }

    /// a field's type is a type position like any other, so it carries a type modifier: `literal
    /// str` is written `LiteralString`, and `final`, which python has no spelling for, is written
    /// as the type it marks. the field is quoted, and a quoted type cannot start with the blanks
    /// the keyword was replaced with in the source
    #[test]
    fn a_type_modifier_on_a_field() {
        let out = transpile(
            indoc! {"
                a: {\"s\": literal str, \"n\": final int, \"l\": list[final int], **: final str}
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("(TypedDict, extra_items=\"str\"):"),
            "got: {out}"
        );
        assert!(out.contains("    s: \"LiteralString\"\n"), "got: {out}");
        assert!(out.contains("    n: \"int\"\n"), "got: {out}");
        assert!(out.contains("    l: \"list[int]\"\n"), "got: {out}");
    }

    #[test]
    fn quoted_field_type_escapes_inner_quotes() {
        let out = transpile("a: {\"k\": \"Foo\"}\n", &Config::test_default()).unwrap();
        assert!(
            out.contains("    k: \"Literal[\\\"Foo\\\"]\"\n"),
            "got: {out}"
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
        // Dict literal in value position is a regular dict, not a TypedDict,
        // and a builtin display is not reified — it passes through unchanged
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

    /// the class is hoisted out of the annotation, so no other pass's edit reaches a field
    /// type: each is printed lowered here, where a field read back by `get_type_hints`
    /// has to name a real type
    #[test]
    fn field_types_are_printed_lowered() {
        let out = transpile(
            indoc! {"
                y = 1
                a: {\"f\": float, \"d\": dynamic, \"c\": 1.5, \"k\": (int) -> str, \"t\": typeof y}
            "},
            &Config::test_default(),
        )
        .unwrap();
        for field in [
            "f: \"JustFloat\"",
            "d: \"Any\"",
            "c: \"float\"",
            "k: \"Callable[[int], str]\"",
            "t: \"TypeOf[y]\"",
        ] {
            assert!(out.contains(field), "`{field}` is missing from:\n{out}");
        }
    }
}

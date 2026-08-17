//! Lowering of PEP 604 unions that reach the runtime, for targets before 3.10.
//!
//! `int | str` is a call of `type.__or__`, which python only grew in 3.10.
//! Written as an *annotation* that costs nothing — a target this old always
//! gets `from __future__ import annotations`, so no annotation is ever
//! evaluated — but written where the value is really produced it is a
//! `TypeError` at import time:
//!
//! ```text
//! isinstance(x, int | str)   ⇒   isinstance(x, (int, str,))
//! cast(int | str, value)     ⇒   cast(Union[int, str], value)
//! ```
//!
//! The two spellings are not interchangeable: `isinstance` takes a tuple of
//! classes and rejects a `typing.Union`, while everything else wants the
//! `Union` — so the classinfo argument of `isinstance` / `issubclass` is
//! rewritten to a tuple, and every other union to `Union[...]`. Within that
//! argument the tuple form reaches through tuples and lists, since `isinstance`
//! accepts those nested; anywhere else inside it — a subscript's slice, a call's
//! arguments — is ordinary value context again.
//!
//! An arm written as `None` becomes `type(None)` in the tuple form. `None` is a
//! value, not a class, and only the union operator accepts it as shorthand for
//! `NoneType`.
//!
//! Whether a `|` is a union at all is asked of the checker rather than guessed
//! from the shape: `a | b` is overwhelmingly a bitwise or, and only the types of
//! its operands tell the two apart.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ExprCall, Operator, PythonVersion, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// the version `type.__or__` arrived in
const MIN_VERSION: PythonVersion = PythonVersion::PY310;

pub(crate) struct RuntimeUnionPass {
    min_version: PythonVersion,
}

impl RuntimeUnionPass {
    pub(crate) fn new(min_version: PythonVersion) -> Self {
        Self { min_version }
    }
}

impl TypeAwarePass for RuntimeUnionPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        if self.min_version >= MIN_VERSION {
            return;
        }
        let mut lower = Lower {
            types,
            edits: Vec::new(),
            needs_import: false,
        };
        for stmt in stmts {
            lower.visit_stmt(stmt);
        }
        if lower.needs_import {
            ctx.required_imports
                .push("from typing import Union".to_owned());
        }
        ctx.template_edits.extend(lower.edits);
    }
}

struct Lower<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    needs_import: bool,
}

impl<'ast> Visitor<'ast> for Lower<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    /// an annotation is a string at runtime for every target this pass runs
    /// for, so nothing in one is ever evaluated
    fn visit_annotation(&mut self, _expr: &'ast Expr) {}

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr
            && let Some(classinfo) = self.classinfo_argument(call)
        {
            for (index, argument) in call.arguments.args.iter().enumerate() {
                if index == 1 {
                    self.visit_classinfo(classinfo);
                } else {
                    self.visit_expr(argument);
                }
            }
            for keyword in &call.arguments.keywords {
                self.visit_expr(&keyword.value);
            }
            self.visit_expr(&call.func);
            return;
        }

        if let Some(arms) = self.union_arms(expr) {
            self.needs_import = true;
            let fragments = spell(&arms, Form::Union);
            self.edits.push((expr.range(), fragments));
            for arm in arms {
                self.visit_expr(arm);
            }
            return;
        }

        walk_expr(self, expr);
    }
}

/// How a union is spelled, which is decided by where it stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    /// a tuple of classes, for `isinstance` / `issubclass`
    ClassInfo,
    /// `Union[...]`, for everywhere else
    Union,
}

impl<'ast> Lower<'_> {
    /// The second positional argument of a call to the real `isinstance` or
    /// `issubclass`. A file that binds either name means something else by it,
    /// and gets no special treatment.
    fn classinfo_argument(&self, call: &'ast ExprCall) -> Option<&'ast Expr> {
        let Expr::Name(name) = call.func.as_ref() else {
            return None;
        };
        if !matches!(name.id.as_str(), "isinstance" | "issubclass") {
            return None;
        }
        if !self.types.is_unbound_at(name.id.as_str(), &call.func) {
            return None;
        }
        call.arguments.args.get(1)
    }

    /// The arms of `expr`, when it is a union the runtime will evaluate.
    fn union_arms(&self, expr: &'ast Expr) -> Option<Vec<&'ast Expr>> {
        let Expr::BinOp(binop) = expr else {
            return None;
        };
        if binop.op != Operator::BitOr || !self.types.is_runtime_union(expr) {
            return None;
        }
        let mut arms = Vec::new();
        collect_arms(expr, &mut arms);
        Some(arms)
    }

    /// Visit an expression standing where `isinstance` expects classes. A union
    /// here becomes a tuple, and so does one nested inside a tuple or list the
    /// argument already spells; everything else is ordinary value context.
    fn visit_classinfo(&mut self, expr: &'ast Expr) {
        if let Some(arms) = self.union_arms(expr) {
            let fragments = spell(&arms, Form::ClassInfo);
            self.edits.push((expr.range(), fragments));
            for arm in arms {
                self.visit_classinfo(arm);
            }
            return;
        }
        match expr {
            Expr::Tuple(tuple) => {
                for element in &tuple.elts {
                    self.visit_classinfo(element);
                }
            }
            Expr::List(list) => {
                for element in &list.elts {
                    self.visit_classinfo(element);
                }
            }
            _ => self.visit_expr(expr),
        }
    }
}

/// The union as the target can spell it. Each arm passes through as source so
/// that a lowering inside it — an optional `T?`, a tuple type — still composes.
fn spell(arms: &[&Expr], form: Form) -> Vec<Fragment> {
    let mut fragments = Vec::with_capacity(arms.len() * 2 + 2);
    fragments.push(Fragment::Lit(
        match form {
            Form::ClassInfo => "(",
            Form::Union => "Union[",
        }
        .to_owned(),
    ));
    for (index, arm) in arms.iter().enumerate() {
        if index > 0 {
            fragments.push(Fragment::Lit(", ".to_owned()));
        }
        if form == Form::ClassInfo && arm.is_none_literal_expr() {
            fragments.push(Fragment::Lit("type(None)".to_owned()));
        } else {
            fragments.push(Fragment::Src(arm.range()));
        }
    }
    // the tuple's trailing comma is what makes a one-arm union a tuple rather
    // than a parenthesized class
    fragments.push(Fragment::Lit(
        match form {
            Form::ClassInfo => ",)",
            Form::Union => "]",
        }
        .to_owned(),
    ));
    fragments
}

/// Flatten `a | b | c`, which parses as `(a | b) | c`, into its arms.
fn collect_arms<'ast>(expr: &'ast Expr, arms: &mut Vec<&'ast Expr>) {
    if let Expr::BinOp(binop) = expr
        && binop.op == Operator::BitOr
    {
        collect_arms(&binop.left, arms);
        collect_arms(&binop.right, arms);
        return;
    }
    arms.push(expr);
}

#[cfg(test)]
mod tests {
    use crate::{Config, PythonVersion, transpile};
    use indoc::indoc;

    /// transpile for a target that predates `type.__or__`
    fn lowered(input: &str) -> String {
        let config = Config {
            min_version: PythonVersion::PY39,
            ..Config::test_default()
        };
        transpile(input, &config).unwrap()
    }

    /// a union assigned as a value builds a `types.UnionType`, which is what
    /// 3.10 added; `typing.Union` is the spelling every version has
    #[test]
    fn an_alias_is_spelled_out() {
        assert_eq!(
            lowered("Alias = int | str\n"),
            indoc! {"
                from __future__ import annotations
                from typing import Union
                Alias = Union[int, str]
            "}
        );
    }

    /// `isinstance` rejects a `typing.Union` and accepts a tuple, so the
    /// classinfo argument gets the other spelling
    #[test]
    fn isinstance_takes_a_tuple() {
        let out = lowered("def f(x: object):\n    return isinstance(x, int | str)\n");
        assert!(out.contains("isinstance(x, (int, str,))"), "got:\n{out}");
        assert!(!out.contains("Union"), "got:\n{out}");
    }

    /// `None` is shorthand the union operator understands and a tuple does not
    #[test]
    fn none_becomes_its_class_in_a_tuple() {
        let out = lowered("def f(x: object):\n    return isinstance(x, int | None)\n");
        assert!(
            out.contains("isinstance(x, (int, type(None),))"),
            "got:\n{out}"
        );
    }

    /// a union nested in the tuple `isinstance` was already given is still a
    /// tuple — nesting is something `isinstance` accepts
    #[test]
    fn a_nested_classinfo_union_is_a_tuple_too() {
        let out = lowered("def f(x: object):\n    return isinstance(x, (bytes, int | str))\n");
        assert!(
            out.contains("isinstance(x, (bytes, (int, str,)))"),
            "got:\n{out}"
        );
    }

    /// a `cast` target is a type expression the runtime still evaluates
    #[test]
    fn a_cast_target_is_spelled_out() {
        let out = lowered(indoc! {"
            from typing import cast
            def f(v: object):
                return cast(int | str, v)
        "});
        assert!(out.contains("cast(Union[int, str], v)"), "got:\n{out}");
    }

    /// an annotation is a string on every target this runs for, so it needs no
    /// rewriting and keeps the spelling the author chose
    #[test]
    fn an_annotation_is_left_alone() {
        let out = lowered("def f(x: int | str) -> bytes | None: ...\n");
        assert!(out.contains("x: int | str"), "got:\n{out}");
        assert!(out.contains("-> bytes | None"), "got:\n{out}");
    }

    /// an ordinary bitwise or is not a union whatever it is written between
    #[test]
    fn a_bitwise_or_is_untouched() {
        let out = lowered("def f(a: int, b: int) -> int:\n    return a | b\n");
        assert!(out.contains("return a | b"), "got:\n{out}");
    }

    /// a file that means something else by `isinstance` gets no special
    /// treatment for its second argument
    #[test]
    fn a_shadowed_isinstance_is_not_a_classinfo_call() {
        let out = lowered(indoc! {"
            def isinstance(x: object, t: object) -> bool:
                return True

            def f(x: object):
                return isinstance(x, int | str)
        "});
        assert!(
            out.contains("isinstance(x, Union[int, str])"),
            "got:\n{out}"
        );
    }

    /// a target that has `type.__or__` keeps every union as written
    #[test]
    fn untouched_from_python_310() {
        let out = transpile(
            "Alias = int | str\ndef f(x: object):\n    return isinstance(x, int | str)\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("Alias = int | str"), "got:\n{out}");
        assert!(out.contains("isinstance(x, int | str)"), "got:\n{out}");
    }
}

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
//! An optional `T?` the runtime evaluates is the union `T | None` too, and is
//! spelled by the same rule: `(T, type(None),)` where `isinstance` expects
//! classes, `Union[T, None]` everywhere else. The optional lowering writes it, and
//! asks [`classinfo_optionals`] which of its optionals stand in the classinfo
//! argument.
//!
//! Which expressions of the argument are read as classes is decided once, in
//! `ty_python_semantic::types::classinfo_spelling`, which ty reads too: a union
//! spelled there is one it does not report as a union `isinstance` rejects.
//!
//! Whether a `|` is a union at all is asked of the checker rather than guessed
//! from the shape: `a | b` is overwhelmingly a bitwise or, and only the types of
//! its operands tell the two apart.

use std::collections::HashSet;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Operator, PythonVersion, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::types::classinfo_spelling;

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::repeated_underscore::WrittenNames;
use crate::type_info::TypeInfo;

/// the version `type.__or__` arrived in
const MIN_VERSION: PythonVersion = PythonVersion::PY310;

pub(crate) struct RuntimeUnionPass<'src> {
    written: WrittenNames<'src>,
    min_version: PythonVersion,
}

impl<'src> RuntimeUnionPass<'src> {
    pub(crate) fn new(written: WrittenNames<'src>, min_version: PythonVersion) -> Self {
        Self {
            written,
            min_version,
        }
    }
}

impl TypeAwarePass for RuntimeUnionPass<'_> {
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::RuntimeUnion)
    }

    // only a union the runtime evaluates needs the older spelling. a checker
    // reads `X | Y` in a stub whatever version the stub is for
    fn runtime_only(&self) -> bool {
        true
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        if self.min_version >= MIN_VERSION {
            return;
        }
        let mut lower = Lower::new(
            types,
            self.written.imported("typing", "Union"),
            self.written.builtin("type"),
        );
        for stmt in stmts {
            lower.visit_stmt(stmt);
        }
        if lower.needs_import {
            ctx.required_imports
                .push(self.written.import_from("typing", &["Union"]));
        }
        ctx.template_edits.extend(lower.edits);
    }
}

/// The optionals in `stmts` that stand where `isinstance` / `issubclass` expects
/// classes, and so are spelled as a tuple of classes below 3.10.
pub(crate) fn classinfo_optionals(stmts: &[Stmt], types: &dyn TypeInfo) -> HashSet<TextRange> {
    // only the ranges are read, never the spelling
    let mut lower = Lower::new(types, String::new(), String::new());
    for stmt in stmts {
        lower.visit_stmt(stmt);
    }
    lower.classinfo_optionals
}

struct Lower<'a> {
    types: &'a dyn TypeInfo,
    /// the name `typing.Union` is written under
    union: String,
    /// the name the builtin `type` is written under
    type_: String,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    needs_import: bool,
    /// every optional the classinfo walk reached, for the optional lowering
    classinfo_optionals: HashSet<TextRange>,
}

impl<'a> Lower<'a> {
    fn new(types: &'a dyn TypeInfo, union: String, type_: String) -> Self {
        Self {
            types,
            union,
            type_,
            edits: Vec::new(),
            needs_import: false,
            classinfo_optionals: HashSet::new(),
        }
    }
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
            && let Some(classinfo) = self.types.classinfo_argument(call)
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
            let fragments = spell(&arms, Form::Union(&self.union));
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
enum Form<'a> {
    /// a tuple of classes, for `isinstance` / `issubclass`, the builtin `type` written
    /// under the name given
    ClassInfo(&'a str),
    /// `Union[...]`, for everywhere else, `Union` written under the name given
    Union(&'a str),
}

impl<'ast> Lower<'_> {
    /// The arms of `expr`, when it is a union the runtime will evaluate.
    fn union_arms(&self, expr: &'ast Expr) -> Option<Vec<&'ast Expr>> {
        let Expr::BinOp(binop) = expr else {
            return None;
        };
        if binop.op != Operator::BitOr || !self.types.is_runtime_union(expr) {
            return None;
        }
        Some(classinfo_spelling::union_arms(expr))
    }

    /// Visit the argument `isinstance` reads as classes. Each union where it reads classes
    /// becomes a tuple, and each optional there is spelled as one by the optional lowering;
    /// everything else is ordinary value context.
    fn visit_classinfo(&mut self, classinfo: &'ast Expr) {
        let types = self.types;
        let positions =
            classinfo_spelling::class_positions(classinfo, &|expr| types.is_runtime_union(expr));
        for position in positions {
            match position {
                Expr::Tuple(_) | Expr::List(_) => {}
                Expr::UnaryOp(optional)
                    if optional.op == UnaryOp::Optional && types.is_runtime_union(position) =>
                {
                    self.classinfo_optionals.insert(position.range());
                }
                _ => match self.union_arms(position) {
                    Some(arms) => {
                        let fragments = spell(&arms, Form::ClassInfo(&self.type_));
                        self.edits.push((position.range(), fragments));
                    }
                    None => self.visit_expr(position),
                },
            }
        }
    }
}

/// The union as the target can spell it. Each arm passes through as source so
/// that a lowering inside it — an optional `T?`, a tuple type — still composes.
fn spell(arms: &[&Expr], form: Form) -> Vec<Fragment> {
    let mut fragments = Vec::with_capacity(arms.len() * 2 + 2);
    fragments.push(Fragment::Lit(match form {
        Form::ClassInfo(_) => "(".to_owned(),
        Form::Union(union) => format!("{union}["),
    }));
    for (index, arm) in arms.iter().enumerate() {
        if index > 0 {
            fragments.push(Fragment::Lit(", ".to_owned()));
        }
        if let Form::ClassInfo(type_) = form
            && arm.is_none_literal_expr()
        {
            fragments.push(Fragment::Lit(format!("{type_}(None)")));
        } else {
            fragments.push(Fragment::Src(arm.range()));
        }
    }
    // the tuple's trailing comma is what makes a one-arm union a tuple rather
    // than a parenthesized class
    fragments.push(Fragment::Lit(
        match form {
            Form::ClassInfo(_) => ",)",
            Form::Union(_) => "]",
        }
        .to_owned(),
    ));
    fragments
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

    /// an optional is the union `T | None`, and where `isinstance` expects classes it
    /// is spelled as that union is there
    #[test]
    fn an_optional_classinfo_is_a_tuple() {
        let out = lowered(indoc! {"
            def f(x: object, t: type):
                return isinstance(x, int?) or issubclass(t, int?)
        "});
        assert!(
            out.contains("isinstance(x, (int, type(None),)) or issubclass(t, (int, type(None),))"),
            "got:\n{out}"
        );
        assert!(!out.contains("Union"), "got:\n{out}");
    }

    /// an optional inside a classinfo union or tuple is still a classinfo, and so is a
    /// union an optional wraps
    #[test]
    fn a_nested_classinfo_optional_is_a_tuple_too() {
        let out = lowered(indoc! {"
            def f(x: object):
                return isinstance(x, int? | str) or isinstance(x, (float?, bytes)) or isinstance(x, (int | str)?)
        "});
        assert!(
            out.contains("isinstance(x, ((int, type(None),), str,))"),
            "got:\n{out}"
        );
        assert!(
            out.contains("isinstance(x, ((float, type(None),), bytes))"),
            "got:\n{out}"
        );
        assert!(
            out.contains("isinstance(x, (((int, str,)), type(None),))"),
            "got:\n{out}"
        );
        assert!(!out.contains("Union"), "got:\n{out}");
    }

    /// an optional anywhere else keeps the `Union` spelling, a subscript inside the
    /// classinfo argument among them
    #[test]
    fn an_optional_outside_a_classinfo_is_a_union() {
        let out = lowered(indoc! {"
            from typing import cast
            def f(v: object):
                return cast(int?, v)
        "});
        assert!(out.contains("cast(Union[int, None], v)"), "got:\n{out}");
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

    /// the builtin reached by another name is the builtin all the same
    #[test]
    fn an_isinstance_by_another_name_takes_a_tuple() {
        let out = lowered(indoc! {"
            from builtins import isinstance as is_instance

            def f(x: object):
                return is_instance(x, int | str)
        "});
        assert!(out.contains("is_instance(x, (int, str,))"), "got:\n{out}");
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

    /// nothing in a stub is evaluated, so a union in one keeps the spelling a
    /// checker reads whatever the target
    #[test]
    fn a_stub_keeps_its_unions() {
        let config = Config {
            is_stub: true,
            min_version: PythonVersion::PY39,
            ..Config::test_default()
        };
        assert_eq!(
            transpile("Alias = int | str\n", &config).unwrap(),
            "Alias = int | str\n"
        );
    }
}

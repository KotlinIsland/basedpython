//! The test a `.by` operator dunder makes of its operand before its body runs.
//!
//! python calls an operator dunder with whatever the other operand is, and moves on to the
//! other operand's reflected dunder (or from `__iadd__` to `__add__`) when the method returns
//! `NotImplemented`. the checker reads a parameter's *domain* — its annotation with every type
//! argument taken off — as the operands the method hands back that way, so a `.by` method
//! hands back exactly those:
//!
//! ```by
//! class A:
//!     def __add__(self, other: int) -> int:
//!         return other + 1
//! ```
//!
//! lowers to
//!
//! ```python
//! class A:
//!     def __add__(self, other: int) -> int:
//!         if not isinstance(other, int): return NotImplemented
//!         return other + 1
//! ```
//!
//! the test is the one `other is int` makes ([`build_predicate`](super::parametric_is)), and
//! which test that is, or that there is none to make, is the checker's to say
//! ([`TypeInfo::operand_guard`]): a domain every operand is inside needs none, and one no
//! runtime test can tell an operand apart from is reported where the parameter is annotated.
//! a native build of the module hands such an operand to the interpreted definition, which is
//! this code.

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_stmt};
use ruff_python_ast::{Expr, ExprNoneLiteral, Stmt, StmtFunctionDef};
use ruff_text_size::Ranged;

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::mutable_defaults::DefaultMarkers;
use super::parametric_is::{PredicateNeeds, PredicateRuntime, ProbeStrictness, fragments_for_plan};
use super::repeated_underscore::WrittenNames;
use super::soundness::RerenderedCheck;
use super::source_util::{PrologueStatement, body_prologue};
use crate::type_info::TypeInfo;

/// the guard statement, which is one line and re-establishes no indentation
struct Guard(Vec<Fragment>);

impl PrologueStatement for Guard {
    fn push(&self, frags: &mut Vec<Fragment>, _indent: &str) {
        frags.extend(self.0.iter().cloned());
    }
}

pub(crate) struct OperandGuardPass<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
    markers: &'src DefaultMarkers,
}

impl<'src> OperandGuardPass<'src> {
    pub(crate) fn new(
        source: &'src str,
        written: WrittenNames<'src>,
        markers: &'src DefaultMarkers,
    ) -> Self {
        Self {
            source,
            written,
            markers,
        }
    }
}

/// every `def` in a statement, at any depth
#[derive(Default)]
struct Functions<'ast>(Vec<&'ast StmtFunctionDef>);

impl<'ast> SourceOrderVisitor<'ast> for Functions<'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt {
            self.0.push(function);
        }
        walk_stmt(self, stmt);
    }
}

impl TypeAwarePass for OperandGuardPass<'_> {
    // the test runs where the body begins, which a stub has none of
    fn runtime_only(&self) -> bool {
        true
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut needs = PredicateNeeds::default();
        let mut inserts = Vec::new();
        let mut rerendered = Vec::new();
        for (index, stmt) in stmts.iter().enumerate() {
            let mut functions = Functions::default();
            functions.visit_stmt(stmt);
            let mut checks = Vec::new();
            for function in functions.0 {
                let Some((parameter, plan)) = types.operand_guard(function) else {
                    continue;
                };
                // the plan spells a type the source writes only from the operand's own
                // annotation. one that takes its type from the method it overrides is spelled
                // whole, and never from this placeholder
                let placeholder;
                let annotation = match parameter.annotation() {
                    Some(annotation) => annotation,
                    None => {
                        placeholder = Expr::NoneLiteral(ExprNoneLiteral::default());
                        &placeholder
                    }
                };
                let name =
                    crate::python_parameter_name(&function.parameters, parameter, self.written)
                        .to_string();
                let value = || Fragment::Lit(name.clone());
                let mut guard = vec![Fragment::Lit("if not ".to_owned())];
                // a default swapped for a sentinel is put back after this test: the sentinel
                // stands for the default, which is inside the annotation, and only a direct
                // call that leaves the operand out passes it. python's operators never do
                let sentinel = function
                    .parameters
                    .posonlyargs
                    .iter()
                    .chain(&function.parameters.args)
                    .find(|candidate| candidate.parameter.range() == parameter.range())
                    .and_then(|candidate| candidate.default.as_deref())
                    .and_then(|default| {
                        self.markers
                            .sentinel_for(self.written, default, self.source)
                    });
                if let Some(sentinel) = &sentinel {
                    guard.push(Fragment::Lit(format!("({name} is {sentinel} or ")));
                }
                guard.extend(fragments_for_plan(
                    types,
                    self.written,
                    plan,
                    &value,
                    annotation,
                    ProbeStrictness::Strict,
                    &mut needs,
                ));
                if sentinel.is_some() {
                    guard.push(Fragment::Lit(")".to_owned()));
                }
                guard.push(Fragment::Lit(format!(
                    ": return {}",
                    self.written.builtin("NotImplemented")
                )));
                // an AST-mutation pass rewrote this statement and prints it from its syntax
                // tree, where an edit of the source would be dropped
                if ctx.changed.contains(&index) {
                    let Some(text) = written_out(&guard) else {
                        ctx.errors.push(format!(
                            "`{}` tests its operand against a type the source spells, in a \
                             statement a lowering rewrote",
                            function.name
                        ));
                        continue;
                    };
                    checks.push(RerenderedCheck::Guards {
                        function: function.range(),
                        statements: vec![text],
                    });
                    continue;
                }
                match body_prologue(self.source, function, &[Guard(guard)]) {
                    Some(anchored) => inserts.extend(anchored),
                    // a native build hands the operands the test fails to this definition, so
                    // a test with nowhere to go is an error rather than one dropped
                    None => ctx.errors.push(format!(
                        "`{}` has a body nothing in the source anchors, so the test of its \
                         operand has nowhere to go",
                        function.name
                    )),
                }
            }
            if !checks.is_empty() {
                rerendered.push((index, checks));
            }
        }
        for runtime in PredicateRuntime::used(&needs) {
            ctx.runtime.extend(runtime.helpers());
        }
        ctx.statement_inserts.extend(inserts);
        ctx.rerendered_checks.extend(rerendered);
    }
}

/// `fragments` as the text they write, where none passes the source through
fn written_out(fragments: &[Fragment]) -> Option<String> {
    fragments
        .iter()
        .map(|fragment| match fragment {
            Fragment::Lit(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    fn out(input: &str) -> String {
        transpile(
            input,
            &Config {
                min_version: PythonVersion::PY313,
                ..Config::test_default()
            },
        )
        .unwrap()
    }

    #[test]
    fn an_annotated_operand_is_tested_where_the_body_begins() {
        assert_eq!(
            out(indoc! {r#"
                class A:
                    def __add__(self, other: int) -> int:
                        return other

                    def __radd__(self, other: list[int] | None) -> int:
                        """the docstring stays first"""
                        return 0
            "#}),
            indoc! {r#"
                class A:
                    def __add__(self, other: int) -> int:
                        if not isinstance(other, int): return NotImplemented
                        return other

                    def __radd__(self, other: list[int] | None) -> int:
                        """the docstring stays first"""
                        if not (isinstance(other, list) or other is None): return NotImplemented
                        return 0
            "#}
        );
    }

    #[test]
    fn a_method_that_takes_every_operand_tests_nothing() {
        let source = indoc! {"
            class A:
                def __eq__(self, other: object) -> bool:
                    return True

                def __contains__(self, item: int) -> bool:
                    return True

                def __sub__(self, *others: int) -> int:
                    return 0
        "};
        assert_eq!(out(source), source);
    }

    #[test]
    fn a_body_on_the_clause_line_is_broken_onto_its_own() {
        assert_eq!(
            out(indoc! {"
                class A:
                    def __mul__(self, other: int) -> int: return other
            "}),
            "class A:\n    def __mul__(self, other: int) -> int: \n        \
             if not isinstance(other, int): return NotImplemented\n        return other\n"
        );
    }

    #[test]
    fn an_implementation_tests_the_domain_of_every_overload() {
        let out = out(indoc! {"
            from typing import overload

            class A:
                @overload
                def __add__(self, other: int) -> int: ...
                @overload
                def __add__(self, other: str) -> str: ...
                def __add__(self, other: int | str) -> int | str:
                    return other
        "});
        assert_eq!(
            out.matches("return NotImplemented").count(),
            1,
            "only the implementation runs: {out}"
        );
        assert!(
            out.contains(
                "        if not (isinstance(other, int) or isinstance(other, str)): return NotImplemented\n        return other"
            ),
            "{out}"
        );
    }

    #[test]
    fn a_class_declared_further_down_is_read_as_the_body_runs() {
        assert!(
            out(indoc! {"
                class A:
                    def __lt__(self, other: B) -> bool:
                        return True

                class B: ...
            "})
            .contains("        if not isinstance(other, B): return NotImplemented\n")
        );
    }

    #[test]
    fn a_literal_operand_is_tested_by_value() {
        assert!(
            out(indoc! {r#"
                class A:
                    def __mul__(self, other: "a" | 1) -> int:
                        return 0
            "#})
            .contains(
                r#"if not ((type(other) is str and other == "a") or (type(other) is int and other == 1)): return NotImplemented"#
            )
        );
    }

    #[test]
    fn a_statement_a_lowering_rewrote_carries_the_test() {
        // a repeated `_` is a rewrite of the syntax tree, which prints the whole class
        let out = out(indoc! {"
            class A:
                def __add__(self, other: int) -> int:
                    return other

                def ignore(self, _, _) -> None: ...
        "});
        assert!(
            out.contains("        if not isinstance(other, int):\n            return NotImplemented\n        return other"),
            "{out}"
        );
    }

    #[test]
    fn the_test_runs_ahead_of_the_soundness_checks_in_a_rewritten_statement() {
        let out = transpile(
            indoc! {"
                class Box[T]:
                    pass

                class A:
                    def __add__(self, other: Box[int]) -> int:
                        return 1

                    def ignore(self, _, _) -> None: ...
            "},
            &Config {
                min_version: PythonVersion::PY313,
                soundness: crate::SoundnessPositions::all(),
                ..Config::test_default()
            },
        )
        .unwrap();
        let test = out.find("if not isinstance(other, Box)").expect(&out);
        let check = out.find("_soundness_parametric(other").expect(&out);
        assert!(test < check, "{out}");
    }

    #[test]
    fn a_method_of_an_extension_tests_nothing() {
        // an extension's method is called directly, never dispatched to, so a `NotImplemented`
        // it handed back would be the call's value
        let out = out(indoc! {"
            class V:
                pass

            extension V:
                def __add__(self, other: int) -> int:
                    return other
        "});
        assert!(!out.contains("NotImplemented"), "{out}");
    }

    #[test]
    fn the_test_follows_the_global_declarations_a_body_starts_with() {
        // python requires a `global` ahead of any use of the name it declares
        let out = out(indoc! {"
            class A:
                def __add__(self, other: int) -> int:
                    global count
                    count = 1
                    return 1

            count = 0
        "});
        assert!(
            out.contains(
                "        global count\n        if not isinstance(other, int): return NotImplemented"
            ),
            "{out}"
        );
    }

    #[test]
    fn a_default_swapped_for_a_sentinel_passes_the_test() {
        // python's operators always pass the operand, so only a direct call that leaves it out
        // passes the sentinel, which stands for the default
        let out = out(indoc! {"
            class A:
                def __add__(self, other: list[int] = []) -> int:
                    return len(other)
        "});
        assert!(
            out.contains(
                "if not (other is _MISSING or isinstance(other, list)): return NotImplemented\n        if other is _MISSING:"
            ),
            "{out}"
        );
    }

    #[test]
    fn an_operand_that_inherits_its_type_is_tested_against_it() {
        let out = out(indoc! {"
            class A:
                def __add__(self, other: int) -> int:
                    return 1

            class B(A):
                override def __add__(self, other) -> int:
                    return 2
        "});
        assert_eq!(
            out.matches("if not isinstance(other, int): return NotImplemented")
                .count(),
            2,
            "{out}"
        );
    }

    #[test]
    fn a_protocol_member_tests_nothing() {
        let out = out(indoc! {"
            from typing import Protocol

            class SupportsAdd(Protocol):
                def __add__(self, other: int) -> int: ...
        "});
        assert!(!out.contains("NotImplemented"), "{out}");
    }
}

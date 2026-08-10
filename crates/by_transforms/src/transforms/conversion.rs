//! Lowering for conversion sites.
//!
//! A conversion site is a position where the checker accepted a value that is
//! not assignable to the type declared there, because some conversion repairs
//! it. There are four, and ty resolves which one applies — this pass only emits
//! the call it was handed:
//!
//! ```text
//! report(celsius)         →  report(Fahrenheit.__from__(celsius))
//! v: Vec3 = [1.0, 2.0]    →  v: Vec3 = Vec3.__of__([1.0, 2.0])
//! report(celsius)         →  report((celsius).__into__())
//! ```
//!
//! Which one it is never matters here: a conversion arrives as the text to put
//! on each side of the value, plus the module-level name that text references.
//! Resolving it in the checker is what keeps the emitted call and the type it
//! was accepted for from ever disagreeing (see `TypeInfo::call_conversions`).

use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::ConversionInfo;

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// emit the conversion the checker resolved at every conversion site
pub(crate) struct ConversionPass<'a> {
    source: &'a str,
}

impl<'a> ConversionPass<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        Self { source }
    }
}

/// split `[start, end)` into the span without its final character and that
/// character's text. Used to keep a `Src` fragment from ending on a position
/// where a sibling insertion lives
fn shave_last_char(source: &str, start: TextSize, end: TextSize) -> (TextRange, String) {
    let text = &source[usize::from(start)..usize::from(end)];
    match text.chars().next_back() {
        Some(last) => (
            TextRange::new(start, end - TextSize::of(last)),
            last.to_string(),
        ),
        None => (TextRange::new(start, end), String::new()),
    }
}

/// the first character of `[start, limit)` and the position after it. Used to
/// keep a `Src` fragment from starting on a position where a sibling insertion
/// lives
fn shave_first_char(source: &str, start: TextSize, limit: TextSize) -> (String, TextSize) {
    let text = &source[usize::from(start)..usize::from(limit)];
    match text.chars().next() {
        Some(first) => (first.to_string(), start + TextSize::of(first)),
        None => (String::new(), start),
    }
}

impl TypeAwarePass for ConversionPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut collector = ConversionCollector {
            types,
            sites: Vec::new(),
            function_depth: 0,
        };
        for stmt in stmts {
            collector.visit_stmt(stmt);
        }
        if collector.sites.is_empty() {
            return;
        }

        // where each module-level class this file declares is bound, so a
        // conversion that runs at import time can be checked against it
        let declarations: Vec<(String, TextRange)> = stmts
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::ClassDef(class) => Some((class.name.to_string(), class.range)),
                _ => None,
            })
            .collect();

        for site in collector.sites {
            let mut rejected = false;
            for (value_range, info) in &site.wraps {
                let ConversionInfo::Call {
                    referenced_name,
                    import,
                    ..
                } = info
                else {
                    // the checker accepted this site, so saying nothing would emit
                    // python that type-checks and never converts
                    let ConversionInfo::Rejected(reason) = info else {
                        continue;
                    };
                    ctx.errors.push(format!(
                        "{reason} (offset {})",
                        u32::from(value_range.start())
                    ));
                    rejected = true;
                    continue;
                };
                if let Some(import) = import {
                    // always aliased: the class's own name may already mean
                    // something else here, and an import that rebinds it — or that
                    // this file's own class then shadows — would send the call to
                    // the wrong object at runtime
                    if import.alias == import.name {
                        ctx.required_imports
                            .push(format!("from {} import {}", import.module, import.name));
                    } else {
                        ctx.required_imports.push(format!(
                            "from {} import {} as {}",
                            import.module, import.name, import.alias
                        ));
                    }
                    continue;
                }
                // python binds a class name when its statement executes, so a
                // conversion that runs at import time cannot precede the class it
                // converts through. inside a function body the name is resolved at
                // call time, so order does not matter there
                if site.runs_at_import
                    && let Some(name) = referenced_name
                    && let Some((_, declared_at)) =
                        declarations.iter().find(|(declared, _)| declared == name)
                    && value_range.start() < declared_at.start()
                {
                    ctx.errors.push(format!(
                        "the conversion this value needs goes through `{name}`, which is \
                        declared later in the module (offset {}); move it above the \
                        conversion, or convert the value explicitly",
                        u32::from(value_range.start()),
                    ));
                    rejected = true;
                }
            }
            if rejected {
                continue;
            }

            // ONE edit spanning the whole argument list, not one per argument: a
            // peer pass that rewrites an argument outright (`x cast T`) claims
            // exactly the argument's range, and two replacements over the same
            // range collide — the loser is dropped silently. Claiming the enclosing
            // span makes every such edit strictly inside a `Src` fragment, where it
            // is materialized instead. Everything but the inserted calls passes
            // through as source, so comments and formatting survive
            let mut fragments: Vec<Fragment> = Vec::new();
            let mut cursor = site.claim_range.start();
            for (value_range, info) in &site.wraps {
                // No `Src` fragment may *end* exactly where another *begins*, or a
                // sibling zero-width insertion at that position is emitted twice —
                // once as the left span's end, once as the right span's start (see
                // `materialize_fragments`). An operand's boundaries are exactly such
                // positions: `x!` inserts `_force_unwrap(` at the operand's start and
                // its `)` at the end. So the punctuation character on each side of
                // the operand is copied as a literal, moving the `Src` boundary off
                // the insertion point
                let (lead, opening_punctuation) =
                    shave_last_char(self.source, cursor, value_range.start());
                fragments.push(Fragment::Src(lead));
                fragments.push(Fragment::Lit(opening_punctuation));
                let ConversionInfo::Call { prefix, suffix, .. } = info else {
                    continue;
                };
                fragments.push(Fragment::Lit(prefix.clone()));
                fragments.push(Fragment::Src(*value_range));
                fragments.push(Fragment::Lit(suffix.clone()));
                let (closing_punctuation, rest) =
                    shave_first_char(self.source, value_range.end(), site.claim_range.end());
                fragments.push(Fragment::Lit(closing_punctuation));
                cursor = rest;
            }
            fragments.push(Fragment::Src(TextRange::new(
                cursor,
                site.claim_range.end(),
            )));
            ctx.template_edits.push((site.claim_range, fragments));
        }
    }
}

/// the conversions one site needs, and whether that site runs at import time
struct SiteConversions {
    /// the span the edit claims: wide enough to strictly contain any peer edit
    /// over one of the wrapped values — a call's whole argument list, or an
    /// annotated assignment's value together with the `=` before it
    claim_range: TextRange,
    /// `(value range, conversion)` in source order
    wraps: Vec<(TextRange, ConversionInfo)>,
    runs_at_import: bool,
}

struct ConversionCollector<'a> {
    types: &'a dyn TypeInfo,
    sites: Vec<SiteConversions>,
    /// how many function bodies enclose the node being visited. a call inside one
    /// resolves its names when the function runs, not when the module loads
    function_depth: usize,
}

impl<'ast> ast::visitor::Visitor<'ast> for ConversionCollector<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // an annotated assignment, an attribute assignment, or a `return`: the
        // claim starts before the value (so a peer edit over it nests) and runs to
        // the last converted range
        let mut wraps = self.types.statement_conversions(stmt);
        if !wraps.is_empty() {
            wraps.sort_by_key(|(range, _)| range.start());
            let claim_start = match stmt {
                Stmt::AnnAssign(assignment) => assignment.annotation.range().end(),
                Stmt::Assign(assignment) => assignment
                    .targets
                    .last()
                    .map_or(stmt.range().start(), |target| target.range().end()),
                // `return <value>`: start inside the keyword so the span opens
                // before the value without depending on the whitespace between
                _ => stmt.range().start() + TextSize::of("return"),
            };
            let claim_end = wraps
                .last()
                .map_or(stmt.range().end(), |(range, _)| range.end());
            self.sites.push(SiteConversions {
                claim_range: TextRange::new(claim_start, claim_end),
                wraps,
                runs_at_import: self.function_depth == 0,
            });
        }
        if matches!(stmt, Stmt::FunctionDef(_)) {
            // the decorators and signature still run eagerly; only the body defers
            self.function_depth += 1;
            ast::visitor::walk_stmt(self, stmt);
            self.function_depth -= 1;
            return;
        }
        ast::visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Lambda(_) = expr {
            // a lambda body resolves its names when it is called, like a `def`
            self.function_depth += 1;
            ast::visitor::walk_expr(self, expr);
            self.function_depth -= 1;
            return;
        }
        if let Expr::Call(call) = expr {
            let mut wraps = self.types.call_conversions(call);
            if !wraps.is_empty() {
                wraps.sort_by_key(|(range, _)| range.start());
                self.sites.push(SiteConversions {
                    claim_range: call.arguments.range(),
                    wraps,
                    runs_at_import: self.function_depth == 0,
                });
            }
        }
        ast::visitor::walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use crate::{Config, transpile};

    fn check(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    /// a `__from__` on the target, and the three positions that reach it
    const TEMPERATURES: &str = "\
class Celsius:
    init(degrees: float)

class Fahrenheit:
    init(degrees: float)

    @classmethod
    def __from__(cls, value: Celsius) -> Self:
        return Fahrenheit(value.degrees * 9 / 5 + 32)

";

    #[test]
    fn from_converts_a_call_argument() {
        let out = check(&format!(
            "{TEMPERATURES}def report(t: Fahrenheit) -> None: ...\n\nreport(Celsius(1.0))\n"
        ));
        assert!(
            out.contains("report(Fahrenheit.__from__(Celsius(1.0)))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn from_converts_an_annotated_assignment() {
        let out = check(&format!("{TEMPERATURES}x: Fahrenheit = Celsius(1.0)\n"));
        assert!(
            out.contains("x: Fahrenheit = Fahrenheit.__from__(Celsius(1.0))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn from_converts_a_plain_assignment_to_a_declared_name() {
        let out = check(&format!(
            "{TEMPERATURES}x: Fahrenheit = Fahrenheit(1.0)\nx = Celsius(1.0)\n"
        ));
        assert!(
            out.contains("x = Fahrenheit.__from__(Celsius(1.0))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn several_targets_convert_nothing() {
        // one value reaches both names, so there is no wrap that serves either
        let out = check(&format!(
            "{TEMPERATURES}x: Fahrenheit = Fahrenheit(1.0)\ny: Fahrenheit = Fahrenheit(1.0)\n\
             x = y = Fahrenheit(2.0)\n"
        ));
        assert!(out.contains("x = y = Fahrenheit(2.0)"), "got:\n{out}");
    }

    #[test]
    fn from_converts_a_return() {
        let out = check(&format!(
            "{TEMPERATURES}def make(c: Celsius) -> Fahrenheit:\n    return c\n"
        ));
        assert!(out.contains("return Fahrenheit.__from__(c)"), "got:\n{out}");
    }

    #[test]
    fn into_converts_through_the_source() {
        let out = check(indoc! {"
            class Kelvin:
                init(degrees: float)

            class Celsius:
                init(degrees: float)

                def __into__(self) -> Kelvin:
                    return Kelvin(self.degrees + 273.15)

            def report(k: Kelvin) -> None: ...

            report(Celsius(1.0))
        "});
        assert!(
            out.contains("report((Celsius(1.0)).__into__())"),
            "got:\n{out}"
        );
    }

    const METERS: &str = "\
class Meters:
    init(value: float)

    @classmethod
    def __of__(cls, value: int | float) -> Self:
        return Meters(float(value))

";

    #[test]
    fn of_converts_a_literal() {
        let out = check(&format!("{METERS}d: Meters = 5\n"));
        assert!(out.contains("d: Meters = Meters.__of__(5)"), "got:\n{out}");
    }

    #[test]
    fn of_converts_each_element_of_a_literal_collection() {
        let out = check(&format!("{METERS}xs: list[Meters] = [1, 2, 3]\n"));
        assert!(
            out.contains(
                "xs: list[Meters] = [Meters.__of__(1), Meters.__of__(2), Meters.__of__(3)]"
            ),
            "got:\n{out}"
        );
    }

    #[test]
    fn of_leaves_a_non_literal_alone() {
        // a name holding the same value is not a literal, so nothing converts and
        // the assignment keeps its ordinary error
        let out = transpile(
            &format!("{METERS}n = 5\nd: Meters = n\n"),
            &Config::test_default(),
        );
        assert!(
            out.is_err() || !out.as_ref().unwrap().contains("__of__(n)"),
            "got:\n{out:?}"
        );
    }

    #[test]
    fn a_whole_value_conversion_wins_over_its_elements() {
        // `Vec3.__of__` takes the list itself, so the literal converts once rather
        // than element-wise
        let out = check(indoc! {"
            class Vec3:
                init(x: float, y: float, z: float)

                @classmethod
                def __of__(cls, value: list[float]) -> Self:
                    return Vec3(*value)

            v: Vec3 = [1.0, 2.0, 3.0]
        "});
        assert!(
            out.contains("v: Vec3 = Vec3.__of__([1.0, 2.0, 3.0])"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_value_that_needs_no_conversion_is_left_alone() {
        let out = check(&format!(
            "{TEMPERATURES}def report(t: Fahrenheit) -> None: ...\n\nreport(Fahrenheit(1.0))\n"
        ));
        assert!(out.contains("report(Fahrenheit(1.0))"), "got:\n{out}");
        assert!(!out.contains("Fahrenheit.__from__("), "got:\n{out}");
    }

    #[test]
    fn several_conversions_in_one_call_keep_the_source_intact() {
        let out = check(&format!(
            "{TEMPERATURES}def report(a: Fahrenheit, b: Fahrenheit) -> None: ...\n\n\
             report(Celsius(1.0), b=Celsius(2.0))\n"
        ));
        assert!(
            out.contains(
                "report(Fahrenheit.__from__(Celsius(1.0)), b=Fahrenheit.__from__(Celsius(2.0)))"
            ),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_conversion_before_its_class_is_rejected() {
        // the class binds its name when its statement runs, so an import-time
        // conversion cannot precede it. only reachable through an automatic
        // forward reference
        let error = transpile(
            indoc! {"
                x: Later = 1

                class Later:
                    @classmethod
                    def __of__(cls, value: int) -> Self: ...
            "},
            &Config::test_default(),
        )
        .expect_err("a use-before-declaration conversion must not be emitted");
        assert!(
            error.contains("declared later in the module"),
            "got:\n{error}"
        );
    }

    #[test]
    fn a_deferred_conversion_before_its_class_is_fine() {
        // inside a function the name resolves when the function runs
        let out = check(indoc! {"
            def f() -> Later:
                return 1

            class Later:
                @classmethod
                def __of__(cls, value: int) -> Self: ...
        "});
        assert!(out.contains("return Later.__of__(1)"), "got:\n{out}");
    }

    #[test]
    fn a_shadowed_target_is_rejected() {
        // the emitted `Fahrenheit.__from__(c)` would resolve `Fahrenheit` to the
        // local `3` and fail at runtime, so it must not be emitted at all
        let error = transpile(
            &format!(
                "{TEMPERATURES}def report(t: Fahrenheit) -> None: ...\n\n\
                 def use(c: Celsius) -> None:\n    Fahrenheit = 3\n    report(c)\n"
            ),
            &Config::test_default(),
        )
        .expect_err("a shadowed conversion target must not be emitted");
        assert!(error.contains("shadowed"), "got:\n{error}");
    }

    #[test]
    fn an_unshadowed_target_still_converts_inside_a_function() {
        // the shadow check must not fire on an ordinary local of another name
        let out = check(&format!(
            "{TEMPERATURES}def report(t: Fahrenheit) -> None: ...\n\n\
             def use(c: Celsius) -> None:\n    scale = 3\n    report(c)\n"
        ));
        assert!(
            out.contains("report(Fahrenheit.__from__(c))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn naming_the_target_in_an_annotation_is_not_shadowing() {
        // a scope's place table holds every name the scope mentions, so the
        // annotation of the very assignment being converted used to read as a
        // shadowing binding — which rejected every function-scope conversion
        // whose target appeared in an annotation
        let out = check(&format!(
            "{TEMPERATURES}def use(c: Celsius) -> None:\n    f: Fahrenheit = c\n"
        ));
        assert!(
            out.contains("f: Fahrenheit = Fahrenheit.__from__(c)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_collection_element_conversion_inside_a_function_lowers() {
        let out = check(&format!(
            "{TEMPERATURES}def use(c: Celsius) -> None:\n    fs: list[Fahrenheit] = [c]\n"
        ));
        assert!(
            out.contains("fs: list[Fahrenheit] = [Fahrenheit.__from__(c)]"),
            "got:\n{out}"
        );
    }

    #[test]
    fn an_ambiguous_conversion_is_rejected_rather_than_skipped() {
        // the checker accepts the site, so emitting nothing would leave python
        // that type-checks and never converts
        let error = transpile(
            indoc! {"
                class Fahrenheit:
                    degrees: float = 0.0

                    @classmethod
                    def __from__(cls, value: Celsius) -> Self:
                        return cls()

                class Celsius:
                    degrees: float = 0.0

                    def __into__(self) -> Fahrenheit:
                        return Fahrenheit()

                def report(t: Fahrenheit) -> None: ...

                report(Celsius())
            "},
            &Config::test_default(),
        )
        .expect_err("an ambiguous conversion must not be emitted");
        assert!(error.contains("more than one conversion"), "got:\n{error}");
    }

    #[test]
    fn into_converts_a_union_source() {
        let out = check(indoc! {"
            class Kelvin:
                init(degrees: float)

            class A:
                def __into__(self) -> Kelvin:
                    return Kelvin(1.0)

            class B:
                def __into__(self) -> Kelvin:
                    return Kelvin(2.0)

            def report(k: Kelvin) -> None: ...

            def use(x: A | B) -> None:
                report(x)
        "});
        assert!(out.contains("report((x).__into__())"), "got:\n{out}");
    }

    #[test]
    fn reverse_transpiling_leaves_a_conversion_call_alone() {
        // an explicit `T.__from__(x)` / `x.__into__()` is valid basedpython that
        // means what it says, so nothing is lost by not re-sugaring it — and
        // unwrapping one would be a guess about whether the site re-inserts it
        let python = r#"class Celsius:
    degrees: float = 0.0

class Fahrenheit:
    degrees: float = 0.0

    @classmethod
    def __from__(cls, value: Celsius) -> "Fahrenheit":
        return cls()

def report(t: Fahrenheit) -> None: ...

report(Fahrenheit.__from__(Celsius()))
"#;
        let back = crate::reverse_transpile(python, &Config::test_default())
            .expect("reverse transpile should succeed");
        assert!(
            back.contains("report(Fahrenheit.__from__(Celsius()))"),
            "the conversion call should survive verbatim, got:\n{back}"
        );
    }
}

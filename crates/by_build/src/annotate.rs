//! the `--annotate` report: what compiled, what did not, and why
//!
//! a decline is invisible unless you look, and the count `by compile` prints says
//! how *many* without saying which. this writes the rest of the answer down:
//! every function's BIR next to its calling convention, and every decline next to
//! its reason.
//!
//! the "why not" half is the point. a compiler whose failures are silent is a
//! compiler nobody can tune against.

use std::fmt::Write;

use by_ir::function::{Function, ModuleIr};
use by_ir::print::print_function;

/// render the report for a lowered module
pub fn report(module: &ModuleIr) -> String {
    let mut out = format!("# {}\n", module.name.dotted());

    let native: Vec<&Function> = module.all_functions().collect();
    let _ = writeln!(
        out,
        "\n{} compiled, {} left interpreted\n",
        native.len(),
        module.declined.len()
    );

    if !module.declined.is_empty() {
        out.push_str("## left to the interpreted definition\n\n");
        for declined in &module.declined {
            let _ = writeln!(out, "- {}: {}", declined.name, declined.reason);
        }
        out.push('\n');
    }

    if !module.gradual.is_empty() {
        // a gradual place is not a decline on its own, but it is the commonest
        // reason a body ends up going through the object protocol
        out.push_str("## gradual places\n\n");
        for use_ in &module.gradual {
            let _ = writeln!(out, "- {}: `{}`", use_.function, use_.place);
        }
        out.push('\n');
    }

    if !module.promoted.is_empty() {
        // nothing here declined — a promoted place compiles, to boxed arithmetic.
        // this is the moment someone wants to know the setting exists, so it says so
        out.push_str(
            "## representations python's numeric promotion cost\n\na `float` annotation admits an `int`, so these places hold `int | float` and cannot\nbe unboxed. `strict-float` in `[tool.ty.analysis]` opts the module out, and a `.by`\nsource has that model already.\n\n",
        );
        for place in &module.promoted {
            let _ = writeln!(
                out,
                "- {}: `{}` would have been {}",
                place.function, place.place, place.missed
            );
        }
        out.push('\n');
    }

    for class in &module.classes {
        let fields = class
            .fields
            .iter()
            .map(|field| format!("{}: {}", field.name, field.ty))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "## class {}{}\n\nfixed layout: {{{fields}}}\n",
            class.name,
            if class.immutable { " (frozen)" } else { "" }
        );
    }

    out.push_str("## bir\n");
    for function in native {
        let borrowed = function
            .registers
            .iter()
            .enumerate()
            .filter(|(_, decl)| decl.borrowed)
            .map(|(index, _)| format!("r{index}"))
            .collect::<Vec<_>>();
        let _ = writeln!(
            out,
            "\n### {}\n\nconvention: {}{}\n\n```\n{}```",
            function.qualified_name(),
            if function.convention.can_fail() {
                "native"
            } else {
                "native, infallible — no error check after a call to it"
            },
            if borrowed.is_empty() {
                String::new()
            } else {
                format!("\nborrowed: {}", borrowed.join(", "))
            },
            print_function(function)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use by_ir::function::Declined;

    fn lowered(source: &str) -> ModuleIr {
        let mut module =
            by_irbuild::module_from_source(source, "app", by_irbuild::Language::default());
        by_opt::optimize(&mut module).expect("the pipeline verifies");
        module
    }

    #[test]
    fn a_report_names_every_decline_and_its_reason() {
        let module = lowered(
            "\
def fast(a: int) -> int:
    return a + 1

def slow(a: int) -> None:
    try:
        pass
    except* ValueError:
        pass
",
        );
        let text = report(&module);
        assert!(text.contains("1 compiled, 1 left interpreted"), "{text}");
        assert!(text.contains("- slow: `except*`"), "{text}");
        assert!(text.contains("### fast"), "{text}");
    }

    #[test]
    fn an_infallible_function_says_so() {
        let module = lowered("def scale(x: float) -> float:\n    return x * 2.0\n");
        let text = report(&module);
        assert!(text.contains("infallible"), "{text}");
    }

    #[test]
    fn a_class_reports_its_layout_and_its_methods() {
        let module = lowered(
            "\
frozen data class Point:
    x: int
    y: int

    def total(self) -> int:
        return self.x + self.y
",
        );
        let text = report(&module);
        assert!(text.contains("## class Point (frozen)"), "{text}");
        assert!(text.contains("fixed layout: {x: int, y: int}"), "{text}");
        // a method is a function too, and appears under its qualified name
        assert!(text.contains("### Point.total"), "{text}");
    }

    #[test]
    fn a_borrowed_register_is_called_out() {
        let module = lowered(
            "\
data class Holder:
    label: str

data class Nest:
    inner: Holder

def inner_label(n: Nest) -> str:
    return n.inner.label
",
        );
        let text = report(&module);
        assert!(text.contains("borrowed: r1"), "{text}");
    }

    #[test]
    fn a_representation_the_promotion_cost_is_named() {
        // nothing here declines, so there is no decline message to carry it — the
        // report is the only place a `.py` module learns the setting exists
        let mut module = by_irbuild::module_from_source(
            "\
def total(xs: list[float], k: float) -> float:
    out = 0.0
    i = 0
    while i < len(xs):
        out = out + xs[i] * k
        i = i + 1
    return out


def named(labels: list[str]) -> int:
    return len(labels)
",
            "app",
            by_irbuild::Language::Python,
        );
        by_opt::optimize(&mut module).expect("the pipeline verifies");
        let text = report(&module);
        assert!(text.contains("0 left interpreted"), "{text}");
        assert!(text.contains("`xs` would have been [float]"), "{text}");
        assert!(text.contains("`k` would have been float"), "{text}");
        assert!(text.contains("strict-float"), "{text}");
        // a `list[str]` was never going to be a buffer, so it is not a missed one
        assert!(!text.contains("`labels`"), "{text}");
    }

    #[test]
    fn a_by_source_has_nothing_to_report_there() {
        // `.by` has the strict numeric model already, so the same source misses
        // nothing and the section does not appear at all
        let module = lowered(
            "\
def total(xs: list[float], k: float) -> float:
    out = 0.0
    i = 0
    while i < len(xs):
        out = out + xs[i] * k
        i = i + 1
    return out
",
        );
        let text = report(&module);
        assert!(!text.contains("numeric promotion cost"), "{text}");
    }

    #[test]
    fn a_module_with_nothing_compiled_still_reports() {
        let mut module = ModuleIr::new("app");
        module.declined.push(Declined {
            name: "f".to_string(),
            reason: "because".to_string(),
            range: None,
        });
        let text = report(&module);
        assert!(text.contains("0 compiled, 1 left interpreted"), "{text}");
        assert!(text.contains("- f: because"), "{text}");
    }
}

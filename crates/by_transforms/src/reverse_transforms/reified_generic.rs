//! reverse of `crate::transforms::reified_generic` and
//! `crate::transforms::reified_class`:
//!   `@generic  # basedpython: reified\ndef f[T]: …` → `def f[T]: …`
//!   `@generic_class  # basedpython: reified\nclass A[T]: …` → `class A[T]: …`
//!
//! the forward transforms tag the decorator line they synthesize with the
//! [`REIFIED_MARKER`](crate::transforms::reified_generic::REIFIED_MARKER)
//! comment — provenance that this decorator came from reification, not from a
//! user's own. only a `@generic` / `@generic_class` carrying that marker is
//! unwrapped; a hand-written one (no marker) is left untouched. a marked class
//! also drops the `T = _type_argument(self, "T")` bindings the forward
//! transform wrote at the top of each method: reading `T` is what the binding
//! stands in for, so the basedpython source is the body without it. the
//! polyfills and their `types` / `typing` imports are dead once the wrappers
//! are removed, and `prune_imports` drops them on the way out.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Decorator, Expr, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::reified_generic::REIFIED_MARKER;
use crate::transforms::source_util::{line_past_end, line_start};

pub(crate) struct ReifiedGenericReverse<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> ReifiedGenericReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    /// whether the decorator is a bare `@`-name whose line carries the
    /// reified-provenance marker comment. the marker lives in trivia (not the
    /// AST), so it is matched against the source slice from the decorator's end
    /// to the end of its physical line
    fn is_marked(&self, decorator: &Decorator, wrapper: &str) -> bool {
        if !matches!(&decorator.expression, Expr::Name(n) if n.id.as_str() == wrapper) {
            return false;
        }
        let after = usize::from(decorator.range().end());
        let rest = &self.source[after..];
        let line = rest.split('\n').next().unwrap_or(rest);
        line.contains(REIFIED_MARKER.trim_start())
    }

    /// delete every marked `wrapper` decorator from `decorators`, reporting
    /// whether one was there
    fn unwrap_decorator(&mut self, decorators: &[Decorator], wrapper: &str) -> bool {
        let mut found = false;
        for decorator in decorators {
            if !self.is_marked(decorator, wrapper) {
                continue;
            }
            // the decorator's own physical line, which is exactly what the
            // forward transform wrote: the `@name`, the marker comment that
            // shares the line, and the newline ending it. anything else between
            // here and the header is the source's own and stays
            let start = line_start(self.source, decorator.range().start());
            let end = line_past_end(self.source, decorator.range().end());
            self.edits
                .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                    start, end,
                ))));
            found = true;
        }
        found
    }

    fn unwrap_function(&mut self, function: &StmtFunctionDef) {
        self.unwrap_decorator(&function.decorator_list, "generic");
    }

    /// a marked class loses the decorator and every type-argument binding its
    /// methods open with — the basedpython source reads the parameter directly
    fn unwrap_class(&mut self, class: &StmtClassDef) {
        if !self.unwrap_decorator(&class.decorator_list, "generic_class") {
            return;
        }
        for stmt in &class.body {
            let Stmt::FunctionDef(method) = stmt else {
                continue;
            };
            // the bindings open the body, after a docstring if there is one
            let docstring = usize::from(matches!(
                method.body.first(),
                Some(Stmt::Expr(e)) if matches!(e.value.as_ref(), Expr::StringLiteral(_))
            ));
            for binding in method
                .body
                .iter()
                .skip(docstring)
                .take_while(|s| is_type_argument_binding(s))
            {
                let start = line_start(self.source, binding.range().start());
                let end = line_past_end(self.source, binding.range().end());
                self.edits
                    .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                        start, end,
                    ))));
            }
        }
    }
}

/// whether the statement is a `T = _type_argument(receiver, "T")` binding the
/// forward transform wrote — the name assigned and the name asked for have to
/// agree, which a hand-written call to the polyfill need not
fn is_type_argument_binding(stmt: &Stmt) -> bool {
    let Stmt::Assign(assign) = stmt else {
        return false;
    };
    let [Expr::Name(target)] = &assign.targets[..] else {
        return false;
    };
    let Expr::Call(call) = assign.value.as_ref() else {
        return false;
    };
    if !matches!(call.func.as_ref(), Expr::Name(n) if n.id.as_str() == "_type_argument") {
        return false;
    }
    matches!(
        &call.arguments.args[..],
        [Expr::Name(_), Expr::StringLiteral(asked)] if asked.value.to_str() == target.id.as_str()
    )
}

impl<'ast> Visitor<'ast> for ReifiedGenericReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => self.unwrap_function(function),
            Stmt::ClassDef(class) => self.unwrap_class(class),
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::transforms::reified_generic::REIFIED_MARKER;
    use crate::{Config, reverse_transpile};
    use ruff_python_ast::PythonVersion;

    fn rev(source: &str) -> String {
        reverse_transpile(source, &Config::test_default()).unwrap()
    }

    #[test]
    fn marked_generic_is_unwrapped() {
        let src = format!("@generic{REIFIED_MARKER}\ndef f[T]():\n    print(T)\n");
        let out = rev(&src);
        assert!(
            !out.contains("@generic"),
            "wrapper should be removed: {out}"
        );
        assert!(out.contains("def f[T]():"), "def should remain: {out}");
    }

    #[test]
    fn marked_async_generic_keeps_async_keyword() {
        let src = format!("@generic{REIFIED_MARKER}\nasync def f[T]():\n    print(T)\n");
        let out = rev(&src);
        assert!(
            !out.contains("@generic"),
            "wrapper should be removed: {out}"
        );
        assert!(
            out.contains("async def f[T]():"),
            "async header must survive the unwrap: {out}"
        );
    }

    /// the python the forward transform writes for `source`, so the reverse
    /// fixtures are the real thing rather than a hand-typed approximation of it
    fn lowered(source: &str) -> String {
        let config = Config {
            min_version: PythonVersion::PY312,
            ..Config::test_default()
        };
        let out = crate::transpile(source, &config).unwrap();
        let class = out
            .find("@generic_class")
            .expect("the class should be wrapped");
        out[class..].to_owned()
    }

    #[test]
    fn marked_generic_class_is_unwrapped() {
        let src = lowered("class A[T]:\n    def f(self):\n        print(T)\n");
        assert!(
            src.contains("        T = _type_argument(self, \"T\")\n"),
            "the fixture should be the lowering it is testing: {src}"
        );
        let out = rev(&src);
        assert!(
            !out.contains("@generic_class"),
            "wrapper should be removed: {out}"
        );
        assert!(out.contains("class A[T]:"), "class should remain: {out}");
        assert!(
            !out.contains("_type_argument"),
            "the binding stands in for reading `T`, so it goes too: {out}"
        );
        assert!(out.contains("print(T)"), "the read should remain: {out}");
    }

    #[test]
    fn a_binding_after_a_docstring_is_unwrapped() {
        let src = lowered("class A[T]:\n    def f(self):\n        \"doc\"\n        print(T)\n");
        let out = rev(&src);
        assert!(
            !out.contains("_type_argument"),
            "the docstring is not the end of the bindings: {out}"
        );
        assert!(out.contains("\"doc\""), "the docstring stays: {out}");
    }

    #[test]
    fn a_comment_naming_the_keyword_is_not_the_header() {
        // the header is the line that *opens* with `class`, not the first line
        // the word appears on
        let src = format!(
            "@generic_class{REIFIED_MARKER}\n{}",
            concat!(
                "# a class that keeps its type argument\n",
                "class A[T]:\n",
                "    def f(self):\n",
                "        T = _type_argument(self, \"T\")\n",
                "        print(T)\n",
            )
        );
        let out = rev(&src);
        assert!(
            out.contains("# a class that keeps its type argument\nclass A[T]:"),
            "the comment and the header must both survive intact: {out}"
        );
    }

    #[test]
    fn class_round_trip_rewraps() {
        let source = "class A[T]:\n    def f(self):\n        print(T)\n";
        let bare = rev(&lowered(source));
        assert!(
            !bare.contains("_type_argument") && !bare.contains("@generic_class"),
            "the reverse should give the source back: {bare}"
        );
        let config = Config {
            min_version: PythonVersion::PY312,
            ..Config::test_default()
        };
        let forward = crate::transpile(&bare, &config).unwrap();
        assert!(
            forward.contains("@generic_class  # basedpython: reified"),
            "forward should re-wrap the bare reified class: {forward}"
        );
        assert!(
            forward.contains("        T = _type_argument(self, \"T\")"),
            "forward should re-bind the read: {forward}"
        );
    }

    #[test]
    fn handwritten_generic_class_is_preserved() {
        // no marker — a user's own `@generic_class` decorator, and a call to the
        // polyfill that is not the binding the forward transform writes
        let src = concat!(
            "@generic_class\n",
            "class A[T]:\n",
            "    def f(self):\n",
            "        U = _type_argument(self, \"T\")\n",
            "        return U\n",
        );
        let out = rev(src);
        assert!(
            out.contains("@generic_class"),
            "hand-written decorator must be preserved: {out}"
        );
        assert!(
            out.contains("_type_argument"),
            "an unmarked class keeps its body: {out}"
        );
    }

    #[test]
    fn handwritten_generic_is_preserved() {
        // no marker — a user's own `@generic` decorator stays put
        let src = "@generic\ndef f(x):\n    return x\n";
        let out = rev(src);
        assert!(
            out.contains("@generic"),
            "hand-written decorator must be preserved: {out}"
        );
    }

    #[test]
    fn round_trip_rewraps() {
        // reverse then forward reproduces the wrapper
        let src = format!("@generic{REIFIED_MARKER}\ndef f[T]():\n    print(T)\n");
        let bare = rev(&src);
        let config = Config {
            min_version: PythonVersion::PY312,
            ..Config::test_default()
        };
        let forward = crate::transpile(&bare, &config).unwrap();
        assert!(
            forward.contains("@generic  # basedpython: reified"),
            "forward should re-wrap the bare reified def: {forward}"
        );
    }
}

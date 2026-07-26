//! reverse of `crate::transforms::implementation`:
//!   `class _by_impl__A__B(_by_Implementation, A):  # basedpython: implementation A for B`
//!   → `implementation A for B:`, and `_by_impl__A__B(b)` → `b`
//!
//! the forward lowering tags the witness class's header line with a
//! `# basedpython: implementation <header>` marker carrying the original header
//! (interface, implemented type, bounds and `as` name). only marked classes
//! re-sugar; a witness-shaped class written by hand is ordinary python.
//!
//! an *anonymous* implementation's witness constructor is a conversion the
//! transpiler inserted, so it unwraps back to its argument. a *named* one stays
//! a call: `BAsA(b)` is what the user wrote, and is valid basedpython either way.
//! a witness class imported from another module also stays, conservatively — the
//! defining module owns its re-sugaring

use std::collections::HashSet;

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtClassDef};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::implementation::{
    IMPLEMENTATION_MARKER, IMPLEMENTATION_RUNTIME_NAME, WITNESS_NAME_PREFIX,
};
use crate::transforms::source_util::line_start;

pub(crate) struct ImplementationReverse<'src> {
    source: &'src str,
    /// witness classes declared *anonymously* in this file, whose constructor
    /// calls are conversions to unwrap
    anonymous_witnesses: HashSet<String>,
    pub(crate) edits: Vec<Fix>,
}

/// the `<header>` payload of a marker comment on `span`'s **first line only**.
///
/// Scanning the whole span would re-sugar any class with the marker text
/// somewhere in a method body — the forward pass always writes it on the header
/// line, so an occurrence anywhere else is ordinary python that must be left alone
fn marker_header(source: &str, span: TextRange) -> Option<&str> {
    let text = &source[usize::from(span.start())..usize::from(span.end())];
    let header_line = text.split('\n').next().unwrap_or(text);
    let after = header_line.split_once(IMPLEMENTATION_MARKER)?.1;
    Some(after.trim())
}

impl<'src> ImplementationReverse<'src> {
    pub(crate) fn new(source: &'src str, suite: &[Stmt]) -> Self {
        let mut reverse = Self {
            source,
            anonymous_witnesses: HashSet::new(),
            edits: Vec::new(),
        };
        reverse.resugar_blocks(suite);
        reverse
    }

    /// re-sugar each marked witness class to its `implementation` block, and drop
    /// the shared runtime base the forward pass injected
    fn resugar_blocks(&mut self, suite: &[Stmt]) {
        for stmt in suite {
            match stmt {
                Stmt::ClassDef(class) if class.name.as_str() == IMPLEMENTATION_RUNTIME_NAME => {
                    // the polyfill is generated, not written: remove it whole
                    let start = line_start(self.source, class.range().start());
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                            start,
                            class.range().end(),
                        ))));
                }
                Stmt::ClassDef(class) => self.resugar_block(class),
                _ => {}
            }
        }
    }

    fn resugar_block(&mut self, class: &StmtClassDef) {
        let Some(header) = marker_header(self.source, class.range()) else {
            return;
        };
        // an anonymous implementation's witness carries the mangled name the
        // forward pass derived; a named one carries the user's `as` name. testing
        // the prefix is exact, where sniffing the header for `" as "` would
        // misread an interface whose type arguments contain that text
        if class.name.starts_with(WITNESS_NAME_PREFIX) {
            self.anonymous_witnesses.insert(class.name.to_string());
        }

        let Some(first_stmt) = class.body.first() else {
            return;
        };
        let header_end = line_start(self.source, first_stmt.range().start());
        let header_span = TextRange::new(class.range().start(), header_end);

        // the emitted body opens with the `__slots__ = ()` line the forward pass
        // added; it is generated, so it goes with the header
        let body_start = match class.body.first() {
            Some(Stmt::Assign(assign))
                if matches!(&assign.targets.as_slice(),
                    [Expr::Name(name)] if name.id.as_str() == "__slots__") =>
            {
                class
                    .body
                    .get(1)
                    .map(|stmt| line_start(self.source, stmt.range().start()))
            }
            _ => None,
        };
        let span = match body_start {
            Some(body_start) => TextRange::new(class.range().start(), body_start),
            None => header_span,
        };
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            format!("implementation {header}:\n"),
            span,
        )));
    }

    /// unwrap a conversion an anonymous implementation's witness constructor
    /// performs: `_by_impl__A__B(b)` → `b`
    fn unwrap_conversion(&mut self, call: &ruff_python_ast::ExprCall) {
        let Expr::Name(callee) = &*call.func else {
            return;
        };
        if !self.anonymous_witnesses.contains(callee.id.as_str()) {
            return;
        }
        let [argument] = call.arguments.args.as_ref() else {
            return;
        };
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            self.source[usize::from(argument.range().start())..usize::from(argument.range().end())]
                .to_owned(),
            call.range(),
        )));
    }
}

impl<'ast> Visitor<'ast> for ImplementationReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.unwrap_conversion(call);
        }
        ruff_python_ast::visitor::walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile};

    fn back(input: &str) -> String {
        reverse_transpile(input, &Config::test_default()).unwrap()
    }

    #[test]
    fn witness_class_resugars_to_a_block() {
        let out = back(
            "class _by_impl__A__B(_by_Implementation, A):  # basedpython: implementation A for B\n\
             \x20   __slots__ = ()\n\
             \x20   def f(self):\n\
             \x20       return self.a\n",
        );
        assert!(out.contains("implementation A for B:"), "got:\n{out}");
        assert!(!out.contains("_by_Implementation"), "got:\n{out}");
        assert!(!out.contains("__slots__"), "got:\n{out}");
        assert!(out.contains("return self.a"), "got:\n{out}");
    }

    #[test]
    fn an_anonymous_witness_call_unwraps() {
        let out = back(
            "class _by_impl__A__B(_by_Implementation, A):  # basedpython: implementation A for B\n\
             \x20   __slots__ = ()\n\
             \x20   def f(self):\n\
             \x20       return self.a\n\
             takes_a(_by_impl__A__B(b))\n",
        );
        assert!(out.contains("takes_a(b)"), "got:\n{out}");
    }

    #[test]
    fn a_named_witness_call_is_kept() {
        // `BAsA(b)` is what the user wrote; an explicit witness call is valid
        // basedpython, so unwrapping it would change the source's meaning
        let out = back(
            "class BAsA(_by_Implementation, A):  # basedpython: implementation A for B as BAsA\n\
             \x20   __slots__ = ()\n\
             \x20   def f(self):\n\
             \x20       return self.a\n\
             takes_a(BAsA(b))\n",
        );
        assert!(
            out.contains("implementation A for B as BAsA:"),
            "got:\n{out}"
        );
        assert!(out.contains("takes_a(BAsA(b))"), "got:\n{out}");
    }

    #[test]
    fn the_runtime_base_is_dropped() {
        let out = back(&format!(
            "{}\nx = 1\n",
            crate::transforms::implementation::IMPLEMENTATION_RUNTIME
        ));
        assert!(!out.contains("class _by_Implementation"), "got:\n{out}");
        assert!(out.contains("x = 1"), "got:\n{out}");
    }

    /// the marker is written on the header line; an occurrence anywhere else is
    /// ordinary python (a comment in a method body) and must not re-sugar the class
    #[test]
    fn a_marker_inside_a_method_body_is_ignored() {
        let source = "class Unrelated:\n    def helper(self):\n        # basedpython: implementation Fake for Bogus\n        return 1\n";
        assert_eq!(back(source), source);
    }

    #[test]
    fn an_unmarked_witness_shaped_class_is_left_alone() {
        let source =
            "class _by_impl__A__B(_by_Implementation, A):\n    def f(self):\n        return 1\n";
        assert_eq!(back(source), source);
    }
}

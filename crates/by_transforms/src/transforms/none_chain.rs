use std::fmt::Write as _;

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::Ranged;

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// rewrites `a?.b` to `None if a is None else a.b`
/// and chains like `a?.b?.c` to `None if a is None else None if (_t := a.b) is None else _t.c`
pub(crate) struct NoneChain<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> NoneChain<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }
}

fn pick_temp_var(types: &dyn TypeInfo, anchor: &Expr) -> &'static str {
    for name in [
        "_t", "_t0", "_t1", "_t2", "_t3", "_t4", "_t5", "_t6", "_t7", "_t8", "_t9",
    ] {
        if types.is_unbound_at(name, anchor) {
            return name;
        }
    }
    "_t9"
}

/// walks an attribute-access chain and returns `Some((python_form, guards))` when
/// any `?.` is present, where `python_form` has all `?.` replaced by `.` and
/// `guards` is the ordered list of accumulated sub-expressions that must be
/// non-None before each subsequent optional access is safe.
///
/// a `?.` whose base is a *wrapped* optional (`int??`, a generic `T?`)
/// reaches its present value through the runtime wrapper: the guard still
/// tests the wrapper against `None`, but the access reads `.value` first
pub(super) fn expand_chain(
    expr: &Expr,
    source: &str,
    types: &dyn TypeInfo,
) -> Option<(String, Vec<String>)> {
    let Expr::Attribute(attr) = expr else {
        return None;
    };
    let field = attr.attr.as_str();
    let unwrap = if attr.optional && types.wrapped_optional(&attr.value) {
        ".value"
    } else {
        ""
    };
    match expand_chain(&attr.value, source, types) {
        Some((v_form, mut guards)) => {
            if attr.optional {
                guards.push(v_form.clone());
            }
            Some((format!("{v_form}{unwrap}.{field}"), guards))
        }
        None => {
            if !attr.optional {
                return None;
            }
            let start = usize::from(attr.value.range().start());
            let end = usize::from(attr.value.range().end());
            let v_form = source[start..end].to_owned();
            Some((format!("{v_form}{unwrap}.{field}"), vec![v_form]))
        }
    }
}

/// builds a `None if ... is None else ...` chain from guards and final result,
/// using walrus assignment to avoid evaluating compound intermediate expressions twice
pub(super) fn build_expansion(guards: &[String], result: &str, temp: &str) -> String {
    let mut s = String::new();
    let mut use_t = false;
    let mut prev_guard: Option<&str> = None;

    for guard in guards {
        let guard_expr = if let Some(prev) = prev_guard.filter(|_| use_t) {
            let incremental = &guard[prev.len() + 1..];
            format!("{temp}.{incremental}")
        } else {
            guard.clone()
        };

        if guard_expr.chars().all(|c| c.is_alphanumeric() || c == '_') {
            let _ = write!(s, "None if {guard_expr} is None else ");
        } else {
            let _ = write!(s, "None if ({temp} := {guard_expr}) is None else ");
            use_t = true;
        }
        prev_guard = Some(guard.as_str());
    }

    if let Some(last) = prev_guard.filter(|_| use_t) {
        let incremental = &result[last.len() + 1..];
        let _ = write!(s, "{temp}.{incremental}");
    } else {
        s.push_str(result);
    }

    s
}

pub(crate) struct NoneChainPass<'src> {
    source: &'src str,
}

impl<'src> NoneChainPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for NoneChainPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = NoneChain::new(self.source, types);
        for stmt in stmts {
            inner.visit_stmt(stmt);
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

impl<'ast> Visitor<'ast> for NoneChain<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Attribute(_) = expr {
            if let Some((form, guards)) = expand_chain(expr, self.source, self.types) {
                let temp = pick_temp_var(self.types, expr);
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    build_expansion(&guards, &form, temp),
                    expr.range(),
                )));
                return;
            }
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn wrapped_base_unwraps_value() {
        // `?.` on a wrapped optional reads the present value through the
        // runtime wrapper: the guard tests the wrapper, the access goes
        // through `.value`
        let out = transpile(
            "def g() -> int??:\n    return Some(5)\nw = g()\nx = w?.bit_length\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("x = None if w is None else w.value.bit_length\n"),
            "got: {out}"
        );
    }

    #[test]
    fn basic_chain() {
        check("x = a?.b\n", "x = None if a is None else a.b\n");
    }

    #[test]
    fn double_chain() {
        check(
            "x = a?.a?.b\n",
            "x = None if a is None else None if (_t := a.a) is None else _t.b\n",
        );
    }

    #[test]
    fn double_chain_t_taken() {
        check(
            "_t = 1\nx = a?.a?.b\n",
            "_t = 1\nx = None if a is None else None if (_t0 := a.a) is None else _t0.b\n",
        );
    }

    #[test]
    fn triple_chain() {
        check(
            "x = a?.b?.c?.d\n",
            "x = None if a is None else None if (_t := a.b) is None else None if (_t := _t.c) is None else _t.d\n",
        );
    }

    #[test]
    fn mixed_chain() {
        check("x = a?.b.c\n", "x = None if a is None else a.b.c\n");
    }

    #[test]
    fn optional_after_plain_attr() {
        check(
            "x = a.b?.c\n",
            "x = None if (_t := a.b) is None else _t.c\n",
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged("x = None if a is None else a.b\n");
    }
}

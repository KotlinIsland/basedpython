//! Lowers keyword arguments in subscriptions to a `__getitem__` call.
//!
//! `x[a, z=1]` → `x.__getitem__(a, z=1)`
//!
//! Python's subscript grammar doesn't accept keyword args (PEP 637 was
//! rejected), so basedpython's surface syntax falls back to the explicit
//! method call. positional and keyword args are forwarded in source order

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

pub(crate) struct KwSubscript<'src, T: TypeInfo + ?Sized> {
    source: &'src str,
    types: Option<&'src T>,
    pub(crate) edits: Vec<Fix>,
}

impl<'src, T: TypeInfo + ?Sized> KwSubscript<'src, T> {
    pub(crate) fn new(source: &'src str, types: Option<&'src T>) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// render a subscript argument value, lowering any postfix `?` it carries
    /// (`M[V=int?]`). our whole-subscript replacement subsumes `optional_type`'s
    /// narrow edits, so we lower the optional here; the runtime `Optional[...]`
    /// import for nested `??` is still raised by `OptionalTypePass`, which walks
    /// every expression independently
    fn value_src(&self, expr: &Expr) -> String {
        crate::transforms::optional_type::rewrite_type_expr(self.source, expr)
            .unwrap_or_else(|| self.src(expr.range()).to_owned())
    }

    /// Lower a subscript of a class declaring a keyword-variadic pack.
    ///
    /// `class A[**Kwargs]` is a `ParamSpec` at runtime, and python has no keyword subscript, so
    /// the pack's fields lower to the `ParamSpec` list form — `A[foo=int, bar=str]` → `A[[int,
    /// str]]`, `A[()]` → `A[[]]`. field names are erased, which matches python's own erasure of
    /// type arguments; the names are checked against the `.by` source, not this output.
    ///
    /// Returns whether the subscript was rewritten.
    fn rewrite_keyword_pack_subscript(
        &mut self,
        sub: &ruff_python_ast::ExprSubscript,
        pack_index: usize,
    ) -> bool {
        let elements: Vec<&Expr> = match sub.slice.as_ref() {
            Expr::Tuple(t) => t.elts.iter().collect(),
            single => vec![single],
        };

        let mut fields: Vec<String> = Vec::new();
        let mut positional: Vec<String> = Vec::new();
        for element in elements {
            if let Expr::Named(n) = element
                && let Expr::Name(target) = n.target.as_ref()
                && matches!(target.ctx, ruff_python_ast::ExprContext::Invalid)
            {
                fields.push(self.value_src(n.value.as_ref()));
            } else {
                positional.push(self.value_src(element));
            }
        }

        // an all-positional subscript with no pack slot to fill is already valid python
        if fields.is_empty() && positional.len() > pack_index {
            return false;
        }

        let mut parts = positional;
        let pack = format!("[{}]", fields.join(", "));
        if pack_index <= parts.len() {
            parts.insert(pack_index, pack);
        } else {
            parts.push(pack);
        }

        let value_src = self.src(sub.value.range()).to_owned();
        let replacement = format!("{value_src}[{}]", parts.join(", "));
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            replacement,
            sub.range(),
        )));
        true
    }

    /// whether the subscript carries a keyword field (`x[a=1]`) — the parser
    /// spells one as a named expression whose target is [`ExprContext::Invalid`]
    fn is_keyword_field(element: &Expr) -> bool {
        matches!(element, Expr::Named(n)
            if matches!(n.target.as_ref(), Expr::Name(t)
                if matches!(t.ctx, ruff_python_ast::ExprContext::Invalid)))
    }

    fn subscript_elements(sub: &ruff_python_ast::ExprSubscript) -> Vec<&Expr> {
        match sub.slice.as_ref() {
            Expr::Tuple(t) if !t.parenthesized => t.elts.iter().collect(),
            single => vec![single],
        }
    }

    /// the subscript's arguments, keyword fields rendered as `name=value`
    fn subscript_parts(&self, sub: &ruff_python_ast::ExprSubscript) -> Vec<String> {
        Self::subscript_elements(sub)
            .into_iter()
            .map(|element| {
                if let Expr::Named(n) = element
                    && let Expr::Name(target) = n.target.as_ref()
                    && matches!(target.ctx, ruff_python_ast::ExprContext::Invalid)
                {
                    return format!(
                        "{}={}",
                        target.id.as_str(),
                        self.value_src(n.value.as_ref())
                    );
                }
                self.value_src(element)
            })
            .collect()
    }

    fn rewrite_subscript(&mut self, sub: &ruff_python_ast::ExprSubscript) {
        // subscripting a function is a *type* specialization, not a runtime
        // keyword subscript: an erased generic has its whole `[…]` stripped by
        // `generic_call`, and a reified one routes the fields through the
        // `generic` wrapper's `__getitem__`, which takes them as keywords
        if let Some(types) = self.types
            && let Expr::Name(name) = sub.value.as_ref()
            && types.is_function(name)
        {
            if types.is_reified_function(name)
                && Self::subscript_elements(sub)
                    .iter()
                    .any(|element| Self::is_keyword_field(element))
            {
                let value_src = self.src(sub.value.range()).to_owned();
                let replacement = format!(
                    "{value_src}.__getitem__({})",
                    self.subscript_parts(sub).join(", ")
                );
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    replacement,
                    sub.range(),
                )));
            }
            return;
        }
        if let Some(types) = self.types
            && let Some(pack_index) = types.class_keyword_pack_index(&sub.value)
            && self.rewrite_keyword_pack_subscript(sub, pack_index)
        {
            return;
        }
        // single keyword arg, e.g. `A[T=int]` (no surrounding tuple).
        // for a multi-typevar class with declared defaults, expand to a
        // positional list filling unbound slots with their declared defaults
        // (`A[R=int]` with `class A[T=int, R=str]` → `A[int, int]`).
        // single-typevar class falls back to dropping the kw name
        if let Expr::Named(n) = sub.slice.as_ref()
            && let Expr::Name(target) = n.target.as_ref()
            && matches!(target.ctx, ruff_python_ast::ExprContext::Invalid)
        {
            if let Some(types) = self.types
                && let Some(typevars) = types.class_typevars(&sub.value)
                && typevars.len() > 1
            {
                let value_src = self.value_src(n.value.as_ref());
                let mut parts: Vec<String> = Vec::with_capacity(typevars.len());
                for (tv_name, tv_default) in &typevars {
                    if tv_name == target.id.as_str() {
                        parts.push(value_src.clone());
                    } else if let Some(default) = tv_default {
                        parts.push(default.clone());
                    } else {
                        // typevar has no default and no kw arg — fall back
                        // to drop-name behavior; ty's diagnostics will catch
                        // the missing-arg case
                        let value_src = self.value_src(n.value.as_ref());
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            value_src,
                            n.range(),
                        )));
                        return;
                    }
                }
                let value_src = self.src(sub.value.range()).to_owned();
                let replacement = format!("{value_src}[{}]", parts.join(", "));
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    replacement,
                    sub.range(),
                )));
                return;
            }
            let value_src = self.value_src(n.value.as_ref());
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                value_src,
                n.range(),
            )));
            return;
        }
        let Expr::Tuple(t) = sub.slice.as_ref() else {
            return;
        };
        if t.parenthesized {
            return;
        }
        if !t.elts.iter().any(Self::is_keyword_field) {
            return;
        }
        // when the value is a known generic class, reorder by typevar declaration and emit a
        // positional subscript. positional arguments fill the leading typevars in source order,
        // keyword arguments bind by name, and any remaining typevar falls back to its declared
        // default (`A[R=str, T=int]` → `A[int, str]`; `C[int, D=bytes]` on `class C[A, B, D]` with
        // `B`'s default → `C[int, <B default>, bytes]`)
        if let Some(types) = self.types
            && let Some(typevars) = types.class_typevars(&sub.value)
        {
            let mut by_name: std::collections::HashMap<&str, &Expr> =
                std::collections::HashMap::new();
            let mut positional: Vec<&Expr> = Vec::new();
            for elt in &t.elts {
                if let Expr::Named(n) = elt
                    && let Expr::Name(target) = n.target.as_ref()
                    && matches!(target.ctx, ruff_python_ast::ExprContext::Invalid)
                {
                    by_name.insert(target.id.as_str(), n.value.as_ref());
                } else {
                    positional.push(elt);
                }
            }
            let mut positional_iter = positional.iter();
            let mut parts: Vec<String> = Vec::with_capacity(typevars.len());
            let mut filled_all = true;
            for (tv_name, tv_default) in &typevars {
                if let Some(value_expr) = by_name.get(tv_name.as_str()) {
                    parts.push(self.value_src(value_expr));
                } else if let Some(value_expr) = positional_iter.next() {
                    parts.push(self.value_src(value_expr));
                } else if let Some(default) = tv_default {
                    parts.push(default.clone());
                } else {
                    filled_all = false;
                    break;
                }
            }
            // a leftover positional argument means the source over-specified the class; leave it
            // for the generic form below so ty's diagnostic is the authority on the arity
            if filled_all && positional_iter.next().is_none() {
                let value_src = self.src(sub.value.range()).to_owned();
                let replacement = format!("{value_src}[{}]", parts.join(", "));
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    replacement,
                    sub.range(),
                )));
                return;
            }
            // missing typevar without default — fall through to the
            // generic `value.__getitem__(name=value, ...)` form so the
            // output is at least syntactically valid Python
        }
        // Build `value.__getitem__(<args>)` where each Named field renders
        // as `name=value` and bare exprs render verbatim
        let value_src = self.src(sub.value.range()).to_owned();
        let replacement = format!(
            "{value_src}.__getitem__({})",
            self.subscript_parts(sub).join(", ")
        );
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            replacement,
            sub.range(),
        )));
    }
}

pub(crate) struct KwSubscriptPass<'src> {
    source: &'src str,
}

impl<'src> KwSubscriptPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl TypeAwarePass for KwSubscriptPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner: KwSubscript<'_, dyn TypeInfo> = KwSubscript::new(self.source, Some(types));
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

impl<'ast, T: TypeInfo + ?Sized> Visitor<'ast> for KwSubscript<'_, T> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Subscript(s) = expr {
            self.rewrite_subscript(s);
        }
        walk_expr(self, expr);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
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

    // multi-arg kw reorder lives under transpile_typed (needs ty type info).
    // exercised via mdtest `basedpython_kw_subscript`

    #[test]
    fn simple_kwarg() {
        check("x[a, z=1]\n", "x.__getitem__(a, z=1)\n");
    }

    #[test]
    fn multiple_kwargs() {
        check(
            "x[a, b, key=1, val=\"v\"]\n",
            "x.__getitem__(a, b, key=1, val=\"v\")\n",
        );
    }

    #[test]
    fn no_kwargs_unchanged() {
        check("x[a, b]\n", "x[a, b]\n");
    }

    #[test]
    fn single_kw_drops_name() {
        check("a: A[T=int]\n", "a: A[int]\n");
    }

    /// a `?` on a kw-subscript value lowers instead of leaking the bare token
    /// (our whole-subscript edit would otherwise subsume `optional_type`'s)
    #[test]
    fn single_kw_value_optional_lowers() {
        check("a: A[T=int?]\n", "a: A[int | None]\n");
    }

    #[test]
    fn getitem_kw_value_optional_lowers() {
        check(
            "d = data[idx, mode=int?]\n",
            "d = data.__getitem__(idx, mode=int | None)\n",
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged("x[a, b]\n");
    }

    /// a keyword-variadic pack is a `ParamSpec` at runtime, so its fields lower to the
    /// `ParamSpec` list form. needs type info to know the class declares a pack
    mod keyword_pack {
        use ruff_db::files::system_path_to_file;
        use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
        use ty_project::{ProjectMetadata, TestDb};

        use crate::{Config, transpile_typed};

        fn transpiled(source: &str) -> String {
            let mut db = TestDb::new(ProjectMetadata::new(
                ruff_python_ast::name::Name::new_static(""),
                SystemPathBuf::from("/proj"),
            ));
            db.write_file("/proj/main.by", source)
                .expect("write file failed");
            db.init_program().expect("program init failed");
            let file = system_path_to_file(&db, "/proj/main.by").expect("file not in db");
            transpile_typed(&db, file, &Config::test_default()).expect("transpile failed")
        }

        #[test]
        fn fields_lower_to_a_parameter_list() {
            let output =
                transpiled("class A[**Kwargs]: ...\n\ndef f(a: A[foo=int, bar=str]): ...\n");
            assert!(
                output.contains("def f(a: A[[int, str]]): ..."),
                "unexpected output:\n{output}"
            );
        }

        #[test]
        fn empty_pack_lowers_to_an_empty_list() {
            let output = transpiled("class A[**Kwargs]: ...\n\ndef f(a: A[()]): ...\n");
            assert!(
                output.contains("def f(a: A[[]]): ..."),
                "unexpected output:\n{output}"
            );
        }

        #[test]
        fn positional_type_arguments_keep_their_slots() {
            let output =
                transpiled("class Two[T, **Kwargs]: ...\n\ndef f(t: Two[bytes, foo=int]): ...\n");
            assert!(
                output.contains("def f(t: Two[bytes, [int]]): ..."),
                "unexpected output:\n{output}"
            );
        }

        #[test]
        fn mixed_positional_and_keyword_reorder_to_positional() {
            // a positional argument followed by keyword ones must resolve to a positional
            // subscript, not a `__getitem__` call that crashes at runtime
            let output =
                transpiled("class C[A, B, D]: ...\n\ndef f(c: C[int, B=str, D=bytes]): ...\n");
            assert!(
                output.contains("def f(c: C[int, str, bytes]): ..."),
                "unexpected output:\n{output}"
            );
            assert!(
                !output.contains("__getitem__"),
                "unexpected output:\n{output}"
            );
        }

        #[test]
        fn positional_fills_leading_slots_with_default_gap() {
            let output = transpiled(
                "class C[A, B = str, D = bytes]: ...\n\ndef f(c: C[int, D=complex]): ...\n",
            );
            assert!(
                output.contains("def f(c: C[int, str, complex]): ..."),
                "unexpected output:\n{output}"
            );
        }
    }
}

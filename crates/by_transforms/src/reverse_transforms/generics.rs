//! partial reverse of `crate::transforms::generics`:
//!   `Alias: TypeAlias = T` → `type Alias = T`
//!
//! only handles the `TypeAlias` annotation form.
//! TypeVar-defs + Generic[...] → PEP 695 type-param syntax is not reversed:
//! detecting which `TypeVar` groups belong to which class/function is too heuristic
//! for a conservative reverse pass

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::helpers::TOP_PARAMETERS_FORM;
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt, TypeParam};
use ruff_text_size::{Ranged, TextRange};

pub(crate) struct GenericsReverse<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> GenericsReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// `[**P]` → `[P: (*: *, **: *)]`.
    ///
    /// In a `.by` file `[**P]` declares a keyword-variadic pack, so a python `ParamSpec` reverses
    /// to a type variable bound by the *top parameters* form — an anonymous variadic and an
    /// anonymous keyword-variadic, both admitting anything. Every parameter list is a subtype of
    /// it, so the bound ranges over all parameter lists, which is exactly a `ParamSpec`
    fn rewrite_paramspec(&mut self, params: &[TypeParam]) {
        for param in params {
            if let TypeParam::ParamSpec(ps) = param {
                let name = ps.name.id.as_str();
                let default = ps
                    .default
                    .as_deref()
                    .map(|default| format!(" = {}", self.src(default.range())))
                    .unwrap_or_default();
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    format!("{name}: {TOP_PARAMETERS_FORM}{default}"),
                    param.range(),
                )));
            }
        }
    }
}

impl<'ast> Visitor<'ast> for GenericsReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::AnnAssign(a) = stmt {
            // `Alias: TypeAlias = T` → `type Alias = T`
            if let Expr::Name(ann) = a.annotation.as_ref()
                && ann.id.as_str() == "TypeAlias"
                && let Some(value) = &a.value
                && let Expr::Name(target) = a.target.as_ref()
            {
                let name = target.id.as_str();
                let value_src = self.src(value.range());
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    format!("type {name} = {value_src}"),
                    stmt.range(),
                )));
                return;
            }
        }
        let type_params = match stmt {
            Stmt::ClassDef(c) => c.type_params.as_deref(),
            Stmt::FunctionDef(f) => f.type_params.as_deref(),
            Stmt::TypeAlias(a) => a.type_params.as_deref(),
            _ => None,
        };
        if let Some(tp) = type_params {
            self.rewrite_paramspec(&tp.type_params);
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    #[test]
    fn type_alias() {
        check(
            indoc! {"
                from typing import TypeAlias
                Alias: TypeAlias = int
            "},
            indoc! {"
                from typing import TypeAlias
                type Alias = int
            "},
        );
    }

    #[test]
    fn type_alias_complex() {
        check(
            indoc! {"
                from typing import TypeAlias
                Vector: TypeAlias = list[float]
            "},
            indoc! {"
                from typing import TypeAlias
                type Vector = list[float]
            "},
        );
    }

    #[test]
    fn non_typealias_unchanged() {
        check("x: int = 5\n", "x: int = 5\n");
    }

    #[test]
    fn paramspec_class_reversed() {
        check(
            "class A[**P]: ...\n",
            // empty_class also strips `: ...`
            "class A[P: (*: *, **: *)]\n",
        );
    }

    /// a `ParamSpec` default rides along on the reversed bound
    #[test]
    fn paramspec_default_reversed() {
        check(
            "class A[**P = ...]: ...\n",
            "class A[P: (*: *, **: *) = ...]\n",
        );
    }

    /// the reversed form is what the forward transform reads back as a `ParamSpec`, so the pair
    /// round-trips rather than drifting
    #[test]
    fn paramspec_round_trips_through_the_forward_transform() {
        use crate::transpile;
        let reversed = reverse_transpile("class A[**P]: ...\n", &Config::test_default())
            .expect("reverse failed");
        assert_eq!(reversed, "class A[P: (*: *, **: *)]\n");
        let forward = transpile(&reversed, &Config::test_default()).expect("forward failed");
        assert!(
            forward.contains("_P = ParamSpec(\"_P\")") && forward.contains("Generic[_P]"),
            "expected a ParamSpec, got:\n{forward}"
        );
    }

    #[test]
    fn paramspec_function_reversed() {
        check(
            indoc! {"
                def f[**P](x: int) -> int:
                    return x
            "},
            indoc! {"
                def f[P: (*: *, **: *)](x: int) -> int:
                    return x
            "},
        );
    }

    #[test]
    fn mixed_typevar_and_paramspec_reversed() {
        check("class A[T, **P]: ...\n", "class A[T, P: (*: *, **: *)]\n");
    }
}

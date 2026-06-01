//! Abstraction over type/binding information consumed by transforms.

use ruff_python_ast::{Expr, ExprName};
use ty_python_core::{global_scope, place_table, semantic_index};
use ty_python_semantic::types::{DynamicType, KnownClass, Type};
use ty_python_semantic::{HasType, SemanticModel};

/// How the postfix `^` / `!` operators test the "absent" arm of an operand's
/// wrapped type. `T?` lowers to `T | None`, so its absent arm is `None`; a
/// result-like `T | E` (e.g. `int | TypeError`) signals absence with an error
/// value, so the guard tests against `BaseException` instead
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AbsentTest {
    /// optional form — guard tests `x is None` and returns the `None`
    IsNone,
    /// result form — guard tests `isinstance(x, BaseException)` and returns
    /// the error value
    IsException,
}

pub(crate) trait TypeInfo {
    /// whether `X[…]` where `X` is `name` treats the slice as type arguments.
    /// returns `true` for unresolved / unknown names (covers builtins like
    /// `list`, unimported sugar like `Union`)
    fn subscript_is_type_context(&self, name: &ExprName) -> bool;

    /// stricter variant: only `true` when ty *resolved* `name` to a class /
    /// generic / special form. unresolved names return `false`. used by
    /// transforms that may fire on value-position subscripts (where an
    /// unresolved name should be treated as a runtime subscript, not a type)
    fn subscript_is_known_type_context(&self, name: &ExprName) -> bool;

    /// whether `base.attr[…]` (base = a module or class) treats slice as type args
    fn attr_base_is_type_context(&self, base: &ExprName) -> bool;

    fn is_function(&self, name: &ExprName) -> bool;

    /// whether `expr` resolves to `typing.Any` (the explicitly-annotated
    /// dynamic type). distinguishes the special form from a shadowing binding
    /// or the `Unknown` that an unresolved / invalid type expression yields,
    /// both of which are also dynamic types
    fn is_any(&self, expr: &Expr) -> bool;

    /// whether `name` is unbound at the scope enclosing `anchor`
    /// (used to pick a fresh temp-variable name)
    fn is_unbound_at(&self, name: &str, anchor: &Expr) -> bool;

    /// whether `name` is bound at module level (used to avoid duplicate imports)
    fn is_bound_globally(&self, name: &str) -> bool;

    /// rendered inferred (literal-promoted) type of `expr`, or `None` when ty
    /// cannot resolve a type (unresolved import, parse error, etc.).
    /// example: a literal `20` inferred as `Literal[20]` is promoted to
    /// `"int"` here so two value-forms with structurally compatible fields
    /// hash to the same class shape.
    fn promoted_type_display(&self, expr: &Expr) -> Option<String>;

    /// rendered exact (non-promoted) type of `expr` in a type position. used to
    /// fold symbolic operations such as `1 + 1` → `Literal[2]` or `A + B` →
    /// `Literal[3]`: ty already evaluates these in `infer_type_expression`, so
    /// this just reads the resolved type back as source text. unlike
    /// [`promoted_type_display`](TypeInfo::promoted_type_display) literals are
    /// kept precise. returns `None` when ty resolves no concrete type — e.g. an
    /// unsupported operation inferred as `Unknown` — so the caller leaves the
    /// source unchanged and ty's own diagnostic stands
    fn symbolic_type_fold(&self, expr: &Expr) -> Option<String>;

    /// names + rendered default types of the type parameters of the class
    /// referenced by `expr`. element is `(name, Some(default))` if the
    /// typevar has a declared default, `(name, None)` otherwise. returns
    /// `None` if `expr` is not a generic class
    fn class_typevars(&self, expr: &Expr) -> Option<Vec<(String, Option<String>)>>;

    /// whether the first type parameter of the class referenced by `expr`
    /// is a `ParamSpec` (e.g. `class A[**P]` or `class A[P: Parameters]`).
    /// returns `false` when `expr` is not a generic class
    fn class_first_typevar_is_paramspec(&self, expr: &Expr) -> bool;

    /// classify the "absent" arm of `expr`'s type for `^` / `!` propagation.
    /// returns [`AbsentTest::IsException`] when any arm of the (possibly union)
    /// type is a `BaseException` subtype — a result-like `T | E` — else
    /// [`AbsentTest::IsNone`] when the type admits `None`. `None` when ty
    /// resolves no type, or the type is neither optional nor result-like
    fn propagate_absent_test(&self, expr: &Expr) -> Option<AbsentTest>;
}

impl TypeInfo for SemanticModel<'_> {
    fn subscript_is_type_context(&self, name: &ExprName) -> bool {
        match name.inferred_type(self) {
            Some(ty) => ty.is_subscript_type_context(),
            // unresolved → assume type context (covers builtins like `list`,
            // unknown imports, basedpython sugar contexts)
            None => true,
        }
    }

    fn subscript_is_known_type_context(&self, name: &ExprName) -> bool {
        match name.inferred_type(self) {
            Some(ty) => ty.is_subscript_type_context() && !ty.is_dynamic(),
            None => false,
        }
    }

    fn attr_base_is_type_context(&self, base: &ExprName) -> bool {
        match base.inferred_type(self) {
            Some(ty) => ty.is_module_or_type(),
            None => true,
        }
    }

    fn is_function(&self, name: &ExprName) -> bool {
        name.inferred_type(self)
            .is_some_and(|ty| ty.as_function_literal().is_some())
    }

    fn is_any(&self, expr: &Expr) -> bool {
        expr.inferred_type(self)
            .is_some_and(|ty| matches!(ty, Type::Dynamic(DynamicType::Any)))
    }

    fn is_unbound_at(&self, name: &str, anchor: &Expr) -> bool {
        let db = self.db();
        let file = self.file();
        let index = semantic_index(db, file);
        let Some(scope_id) = index.try_expression_scope_id(anchor) else {
            return true;
        };
        for (ancestor_id, _) in index.ancestor_scopes(scope_id) {
            let scope = ancestor_id.to_scope_id(db, file);
            let table = place_table(db, scope);
            if table
                .symbol_by_name(name)
                .is_some_and(ty_python_core::symbol::Symbol::is_bound)
            {
                return false;
            }
        }
        true
    }

    fn is_bound_globally(&self, name: &str) -> bool {
        let global = global_scope(self.db(), self.file());
        let table = place_table(self.db(), global);
        table
            .symbol_by_name(name)
            .is_some_and(ty_python_core::symbol::Symbol::is_bound)
    }

    fn promoted_type_display(&self, expr: &Expr) -> Option<String> {
        let ty = expr.inferred_type(self)?;
        let promoted = ty.promote(self.db());
        let rendered = promoted.display(self.db()).to_string();
        // ty's default display tags type variables with their binding scope
        // for disambiguation (e.g. `T@render`); that suffix is not valid in
        // emitted Python source. strip it before returning so the rendered
        // type is a syntactically valid type expression
        Some(strip_binding_context_suffix(&rendered))
    }

    fn symbolic_type_fold(&self, expr: &Expr) -> Option<String> {
        let ty = expr.inferred_type(self)?;
        // fold concrete types and explicit `Any` (e.g. `dynamic + 1`, which ty
        // resolves to `Any`), but not the `Unknown` / Todo dynamics an
        // *unsupported* operation (`A + B` between two classes) resolves to —
        // leave those untouched so ty's own diagnostic stands
        if ty.is_dynamic() && !matches!(ty, Type::Dynamic(DynamicType::Any)) {
            return None;
        }
        // display with the standard (non-basedpython) renderer so literals come
        // out as `Literal[..]` rather than bare — the transpiler emits python
        Some(strip_binding_context_suffix(
            &ty.display(self.db()).to_string(),
        ))
    }

    fn class_typevars(&self, expr: &Expr) -> Option<Vec<(String, Option<String>)>> {
        let ty = expr.inferred_type(self)?;
        let class = ty.as_class_literal()?;
        let ctx = class.generic_context(self.db())?;
        Some(
            ctx.variables(self.db())
                .map(|tv| {
                    let name = tv.name(self.db()).to_string();
                    let default = tv
                        .default_type(self.db())
                        .map(|d| d.display(self.db()).to_string());
                    (name, default)
                })
                .collect(),
        )
    }

    fn class_first_typevar_is_paramspec(&self, expr: &Expr) -> bool {
        let Some(ty) = expr.inferred_type(self) else {
            return false;
        };
        let Some(class) = ty.as_class_literal() else {
            return false;
        };
        let Some(ctx) = class.generic_context(self.db()) else {
            return false;
        };
        ctx.variables(self.db())
            .next()
            .is_some_and(|tv| tv.is_paramspec(self.db()))
    }

    fn propagate_absent_test(&self, expr: &Expr) -> Option<AbsentTest> {
        let ty = expr.inferred_type(self)?;
        let db = self.db();
        let base_exception = KnownClass::BaseException.to_instance(db);
        let elements: Vec<Type> = match ty {
            Type::Union(union) => union.elements(db).to_vec(),
            other => vec![other],
        };
        // an exception arm wins over a `None` arm: a `T | E` (or a decomposed
        // `(T ? E)?` carrying both) propagates the error. `Any`/`Unknown` arms
        // are assignable to anything, so exclude them from the exception probe
        if elements
            .iter()
            .any(|t| !t.is_dynamic() && t.is_assignable_to(db, base_exception))
        {
            Some(AbsentTest::IsException)
        } else if elements.iter().any(|t| t.is_none(db)) {
            Some(AbsentTest::IsNone)
        } else {
            None
        }
    }
}

/// Strip ty's `@<scope>` binding-context suffix from type variable display
/// (e.g. `T@render` → `T`, `dict[str, T@render]` → `dict[str, T]`). Used
/// when feeding ty's display string back into emitted Python source where
/// the suffix would be invalid syntax
fn strip_binding_context_suffix(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            // skip `@` and any following identifier chars
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

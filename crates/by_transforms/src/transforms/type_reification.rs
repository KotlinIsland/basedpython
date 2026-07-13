//! type reification (basedpython)
//!
//! at runtime, `A(1)` constructs an `A` whose specialization is invisible —
//! nothing records that this was an `A[int]`. the transpiler makes every
//! inferred specialization explicit in the generated python:
//!
//! ```by
//! class A[T]:
//!     def __init__(self, t: T): ...
//!
//! a = A(1)
//! xs = [1, 2]
//! ```
//!
//! →
//!
//! ```python
//! a = A[int](1)
//! xs = list[int]([1, 2])
//! ```
//!
//! `A[int](…)` routes through `types.GenericAlias.__call__`, which stamps
//! `__orig_class__` on the constructed instance, so the specialization is
//! observable at runtime. collection literals (list / set / dict / tuple
//! displays) are wrapped in the equivalent explicit constructor for the same
//! reason — the builtins silently reject the `__orig_class__` stamp, but the
//! constructed value is identical and the generated source records the
//! element types explicitly. a tuple display always gets its own inner
//! parentheses so it stays a single constructor argument
//!
//! reification is best-effort: it fires only when ty solved the
//! specialization to types with a runtime spelling (see ty's `reified_infer`
//! module) — dynamic, unsolved or scope-local arguments leave the call or
//! literal as written. type positions never reify (annotations, type
//! parameter lists, `type X = …` values, type-context subscript slices such
//! as legacy `Callable[[int], str]` parameter lists), and dunders that static
//! readers require to stay literal displays (`__all__`, `__slots__`,
//! `__match_args__`) are skipped. pep 585 makes the builtins subscriptable
//! at runtime in 3.9, so the pass is inert below that target, and stubs have
//! no runtime to observe, so stub sources are left alone

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, PythonVersion, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// dunder assignments whose values static readers (linters, type checkers,
/// dataclass machinery) require to stay literal displays
fn is_static_literal_dunder(target: &Expr) -> bool {
    matches!(
        target,
        Expr::Name(name) if matches!(name.id.as_str(), "__all__" | "__slots__" | "__match_args__")
    )
}

/// `sys.version_info` (or a slice of it) — comparisons against it are a
/// structural idiom every static reader (including ty on the generated
/// python) must see with a literal tuple operand
fn is_version_info(expr: &Expr) -> bool {
    match expr {
        Expr::Attribute(attribute) => {
            attribute.attr.as_str() == "version_info"
                && matches!(attribute.value.as_ref(), Expr::Name(name) if name.id.as_str() == "sys")
        }
        Expr::Subscript(subscript) => is_version_info(&subscript.value),
        _ => false,
    }
}

/// the collection literal that `call` already reifies explicitly — the sole
/// argument of a subscripted builtin collection name of the matching kind
/// (`list[int]([1, 2])`) — so the wrap below must not wrap it again
fn explicitly_reified_literal(call: &ast::ExprCall) -> Option<&Expr> {
    let Expr::Subscript(subscript) = call.func.as_ref() else {
        return None;
    };
    let Expr::Name(name) = subscript.value.as_ref() else {
        return None;
    };
    if !call.arguments.keywords.is_empty() {
        return None;
    }
    let [argument] = call.arguments.args.as_ref() else {
        return None;
    };
    let matches_kind = match name.id.as_str() {
        "list" => matches!(argument, Expr::List(_)),
        "set" => matches!(argument, Expr::Set(_)),
        "dict" => matches!(argument, Expr::Dict(_)),
        "tuple" => matches!(argument, Expr::Tuple(_)),
        _ => false,
    };
    matches_kind.then_some(argument)
}

struct Reifier<'ti> {
    types: &'ti dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
}

impl Reifier<'_> {
    fn reify_collection_literal(&mut self, expr: &Expr) {
        let Some(spelling) = self.types.collection_literal_spelling(expr) else {
            return;
        };
        // an unparenthesized tuple display needs its own pair to stay a
        // single constructor argument; a parenthesized one already carries it
        let (open, close) = match expr {
            Expr::Tuple(tuple) if !tuple.parenthesized => ("((", "))"),
            _ => ("(", ")"),
        };
        self.edits.push((
            TextRange::empty(expr.range().start()),
            format!("{spelling}{open}"),
        ));
        self.edits
            .push((TextRange::empty(expr.range().end()), close.to_owned()));
    }
}

impl<'ast> Visitor<'ast> for Reifier<'_> {
    // type positions never reify
    fn visit_annotation(&mut self, _expr: &'ast Expr) {}
    fn visit_type_params(&mut self, _type_params: &'ast ast::TypeParams) {}

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            // a `type X = …` value is a type expression
            Stmt::TypeAlias(_) => {}
            Stmt::Assign(assign) if assign.targets.iter().any(is_static_literal_dunder) => {}
            Stmt::AugAssign(assign) if is_static_literal_dunder(&assign.target) => {}
            // a function's parameter list never reifies: annotations are type
            // positions, scalar defaults contain no displays, and every
            // non-scalar default is consumed whole by the mutable-defaults
            // lowering (swapped for `_MISSING`, re-evaluated verbatim in a
            // body guard). lambda parameters keep their defaults and walk
            // normally
            Stmt::FunctionDef(function) => {
                for decorator in &function.decorator_list {
                    self.visit_decorator(decorator);
                }
                self.visit_body(&function.body);
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Call(call) => {
                // bare constructor call of a generic class: make the solved
                // specialization explicit. an explicit `A[int](…)` (subscript
                // callee) already carries it
                if !matches!(call.func.as_ref(), Expr::Subscript(_))
                    && let Some(arguments) = self.types.constructor_specialization(call)
                {
                    self.edits.push((
                        TextRange::empty(call.func.range().end()),
                        format!("[{arguments}]"),
                    ));
                }
                if let Some(literal) = explicitly_reified_literal(call) {
                    self.visit_expr(&call.func);
                    // the already-wrapped literal itself stays bare; its
                    // elements still reify
                    walk_expr(self, literal);
                    return;
                }
                walk_expr(self, expr);
            }
            Expr::Compare(compare)
                if is_version_info(&compare.left)
                    || compare.comparators.iter().any(is_version_info) => {}
            Expr::Subscript(subscript) => {
                // in a type-context subscript (`dict[str, int]`, legacy
                // `Callable[[int], str]`) the slice is a type expression, and
                // a display there is type syntax, not a value
                let type_context = match subscript.value.as_ref() {
                    Expr::Name(name) => self.types.subscript_is_type_context(name),
                    Expr::Attribute(attribute) => match attribute.value.as_ref() {
                        Expr::Name(base) => self.types.attr_base_is_type_context(base),
                        _ => false,
                    },
                    _ => false,
                };
                if type_context {
                    self.visit_expr(&subscript.value);
                } else {
                    self.visit_expr(&subscript.value);
                    // a value-position subscript key is a structural index
                    // (tuple keys and kw-subscripts read it verbatim), not a
                    // constructed value — its own display never wraps, though
                    // displays nested inside it still do
                    match subscript.slice.as_ref() {
                        slice @ (Expr::List(_) | Expr::Set(_) | Expr::Dict(_) | Expr::Tuple(_)) => {
                            walk_expr(self, slice);
                        }
                        slice => self.visit_expr(slice),
                    }
                }
            }
            Expr::List(list) if list.ctx.is_load() => {
                self.reify_collection_literal(expr);
                walk_expr(self, expr);
            }
            Expr::Tuple(tuple) if tuple.ctx.is_load() && !tuple.elts.is_empty() => {
                self.reify_collection_literal(expr);
                walk_expr(self, expr);
            }
            Expr::Set(_) | Expr::Dict(_) => {
                self.reify_collection_literal(expr);
                walk_expr(self, expr);
            }
            _ => walk_expr(self, expr),
        }
    }
}

pub(crate) struct TypeReificationPass {
    min_version: PythonVersion,
    is_stub: bool,
}

impl TypeReificationPass {
    pub(crate) fn new(min_version: PythonVersion, is_stub: bool) -> Self {
        Self {
            min_version,
            is_stub,
        }
    }
}

impl TypeAwarePass for TypeReificationPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        if self.min_version < PythonVersion::PY39 || self.is_stub {
            return;
        }
        let mut reifier = Reifier {
            types,
            edits: Vec::new(),
        };
        for stmt in stmts {
            reifier.visit_stmt(stmt);
        }
        ctx.text_edits.extend(reifier.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    fn out(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    fn out_at(input: &str, version: PythonVersion) -> String {
        transpile(
            input,
            &Config {
                min_version: version,
                ..Config::test_default()
            },
        )
        .unwrap()
    }

    #[test]
    fn generic_constructor_gets_inferred_specialization() {
        let out = out_at(
            indoc! {"
                class A[T]:
                    def __init__(self, t: T): ...
                a = A(1)
                b = A(\"x\")
            "},
            PythonVersion::PY312,
        );
        assert!(
            out.contains("a = A[int](1)"),
            "int ctor should inject: {out}"
        );
        assert!(
            out.contains("b = A[str](\"x\")"),
            "str ctor should inject: {out}"
        );
    }

    #[test]
    fn explicit_constructor_specialization_unchanged() {
        let out = out_at(
            indoc! {"
                class A[T]:
                    def __init__(self, t: T): ...
                a = A[int](1)
            "},
            PythonVersion::PY312,
        );
        assert!(
            out.contains("a = A[int](1)"),
            "explicit spec should be untouched: {out}"
        );
        assert!(!out.contains("A[int][int]"), "no double injection: {out}");
    }

    #[test]
    fn non_generic_constructor_untouched() {
        let out = out(indoc! {"
            class A:
                def __init__(self, t: int): ...
            a = A(1)
        "});
        assert!(out.contains("a = A(1)"), "non-generic stays bare: {out}");
    }

    #[test]
    fn unsolvable_constructor_stays_bare() {
        let out = out_at(
            indoc! {"
                class A[T]:
                    def __init__(self) -> None: ...
                a = A()
            "},
            PythonVersion::PY312,
        );
        assert!(out.contains("a = A()"), "unsolved ctor stays bare: {out}");
    }

    #[test]
    fn list_literal_wrapped() {
        let out = out("xs = [1, 2]\n");
        assert!(
            out.contains("xs = list[int]([1, 2])"),
            "list should wrap: {out}"
        );
    }

    #[test]
    fn set_literal_wrapped() {
        let out = out("xs = {1, 2}\n");
        assert!(
            out.contains("xs = set[int]({1, 2})"),
            "set should wrap: {out}"
        );
    }

    #[test]
    fn dict_literal_wrapped() {
        let out = out("xs = {\"a\": 1}\n");
        assert!(
            out.contains("xs = dict[str, int]({\"a\": 1})"),
            "dict should wrap: {out}"
        );
    }

    #[test]
    fn tuple_literals_wrapped_with_inner_parens() {
        let bare = out("t = 1, \"x\"\n");
        assert!(
            bare.contains("t = tuple[int, str]((1, \"x\"))"),
            "unparenthesized tuple should wrap with inner parens: {bare}"
        );
        let parenthesized = out("t = (1, \"x\")\n");
        assert!(
            parenthesized.contains("t = tuple[int, str]((1, \"x\"))"),
            "parenthesized tuple keeps its own parens as the argument's: {parenthesized}"
        );
    }

    #[test]
    fn function_defaults_left_for_mutable_defaults_lowering() {
        // a non-scalar default is swapped for `_MISSING` and re-evaluated in
        // a body guard — the default expression must not carry a wrap
        let out = out(indoc! {"
            def f(x=[1, 2]):
                pass
        "});
        assert!(
            out.contains("def f(x=_MISSING):"),
            "default must stay a bare sentinel: {out}"
        );
        assert!(
            !out.contains("](_MISSING"),
            "no reification wrapper may leak around the sentinel: {out}"
        );
    }

    #[test]
    fn lambda_default_wraps() {
        // lambdas keep their defaults (no sentinel lowering), so they reify
        let out = out("f = lambda xs=[1]: xs\n");
        assert!(
            out.contains("lambda xs=list[int]([1]): xs"),
            "lambda default should wrap: {out}"
        );
    }

    #[test]
    fn version_check_tuple_untouched() {
        let out = out(indoc! {"
            import sys
            if sys.version_info >= (3, 14):
                pass
            if sys.version_info[:2] == (3, 12):
                pass
        "});
        assert!(
            out.contains("sys.version_info >= (3, 14)"),
            "version gates must keep a literal tuple: {out}"
        );
        assert!(
            out.contains("sys.version_info[:2] == (3, 12)"),
            "sliced version gates too: {out}"
        );
    }

    #[test]
    fn empty_literals_untouched() {
        let out = out(indoc! {"
            a = []
            b = {}
            c = ()
        "});
        assert!(out.contains("a = []"), "empty list stays bare: {out}");
        assert!(out.contains("b = {}"), "empty dict stays bare: {out}");
        assert!(out.contains("c = ()"), "empty tuple stays bare: {out}");
    }

    #[test]
    fn mixed_elements_spell_a_union() {
        let out = out("xs = [1, \"x\"]\n");
        assert!(
            out.contains("xs = list[int | str]([1, \"x\"])"),
            "union elements should spell: {out}"
        );
    }

    #[test]
    fn nested_literals_each_wrap() {
        let out = out("xs = [[1], [2]]\n");
        assert!(
            out.contains("xs = list[list[int]]([list[int]([1]), list[int]([2])])"),
            "nested literals should each wrap: {out}"
        );
    }

    #[test]
    fn annotation_type_expressions_untouched() {
        let out = out("x: list[int] = [1]\n");
        assert!(
            out.contains("x: list[int] = list[int]([1])"),
            "annotation stays, value wraps: {out}"
        );
    }

    #[test]
    fn type_context_subscript_slice_untouched() {
        // `str, int` in a value-position type subscript is a type expression,
        // not a tuple display
        let out = out("X = dict[str, int]\n");
        assert!(
            out.contains("X = dict[str, int]"),
            "type-context slice stays: {out}"
        );
    }

    #[test]
    fn value_subscript_key_untouched() {
        // a tuple key is a structural index, not a constructed value
        let out = out(indoc! {"
            d = {}
            d[(1, 2)]
        "});
        assert!(
            out.contains("d[(1, 2)]"),
            "keys pass through verbatim: {out}"
        );
    }

    #[test]
    fn dunder_all_untouched() {
        let out = out(indoc! {"
            __all__ = [\"a\"]
            a = 1
        "});
        assert!(
            out.contains("__all__ = [\"a\"]"),
            "__all__ must stay a literal display: {out}"
        );
    }

    #[test]
    fn shadowed_builtin_skips_wrap() {
        let out = out(indoc! {"
            def list(): ...
            xs = [1, 2]
        "});
        assert!(
            out.contains("xs = [1, 2]"),
            "shadowed builtin cannot be spelled: {out}"
        );
    }

    #[test]
    fn explicit_wrapper_not_double_wrapped() {
        let out = out("xs = list[int]([1, 2])\n");
        assert!(
            out.contains("xs = list[int]([1, 2])"),
            "explicit wrapper is already reified: {out}"
        );
    }

    #[test]
    fn unspellable_element_stays_bare() {
        // the element class is function-local, so its bare name has no
        // module-scope runtime spelling
        let out = out(indoc! {"
            def f():
                class C: ...
                return [C()]
        "});
        assert!(
            out.contains("return [C()]"),
            "scope-local element class stays bare: {out}"
        );
    }

    #[test]
    fn inert_below_39() {
        let out = out_at("xs = [1, 2]\n", PythonVersion::PY38);
        assert!(out.contains("xs = [1, 2]"), "no pep 585 below 3.9: {out}");
    }
}

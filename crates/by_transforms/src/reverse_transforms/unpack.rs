//! reverse of `crate::transforms::unpack`:
//!   `*args: Unpack[T]` → `*args: *T`
//!   `*args: P.args, **kwargs: P.kwargs` → `*args: *P, **kwargs: **P`
//!
//! the `Unpack` rewrite only fires on vararg annotations when `Unpack` resolves to the typing
//! import. the parameter-pack rewrite only fires on the pair, which is the only shape the typing
//! spec allows a `ParamSpec` to be forwarded in

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::type_info::TypeInfo;

pub(crate) struct UnpackReverse<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> UnpackReverse<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo) -> Self {
        Self {
            source,
            types,
            edits: Vec::new(),
        }
    }

    fn is_unpack_name(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Name(n) => n.id.as_str() == "Unpack" && self.types.subscript_is_type_context(n),
            Expr::Attribute(a) => {
                a.attr.id.as_str() == "Unpack"
                    && matches!(a.value.as_ref(), Expr::Name(n) if self.types.attr_base_is_type_context(n))
            }
            _ => false,
        }
    }

    fn process_vararg_annotation(&mut self, ann: &Expr) {
        let Expr::Subscript(s) = ann else {
            return;
        };
        if !self.is_unpack_name(&s.value) {
            return;
        }
        // only the `Unpack[` and its `]` are rewritten. the inner type is a type
        // expression another reverse transform may have edited, and re-rendering
        // it from raw source would silently undo that
        let value_end = usize::from(s.value.end());
        let open_end =
            TextSize::try_from(value_end + self.source[value_end..].find('[').map_or(0, |i| i + 1))
                .unwrap_or_else(|_| s.slice.range().start());
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            "*".to_owned(),
            TextRange::new(ann.range().start(), open_end),
        )));
        self.edits
            .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                ann.range().end() - TextSize::from(1),
                ann.range().end(),
            ))));
    }

    /// `P.args` / `P.kwargs` → `*P` / `**P`.
    ///
    /// The pack's own source is kept and only the stars and the suffix are edited, so a reverse
    /// transform that rewrote something inside the receiver still lands.
    fn rewrite_pack_component(&mut self, ann: &Expr, receiver: &Expr, stars: &str) {
        self.edits.push(Fix::safe_edit(Edit::insertion(
            stars.to_owned(),
            ann.range().start(),
        )));
        self.edits
            .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                receiver.range().end(),
                ann.range().end(),
            ))));
    }

    /// the receiver of an `X.<name>` annotation
    fn component_receiver<'a>(ann: Option<&'a Expr>, name: &str) -> Option<&'a Expr> {
        let Some(Expr::Attribute(attribute)) = ann else {
            return None;
        };
        (attribute.attr.id == name).then(|| attribute.value.as_ref())
    }

    /// `*args: P.args, **kwargs: P.kwargs` — the paired form, the only one the typing spec
    /// allows a `ParamSpec` to be forwarded in, and so the only one that identifies `P` as one
    fn process_paramspec_pair(&mut self, parameters: &ruff_python_ast::Parameters) {
        let vararg = parameters
            .vararg
            .as_deref()
            .and_then(|vararg| vararg.annotation.as_deref());
        let kwarg = parameters
            .kwarg
            .as_deref()
            .and_then(|kwarg| kwarg.annotation.as_deref());
        let (Some(args), Some(kwargs)) = (
            Self::component_receiver(vararg, "args"),
            Self::component_receiver(kwarg, "kwargs"),
        ) else {
            return;
        };
        if self.src(args.range()) != self.src(kwargs.range()) {
            return;
        }
        self.rewrite_pack_component(vararg.expect("checked above"), args, "*");
        self.rewrite_pack_component(kwarg.expect("checked above"), kwargs, "**");
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }
}

impl<'ast> Visitor<'ast> for UnpackReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(f) = stmt {
            if let Some(vararg) = &f.parameters.vararg {
                if let Some(ann) = &vararg.annotation {
                    self.process_vararg_annotation(ann);
                }
            }
            self.process_paramspec_pair(&f.parameters);
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
    fn basic_unpack() {
        check(
            indoc! {"
                from typing import Unpack
                def f(*args: Unpack[tuple[int, ...]]): ...
            "},
            indoc! {"
                from typing import Unpack
                def f(*args: *tuple[int, ...])
            "},
        );
    }

    #[test]
    fn nested_function() {
        check(
            indoc! {"
                from typing import Unpack
                class A:
                    def method(self, *args: Unpack[tuple[str, ...]]): ...
            "},
            indoc! {"
                from typing import Unpack
                class A:
                    def method(self, *args: *tuple[str, ...])
            "},
        );
    }

    /// a forwarded `ParamSpec` takes basedpython's starred spelling
    #[test]
    fn paramspec_pair_reversed() {
        check(
            "def f(*args: P.args, **kwargs: P.kwargs): ...\n",
            "def f(*args: *P, **kwargs: **P)\n",
        );
    }

    /// the rewrite is what the forward transform reads back, so the pair round-trips
    #[test]
    fn paramspec_pair_round_trips_through_the_forward_transform() {
        use crate::transpile;
        let reversed = reverse_transpile(
            "def f(*args: P.args, **kwargs: P.kwargs): ...\n",
            &Config::test_default(),
        )
        .expect("reverse failed");
        assert_eq!(reversed, "def f(*args: *P, **kwargs: **P)\n");
        let forward = transpile(&reversed, &Config::test_default()).expect("forward failed");
        assert!(
            forward.contains("*args: P.args, **kwargs: P.kwargs"),
            "expected the runtime spelling back, got:\n{forward}"
        );
    }

    /// only the paired form identifies a `ParamSpec`; a lone `.args` is left alone
    #[test]
    fn lone_args_component_unchanged() {
        check("def f(*args: P.args): ...\n", "def f(*args: P.args)\n");
    }

    /// two different receivers are not a pair
    #[test]
    fn mismatched_receivers_unchanged() {
        check(
            "def f(*args: P.args, **kwargs: Q.kwargs): ...\n",
            "def f(*args: P.args, **kwargs: Q.kwargs)\n",
        );
    }

    #[test]
    fn regular_arg_unchanged_by_unpack() {
        // unpack reverse leaves it alone; empty-declarations strips `: ...`
        check("def f(x: int): ...\n", "def f(x: int)\n");
    }

    /// the inner type keeps an edit another reverse transform made inside it —
    /// a whole-expression replacement rendered from raw source would undo it
    #[test]
    fn keeps_a_nested_rewrite() {
        check(
            indoc! {"
                from typing import Unpack
                def f(*args: Unpack[tuple[int, tuple[str, bytes]]]): ...
            "},
            indoc! {"
                from typing import Unpack
                def f(*args: *(int, (str, bytes)))
            "},
        );
    }

    #[test]
    fn shadowed_unchanged() {
        check(
            indoc! {"
                Unpack = object()
                def f(*args: Unpack[tuple[int, ...]]): ...
            "},
            indoc! {"
                Unpack = object()
                def f(*args: Unpack[tuple[int, ...]])
            "},
        );
    }
}

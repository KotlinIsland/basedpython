//! Lowers a decorator written above a binding.
//!
//! Python allows a decorator only on a `def` or a `class`. basedpython allows one
//! above a binding too, where it attaches metadata to the binding's type — the
//! same thing writing it in the type position does, put where a long decorator
//! reads better.
//!
//! ```by
//! @Field(gt=0)
//! let age: int = 1
//! ```
//!
//! becomes `age: Final[Annotated[int, Field(gt=0)]] = 1`, which is what pydantic,
//! attrs, msgspec and `annotated-types` all read. Applying the decorator to the
//! *value* instead would be the faithful `def` analogy and useless with every one
//! of them: `Field(gt=0)(1)` is not something any of them accept.
//!
//! A chain reads the way it does on a `def` — the decorator closest to the binding
//! is the innermost, so `@a` above `@b` above `x: int = 1` is
//! `Annotated[int, b, a]`, matching what `@a @b int` spells in a type position.
//!
//! The lowering deletes each decorator line and wraps the written type. The wrap is
//! a template so that whatever else lowers inside that type — an `int?`, a callable
//! arrow, a `dynamic` — still applies. The `Final` an
//! [immutable declaration](super::modifiers) adds is a separate edit over the
//! statement's prefix, and the two compose because that prefix now starts past
//! the decorators rather than at the statement.

use ruff_python_ast::helpers::written_annotation_type;
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Decorator, Expr, ModModule, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};

pub(crate) struct DecoratedBindingPass<'src> {
    source: &'src str,
}

impl<'src> DecoratedBindingPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for DecoratedBindingPass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut inner = BindingVisitor {
            source: self.source,
            edits: Vec::new(),
        };
        for stmt in &module.body {
            inner.visit_stmt(stmt);
        }
        if !inner.edits.is_empty() {
            ctx.required_imports
                .push("from typing import Annotated".to_owned());
        }
        ctx.text_edits.extend(inner.edits);
    }
}

struct BindingVisitor<'src> {
    source: &'src str,
    /// `(range, replacement)` — the decorator-line erasures, and the two inserts
    /// that put the `Annotated` around the written type
    edits: Vec<(TextRange, String)>,
}

impl BindingVisitor<'_> {
    /// Record the two edits one decorated binding needs: erase the decorator
    /// lines, and wrap the written type in the `Annotated` they become.
    fn lower(&mut self, decorators: &[Decorator], written_type: &Expr) {
        // each decorator is erased with the line break that ends it, up to the
        // next line's first character, so the binding inherits the indentation
        // the decorator line had. Erasing the whole span from the statement to
        // the binding instead would take a comment written between them with it
        for decorator in decorators {
            self.edits
                .push((decorator_line(self.source, decorator), String::new()));
        }

        // the metadata reads innermost first, so the decorator written closest to
        // the binding comes first — the order `@a @b int` puts them in
        let metadata: Vec<&str> = decorators
            .iter()
            .rev()
            .map(|decorator| &self.source[decorator.expression.range()])
            .collect();
        // two inserts at the type's edges rather than one edit over the type: the
        // type is a rewrite target of its own — `int?`, a callable arrow, a
        // decoration already written there — and an edit spanning it would be the
        // one that won, dropping the metadata. Nothing sits at a zero-width range,
        // so these two compose with whatever rewrites the type between them
        let range = written_type.range();
        self.edits
            .push((TextRange::empty(range.start()), "Annotated[".to_owned()));
        self.edits.push((
            TextRange::empty(range.end()),
            format!(", {}]", metadata.join(", ")),
        ));
    }
}

/// The source a decorator occupies together with the line break after it, up to
/// the first character of the next line — the span erasing it should take.
fn decorator_line(source: &str, decorator: &Decorator) -> TextRange {
    let after = usize::from(decorator.range().end());
    let trailing = source[after..]
        .find(|c: char| !c.is_whitespace())
        .unwrap_or(source.len() - after);
    TextRange::new(
        decorator.range().start(),
        decorator.range().end() + TextSize::try_from(trailing).unwrap_or_default(),
    )
}

impl<'ast> Visitor<'ast> for BindingVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // the parser only attaches decorators to a binding that writes both a value
        // and a type, so anything reaching here with a decorator has one to annotate
        if let Stmt::AnnAssign(declaration) = stmt
            && !declaration.decorator_list.is_empty()
            && let Some(written_type) = written_annotation_type(&declaration.annotation)
        {
            self.lower(&declaration.decorator_list, written_type);
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn decorated_annotated_assignment() {
        check(
            indoc! {"
                @foo
                x: int = 1
            "},
            indoc! {"
                from typing import Annotated
                x: Annotated[int, foo] = 1
            "},
        );
    }

    #[test]
    fn decorated_let_declaration() {
        check(
            indoc! {"
                @foo
                let a: int = 1
            "},
            indoc! {"
                from typing import Annotated, Final
                a: Final[Annotated[int, foo]] = 1
            "},
        );
    }

    #[test]
    fn a_chain_reads_innermost_first() {
        // the same order `@a @b int` puts them in, and the same order they apply in
        // on a `def`
        check(
            indoc! {"
                @a
                @b
                x: int = 1
            "},
            indoc! {"
                from typing import Annotated
                x: Annotated[int, b, a] = 1
            "},
        );
    }

    #[test]
    fn decorator_with_arguments() {
        // the form every library that wants field metadata is written in
        check(
            indoc! {"
                @field(gt=0)
                x: int = 1
            "},
            indoc! {"
                from typing import Annotated
                x: Annotated[int, field(gt=0)] = 1
            "},
        );
    }

    #[test]
    fn dotted_decorator() {
        check(
            indoc! {"
                @mod.foo
                x: int = 1
            "},
            indoc! {"
                from typing import Annotated
                x: Annotated[int, mod.foo] = 1
            "},
        );
    }

    #[test]
    fn decorated_class_attribute() {
        check(
            indoc! {"
                class A:
                    @foo
                    let a: int = 1
            "},
            indoc! {"
                from typing import Annotated
                class A:
                    a: Annotated[int, foo] = 1
            "},
        );
    }

    #[test]
    fn decorated_var_declaration() {
        check(
            indoc! {"
                @foo
                var x: int = 1
            "},
            indoc! {"
                from typing import Annotated
                x: Annotated[int, foo] = 1
            "},
        );
    }

    #[test]
    fn a_visibility_modifier_composes_with_a_decorator() {
        // a modifier chain can prefix a binding as well as a definition, and the
        // decorator above it annotates the type either way. (`export` on a
        // declaration contributes no `__all__` entry, with or without a decorator
        // — that is the modifier lowering's own behaviour, unchanged here)
        check(
            indoc! {"
                @foo
                export let a: int = 1
            "},
            indoc! {"
                from typing import Annotated, Final
                a: Final[Annotated[int, foo]] = 1
            "},
        );
    }

    #[test]
    fn a_modifier_on_a_decorated_definition_still_reads() {
        check(
            indoc! {"
                @foo
                final def f(): ...
            "},
            indoc! {"
                from typing import final
                @foo
                @final
                def f(): ...
            "},
        );
    }

    #[test]
    fn a_comment_between_the_decorator_and_the_binding_survives() {
        check(
            indoc! {"
                @foo
                # why
                x: int = 1
            "},
            indoc! {"
                from typing import Annotated
                # why
                x: Annotated[int, foo] = 1
            "},
        );
    }

    #[test]
    fn lowerings_inside_the_type_still_apply() {
        check(
            indoc! {"
                @foo
                x: int? = None
            "},
            indoc! {"
                from typing import Annotated
                x: Annotated[int | None, foo] = None
            "},
        );
    }

    #[test]
    fn the_value_is_left_alone() {
        check(
            indoc! {"
                def g() -> int?: ...
                @foo
                x: int = g() ?? 1
            "},
            indoc! {"
                from typing import Annotated
                def g() -> int | None: ...
                x: Annotated[int, foo] = __by_t_0__ if (__by_t_0__ := g()) is not None else 1
            "},
        );
    }
}

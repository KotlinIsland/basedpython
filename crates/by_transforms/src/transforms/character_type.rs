//! Emits the `from ty_extensions import Character` import for bare `Character` in
//! type-expression position.
//!
//! `Character` — the single-character string type — is implicitly available in
//! basedpython type expressions (ty resolves the bare name to
//! `ty_extensions.Character`), but generated Python needs the explicit import. the
//! name itself is left untouched; only the import is injected, and only when
//! the unshadowed implicit name is used (a local `Character = …` binding keeps its
//! identity and needs no import).
//!
//! traversal is delegated to [`type_expr_walker`], mirroring `just_float`, so
//! every recognised type position (annotations, returns, type-alias RHS,
//! type-param bound/default, class bases, value-position type applications,
//! `cast` / `Annotated` first arg, `Callable[[P], R]`) is covered

use ruff_python_ast::{Expr, Stmt};

use crate::transforms::ast_driver::{PassContext, TypeAwarePass};
use crate::transforms::type_expr_walker::{
    Recurse, TypeExprVisitor, TypePos, walk_one_type_expr, walk_type_positions,
};
use crate::type_info::TypeInfo;

pub(crate) const CHARACTER_IMPORT: &str = "from ty_extensions import Character";

pub(crate) struct CharacterType<'src> {
    types: &'src dyn TypeInfo,
    pub(crate) needs_character_import: bool,
}

impl<'src> CharacterType<'src> {
    pub(crate) fn new(types: &'src dyn TypeInfo) -> Self {
        Self {
            types,
            needs_character_import: false,
        }
    }

    /// public so [`rewrite_type_expr_with_imports`] can drive a one-off
    /// check over a single expression without spinning up a pass
    ///
    /// [`rewrite_type_expr_with_imports`]: crate::transforms::just_float::rewrite_type_expr_with_imports
    pub(crate) fn emit_in_type_expr(&mut self, expr: &Expr) {
        walk_one_type_expr(expr, self);
    }
}

impl TypeExprVisitor for CharacterType<'_> {
    fn visit(&mut self, expr: &Expr, _pos: TypePos) -> Recurse {
        if let Expr::Name(n) = expr
            && n.id.as_str() == "Character"
            && self.types.is_unbound_at("Character", expr)
        {
            self.needs_character_import = true;
        }
        Recurse::Descend
    }
}

pub(crate) struct CharacterTypePass;

impl CharacterTypePass {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl TypeAwarePass for CharacterTypePass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = CharacterType::new(types);
        walk_type_positions(stmts, Some(types), &mut inner);
        if inner.needs_character_import {
            ctx.required_imports.push(CHARACTER_IMPORT.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn bare_char_annotation() {
        check(
            "c: Character\n",
            indoc! {"
                from ty_extensions import Character
                c: Character
            "},
        );
    }

    #[test]
    fn char_in_function_signature() {
        check(
            indoc! {"
                def f(c: Character) -> Character:
                    return c
            "},
            indoc! {"
                from ty_extensions import Character
                def f(c: Character) -> Character:
                    return c
            "},
        );
    }

    #[test]
    fn char_inside_generic_and_union() {
        check(
            "a: list[Character]\nb: Character | None\n",
            indoc! {"
                from ty_extensions import Character
                a: list[Character]
                b: Character | None
            "},
        );
    }

    #[test]
    fn import_emitted_once() {
        check(
            "a: Character\nb: Character\n",
            indoc! {"
                from ty_extensions import Character
                a: Character
                b: Character
            "},
        );
    }

    #[test]
    fn explicit_import_not_duplicated() {
        unchanged("from ty_extensions import Character\nc: Character\n");
    }

    #[test]
    fn shadowed_char_not_imported() {
        // local rebinding shadows the implicit name — no import
        check(
            indoc! {"
                Character = int
                a: Character
            "},
            indoc! {"
                Character = int
                a: Character
            "},
        );
    }

    #[test]
    fn value_context_unchanged() {
        // value-position `Character` is an ordinary identifier
        unchanged("print(Character)\n");
    }
}

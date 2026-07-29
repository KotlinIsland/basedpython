//! Lowers basedpython match types and `TypeVarTuple` bounds.
//!
//! ```by
//! type NDTuple[T, *Shape: int] = match *Shape:
//!     case ():
//!         T
//!     case (Dim, *Rest):
//!         (NDTuple[T, *Rest],) * Dim
//! ```
//!
//! lowers to
//!
//! ```python
//! type NDTuple[T, *Shape] = object
//! ```
//!
//! A match type is decided entirely by the type checker: every application is
//! resolved to a concrete type before anything runs, so the `case` blocks have no
//! runtime meaning. The alias itself is *kept*, because annotations still name it
//! (`NDTuple[int, Literal[2], Literal[3]]` is a live subscript at runtime on
//! Python < 3.14), and `object` is the honest value for a name that stands for
//! "whatever the checker worked out".
//!
//! A variadic pack's bound — `*Shape: int` on a `TypeVarTuple`, `**Kwargs: **{"a": int}` on a
//! keyword-variadic pack — is a basedpython-only annotation that CPython rejects outright, so it
//! is deleted wherever it appears, not just on match types.

use ruff_python_ast::visitor::{Visitor, walk_stmt, walk_type_param};
use ruff_python_ast::{ModModule, Stmt, StmtTypeAlias, TypeParam};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};

pub(crate) struct MatchTypePass<'src> {
    source: &'src str,
}

impl<'src> MatchTypePass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for MatchTypePass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut inner = MatchTypeLowering {
            source: self.source,
            edits: Vec::new(),
        };
        for stmt in &module.body {
            inner.visit_stmt(stmt);
        }
        ctx.text_edits.extend(inner.edits);
    }
}

struct MatchTypeLowering<'src> {
    source: &'src str,
    edits: Vec<(TextRange, String)>,
}

impl<'ast> Visitor<'ast> for MatchTypeLowering<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::TypeAlias(alias) = stmt
            && !alias.cases.is_empty()
        {
            self.edits.push((
                match_body_range(alias, self.source),
                " = object".to_string(),
            ));
            // the case bodies are erased with the rest of the statement, so nothing
            // inside them is worth descending into
            if let Some(type_params) = alias.type_params.as_deref() {
                for type_param in type_params {
                    self.visit_type_param(type_param);
                }
            }
            return;
        }
        walk_stmt(self, stmt);
    }

    fn visit_type_param(&mut self, type_param: &'ast TypeParam) {
        let pack_bound = match type_param {
            TypeParam::TypeVarTuple(typevartuple) => {
                Some((&typevartuple.name, &typevartuple.bound))
            }
            TypeParam::ParamSpec(paramspec) => Some((&paramspec.name, &paramspec.bound)),
            TypeParam::TypeVar(_) => None,
        };
        if let Some((name, Some(bound))) = pack_bound {
            self.edits
                .push((TextRange::new(name.end(), bound.end()), String::new()));
        }
        walk_type_param(self, type_param);
    }
}

/// The span to replace with the alias's runtime value: everything from the end of the
/// header — the name, or the type parameter list when there is one — to the end of the
/// last `case` block, plus any comments that belong to the block.
///
/// Comments are not part of a node's range, so without extending past them a comment inside
/// the erased block survives into the output — describing a `case` that is no longer there,
/// and at an indentation that reads as if it belonged to something.
fn match_body_range(alias: &StmtTypeAlias, source: &str) -> TextRange {
    let header_end = alias
        .type_params
        .as_deref()
        .map_or_else(|| alias.name.end(), Ranged::end);

    let statement_start = usize::from(alias.range().start());
    let line_start = source[..statement_start]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let indent = statement_start - line_start;

    let mut end = usize::from(alias.end());
    // a comment trailing the last case body on its own line
    if let Some(rest) = source.get(end..)
        && rest
            .split('\n')
            .next()
            .is_some_and(|line| line.trim_start().starts_with('#'))
    {
        end += rest.split('\n').next().unwrap_or("").len();
    }

    // then whole lines that are blank or comment-only *and* indented inside the block; a
    // comment at the statement's own indentation introduces whatever comes next instead
    while let Some(newline) = source[end..].find('\n') {
        let next_line = &source[end + newline + 1..];
        let line = next_line.split('\n').next().unwrap_or(next_line);
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            break;
        }
        if line.len() - trimmed.len() <= indent {
            break;
        }
        end += newline + 1 + line.len();
    }

    TextRange::new(header_end, TextSize::try_from(end).unwrap_or(alias.end()))
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    /// Lowering for a target that has native PEP 695 syntax, so the `type` statement
    /// survives and this pass's own edit is what shapes it.
    fn lower_native(input: &str) -> String {
        let config = Config {
            min_version: PythonVersion::PY312,
            ..Config::test_default()
        };
        transpile(input, &config).unwrap()
    }

    /// Lowering for a target below 3.12, where the PEP 695 polyfill re-renders the whole
    /// statement and has to write the same value itself.
    fn lower_polyfilled(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    const NDTUPLE: &str = indoc! {"
        type NDTuple[T, *Shape: int] = match *Shape:
            case ():
                T
            case (Dim, *Rest):
                (NDTuple[T, *Rest],) * Dim


        def f(x: NDTuple[int, 2]) -> None:
            pass
    "};

    #[test]
    fn match_type_lowers_to_object() {
        let output = lower_native(NDTUPLE);
        assert!(
            output.contains("type NDTuple[T, *Shape] = object"),
            "{output}"
        );
        assert!(!output.contains("case ("), "{output}");
    }

    #[test]
    fn polyfilled_match_type_lowers_to_object() {
        let output = lower_polyfilled(NDTUPLE);
        // `type_params=` takes the parameter objects themselves — an
        // `Unpack[_Shape]` there is rejected by `TypeAliasType` at import
        assert!(
            output.contains("NDTuple = TypeAliasType(\"NDTuple\", object, type_params=(_T, _Shape))"),
            "{output}"
        );
        assert!(!output.contains("case ("), "{output}");
    }

    #[test]
    fn typevartuple_bound_is_stripped() {
        let output = lower_native(indoc! {"
            class Array[T, *Shape: int]:
                pass
        "});
        assert!(output.contains("class Array[T, *Shape]:"), "{output}");
    }

    /// The starred whole-pack forms erase the same way — python has no bound on either kind of
    /// pack, so nothing of them may reach the output.
    #[test]
    fn starred_pack_bounds_are_stripped() {
        let output = lower_native(indoc! {r#"
            class Array[*Shape: *(int, str), **Kwargs: **{"a": int}]:
                pass
        "#});
        assert!(
            output.contains("class Array[*Shape, **Kwargs]:"),
            "{output}"
        );
    }

    #[test]
    fn keyword_pack_bound_is_stripped() {
        let output = lower_native(indoc! {"
            class Array[**Kwargs: int]:
                pass
        "});
        assert!(output.contains("class Array[**Kwargs]:"), "{output}");
    }

    /// Below 3.12 the PEP 695 polyfill re-renders the type parameter list from the AST rather
    /// than editing the source, so a bound has to be absent from *that* path too.
    #[test]
    fn polyfilled_pack_bounds_are_stripped() {
        let output = lower_polyfilled(indoc! {r#"
            class Array[*Shape: *(int, str), **Kwargs: **{"a": int}]:
                pass
        "#});
        assert!(!output.contains("int, str"), "{output}");
        assert!(
            output.contains("_Shape = TypeVarTuple(\"_Shape\")"),
            "{output}"
        );
        assert!(
            output.contains("_Kwargs = ParamSpec(\"_Kwargs\")"),
            "{output}"
        );
    }

    /// Comments are not part of a node's range, so an erased `case` block's comments have to
    /// be swallowed explicitly — otherwise they survive, describing a case that is gone.
    #[test]
    fn comments_inside_the_erased_block_go_with_it() {
        let output = lower_native(indoc! {"
            type M[*Ts] = match *Ts:
                case ():
                    int
                case _:
                    str  # trailing on the last body
                # dangling inside the block


            x: int = 1
        "});
        assert!(output.contains("type M[*Ts] = object"), "{output}");
        assert!(!output.contains("dangling"), "{output}");
        assert!(!output.contains("trailing"), "{output}");
        // a comment at the statement's own indentation introduces what follows, so it stays
        assert!(output.contains("x: int = 1"), "{output}");
    }

    #[test]
    fn a_comment_after_the_block_is_left_alone() {
        let output = lower_native(indoc! {"
            type M[*Ts] = match *Ts:
                case _:
                    str


            # belongs to the next statement
            x: int = 1
        "});
        assert!(
            output.contains("# belongs to the next statement"),
            "{output}"
        );
    }

    #[test]
    fn private_match_type_takes_the_underscore_name() {
        let output = lower_polyfilled(indoc! {"
            private type Shape[*Dims: int] = match *Dims:
                case ():
                    int
                case (D, *Rest):
                    D
        "});
        assert!(
            output.contains("_Shape = TypeAliasType(\"_Shape\", object"),
            "{output}"
        );
    }
}

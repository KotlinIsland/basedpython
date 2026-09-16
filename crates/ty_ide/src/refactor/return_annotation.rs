//! Add Return Annotation: write the return type ty infers for a `def` that
//! leaves it out, as the file would spell it, importing what it names.

use ruff_diagnostics::Edit;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::{AnyNodeRef, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::types::ide_support::inferred_return_type;

use super::{Plan, RefactorContext, Refusal};
use crate::inlay_hints::annotation_for_type;

pub(super) fn plan(context: &RefactorContext<'_>, range: TextRange) -> Result<Plan, Refusal> {
    let module = context.parsed.syntax();
    if !module.range().contains_range(range) {
        return Err(Refusal::NotApplicable);
    }
    let covering = covering_node(module.into(), range);
    let Some(function) = covering.ancestors().find_map(|node| match node {
        AnyNodeRef::StmtFunctionDef(function) if header(function).contains_range(range) => {
            Some(function)
        }
        _ => None,
    }) else {
        return Err(Refusal::NotApplicable);
    };
    if function.returns.is_some()
        || function.is_asserts_return
        || function.is_trailing_lambda
        // a property declaration names its type after the name, not after `->`
        || function.property_construct_range().is_some()
        // a definition the parser recovered has no header to write into
        || function.name.range().is_empty()
    {
        return Err(Refusal::NotApplicable);
    }

    let title = "Add return annotation";
    let Some(returned) = inferred_return_type(&context.model, function) else {
        return Err(Refusal::refused(
            title,
            "ty could not infer what the function returns",
        ));
    };
    // the annotation is evaluated in the scope around the definition
    let Some((annotation, imports)) = annotation_for_type(
        context.db,
        context.file,
        AnyNodeRef::from(function),
        function.start(),
        returned,
    ) else {
        return Err(Refusal::refused(
            title,
            "the inferred return type has no spelling this file can write",
        ));
    };

    let mut edits = vec![Edit::insertion(
        format!(" -> {annotation}"),
        function.parameters.end(),
    )];
    edits.extend(
        imports
            .into_iter()
            .map(|import| Edit::range_replacement(import.new_text, import.range)),
    );
    Ok(Plan {
        title: format!("Add return annotation `-> {annotation}`"),
        edits,
    })
}

/// The part of a `def` before its body: the modifiers, name, type parameters,
/// parameters and anything written after them.
fn header(function: &StmtFunctionDef) -> TextRange {
    let end = function
        .raises
        .as_deref()
        .map_or(function.parameters.end(), Ranged::end);
    TextRange::new(function.start(), end)
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::refactor::RefactorKind;
    use crate::refactor::test_support::RefactorTest;

    fn annotate(source: &str) -> String {
        RefactorTest::python(source).apply(RefactorKind::AddReturnAnnotation)
    }

    #[test]
    fn writes_the_inferred_type() {
        assert_snapshot!(annotate(
            "
            def <CURSOR>f(x: int):
                return x + 1
            ",
        ), @"
        Add return annotation `-> int`
        ---

        def f(x: int) -> int:
            return x + 1
        ");
    }

    /// basedpython writes `-> T` before a `raises` clause, so the annotation goes after the
    /// parameters rather than at the end of the header.
    #[test]
    fn the_annotation_goes_before_a_raises_clause() {
        assert_snapshot!(
            RefactorTest::basedpython(
                "
                def <CURSOR>f(x: int) raises ValueError:
                    if x < 0:
                        raise ValueError
                    return x
                ",
            )
            .apply(RefactorKind::AddReturnAnnotation),
            @"
        Add return annotation `-> int`
        ---

        def f(x: int) -> int raises ValueError:
            if x < 0:
                raise ValueError
            return x
        "
        );
    }

    #[test]
    fn writes_none_for_a_function_returning_nothing() {
        assert_snapshot!(annotate(
            "
            def <CURSOR>f(x: int):
                print(x)
            ",
        ), @"
        Add return annotation `-> None`
        ---

        def f(x: int) -> None:
            print(x)
        ");
    }

    #[test]
    fn imports_a_type_from_another_module() {
        let test = RefactorTest::with_files(
            "main.py",
            "
            from shapes import make

            def <CURSOR>f():
                return make()
            ",
            &[(
                "shapes.py",
                "
                class Circle: ...

                def make() -> Circle: ...
                ",
            )],
        );
        assert_snapshot!(test.apply(RefactorKind::AddReturnAnnotation), @"
        Add return annotation `-> Circle`
        ---

        from shapes import make, Circle

        def f() -> Circle:
            return make()
        ");
    }

    #[test]
    fn async_function_is_annotated_with_what_it_returns() {
        assert_snapshot!(annotate(
            "
            async def <CURSOR>f():
                return 1
            ",
        ), @"
        Add return annotation `-> Literal[1]`
        ---
        from typing import Literal

        async def f() -> Literal[1]:
            return 1
        ");
    }

    #[test]
    fn annotated_function_is_not_offered() {
        assert_snapshot!(annotate(
            "
            def <CURSOR>f() -> int:
                return 1
            ",
        ), @"not offered");
    }

    #[test]
    fn body_is_not_the_header() {
        assert_snapshot!(annotate(
            "
            def f():
                return <CURSOR>1
            ",
        ), @"not offered");
    }

    #[test]
    fn basedpython_writes_the_type_in_its_own_spelling() {
        assert_snapshot!(
            RefactorTest::basedpython(
                "
                def <CURSOR>f(flag: bool):
                    if flag:
                        return 1
                    return None
                ",
            )
            .apply(RefactorKind::AddReturnAnnotation),
            @"
        Add return annotation `-> 1 | None`
        ---

        def f(flag: bool) -> 1 | None:
            if flag:
                return 1
            return None
        "
        );
    }
}

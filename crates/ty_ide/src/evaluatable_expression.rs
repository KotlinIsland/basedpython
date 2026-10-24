//! the expression a debugger evaluates for a position: what a value tooltip
//! shows on hover, and what *Evaluate Expression* and *Quick Evaluate* start
//! from when nothing is selected
//!
//! it is the reference the name at the position is the end of. on `d` in
//! `a.b[c].d` that is all of `a.b[c].d`, on `b` it is `a.b`, and on `c` it is
//! `c`, the name inside the subscript. a client that guesses from the text
//! cannot tell `d` there from a name of its own, and a guess that stops at the
//! `]` answers nothing at all
//!
//! a debugger evaluates what this answers in the stopped program, so whether
//! the expression may run code the program would not have run is part of the
//! question. reading an attribute or an item is what a value tooltip does in
//! every python debugger and is always answered. a call is answered only when
//! the client says running one is fine, which is the difference between
//! hovering a name and asking for it to be evaluated: on `f` in `f(x)` the
//! answer is `f` when calls are not allowed and `f(x)` when they are, and a
//! reference whose own value is a call's result, like `make().d`, is not
//! answered at all when they are not
//!
//! the stopped program is python, so in a basedpython file only an expression
//! that python reads the same way is answered: not `user?.name`

use ruff_db::PythonFile;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_python_ast::comparable::ComparableExpr;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::helpers::any_over_expr;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::{AnyNodeRef, Expr, ExprContext, Mod};
use ruff_python_parser::{Mode, ParseOptions, parse};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::Db;

/// the range of the expression a debugger evaluates for `offset`, or `None`
/// when the position is not on a name an expression ends with
///
/// `calls` says whether the expression may call anything; see the module
/// documentation
pub fn evaluatable_expression(
    db: &dyn Db,
    file: PythonFile<'_>,
    offset: TextSize,
    calls: bool,
) -> Option<TextRange> {
    let parsed = parsed_module(db, file).load(db);
    // the name is found from the tokens rather than from the tree, so that a
    // position between `.` and a name, or right after a name, is on that name
    let token = parsed
        .tokens()
        .at_offset(offset)
        .find(|token| token.kind() == TokenKind::Identifier || token.kind().is_soft_keyword())?;
    let covering = covering_node(parsed.syntax().into(), token.range());

    let mut ancestors = covering.ancestors();
    let mut node = ancestors.next()?;
    if node.is_identifier() {
        let parent = ancestors.next()?;
        match parent {
            // the attribute a reference ends with is the whole reference
            AnyNodeRef::ExprAttribute(_) => node = parent,
            // a parameter is bound in the frame the debugger is stopped in
            AnyNodeRef::Parameter(_) => return Some(token.range()),
            // the name a `def`, a class, an import or a keyword argument
            // declares is not an expression, and evaluating the same text
            // could mean something else in the stopped frame
            _ => return None,
        }
    }

    let expression = match node {
        AnyNodeRef::ExprName(name) => {
            // a name the lowering made up spans the whole construct it stands
            // for, and has no text of its own to evaluate
            if name.ctx == ExprContext::Invalid {
                return None;
            }
            name.range()
        }
        AnyNodeRef::ExprAttribute(attribute) => {
            if !calls && calls_anything(&attribute.value) {
                return None;
            }
            attribute.range()
        }
        _ => return None,
    };

    let answer = if calls
        && let Some(AnyNodeRef::ExprCall(call)) = ancestors.next()
        && call.func.range() == expression
    {
        call.range()
    } else {
        expression
    };

    let source = source_text(db, file.file(db));
    (!file.file(db).source_type(db).is_basedpython() || reads_the_same_in_python(&source[answer]))
        .then_some(answer)
}

/// whether python reads `text`, an expression in a basedpython file, as the same expression:
/// the program a debugger is stopped in is python, whatever its source was written in
fn reads_the_same_in_python(text: &str) -> bool {
    let read = |basedpython: bool| {
        let options = ParseOptions::from(Mode::Expression).with_basedpython(basedpython);
        match parse(text, options).ok()?.into_syntax() {
            Mod::Expression(expression) => Some(expression.body),
            Mod::Module(_) => None,
        }
    };
    let (Some(python), Some(basedpython)) = (read(false), read(true)) else {
        return false;
    };
    ComparableExpr::from(&python) == ComparableExpr::from(&basedpython)
}

/// whether evaluating `expression` would call something, or otherwise run code
/// the program decides on, rather than read a value
fn calls_anything(expression: &Expr) -> bool {
    any_over_expr(expression, |expression| {
        matches!(
            expression,
            Expr::Call(_)
                | Expr::Await(_)
                | Expr::Yield(_)
                | Expr::YieldFrom(_)
                | Expr::Named(_)
                | Expr::Lambda(_)
                | Expr::ListComp(_)
                | Expr::SetComp(_)
                | Expr::DictComp(_)
                | Expr::Generator(_)
        )
    })
}

#[cfg(test)]
mod tests {
    use crate::tests::CursorTest;

    use super::evaluatable_expression;

    /// the text of the answer for the `<CURSOR>` in `source`, a `.by` file
    fn evaluatable(source: &str, calls: bool) -> Option<String> {
        let test = CursorTest::builder().source("main.by", source).build();
        let file = test.program_file(test.cursor.file).python_file(&test.db);
        evaluatable_expression(&test.db, file, test.cursor.offset, calls)
            .map(|range| test.cursor.source[range].to_string())
    }

    fn both(source: &str) -> (Option<String>, Option<String>) {
        (evaluatable(source, false), evaluatable(source, true))
    }

    const CHAIN: &str = "a.b[c].d + compute(len(c))";

    fn at(marker: &str) -> String {
        let index = CHAIN.find(marker).expect("the marker is in the line");
        format!("{}<CURSOR>{}\n", &CHAIN[..index], &CHAIN[index..])
    }

    #[test]
    fn the_last_name_of_a_reference_is_the_whole_reference() {
        assert_eq!(evaluatable(&at("d +"), false).as_deref(), Some("a.b[c].d"));
    }

    #[test]
    fn a_name_inside_a_reference_is_the_reference_up_to_it() {
        assert_eq!(evaluatable(&at("a."), false).as_deref(), Some("a"));
        assert_eq!(evaluatable(&at("b["), false).as_deref(), Some("a.b"));
    }

    #[test]
    fn a_name_in_a_subscript_is_itself() {
        assert_eq!(evaluatable(&at("c]"), false).as_deref(), Some("c"));
    }

    #[test]
    fn a_callee_is_the_call_only_when_calls_are_allowed() {
        assert_eq!(
            both(&at("compute")),
            (
                Some("compute".to_string()),
                Some("compute(len(c))".to_string())
            )
        );
        assert_eq!(
            both(&at("len")),
            (Some("len".to_string()), Some("len(c)".to_string()))
        );
    }

    #[test]
    fn a_method_is_the_call_only_when_calls_are_allowed() {
        assert_eq!(
            both("items.app<CURSOR>end(1)\n"),
            (
                Some("items.append".to_string()),
                Some("items.append(1)".to_string())
            )
        );
    }

    #[test]
    fn a_reference_through_a_call_is_answered_only_when_calls_are_allowed() {
        assert_eq!(
            both("make().<CURSOR>d\n"),
            (None, Some("make().d".to_string()))
        );
    }

    #[test]
    fn a_position_just_after_a_name_or_just_before_it_is_on_it() {
        assert_eq!(
            evaluatable("total<CURSOR> = 1\n", false).as_deref(),
            Some("total")
        );
        assert_eq!(evaluatable("a.<CURSOR>b\n", false).as_deref(), Some("a.b"));
    }

    #[test]
    fn a_parameter_is_its_name() {
        assert_eq!(
            evaluatable("def f(lim<CURSOR>it: int):\n    pass\n", false).as_deref(),
            Some("limit")
        );
    }

    #[test]
    fn declared_names_are_not_expressions() {
        assert_eq!(
            evaluatable("def comp<CURSOR>ute():\n    pass\n", true),
            None
        );
        assert_eq!(evaluatable("class Po<CURSOR>int:\n    pass\n", true), None);
        assert_eq!(evaluatable("import o<CURSOR>s\n", true), None);
        assert_eq!(evaluatable("f(na<CURSOR>me=1)\n", true), None);
    }

    #[test]
    fn literals_keywords_operators_and_comments_are_nothing() {
        assert_eq!(evaluatable("x = \"te<CURSOR>xt\"\n", true), None);
        assert_eq!(evaluatable("x = No<CURSOR>ne\n", true), None);
        assert_eq!(evaluatable("x = 1 <CURSOR>+ 2\n", true), None);
        assert_eq!(evaluatable("# a <CURSOR>note\n", true), None);
    }

    /// the running program is python, so an expression it would read differently, or not at
    /// all, is not answered
    #[test]
    fn basedpython_only_syntax_is_nothing() {
        assert_eq!(evaluatable("user?.na<CURSOR>me\n", false), None);
        assert_eq!(evaluatable("user?.name.fir<CURSOR>st\n", false), None);
        assert_eq!(
            evaluatable("us<CURSOR>er?.name\n", false).as_deref(),
            Some("user")
        );
    }

    #[test]
    fn a_name_in_an_f_string_is_itself() {
        assert_eq!(
            evaluatable("f\"{coun<CURSOR>t} items\"\n", false).as_deref(),
            Some("count")
        );
    }
}

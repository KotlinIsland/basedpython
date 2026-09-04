use ruff_python_ast::{self as ast, Identifier, Pattern};
use ruff_python_semantic::ScopeKind;
use ruff_text_size::Ranged;

use crate::checkers::ast::Checker;
use crate::codes::Rule;
use crate::rules::pep8_naming;

/// Run lint rules over a [`Pattern`] syntax node.
pub(crate) fn pattern(pattern: &Pattern, checker: &Checker) {
    let (Pattern::MatchAs(ast::PatternMatchAs {
        name: Some(name), ..
    })
    | Pattern::MatchStar(ast::PatternMatchStar {
        name: Some(name), ..
    })
    | Pattern::MatchMapping(ast::PatternMatchMapping {
        rest: Some(name), ..
    })) = pattern
    else {
        return;
    };

    check_pattern_name(checker, name);
}

fn check_pattern_name(checker: &Checker, name: &Identifier) {
    // basedpython: a bare `case Red:` naming an `enum class` variant is a
    // reference to that member, not a variable — so the naming rules, which are
    // about what a *variable* should be called, have nothing to say about it.
    // `Red` is spelled the way its declaration spells it, which is the point
    if checker.semantic().is_based_enum_case_name(name.as_str()) {
        return;
    }
    if checker.is_rule_enabled(Rule::NonLowercaseVariableInFunction)
        && checker.semantic().current_scope().kind.is_function()
    {
        pep8_naming::rules::non_lowercase_variable_in_function(checker, name.range(), &name.id);
    }
    if checker.is_rule_enabled(Rule::MixedCaseVariableInClassScope) {
        if let ScopeKind::Class(class_def) = &checker.semantic().current_scope().kind {
            pep8_naming::rules::mixed_case_variable_in_class_scope(
                checker,
                name.range(),
                &name.id,
                class_def,
            );
        }
    }
    if checker.is_rule_enabled(Rule::MixedCaseVariableInGlobalScope)
        && matches!(checker.semantic().current_scope().kind, ScopeKind::Module)
    {
        pep8_naming::rules::mixed_case_variable_in_global_scope(checker, name.range(), &name.id);
    }
}

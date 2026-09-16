//! Names for what a refactoring introduces.
//!
//! A code action cannot ask for a name, so a refactoring that introduces a
//! binding picks one the file does not already use anywhere — not in any scope,
//! not as a builtin it reads — which is what makes it impossible for the new
//! binding to capture or shadow a name some other code resolves.

use ruff_python_ast::{self as ast, Expr};
use ruff_python_stdlib::builtins::is_python_builtin;
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_python_stdlib::keyword::is_keyword;
use rustc_hash::FxHashSet;
use ty_python_core::semantic_index;

use super::RefactorContext;

/// The words basedpython reads as keywords in some position, which a name
/// introduced by a refactoring stays clear of.
const BASEDPYTHON_WORDS: &[&str] = &[
    "let",
    "var",
    "it",
    "init",
    "data",
    "frozen",
    "extension",
    "sealed",
    "open",
    "override",
    "static",
    "export",
    "public",
    "private",
    "protected",
    "late",
    "context",
    "protocol",
    "enum",
    "some",
    "dynamic",
    "typeof",
    "literal",
    "final",
    "abstract",
];

/// Every name some scope of the file binds, declares or reads.
pub(crate) fn names_in_file(context: &RefactorContext<'_>) -> FxHashSet<String> {
    let index = semantic_index(context.db, context.file);
    let mut names = FxHashSet::default();
    for scope in index.scope_ids() {
        let table = index.place_table(scope.file_scope_id(context.db));
        names.extend(table.symbols().map(|symbol| symbol.name().to_string()));
    }
    names
}

/// `base`, or `base` with the smallest numeric suffix, that `taken` lacks.
pub(crate) fn fresh_name(base: &str, taken: &FxHashSet<String>) -> String {
    let usable = |name: &str| {
        is_identifier(name)
            && !is_keyword(name)
            && !BASEDPYTHON_WORDS.contains(&name)
            && !name.starts_with("__")
            && !taken.contains(name)
    };
    if usable(base) {
        return base.to_string();
    }
    let mut suffix = 1;
    loop {
        let candidate = format!("{base}_{suffix}");
        if usable(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

/// A name that says what `expr` evaluates to, in snake case.
pub(crate) fn suggest_for_expression(expr: &Expr, python_version_minor: u8) -> String {
    let named = match expr {
        Expr::Call(call) => callee_name(&call.func).map(|name| {
            name.strip_prefix("get_")
                .filter(|rest| !rest.is_empty())
                .unwrap_or(name)
                .to_string()
        }),
        Expr::Attribute(attribute) => Some(attribute.attr.id.to_string()),
        Expr::Subscript(subscript) => match &*subscript.value {
            Expr::Name(name) => Some(format!("{}_item", name.id)),
            Expr::Attribute(attribute) => Some(format!("{}_item", attribute.attr.id)),
            _ => None,
        },
        Expr::Await(await_expr) => {
            return suggest_for_expression(&await_expr.value, python_version_minor);
        }
        _ => None,
    };
    let fallback = match expr {
        Expr::StringLiteral(_) | Expr::FString(_) => "text",
        Expr::NumberLiteral(_) => "number",
        Expr::BooleanLiteral(_) | Expr::Compare(_) | Expr::BoolOp(_) => "condition",
        Expr::List(_) | Expr::ListComp(_) => "items",
        Expr::Set(_) | Expr::SetComp(_) => "elements",
        Expr::Dict(_) | Expr::DictComp(_) => "mapping",
        Expr::Tuple(_) => "values",
        Expr::Lambda(_) => "function",
        Expr::Generator(_) => "generator",
        _ => "value",
    };
    named
        .map(|name| to_snake_case(&name))
        .filter(|name| is_identifier(name) && !is_python_builtin(name, python_version_minor, false))
        .unwrap_or_else(|| fallback.to_string())
}

fn callee_name(func: &Expr) -> Option<&str> {
    match func {
        Expr::Name(ast::ExprName { id, .. }) => Some(id.as_str()),
        Expr::Attribute(attribute) => Some(attribute.attr.id.as_str()),
        _ => None,
    }
}

fn to_snake_case(name: &str) -> String {
    let mut snake = String::with_capacity(name.len() + 4);
    let mut previous_lower = false;
    for character in name.chars() {
        if character.is_uppercase() {
            if previous_lower {
                snake.push('_');
            }
            snake.extend(character.to_lowercase());
            previous_lower = false;
        } else {
            snake.push(character);
            previous_lower = character.is_lowercase() || character.is_ascii_digit();
        }
    }
    snake
}

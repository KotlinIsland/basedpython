//! Lowers a keyword argument whose name python cannot spell.
//!
//! basedpython names a keyword argument with a dotted path or a string literal
//! as well as a bare identifier, so a call can reach a `**kwargs` entry whose
//! key is not a python identifier:
//!
//! ```text
//! f(foo.bar=1, "content-type"=2)  →  f(**{"foo.bar": 1}, **{"content-type": 2})
//! ```
//!
//! One mapping per key, spliced where the key was written, keeps the arguments
//! in source order and leaves every other argument's bytes untouched, so a
//! lowering inside a value (`?.`, `??`, …) still applies. A key python *can*
//! spell — `f("timeout"=1)` — only loses its quotes.
//!
//! ## the `*args` ordering rule
//!
//! Python's grammar allows `f(a=1, *args)` but not `f(**d, *args)`: an iterable
//! unpacking may not follow a keyword unpacking. So when a starred argument
//! follows a lowered key, the whole argument list is re-emitted with the
//! positional arguments first. That is not a reordering in any observable
//! sense — the language reference says a `*expression` in a call "is processed
//! before the keyword arguments", so CPython already evaluates `*args` first in
//! `f(a=1, *args)`, and the re-emitted call evaluates its arguments in exactly
//! the same order.

use ruff_python_ast::str::Quote;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_f_string, walk_stmt};
use ruff_python_ast::{
    Arguments, AtomicNodeIndex, Expr, ExprStringLiteral, FString, Keyword, Stmt, StringFlags,
    StringLiteral, StringLiteralFlags, StringLiteralValue,
};
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass, render_expr};
use crate::type_info::TypeInfo;

/// Renders `key` as a python string literal, quoted with `quote`.
///
/// The key came from a name, not from source text, so it can hold anything at
/// all — quotes, newlines, control characters. Rendering it through the code
/// generator escapes it the way python spells any other string
fn quote_key(key: &str, quote: Quote) -> String {
    render_expr(&Expr::StringLiteral(ExprStringLiteral {
        node_index: AtomicNodeIndex::NONE,
        range: TextRange::default(),
        value: StringLiteralValue::single(StringLiteral {
            node_index: AtomicNodeIndex::NONE,
            range: TextRange::default(),
            value: key.into(),
            flags: StringLiteralFlags::empty().with_quote_style(quote),
        }),
    }))
}

/// Whether this keyword argument survives into python as it is written.
///
/// A `**kwargs` unpacking names nothing, and a name python can spell bare is
/// left alone unless the source spelled it as a string.
fn rewrite_kind(keyword: &Keyword) -> Option<Rewrite> {
    let name = keyword.arg.as_ref()?;
    match (is_identifier(name.as_str()), keyword.key.is_quoted()) {
        (true, false) => None,
        (true, true) => Some(Rewrite::Unquote),
        (false, _) => Some(Rewrite::Mapping),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rewrite {
    /// `f("timeout"=1)` → `f(timeout=1)`: python can spell this name, so only
    /// the quotes have to go
    Unquote,
    /// `f(foo.bar=1)` → `f(**{"foo.bar": 1})`: python cannot name this
    /// argument at all, so it is passed as a mapping
    Mapping,
}

struct FlexibleKeyword {
    text_edits: Vec<(TextRange, String)>,
    template_edits: Vec<(TextRange, Vec<Fragment>)>,
    /// the quote characters of the f-strings currently being walked into.
    /// before python 3.12 an interpolation may not reuse its own f-string's
    /// quote, so a key written inside one has to be quoted the other way
    enclosing_quotes: Vec<Quote>,
}

impl FlexibleKeyword {
    fn new() -> Self {
        Self {
            text_edits: Vec::new(),
            template_edits: Vec::new(),
            enclosing_quotes: Vec::new(),
        }
    }

    /// A quote no enclosing f-string is delimited with, preferring the double
    /// quote every other emitted string uses.
    ///
    /// Only nested f-strings can take both, and nesting them is itself 3.12 or
    /// later, so there the reuse this avoids is allowed anyway
    fn quote(&self) -> Quote {
        if self.enclosing_quotes.contains(&Quote::Double)
            && !self.enclosing_quotes.contains(&Quote::Single)
        {
            Quote::Single
        } else {
            Quote::Double
        }
    }

    fn lower(&mut self, arguments: &Arguments) {
        let quote = self.quote();
        // a name written as a string that python can spell bare keeps its
        // position and only sheds its quotes
        for keyword in &arguments.keywords {
            if rewrite_kind(keyword) == Some(Rewrite::Unquote)
                && let Some(name) = &keyword.arg
            {
                self.text_edits
                    .push((name.range(), name.as_str().to_owned()));
            }
        }

        let Some(first_mapped) = arguments
            .keywords
            .iter()
            .find(|keyword| rewrite_kind(keyword) == Some(Rewrite::Mapping))
            .map(Ranged::start)
        else {
            return;
        };

        // `f(a.b=1, *rest)` cannot become `f(**{"a.b": 1}, *rest)` — python
        // rejects an iterable unpacking after a keyword unpacking
        let starred_follows = arguments
            .args
            .iter()
            .any(|arg| arg.is_starred_expr() && arg.start() > first_mapped);

        if starred_follows {
            self.template_edits
                .push((arguments.range(), reordered(arguments, quote)));
            return;
        }

        for keyword in &arguments.keywords {
            if rewrite_kind(keyword) == Some(Rewrite::Mapping) {
                self.template_edits
                    .push((keyword.range(), mapping(keyword, quote)));
            }
        }
    }
}

/// The `**{"key": value}` a lowered keyword argument becomes, with the value
/// passing through as source so lowerings inside it still compose.
///
/// A keyword with no name is a `**kwargs` unpacking, which is never lowered, so
/// its own source is passed straight through.
fn mapping(keyword: &Keyword, quote: Quote) -> Vec<Fragment> {
    let Some(name) = &keyword.arg else {
        return vec![Fragment::Src(keyword.range())];
    };
    vec![
        Fragment::Lit(format!("**{{{}: ", quote_key(name.as_str(), quote))),
        Fragment::Src(keyword.value.range()),
        Fragment::Lit("}".to_owned()),
    ]
}

/// The whole argument list re-emitted with every positional argument ahead of
/// every keyword argument — the order python evaluates them in anyway.
fn reordered(arguments: &Arguments, quote: Quote) -> Vec<Fragment> {
    let mut fragments = vec![Fragment::Lit("(".to_owned())];
    for (index, arg) in arguments.args.iter().enumerate() {
        if index > 0 {
            fragments.push(Fragment::Lit(", ".to_owned()));
        }
        fragments.push(Fragment::Src(arg.range()));
    }
    for (index, keyword) in arguments.keywords.iter().enumerate() {
        if index > 0 || !arguments.args.is_empty() {
            fragments.push(Fragment::Lit(", ".to_owned()));
        }
        if rewrite_kind(keyword) == Some(Rewrite::Mapping) {
            fragments.extend(mapping(keyword, quote));
        } else {
            // an unquoted key's own edit is nested in this passthrough span,
            // which a template materializes rather than clobbers
            fragments.push(Fragment::Src(keyword.range()));
        }
    }
    fragments.push(Fragment::Lit(")".to_owned()));
    fragments
}

impl<'ast> Visitor<'ast> for FlexibleKeyword {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(class) = stmt
            && let Some(arguments) = &class.arguments
        {
            self.lower(arguments);
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.lower(&call.arguments);
        }
        walk_expr(self, expr);
    }

    fn visit_f_string(&mut self, f_string: &'ast FString) {
        self.enclosing_quotes.push(f_string.flags.quote_style());
        walk_f_string(self, f_string);
        self.enclosing_quotes.pop();
    }
}

pub(crate) struct FlexibleKeywordPass;

impl TypeAwarePass for FlexibleKeywordPass {
    fn run(&self, stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = FlexibleKeyword::new();
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        ctx.text_edits.extend(inner.text_edits);
        ctx.template_edits.extend(inner.template_edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    #[test]
    fn dotted_and_string_keys() {
        check(
            "f(foo.bar=1, \"/*\"=2)\n",
            "f(**{\"foo.bar\": 1}, **{\"/*\": 2})\n",
        );
    }

    #[test]
    fn ordinary_keywords_keep_their_place() {
        // one mapping per key, spliced where the key was written, so every
        // other argument stays exactly where the source put it
        check(
            "f(a, b=1, c.d=2, e=3)\n",
            "f(a, b=1, **{\"c.d\": 2}, e=3)\n",
        );
    }

    #[test]
    fn a_quoted_name_python_can_spell_only_loses_its_quotes() {
        check("f(\"timeout\"=1)\n", "f(timeout=1)\n");
    }

    #[test]
    fn a_quoted_python_keyword_is_not_a_name_python_can_spell() {
        // `f(class=1)` is a syntax error, so the key has to be passed as a
        // mapping even though it reads like an identifier
        check("f(\"class\"=1)\n", "f(**{\"class\": 1})\n");
    }

    #[test]
    fn key_text_is_escaped() {
        check(
            "f(\"a\\\"b\"=1, \"c\\nd\"=2)\n",
            "f(**{'a\"b': 1}, **{\"c\\nd\": 2})\n",
        );
    }

    #[test]
    fn a_starred_argument_after_a_key_moves_ahead_of_it() {
        // python rejects `f(**{...}, *rest)`, and evaluates `*rest` before any
        // keyword argument anyway, so the re-emitted call is equivalent
        check("f(a.b=1, *rest)\n", "f(*rest, **{\"a.b\": 1})\n");
    }

    #[test]
    fn a_starred_argument_before_a_key_is_left_alone() {
        check("f(*rest, a.b=1)\n", "f(*rest, **{\"a.b\": 1})\n");
    }

    #[test]
    fn reordering_keeps_the_other_arguments() {
        check(
            "f(x, y=1, a.b=2, *rest, **extra)\n",
            "f(x, *rest, y=1, **{\"a.b\": 2}, **extra)\n",
        );
    }

    #[test]
    fn an_unquoted_name_survives_the_reorder() {
        // the unquote is a narrow edit inside the span the wider template
        // passes through, which materializes it rather than clobbering it
        check(
            "f(\"timeout\"=1, a.b=2, *rest)\n",
            "f(*rest, timeout=1, **{\"a.b\": 2})\n",
        );
    }

    #[test]
    fn a_lowering_inside_the_value_still_applies() {
        // the value passes through as source, so a sibling pass rewrites it
        check(
            "f(a.b=x ?? y)\n",
            "f(**{\"a.b\": x if x is not None else y})\n",
        );
    }

    #[test]
    fn a_key_inside_an_f_string_avoids_the_f_strings_own_quote() {
        // before python 3.12 an interpolation may not reuse its f-string's
        // quote character, so the key is quoted the other way round
        check("x = f\"{f(a.b=1)}\"\n", "x = f\"{f(**{'a.b': 1})}\"\n");
        check("x = f'{f(a.b=1)}'\n", "x = f'{f(**{\"a.b\": 1})}'\n");
        // and outside one it is the double quote every other string uses
        check("x = f(a.b=1)\n", "x = f(**{\"a.b\": 1})\n");
    }

    #[test]
    fn class_arguments() {
        check(
            "class C(Base, \"x-y\"=1): ...\n",
            "class C(Base, **{\"x-y\": 1}): ...\n",
        );
    }

    #[test]
    fn ordinary_calls_are_untouched() {
        unchanged("f(a, b=1, *args, **kwargs)\n");
    }
}

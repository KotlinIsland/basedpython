//! semantic highlighting for django templates
//!
//! the editor already has a grammar for whatever markup surrounds the template
//! constructs, so this deliberately emits nothing for literal text and confines
//! itself to what a textmate grammar cannot know: whether a tag or filter is
//! django's own or the project's, whether a name is a variable or an attribute
//! of one, and which names the template itself binds.

use ruff_text_size::TextRange;

use crate::semantic_tokens::{SemanticToken, SemanticTokenModifier, SemanticTokenType};

use super::builtins;
use super::index::{BindingOrigin, TemplateIndex};
use super::lexer::TokenKind;

/// the semantic tokens of a template, restricted to `range` when one is given
pub(crate) fn semantic_tokens(
    index: &TemplateIndex,
    source: &str,
    range: Option<TextRange>,
) -> Vec<SemanticToken> {
    // the names a tag introduces get the `definition` modifier, and a
    // `{% block %}`/`{% partialdef %}` name is highlighted as the fragment it
    // names rather than as a variable that happens to sit inside a tag
    let mut definitions: Vec<(TextRange, SemanticTokenType)> = index
        .blocks()
        .iter()
        .chain(index.partials())
        .map(|definition| (definition.name_range, SemanticTokenType::Function))
        .chain(
            index
                .bindings()
                .iter()
                // `forloop` is bound by the `{% for %}` tag itself rather than
                // written anywhere, and its range is the tag's own name — which
                // must keep being highlighted as the tag it is
                .filter(|binding| binding.origin != BindingOrigin::ForLoop)
                .map(|binding| (binding.range, SemanticTokenType::Variable)),
        )
        .collect();
    definitions.sort_unstable_by_key(|(range, _)| range.start());

    index
        .lexed()
        .tokens()
        .iter()
        // a token that merely abuts the requested range does not overlap it
        .filter(|token| {
            range.is_none_or(|range| {
                range
                    .intersect(token.range)
                    .is_some_and(|overlap| !overlap.is_empty())
            })
        })
        .filter_map(|token| {
            let text = &source[token.range];
            let (mut token_type, mut modifiers) = classify(token.kind, text)?;

            if let Ok(found) =
                definitions.binary_search_by_key(&token.range.start(), |(range, _)| range.start())
                && definitions[found].0 == token.range
            {
                token_type = definitions[found].1;
                modifiers |= SemanticTokenModifier::DEFINITION;
            }

            Some(SemanticToken {
                range: token.range,
                token_type,
                modifiers,
            })
        })
        .collect()
}

/// the token type and modifiers a lexed token carries, or `None` when it should
/// be left to the editor's own grammar
fn classify(kind: TokenKind, text: &str) -> Option<(SemanticTokenType, SemanticTokenModifier)> {
    let empty = SemanticTokenModifier::empty();

    Some(match kind {
        // the markup around a construct is the editor's business, not ours
        TokenKind::Text | TokenKind::Unknown => return None,
        TokenKind::Delimiter | TokenKind::Operator => (SemanticTokenType::Operator, empty),
        TokenKind::Comment => (SemanticTokenType::Comment, empty),
        TokenKind::TagName => (
            SemanticTokenType::Keyword,
            default_library(is_builtin_tag(text)),
        ),
        TokenKind::FilterName => (
            SemanticTokenType::Function,
            default_library(builtins::filter(text).is_some()),
        ),
        TokenKind::Variable => (SemanticTokenType::Variable, empty),
        TokenKind::Attribute => (SemanticTokenType::Property, empty),
        TokenKind::KeywordArgument => (SemanticTokenType::Parameter, empty),
        TokenKind::Keyword => (SemanticTokenType::Keyword, empty),
        TokenKind::BuiltinConstant => (SemanticTokenType::BuiltinConstant, empty),
        TokenKind::String => (SemanticTokenType::String, empty),
        TokenKind::Number => (SemanticTokenType::Number, empty),
    })
}

/// whether `name` is one of django's own tags, closing and branch tags included
///
/// the builtin table lists a closing tag only as the `closed_by` of the tag it
/// closes, and a branch tag only as one of its `branches`, but `{% endfor %}` and
/// `{% empty %}` are as much django's as `{% for %}` is.
fn is_builtin_tag(name: &str) -> bool {
    builtins::tag(name).is_some()
        || builtins::TAGS
            .iter()
            .any(|tag| tag.closed_by == Some(name) || tag.branches.contains(&name))
}

fn default_library(is_builtin: bool) -> SemanticTokenModifier {
    if is_builtin {
        SemanticTokenModifier::DEFAULT_LIBRARY
    } else {
        SemanticTokenModifier::empty()
    }
}

#[cfg(test)]
mod tests {
    use ruff_text_size::TextRange;

    use crate::semantic_tokens::SemanticTokenModifier;

    use super::super::index::TemplateIndex;
    use super::semantic_tokens;

    /// render the tokens as `type[+modifier…]:text`
    fn highlight(source: &str) -> Vec<String> {
        let index = TemplateIndex::from_source(source);
        semantic_tokens(&index, source, None)
            .into_iter()
            .map(|token| {
                let names: Vec<_> = SemanticTokenModifier::all_names()
                    .into_iter()
                    .enumerate()
                    .filter(|(bit, _)| token.modifiers.bits() & (1 << bit) != 0)
                    .map(|(_, name)| name)
                    .collect();
                let modifiers = if names.is_empty() {
                    String::new()
                } else {
                    format!("+{}", names.join("+"))
                };

                format!(
                    "{}{modifiers}:{}",
                    token.token_type.as_lsp_concept(),
                    &source[token.range]
                )
            })
            .collect()
    }

    #[test]
    fn literal_markup_is_left_to_the_editors_own_grammar() {
        assert!(highlight("<p class=\"x\">hello</p>").is_empty());
    }

    #[test]
    fn a_variable_expression() {
        assert_eq!(
            highlight("{{ book.title|upper }}"),
            [
                "operator:{{",
                "variable:book",
                "operator:.",
                "property:title",
                "operator:|",
                "function+defaultLibrary:upper",
                "operator:}}",
            ]
        );
    }

    #[test]
    fn a_projects_own_filter_is_not_marked_as_djangos() {
        assert_eq!(
            highlight("{{ x|intcomma }}"),
            [
                "operator:{{",
                "variable:x",
                "operator:|",
                "function:intcomma",
                "operator:}}",
            ]
        );
    }

    #[test]
    fn builtin_tags_and_their_closing_tags_are_both_djangos() {
        assert_eq!(
            highlight("{% spaceless %}{% endspaceless %}"),
            [
                "operator:{%",
                "keyword+defaultLibrary:spaceless",
                "operator:%}",
                "operator:{%",
                "keyword+defaultLibrary:endspaceless",
                "operator:%}",
            ]
        );
    }

    #[test]
    fn a_branch_tag_is_djangos_too() {
        assert_eq!(
            highlight("{% empty %}"),
            ["operator:{%", "keyword+defaultLibrary:empty", "operator:%}"]
        );
    }

    #[test]
    fn a_projects_own_tag_is_not_marked_as_djangos() {
        assert_eq!(
            highlight("{% render_bundle 'main' %}"),
            [
                "operator:{%",
                "keyword:render_bundle",
                "string:'main'",
                "operator:%}",
            ]
        );
    }

    #[test]
    fn a_loop_variable_is_a_definition_where_it_is_bound() {
        assert_eq!(
            highlight("{% for book in books %}{{ book }}{% endfor %}"),
            [
                "operator:{%",
                "keyword+defaultLibrary:for",
                "variable+definition:book",
                "keyword:in",
                "variable:books",
                "operator:%}",
                "operator:{{",
                "variable:book",
                "operator:}}",
                "operator:{%",
                "keyword+defaultLibrary:endfor",
                "operator:%}",
            ]
        );
    }

    #[test]
    fn a_block_name_is_highlighted_as_the_fragment_it_names() {
        assert_eq!(
            highlight("{% block content %}{% endblock %}"),
            [
                "operator:{%",
                "keyword+defaultLibrary:block",
                "function+definition:content",
                "operator:%}",
                "operator:{%",
                "keyword+defaultLibrary:endblock",
                "operator:%}",
            ]
        );
    }

    #[test]
    fn a_partial_definition_and_its_use() {
        assert_eq!(
            highlight("{% partialdef card %}{% endpartialdef %}{% partial card %}"),
            [
                "operator:{%",
                "keyword+defaultLibrary:partialdef",
                "function+definition:card",
                "operator:%}",
                "operator:{%",
                "keyword+defaultLibrary:endpartialdef",
                "operator:%}",
                "operator:{%",
                "keyword+defaultLibrary:partial",
                "variable:card",
                "operator:%}",
            ]
        );
    }

    #[test]
    fn keyword_arguments_strings_numbers_and_constants() {
        assert_eq!(
            highlight("{% include 'a.html' with n=3 flag=True %}"),
            [
                "operator:{%",
                "keyword+defaultLibrary:include",
                "string:'a.html'",
                "keyword:with",
                "parameter:n",
                "operator:=",
                "number:3",
                "parameter:flag",
                "operator:=",
                "builtinConstant:True",
                "operator:%}",
            ]
        );
    }

    #[test]
    fn comments() {
        assert_eq!(
            highlight("{# a #}{% comment %}b{% endcomment %}"),
            [
                "operator:{#",
                "comment: a ",
                "operator:#}",
                "operator:{%",
                "keyword+defaultLibrary:comment",
                "operator:%}",
                "comment:b",
                "operator:{%",
                "keyword+defaultLibrary:endcomment",
                "operator:%}",
            ]
        );
    }

    #[test]
    fn a_range_restricts_the_tokens_to_what_it_touches() {
        let source = "{{ a }}{{ b }}";
        let index = TemplateIndex::from_source(source);
        let end = u32::try_from(source.len()).unwrap();

        let tokens = semantic_tokens(&index, source, Some(TextRange::new(7.into(), end.into())));
        assert_eq!(
            tokens
                .iter()
                .map(|token| &source[token.range])
                .collect::<Vec<_>>(),
            ["{{", "b", "}}"]
        );
    }

    #[test]
    fn tokens_come_back_in_source_order() {
        let source = "{% for x in y %}{{ x.z|upper }}{% endfor %}";
        let index = TemplateIndex::from_source(source);

        let tokens = semantic_tokens(&index, source, None);
        assert!(
            tokens
                .windows(2)
                .all(|pair| pair[0].range.start() <= pair[1].range.start()),
            "the encoder relies on tokens arriving sorted"
        );
    }
}

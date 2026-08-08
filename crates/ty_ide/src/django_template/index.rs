//! the structure a flat template token stream implies
//!
//! block nesting in the django template language is imposed by the tags rather
//! than by the delimiters: `{% for %}` opens a block only because a matching
//! `{% endfor %}` closes it. this module replays the construct stream with a
//! stack to recover that structure, and picks out along the way everything the
//! ide features need to look up by name — the blocks and partials the template
//! defines, the templates it extends and includes, the libraries it loads, and
//! the names its tags bind.
//!
//! the index resolves every name it records against the source while it builds,
//! so nothing downstream needs the source text again to interpret it.

use compact_str::{CompactString, ToCompactString};
use ruff_text_size::{TextRange, TextSize};

use super::builtins;
use super::lexer::{Construct, ConstructKind, Lexed, Token, TokenKind, lex, string_contents};

/// what django names a block tag's closing tag by
pub(crate) const END_TAG_PREFIX: &str = "end";

/// a name a template defines: a `{% block %}` or a `{% partialdef %}`
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Definition {
    pub(crate) name: CompactString,
    /// the name as written in the opening tag
    pub(crate) name_range: TextRange,
    /// the opening tag's `{%` through the closing tag's `%}`, or through the end
    /// of the template when the block was never closed
    pub(crate) full_range: TextRange,
}

/// a reference to another template, from `{% extends %}` or `{% include %}`
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct TemplateReference {
    /// the template's path, as the loader will see it
    pub(crate) name: CompactString,
    /// the fragment after a `#`, naming a partial inside that template
    pub(crate) partial: Option<CompactString>,
    /// the string literal, its quotes excluded
    pub(crate) range: TextRange,
}

/// a `{% load %}`ed library
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Load {
    pub(crate) library: CompactString,
    pub(crate) range: TextRange,
}

/// where a name bound inside the template came from
#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub(crate) enum BindingOrigin {
    /// `{% for book in books %}` — one element of the iterable
    LoopVariable,
    /// django's `forloop`, in scope inside every `{% for %}`
    ForLoop,
    /// `{% with total=x %}` or a trailing `… as total`
    Alias,
}

/// a name a tag binds, and the region of the template it is bound over
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Binding {
    pub(crate) name: CompactString,
    /// the name as written
    pub(crate) range: TextRange,
    /// the region the name is visible in
    pub(crate) scope: TextRange,
    pub(crate) origin: BindingOrigin,
    /// the path the name was bound to, when the tag wrote one
    ///
    /// this is what lets `{% with author=book.author %}` carry `book.author`'s
    /// type onto `author`.
    pub(crate) value: Option<TextRange>,
}

/// a block a pair of tags spans
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Block {
    pub(crate) name: CompactString,
    /// the tag that closes it
    pub(crate) end_tag: CompactString,
    /// the opening tag's whole construct
    pub(crate) open_range: TextRange,
    /// the opening tag's `{%` through the closing tag's `%}`, or through the end
    /// of the template when the block was never closed
    pub(crate) full_range: TextRange,
    /// what the block encloses: between the two tags, or from the opening tag to
    /// the end of the template when the block was never closed
    pub(crate) body_range: TextRange,
    /// whether the closing tag was actually written
    pub(crate) closed: bool,
}

/// a closing tag that closed nothing
///
/// what makes a tag a closing one is that some other tag is waiting for it, so a
/// tag nothing is waiting for is only a *candidate* — the reader decides whether
/// the name is one django would have been waiting for.
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Stray {
    pub(crate) name: CompactString,
    /// the whole `{% end… %}` construct
    pub(crate) range: TextRange,
}

/// a template, lexed and indexed
///
/// every collection is boxed: the index is a salsa-cached value, and a `Vec`
/// built by pushing would hold on to as much as half again its length in spare
/// capacity for as long as the file stays open.
#[derive(Debug, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct TemplateIndex {
    lexed: Lexed,
    extends: Option<TemplateReference>,
    extends_unresolved: bool,
    includes: Box<[TemplateReference]>,
    blocks: Box<[Definition]>,
    partials: Box<[Definition]>,
    loads: Box<[Load]>,
    bindings: Box<[Binding]>,
    spans: Box<[Block]>,
    strays: Box<[Stray]>,
}

impl TemplateIndex {
    pub(crate) fn from_source(source: &str) -> Self {
        Builder::new(source).build()
    }

    pub(crate) fn lexed(&self) -> &Lexed {
        &self.lexed
    }

    /// the template this one extends
    pub(crate) fn extends(&self) -> Option<&TemplateReference> {
        self.extends.as_ref()
    }

    /// whether the template extends something no name can be read off
    ///
    /// `{% extends parent %}` picks its base at render time, so this template's
    /// place in the inheritance tree is not knowable — which is a different
    /// thing from extending nothing, and the difference matters to anything that
    /// has to be sure it has found every template in a family.
    pub(crate) fn extends_unresolved(&self) -> bool {
        self.extends_unresolved
    }

    /// every template this one includes
    pub(crate) fn includes(&self) -> &[TemplateReference] {
        &self.includes
    }

    /// the `{% block %}`s this template defines, in source order
    pub(crate) fn blocks(&self) -> &[Definition] {
        &self.blocks
    }

    /// the `{% partialdef %}`s this template defines, in source order
    pub(crate) fn partials(&self) -> &[Definition] {
        &self.partials
    }

    pub(crate) fn loads(&self) -> &[Load] {
        &self.loads
    }

    /// every name bound by a tag, in source order
    pub(crate) fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// the names in scope at `offset`, in source order
    pub(crate) fn bindings_at(&self, offset: TextSize) -> impl Iterator<Item = &Binding> {
        self.bindings
            .iter()
            .filter(move |binding| binding.scope.contains_inclusive(offset))
    }

    /// the binding `name` resolves to at `offset`
    ///
    /// the innermost one wins, and since a nested tag is always written after the
    /// tag enclosing it, that is the last one in source order.
    pub(crate) fn resolve_binding(&self, name: &str, offset: TextSize) -> Option<&Binding> {
        self.bindings_at(offset)
            .filter(|binding| binding.name == name)
            .last()
    }

    /// every block a pair of tags spans, in source order
    pub(crate) fn spans(&self) -> &[Block] {
        &self.spans
    }

    /// every `{% end… %}` that closed nothing, in source order
    pub(crate) fn strays(&self) -> &[Stray] {
        &self.strays
    }

    /// the block tags still open at `offset`, innermost first
    ///
    /// this is what tells a completion inside `{% for %}…{% |` that the tag it
    /// should offer is `endfor`. a cursor in either of the block's own tags is
    /// not inside it, so `{% endif %}` never offers itself a second time.
    pub(crate) fn open_blocks_at(&self, offset: TextSize) -> Vec<&Block> {
        let mut open: Vec<&Block> = self
            .spans
            .iter()
            .filter(|block| block.body_range.contains_inclusive(offset))
            .collect();

        open.sort_by_key(|block| std::cmp::Reverse(block.open_range.start()));
        open
    }
}

struct Builder<'src> {
    source: &'src str,
    lexed: Lexed,
}

impl<'src> Builder<'src> {
    fn new(source: &'src str) -> Self {
        Self {
            source,
            lexed: lex(source),
        }
    }

    fn build(self) -> TemplateIndex {
        let end_tags = self.end_tags_present();

        let mut extends = None;
        let mut extends_unresolved = false;
        let mut includes = Vec::new();
        let mut blocks = Vec::new();
        let mut partials = Vec::new();
        let mut loads = Vec::new();
        let mut bindings: Vec<Binding> = Vec::new();
        let mut spans = Vec::new();
        let mut strays = Vec::new();

        let mut stack: Vec<Frame> = Vec::new();
        let template_end = TextSize::try_from(self.source.len()).unwrap_or_default();

        for construct in self.lexed.constructs() {
            if construct.kind != ConstructKind::Tag {
                continue;
            }
            let Some(name_range) = construct.name else {
                continue;
            };
            let name = &self.source[name_range];
            let arguments = self.arguments(construct);

            // a closing tag ends the innermost block it can close. a stray one
            // closes nothing rather than unwinding the whole stack, so that a
            // half-typed template keeps the structure it does have.
            if let Some(position) = stack.iter().rposition(|frame| frame.end_tag == name) {
                for frame in stack.split_off(position).into_iter().rev() {
                    frame.close(
                        construct.range,
                        &mut blocks,
                        &mut partials,
                        &mut bindings,
                        &mut spans,
                    );
                }
                continue;
            }

            if name.starts_with(END_TAG_PREFIX) {
                strays.push(Stray {
                    name: name.to_compact_string(),
                    range: construct.range,
                });
            }

            match name {
                "extends" if extends.is_none() && !extends_unresolved => {
                    match arguments
                        .iter()
                        .find_map(|token| self.template_reference(token))
                    {
                        Some(reference) => extends = Some(reference),
                        None => extends_unresolved = true,
                    }
                }
                "include" => {
                    if let Some(reference) = arguments
                        .iter()
                        .find_map(|token| self.template_reference(token))
                    {
                        includes.push(reference);
                    }
                }
                "load" => loads.extend(self.loaded_libraries(arguments)),
                _ => {}
            }

            // every name a tag binds starts out scoped to the end of the
            // template; the enclosing block narrows it when it closes
            let first_binding = bindings.len();
            bindings.extend(self.bindings_of(name, arguments, construct, template_end));

            let definition =
                self.definition_of(name, arguments, construct)
                    .map(|(kind, definition)| {
                        let target = match kind {
                            DefinitionKind::Block => &mut blocks,
                            DefinitionKind::Partial => &mut partials,
                        };
                        target.push(definition);
                        (kind, target.len() - 1)
                    });

            let end_tag = builtins::end_tag_for(name)
                .map(ToCompactString::to_compact_string)
                .or_else(|| {
                    // a project's own block tag is recognised by the closing tag
                    // it is actually paired with in this file. this needs no
                    // knowledge of the tag's python definition, which is what
                    // makes unknown libraries behave.
                    let candidate = format!("{END_TAG_PREFIX}{name}");
                    end_tags
                        .contains(&candidate.as_str())
                        .then(|| candidate.to_compact_string())
                });

            if let Some(end_tag) = end_tag {
                stack.push(Frame {
                    name: name.to_compact_string(),
                    end_tag,
                    open_range: construct.range,
                    definition,
                    first_binding,
                });
            }
        }

        // whatever is still open runs to the end of the template
        for frame in std::mem::take(&mut stack).into_iter().rev() {
            frame.close_unclosed(
                template_end,
                &mut blocks,
                &mut partials,
                &mut bindings,
                &mut spans,
            );
        }

        spans.sort_by_key(|block| block.open_range.start());

        TemplateIndex {
            lexed: self.lexed,
            extends,
            extends_unresolved,
            includes: includes.into_boxed_slice(),
            blocks: blocks.into_boxed_slice(),
            partials: partials.into_boxed_slice(),
            loads: loads.into_boxed_slice(),
            bindings: bindings.into_boxed_slice(),
            spans: spans.into_boxed_slice(),
            strays: strays.into_boxed_slice(),
        }
    }

    /// every `end…` tag name written anywhere in the template
    fn end_tags_present(&self) -> Vec<&'src str> {
        let mut names: Vec<_> = self
            .lexed
            .constructs()
            .iter()
            .filter_map(|construct| construct.name)
            .map(|range| &self.source[range])
            .filter(|name| name.starts_with(END_TAG_PREFIX))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// a construct's tokens with its delimiters and its tag name dropped
    fn arguments(&self, construct: &Construct) -> &[Token] {
        let tokens = self.lexed.inner_tokens(construct);

        // the tag name is always the first token inside the delimiters
        match tokens.split_first() {
            Some((first, rest)) if first.kind == TokenKind::TagName => rest,
            _ => tokens,
        }
    }

    /// the block or partial `construct` defines
    fn definition_of(
        &self,
        tag: &str,
        arguments: &[Token],
        construct: &Construct,
    ) -> Option<(DefinitionKind, Definition)> {
        let kind = match tag {
            "block" => DefinitionKind::Block,
            "partialdef" => DefinitionKind::Partial,
            _ => return None,
        };

        let name_token = arguments
            .iter()
            .find(|token| matches!(token.kind, TokenKind::Variable | TokenKind::String))?;
        let (name, name_range) = self.name_of(name_token)?;

        Some((
            kind,
            Definition {
                name,
                name_range,
                full_range: construct.range,
            },
        ))
    }

    /// the template a `{% extends %}`/`{% include %}` argument names
    fn template_reference(&self, token: &Token) -> Option<TemplateReference> {
        let (value, range) = self.string_value(token)?;
        // `template.html#fragment` addresses a partial inside that template
        let (name, partial) = match value.split_once('#') {
            Some((name, partial)) => (name, Some(partial.to_compact_string())),
            None => (value, None),
        };
        Some(TemplateReference {
            name: name.to_compact_string(),
            partial,
            range,
        })
    }

    /// the libraries a `{% load %}` names
    ///
    /// `{% load a b %}` loads two libraries, while `{% load a b from c %}` loads
    /// only `c` and pulls the names `a` and `b` out of it.
    fn loaded_libraries(&self, arguments: &[Token]) -> Vec<Load> {
        let from = arguments.iter().position(|token| {
            token.kind == TokenKind::Keyword && &self.source[token.range] == "from"
        });

        let libraries: &[Token] = match from {
            Some(index) => arguments.get(index + 1..).unwrap_or_default(),
            None => arguments,
        };

        libraries
            .iter()
            .filter(|token| token.kind == TokenKind::Variable)
            .map(|token| Load {
                library: self.source[token.range].to_compact_string(),
                range: token.range,
            })
            .collect()
    }

    /// the names `construct` binds, scoped to the end of the template
    ///
    /// the caller narrows the scope to the enclosing block once its closing tag
    /// turns up.
    fn bindings_of(
        &self,
        tag: &str,
        arguments: &[Token],
        construct: &Construct,
        scope_end: TextSize,
    ) -> Vec<Binding> {
        let mut bindings = Vec::new();
        let body = TextRange::new(construct.range.end(), scope_end.max(construct.range.end()));

        if tag == "for" {
            let iterable_start = arguments.iter().position(|token| {
                token.kind == TokenKind::Keyword && &self.source[token.range] == "in"
            });
            let targets = arguments
                .get(..iterable_start.unwrap_or(arguments.len()))
                .unwrap_or_default();
            let iterable = iterable_start.and_then(|index| self.path_at(arguments, index + 1));

            for token in targets
                .iter()
                .filter(|token| token.kind == TokenKind::Variable)
            {
                bindings.push(Binding {
                    name: self.source[token.range].to_compact_string(),
                    range: token.range,
                    scope: body,
                    origin: BindingOrigin::LoopVariable,
                    value: iterable,
                });
            }

            bindings.push(Binding {
                name: "forloop".to_compact_string(),
                range: construct.name.unwrap_or(construct.range),
                scope: body,
                origin: BindingOrigin::ForLoop,
                value: None,
            });
        }

        // `{% with total=x %}` binds over its block. `{% include … with x=y %}`
        // binds only inside the included template, so it must not appear here.
        if matches!(tag, "with" | "blocktranslate" | "blocktrans") {
            for (index, token) in arguments.iter().enumerate() {
                if token.kind != TokenKind::KeywordArgument {
                    continue;
                }
                bindings.push(Binding {
                    name: self.source[token.range].to_compact_string(),
                    range: token.range,
                    scope: body,
                    origin: BindingOrigin::Alias,
                    // skip the `=` to reach the value
                    value: self.path_at(arguments, index + 2),
                });
            }
        }

        // a trailing `… as name` binds in every tag that has one
        if let Some(index) = arguments.iter().rposition(|token| {
            token.kind == TokenKind::Keyword && &self.source[token.range] == "as"
        }) && let Some(target) = arguments.get(index + 1)
            && target.kind == TokenKind::Variable
        {
            // `{% with x as total %}` writes the value first, unlike every other
            // tag, whose `as` target names the tag's own result
            let value = (tag == "with")
                .then(|| self.path_at(arguments, 0))
                .flatten();

            bindings.push(Binding {
                name: self.source[target.range].to_compact_string(),
                range: target.range,
                scope: body,
                origin: BindingOrigin::Alias,
                value,
            });
        }

        bindings
    }

    /// the whole dotted path starting at `index`, as in `book.author.name`
    fn path_at(&self, arguments: &[Token], index: usize) -> Option<TextRange> {
        let first = arguments.get(index)?;
        if !matches!(
            first.kind,
            TokenKind::Variable | TokenKind::String | TokenKind::Number
        ) {
            return None;
        }

        let mut end = first.range.end();
        let mut cursor = index + 1;
        while let (Some(dot), Some(segment)) = (arguments.get(cursor), arguments.get(cursor + 1)) {
            if dot.kind != TokenKind::Operator
                || &self.source[dot.range] != "."
                || segment.kind != TokenKind::Attribute
            {
                break;
            }
            end = segment.range.end();
            cursor += 2;
        }

        Some(TextRange::new(first.range.start(), end))
    }

    /// a name written either bare or as a string literal
    fn name_of(&self, token: &Token) -> Option<(CompactString, TextRange)> {
        match token.kind {
            TokenKind::String => self
                .string_value(token)
                .map(|(value, range)| (value.to_compact_string(), range)),
            _ => Some((self.source[token.range].to_compact_string(), token.range)),
        }
    }

    /// the contents of a string literal, and its range with the quotes excluded
    fn string_value(&self, token: &Token) -> Option<(&'src str, TextRange)> {
        if token.kind != TokenKind::String {
            return None;
        }

        let range = string_contents(self.source, token.range);
        (range != token.range).then(|| (&self.source[range], range))
    }
}

#[derive(Debug, Clone, Copy)]
enum DefinitionKind {
    Block,
    Partial,
}

/// an open block, and everything whose extent its closing tag settles
///
/// the definition the block declares and the bindings made inside it are already
/// recorded, in declaration order; the frame only remembers where they are so it
/// can trim their extents down to the block when the closing tag turns up.
struct Frame {
    name: CompactString,
    end_tag: CompactString,
    open_range: TextRange,
    definition: Option<(DefinitionKind, usize)>,
    first_binding: usize,
}

impl Frame {
    /// close the block at the `{% end… %}` tag spanning `close_range`
    fn close(
        self,
        close_range: TextRange,
        blocks: &mut [Definition],
        partials: &mut [Definition],
        bindings: &mut [Binding],
        spans: &mut Vec<Block>,
    ) {
        let body_range = TextRange::new(
            self.open_range.end(),
            close_range.start().max(self.open_range.end()),
        );
        self.finish(
            close_range.end(),
            body_range,
            true,
            blocks,
            partials,
            bindings,
            spans,
        );
    }

    /// close a block whose `{% end… %}` tag was never written
    fn close_unclosed(
        self,
        template_end: TextSize,
        blocks: &mut [Definition],
        partials: &mut [Definition],
        bindings: &mut [Binding],
        spans: &mut Vec<Block>,
    ) {
        let end = template_end.max(self.open_range.end());
        let body_range = TextRange::new(self.open_range.end(), end);
        self.finish(end, body_range, false, blocks, partials, bindings, spans);
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the four output collections are the frame's whole purpose; bundling them into a \
                  struct would only rename them"
    )]
    fn finish(
        self,
        end: TextSize,
        body_range: TextRange,
        closed: bool,
        blocks: &mut [Definition],
        partials: &mut [Definition],
        bindings: &mut [Binding],
        spans: &mut Vec<Block>,
    ) {
        let full_range = TextRange::new(self.open_range.start(), end.max(self.open_range.end()));

        if let Some((kind, index)) = self.definition {
            let definition = match kind {
                DefinitionKind::Block => &mut blocks[index],
                DefinitionKind::Partial => &mut partials[index],
            };
            definition.full_range = full_range;
        }

        // a binding declared inside this block dies with it — at the closing tag
        // rather than after it, so that a cursor on the `{% endfor %}`'s heels no
        // longer sees the loop variable. an inner block has already trimmed its
        // own bindings to something tighter, so this only ever narrows.
        for binding in &mut bindings[self.first_binding..] {
            let scope_end = binding
                .scope
                .end()
                .min(body_range.end())
                .max(binding.scope.start());
            binding.scope = TextRange::new(binding.scope.start(), scope_end);
        }

        spans.push(Block {
            name: self.name,
            end_tag: self.end_tag,
            open_range: self.open_range,
            full_range,
            body_range,
            closed,
        });
    }
}

#[cfg(test)]
mod tests {
    use ruff_text_size::TextSize;

    use super::{BindingOrigin, TemplateIndex};

    fn index(source: &str) -> TemplateIndex {
        TemplateIndex::from_source(source)
    }

    fn offset_of(source: &str, needle: &str) -> TextSize {
        TextSize::try_from(source.find(needle).expect("needle to be in the source")).unwrap()
    }

    #[test]
    fn extends_and_includes() {
        let source = "{% extends 'base.html' %}{% include \"card.html\" %}{% include x %}";
        let index = index(source);

        assert_eq!(
            index.extends().map(|reference| reference.name.as_str()),
            Some("base.html")
        );
        assert_eq!(
            index
                .includes()
                .iter()
                .map(|reference| reference.name.as_str())
                .collect::<Vec<_>>(),
            ["card.html"],
            "a variable include names no template statically"
        );
    }

    #[test]
    fn an_extends_that_names_no_template_is_told_apart_from_no_extends_at_all() {
        assert!(!index("{% block a %}{% endblock %}").extends_unresolved());

        let dynamic = index("{% extends parent %}");
        assert!(dynamic.extends().is_none());
        assert!(dynamic.extends_unresolved());
    }

    #[test]
    fn a_reference_can_address_a_partial_inside_a_template() {
        let index = index("{% include 'blog.html#comment-item' %}");
        let reference = &index.includes()[0];
        assert_eq!(reference.name, "blog.html");
        assert_eq!(reference.partial.as_deref(), Some("comment-item"));
    }

    #[test]
    fn a_reference_range_excludes_the_quotes() {
        let source = "{% extends 'base.html' %}";
        let index = index(source);
        assert_eq!(&source[index.extends().unwrap().range], "base.html");
    }

    #[test]
    fn blocks_span_from_their_opening_tag_to_their_closing_one() {
        let source = "{% block content %}hi{% endblock %}";
        let index = index(source);

        let block = &index.blocks()[0];
        assert_eq!(block.name, "content");
        assert_eq!(&source[block.name_range], "content");
        assert_eq!(&source[block.full_range], source);
    }

    #[test]
    fn an_unclosed_block_runs_to_the_end_of_the_template() {
        let source = "{% block content %}hi";
        let index = index(source);
        assert_eq!(&source[index.blocks()[0].full_range], source);
    }

    #[test]
    fn nested_blocks_are_all_recorded() {
        let source = "{% block outer %}{% block inner %}{% endblock %}{% endblock %}";
        let index = index(source);
        assert_eq!(
            index
                .blocks()
                .iter()
                .map(|block| block.name.as_str())
                .collect::<Vec<_>>(),
            ["outer", "inner"]
        );
    }

    #[test]
    fn partials_are_kept_apart_from_blocks() {
        let source = "{% partialdef card inline %}x{% endpartialdef %}{% block b %}{% endblock %}";
        let index = index(source);

        assert_eq!(
            index
                .partials()
                .iter()
                .map(|partial| partial.name.as_str())
                .collect::<Vec<_>>(),
            ["card"]
        );
        assert_eq!(
            index
                .blocks()
                .iter()
                .map(|block| block.name.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
    }

    #[test]
    fn loads() {
        let index = index("{% load static i18n %}{% load humanize %}");
        assert_eq!(
            index
                .loads()
                .iter()
                .map(|load| load.library.as_str())
                .collect::<Vec<_>>(),
            ["static", "i18n", "humanize"]
        );
    }

    #[test]
    fn load_from_names_only_the_library_it_pulls_from() {
        let index = index("{% load bar baz from foo %}");
        assert_eq!(
            index
                .loads()
                .iter()
                .map(|load| load.library.as_str())
                .collect::<Vec<_>>(),
            ["foo"]
        );
    }

    #[test]
    fn a_for_loop_binds_its_targets_and_forloop_over_its_body() {
        let source = "{% for book in books %}x{% endfor %}after";
        let index = index(source);

        let inside = offset_of(source, "x");
        let names: Vec<_> = index
            .bindings_at(inside)
            .map(|binding| binding.name.as_str())
            .collect();
        assert_eq!(names, ["book", "forloop"]);

        let after = offset_of(source, "after");
        assert_eq!(index.bindings_at(after).count(), 0);
    }

    #[test]
    fn a_loop_binding_remembers_the_whole_iterable_path() {
        let source = "{% for book in shelf.books.all %}x{% endfor %}";
        let index = index(source);

        let binding = index
            .bindings()
            .iter()
            .find(|binding| binding.name == "book")
            .unwrap();
        assert_eq!(binding.origin, BindingOrigin::LoopVariable);
        assert_eq!(
            binding.value.map(|range| &source[range]),
            Some("shelf.books.all")
        );
    }

    #[test]
    fn unpacking_a_loop_binds_every_target() {
        let source = "{% for key, value in mapping.items %}x{% endfor %}";
        let index = index(source);

        let names: Vec<_> = index
            .bindings_at(offset_of(source, "x"))
            .map(|binding| binding.name.as_str())
            .collect();
        assert_eq!(names, ["key", "value", "forloop"]);
    }

    #[test]
    fn with_binds_each_of_its_keyword_arguments() {
        let source = "{% with total=cart.total name=user.name %}x{% endwith %}";
        let index = index(source);

        let bindings: Vec<_> = index
            .bindings_at(offset_of(source, "x"))
            .map(|binding| {
                (
                    binding.name.as_str(),
                    binding.value.map(|range| &source[range]),
                )
            })
            .collect();
        assert_eq!(
            bindings,
            [("total", Some("cart.total")), ("name", Some("user.name"))]
        );
    }

    #[test]
    fn the_older_with_spelling_writes_its_value_first() {
        let source = "{% with cart.total as total %}x{% endwith %}";
        let index = index(source);

        let binding = index
            .resolve_binding("total", offset_of(source, "x"))
            .unwrap();
        assert_eq!(
            binding.value.map(|range| &source[range]),
            Some("cart.total")
        );
    }

    #[test]
    fn a_trailing_as_binds_the_tag_result() {
        let source = "{% url 'detail' pk=1 as detail_url %}{{ detail_url }}";
        let index = index(source);

        let names: Vec<_> = index
            .bindings_at(offset_of(source, "{{ detail_url"))
            .map(|binding| binding.name.as_str())
            .collect();
        assert_eq!(names, ["detail_url"]);
    }

    #[test]
    fn an_as_binding_inside_a_block_does_not_escape_it() {
        let source = "{% if x %}{% url 'a' as u %}in{% endif %}out";
        let index = index(source);

        assert_eq!(index.bindings_at(offset_of(source, "in")).count(), 1);
        assert_eq!(index.bindings_at(offset_of(source, "out")).count(), 0);
    }

    #[test]
    fn an_inner_binding_shadows_an_outer_one() {
        let source = "{% for x in a %}{% for x in b %}HERE{% endfor %}{% endfor %}";
        let index = index(source);

        let binding = index
            .resolve_binding("x", offset_of(source, "HERE"))
            .unwrap();
        assert_eq!(binding.value.map(|range| &source[range]), Some("b"));
    }

    #[test]
    fn open_blocks_are_reported_innermost_first() {
        let source = "{% for a in b %}{% if c %}HERE{% endif %}{% endfor %}";
        let index = index(source);

        assert_eq!(
            index
                .open_blocks_at(offset_of(source, "HERE"))
                .iter()
                .map(|block| (block.name.as_str(), block.end_tag.as_str()))
                .collect::<Vec<_>>(),
            [("if", "endif"), ("for", "endfor")]
        );
    }

    #[test]
    fn a_closed_block_is_no_longer_open() {
        let source = "{% if c %}{% endif %}HERE";
        let index = index(source);
        assert!(index.open_blocks_at(offset_of(source, "HERE")).is_empty());
    }

    #[test]
    fn an_unclosed_block_is_open_for_the_rest_of_the_template() {
        let source = "{% if c %}HERE";
        let index = index(source);

        let open = index.open_blocks_at(offset_of(source, "HERE"));
        assert_eq!(open.len(), 1);
        assert!(!open[0].closed);
    }

    #[test]
    fn a_project_block_tag_is_recognised_by_the_end_tag_it_is_paired_with() {
        // `{% modal %}` is nobody's builtin; the `{% endmodal %}` in the file is
        // the only evidence that it opens a block, and it is enough
        let source = "{% modal %}HERE{% endmodal %}";
        let index = index(source);

        assert_eq!(
            index
                .open_blocks_at(offset_of(source, "HERE"))
                .iter()
                .map(|block| block.name.as_str())
                .collect::<Vec<_>>(),
            ["modal"]
        );
    }

    #[test]
    fn a_tag_with_no_matching_end_tag_opens_no_block() {
        let source = "{% csrf_token %}HERE";
        let index = index(source);
        assert!(index.open_blocks_at(offset_of(source, "HERE")).is_empty());
    }

    #[test]
    fn a_stray_closing_tag_closes_nothing() {
        let source = "{% for a in b %}{% endwith %}HERE{% endfor %}";
        let index = index(source);

        assert_eq!(
            index
                .open_blocks_at(offset_of(source, "HERE"))
                .iter()
                .map(|block| block.name.as_str())
                .collect::<Vec<_>>(),
            ["for"]
        );
    }

    #[test]
    fn a_branch_tag_does_not_close_its_block() {
        let source = "{% if a %}{% else %}HERE{% endif %}";
        let index = index(source);

        assert_eq!(
            index
                .open_blocks_at(offset_of(source, "HERE"))
                .iter()
                .map(|block| block.name.as_str())
                .collect::<Vec<_>>(),
            ["if"]
        );
    }

    #[test]
    fn a_quoted_block_name_is_unquoted() {
        let source = "{% block \"content\" %}{% endblock %}";
        let index = index(source);
        assert_eq!(index.blocks()[0].name, "content");
        assert_eq!(&source[index.blocks()[0].name_range], "content");
    }

    #[test]
    fn an_empty_template_indexes_to_nothing() {
        let index = index("");
        assert!(index.blocks().is_empty());
        assert!(index.bindings().is_empty());
        assert!(index.extends().is_none());
    }
}

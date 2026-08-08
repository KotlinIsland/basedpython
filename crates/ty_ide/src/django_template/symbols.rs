//! the outline of a django template
//!
//! what a template declares is its `{% block %}`s and its `{% partialdef %}`s:
//! the names a child template can override and the fragments a `{% partial %}`
//! can render. everything else in the file is markup, which the editor outlines
//! far better than this could.

use ruff_text_size::TextRange;

use crate::{SymbolInfo, SymbolKind};

use super::index::{Definition, TemplateIndex};

/// one entry of a template's outline
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// the name as written in the opening tag
    pub name_range: TextRange,
    /// the opening tag's `{%` through the closing tag's `%}`
    pub full_range: TextRange,
    /// the declarations written inside this one
    pub children: Vec<TemplateSymbol>,
}

impl TemplateSymbol {
    /// this symbol on its own, for a client that wants a flat list
    pub fn symbol_info(&self) -> SymbolInfo<'_> {
        SymbolInfo {
            name: self.name.as_str().into(),
            kind: self.kind,
            deprecated: false,
            imported_from: None,
            name_range: self.name_range,
            full_range: self.full_range,
        }
    }
}

/// the declarations of `index`, nested as they are written
pub(crate) fn document_symbols(index: &TemplateIndex) -> Vec<TemplateSymbol> {
    let mut definitions: Vec<(SymbolKind, &Definition)> = index
        .blocks()
        .iter()
        .map(|block| (SymbolKind::Module, block))
        .chain(
            index
                .partials()
                .iter()
                .map(|partial| (SymbolKind::Function, partial)),
        )
        .collect();

    // an enclosing declaration starts first, and starts at the same offset as
    // nothing else, so ordering by start alone puts every parent before its
    // children
    definitions.sort_unstable_by_key(|(_, definition)| definition.full_range.start());

    let mut roots = Vec::new();
    let mut open: Vec<TemplateSymbol> = Vec::new();

    for (kind, definition) in definitions {
        while open
            .last()
            .is_some_and(|enclosing| !enclosing.full_range.contains_range(definition.full_range))
        {
            let Some(finished) = open.pop() else { break };
            attach(finished, &mut open, &mut roots);
        }

        open.push(TemplateSymbol {
            name: definition.name.to_string(),
            kind,
            name_range: definition.name_range,
            full_range: definition.full_range,
            children: Vec::new(),
        });
    }

    while let Some(finished) = open.pop() {
        attach(finished, &mut open, &mut roots);
    }

    roots
}

/// hand a finished symbol to the declaration enclosing it, or to the outline
fn attach(symbol: TemplateSymbol, open: &mut [TemplateSymbol], roots: &mut Vec<TemplateSymbol>) {
    match open.last_mut() {
        Some(enclosing) => enclosing.children.push(symbol),
        None => roots.push(symbol),
    }
}

#[cfg(test)]
mod tests {
    use crate::django_template::tests::TemplateTest;

    fn template(source: &str) -> TemplateTest {
        TemplateTest::new(&[("blog/templates/blog/post.html", source)])
    }

    #[test]
    fn a_template_outlines_its_blocks() {
        let test = template(
            "<CURSOR>{% block content %}a{% endblock %}\n{% block footer %}b{% endblock %}",
        );
        assert_eq!(test.symbols(), ["Module content", "Module footer"]);
    }

    #[test]
    fn a_block_inside_a_block_nests_under_it() {
        let test =
            template("<CURSOR>{% block content %}{% block inner %}a{% endblock %}{% endblock %}");
        assert_eq!(test.symbols(), ["Module content", "  Module inner"]);
    }

    #[test]
    fn a_partialdef_is_outlined_too() {
        let test = template("<CURSOR>{% partialdef card %}a{% endpartialdef %}");
        assert_eq!(test.symbols(), ["Function card"]);
    }

    #[test]
    fn a_partialdef_inside_a_block_nests_under_it() {
        let test = template(
            "<CURSOR>{% block content %}{% partialdef card %}a{% endpartialdef %}{% endblock %}",
        );
        assert_eq!(test.symbols(), ["Module content", "  Function card"]);
    }

    #[test]
    fn a_block_that_was_never_closed_is_still_outlined() {
        let test = template("<CURSOR>{% block content %}a");
        assert_eq!(test.symbols(), ["Module content"]);
    }

    #[test]
    fn a_template_with_no_declarations_has_no_outline() {
        assert!(
            template("<CURSOR><p>{{ book.title }}</p>")
                .symbols()
                .is_empty()
        );
    }
}

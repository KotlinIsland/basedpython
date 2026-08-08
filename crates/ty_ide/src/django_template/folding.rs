//! folding ranges for django templates
//!
//! a template's foldable regions are exactly its blocks — `{% for %}` to
//! `{% endfor %}`, `{% block %}` to `{% endblock %}`, and whatever the project's
//! own block tags are — which the index has already paired up.

use crate::FoldingRange;

use super::index::TemplateIndex;

/// one foldable range per block `index` spans
pub(crate) fn folding_ranges(index: &TemplateIndex, source: &str) -> Vec<FoldingRange> {
    index
        .spans()
        .iter()
        // a block whose closing tag has not been typed yet runs to the end of the
        // template, and folding the rest of the file away is not what the user is
        // in the middle of asking for
        .filter(|block| block.closed)
        .map(|block| block.full_range)
        .filter(|range| {
            let text = &source[*range];
            text.contains('\n') || text.contains('\r')
        })
        .map(FoldingRange::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::django_template::tests::TemplateTest;

    fn template(source: &str) -> TemplateTest {
        TemplateTest::new(&[("blog/templates/blog/post.html", source)])
    }

    #[test]
    fn a_block_tag_folds_from_its_opening_tag_to_its_closing_one() {
        let test = template(
            "<CURSOR>{% block content %}\n\
             <p>a</p>\n\
             {% endblock %}\n",
        );

        assert_eq!(test.folds(), ["{% block content %} … {% endblock %}"]);
    }

    #[test]
    fn every_block_tag_folds() {
        let test = template(
            "<CURSOR>{% for book in books %}\n\
             {% if book %}\n\
             a\n\
             {% endif %}\n\
             {% endfor %}\n",
        );

        assert_eq!(
            test.folds(),
            [
                "{% for book in books %} … {% endfor %}",
                "{% if book %} … {% endif %}"
            ]
        );
    }

    #[test]
    fn a_block_written_on_one_line_does_not_fold() {
        assert!(
            template("<CURSOR>{% block content %}a{% endblock %}\n")
                .folds()
                .is_empty()
        );
    }

    #[test]
    fn a_block_that_was_never_closed_does_not_fold() {
        assert!(
            template("<CURSOR>{% block content %}\na\n")
                .folds()
                .is_empty()
        );
    }
}

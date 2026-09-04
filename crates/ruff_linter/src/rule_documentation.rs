//! The prose a rule is explained with, rendered once for everyone who asks.
//!
//! `buff rule F401` and the editor's *Explain Rule* are the same question, so they are the same
//! answer: one renderer here, in the crate that owns the rules, rather than one in the CLI and a
//! second wherever else the text was wanted.

use std::fmt::Write as _;

use crate::FixAvailability;
use crate::registry::{Linter, Rule, RuleNamespace};

/// The markdown explaining `rule`: what it is, where it came from, and what it does about it.
///
/// Markdown because a rule's own `explanation` is written in it — the rest is built to match, so a
/// rule with documentation and one without read as the same kind of document.
pub fn rule_documentation(rule: Rule) -> String {
    let mut output = String::new();
    let _ = write!(&mut output, "# {}", rule.name_and_code());
    output.push('\n');
    output.push('\n');

    if let Some(linter) = rule
        .noqa_code()
        .and_then(|code| Linter::parse_code(&code.to_string()).map(|(linter, _)| linter))
    {
        let _ = write!(
            &mut output,
            "Derived from the **{}** linter.",
            linter.name()
        );
        output.push('\n');
        output.push('\n');
    }

    let fix_availability = rule.fixable();
    if matches!(
        fix_availability,
        FixAvailability::Always | FixAvailability::Sometimes
    ) {
        output.push_str(&fix_availability.to_string());
        output.push('\n');
        output.push('\n');
    }

    if rule.is_preview() {
        output.push_str(
            r"This rule is in preview and is not stable. The `--preview` flag is required for use.",
        );
        output.push('\n');
        output.push('\n');
    }

    if let Some(explanation) = rule.explanation() {
        output.push_str(explanation.trim());
    } else {
        // Not every rule carries prose. The formats it reports under say what it looks for, which
        // is more use than an empty document.
        output.push_str("Message formats:");
        for format in rule.message_formats() {
            output.push('\n');
            let _ = write!(&mut output, "* {format}");
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::IntoEnumIterator;

    #[test]
    fn every_rule_renders_a_document_naming_itself() {
        for rule in Rule::iter() {
            let doc = rule_documentation(rule);
            assert!(
                doc.starts_with(&format!("# {}", rule.name_and_code())),
                "{} did not lead with its own name and code",
                rule.name()
            );
            assert!(
                rule.noqa_code().is_none() || doc.contains("linter."),
                "{} did not say which linter it came from",
                rule.name()
            );
        }
    }

    /// A rule with no prose still has to say what it looks for, rather than trailing off.
    #[test]
    fn a_rule_without_an_explanation_lists_its_message_formats() {
        let undocumented = Rule::iter().find(|rule| rule.explanation().is_none());
        let Some(rule) = undocumented else {
            return; // every rule is documented, which is the better problem to have
        };
        assert!(rule_documentation(rule).contains("Message formats:"));
    }
}

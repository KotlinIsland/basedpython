use std::io::{self, BufWriter, Write};

use anyhow::Result;
use serde::ser::SerializeSeq;
use serde::{Serialize, Serializer};
use strum::IntoEnumIterator;

use ruff_linter::FixAvailability;
use ruff_linter::codes::RuleGroup;
use ruff_linter::registry::{Linter, Rule, RuleNamespace};
use ruff_linter::rule_documentation;

use crate::args::HelpFormat;

#[derive(Serialize)]
struct Explanation<'a> {
    name: &'a str,
    code: String,
    linter: &'a str,
    summary: &'a str,
    message_formats: &'a [&'a str],
    fix: String,
    fix_availability: FixAvailability,
    #[expect(clippy::struct_field_names)]
    explanation: Option<&'a str>,
    preview: bool,
    status: RuleGroup,
    source_location: SourceLocation,
}

impl<'a> Explanation<'a> {
    fn from_rule(rule: &'a Rule) -> Self {
        let code = rule.noqa_code().to_string();
        let (linter, _) = Linter::parse_code(&code).unwrap();
        let fix = rule.fixable().to_string();
        Self {
            name: rule.name().as_str(),
            code,
            linter: linter.name(),
            summary: rule.message_formats()[0],
            message_formats: rule.message_formats(),
            fix,
            fix_availability: rule.fixable(),
            explanation: rule.explanation(),
            preview: rule.is_preview(),
            status: rule.group(),
            source_location: SourceLocation {
                file: rule.file(),
                line: rule.line(),
            },
        }
    }
}

/// Explain a `Rule` to the user.
pub(crate) fn rule(rule: Rule, format: HelpFormat) -> Result<()> {
    let mut stdout = BufWriter::new(io::stdout().lock());
    match format {
        HelpFormat::Text => {
            writeln!(stdout, "{}", rule_documentation(rule))?;
        }
        HelpFormat::Json => {
            serde_json::to_writer_pretty(stdout, &Explanation::from_rule(&rule))?;
        }
    }
    Ok(())
}

/// Explain all rules to the user.
pub(crate) fn rules(format: HelpFormat) -> Result<()> {
    let mut stdout = BufWriter::new(io::stdout().lock());
    match format {
        HelpFormat::Text => {
            for rule in Rule::iter() {
                writeln!(stdout, "{}", rule_documentation(rule))?;
                writeln!(stdout)?;
            }
        }
        HelpFormat::Json => {
            let mut serializer = serde_json::Serializer::pretty(stdout);
            let mut seq = serializer.serialize_seq(None)?;
            for rule in Rule::iter() {
                seq.serialize_element(&Explanation::from_rule(&rule))?;
            }
            seq.end()?;
        }
    }
    Ok(())
}

/// The location of the rule's implementation in the Ruff source tree, relative to the repository
/// root.
///
/// For most rules this will point to the `#[derive(ViolationMetadata)]` line above the rule's
/// struct.
#[derive(Serialize)]
struct SourceLocation {
    file: &'static str,
    line: u32,
}

//! `buff/explainRule` — the documentation for one diagnostic code.
//!
//! A custom request rather than a subprocess on the client's side. An editor showing what a rule
//! means is asking the same tool the same question its diagnostics came from, and the running
//! server is the copy of that tool which already resolved this project's configuration. Spawning
//! `buff rule F401` to answer it resolves that configuration a second time, by a different route,
//! and pays a process for a lookup that is a table read.
//!
//! Not a `codeDescription` on the diagnostic: that is a URL, which sends the reader to a browser
//! for prose the server is holding. Not a hover, which is anchored to a position — a reader can
//! ask about a code they typed into a prompt, with no position to anchor to.

use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ruff_linter::registry::Rule;
use ruff_linter::rule_documentation;

use crate::server::api::traits::{BackgroundRequestHandler, RequestHandler};
use crate::session::{Client, Session};

pub(crate) enum ExplainRuleRequest {}

impl Request for ExplainRuleRequest {
    type Params = ExplainRuleParams;
    type Result = Option<RuleExplanation>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("buff/explainRule");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// The code to look up, as the user wrote it.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExplainRuleParams {
    /// A noqa code (`F401`) or a rule name (`unused-import`). Both are what a reader has in hand:
    /// the code is what a diagnostic shows, the name is what the documentation calls it.
    pub(crate) code: String,
}

/// What the rule is, ready to show.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuleExplanation {
    /// The rule's own name, e.g. `unused-import`.
    pub(crate) name: String,
    /// The noqa code, e.g. `F401`.
    pub(crate) code: String,
    /// The full explanation, in markdown.
    pub(crate) documentation: String,
}

pub(crate) struct ExplainRule;

impl RequestHandler for ExplainRule {
    type RequestType = ExplainRuleRequest;
}

impl BackgroundRequestHandler for ExplainRule {
    // The rules are a compiled-in table; there is no session state to snapshot.
    type Snapshot = ();

    fn snapshot(_session: &Session, _params: &ExplainRuleParams) {}

    fn run_with_snapshot(
        (): (),
        _client: &Client,
        params: ExplainRuleParams,
    ) -> crate::server::Result<Option<RuleExplanation>> {
        Ok(explanation_of(&params.code))
    }
}

/// The explanation for [`code`], if this linter owns the rule it names.
fn explanation_of(code: &str) -> Option<RuleExplanation> {
    resolve(code).map(|rule| RuleExplanation {
        name: rule.name().as_str().to_string(),
        code: rule.noqa_code().map(|code| code.to_string()).unwrap_or_default(),
        documentation: rule_documentation(rule),
    })
}

/// The rule [`code`] names, by either of the two names a rule has.
///
/// `None` rather than an error for a code this linter does not own: the type checker owns a
/// disjoint set of rules under the same kind of name, so "not mine" is an ordinary answer that
/// leaves the client free to ask the other server. An error would make a routine miss look like a
/// failure.
fn resolve(code: &str) -> Option<Rule> {
    let code = code.trim();
    Rule::from_code(code)
        .ok()
        .or_else(|| Rule::from_name(code).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form, exactly as a client sends it — the whole contract with a plugin written in
    /// another language, and nothing else in the crate exercises it.
    #[test]
    fn the_params_a_client_sends_parse() {
        let parsed: ExplainRuleParams =
            serde_json::from_str(r#"{"code":"F401"}"#).expect("a client sends just the code");
        assert_eq!(parsed.code, "F401");
    }

    #[test]
    fn a_code_resolves_to_its_rule() {
        let rule = resolve("F401").expect("F401 is this linter's");
        assert_eq!(rule.noqa_code().map(|code| code.to_string()).as_deref(), Some("F401"));
    }

    /// A reader who has the name rather than the code is asking the same question.
    #[test]
    fn a_rule_name_resolves_to_the_same_rule() {
        assert_eq!(resolve("unused-import"), resolve("F401"));
    }

    /// Whitespace around a code pasted out of a diagnostic is not a different code.
    #[test]
    fn surrounding_whitespace_is_not_part_of_the_code() {
        assert_eq!(resolve("  F401 "), resolve("F401"));
    }

    /// The type checker's rules are named the same way and are not this server's to explain.
    #[test]
    fn a_code_this_linter_does_not_own_is_a_miss_rather_than_an_error() {
        assert!(resolve("redundant-return-annotation").is_none());
        assert!(resolve("not-a-rule-at-all").is_none());
    }

    #[test]
    fn an_explanation_carries_the_prose_and_both_names() {
        let explanation = explanation_of("F401").expect("F401 is this linter's");

        assert_eq!(explanation.code, "F401");
        assert_eq!(explanation.name, "unused-import");
        assert!(
            explanation
                .documentation
                .starts_with("# unused-import (F401)")
        );
    }
}

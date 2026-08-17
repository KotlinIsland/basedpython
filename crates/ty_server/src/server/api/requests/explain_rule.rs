//! `by/explainRule` — the documentation for one type-checker lint.
//!
//! A custom request rather than a subprocess on the client's side. An editor showing what a rule
//! means is asking the same tool the same question its diagnostics came from, and the running
//! server is the copy of that tool which already resolved this project's configuration. Spawning
//! `by explain rule <name>` to answer it pays a process for a table lookup.
//!
//! The linter owns a disjoint set of rules under its own `buff/explainRule`. Neither server knows
//! the other's, so a name this one does not have is an ordinary miss rather than an error, and the
//! client is free to ask the other.

use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ty_python_semantic::default_lint_registry;

use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::client::Client;
use crate::session::SessionSnapshot;

pub(crate) enum ExplainRuleRequest {}

impl Request for ExplainRuleRequest {
    type Params = ExplainRuleParams;
    type Result = Option<RuleExplanation>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/explainRule");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// The rule to look up, as the user wrote it.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExplainRuleParams {
    /// A lint name, e.g. `redundant-return-annotation` — what a diagnostic reports under.
    pub(crate) name: String,
}

/// What the rule is, ready to show.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuleExplanation {
    /// The lint's own name.
    pub(crate) name: String,
    /// A one-line summary.
    pub(crate) summary: String,
    /// The full explanation, in markdown.
    pub(crate) documentation: String,
}

pub(crate) struct ExplainRuleHandler;

impl RequestHandler for ExplainRuleHandler {
    type RequestType = ExplainRuleRequest;
}

impl BackgroundRequestHandler for ExplainRuleHandler {
    fn run(
        _snapshot: &SessionSnapshot,
        _client: &Client,
        params: ExplainRuleParams,
    ) -> crate::server::Result<Option<RuleExplanation>> {
        Ok(explanation_of(&params.name))
    }
}

/// The explanation for [`name`], if this server owns the lint it names.
fn explanation_of(name: &str) -> Option<RuleExplanation> {
    let lint = default_lint_registry().get(name.trim()).ok()?;
    Some(RuleExplanation {
        name: lint.name().as_str().to_string(),
        summary: lint.summary().to_string(),
        documentation: lint.documentation_markdown(),
    })
}

impl RetriableRequestHandler for ExplainRuleHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form, exactly as a client sends it — the whole contract with a plugin written in
    /// another language, and nothing else here exercises it.
    #[test]
    fn the_params_a_client_sends_parse() {
        let parsed: ExplainRuleParams =
            serde_json::from_str(r#"{"name":"unresolved-import"}"#).expect("a client sends a name");
        assert_eq!(parsed.name, "unresolved-import");
    }

    #[test]
    fn a_name_resolves_to_its_lint() {
        let explanation = explanation_of("unresolved-import").expect("this server owns it");
        assert_eq!(explanation.name, "unresolved-import");
        assert!(explanation.documentation.starts_with("# unresolved-import"));
        assert!(explanation.documentation.contains("Default level:"));
    }

    /// Whitespace around a name pasted out of a diagnostic is not a different name.
    #[test]
    fn surrounding_whitespace_is_not_part_of_the_name() {
        assert!(explanation_of("  unresolved-import ").is_some());
    }

    /// The linter's rules are not this server's to explain, and saying so is not a failure.
    #[test]
    fn a_name_this_server_does_not_own_is_a_miss_rather_than_an_error() {
        assert!(explanation_of("F401").is_none());
        assert!(explanation_of("not-a-rule-at-all").is_none());
    }
}

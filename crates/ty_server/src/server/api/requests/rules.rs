//! `by/rules` — every rule the type checker reports under, for an editor to list them
//!
//! an editor that gives each rule an entry of its own, an inspection with a severity its user can
//! change, needs the rules before any diagnostic arrives: each diagnostic names the rule it is
//! reported under as its `code`, and this is the list those codes come from. what a rule means is
//! `by/explainRule`'s, and the linter's rules are `buff`'s, as they are for `buff/explainRule`

use lsp_types::{LspRequestMethod, MessageDirection, Request};
use ty_python_semantic::default_lint_registry;
use ty_python_semantic::lint::{Level, LintStatus};

use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

pub(crate) enum RulesRequest {}

impl Request for RulesRequest {
    type Params = Option<RulesParams>;
    type Result = Vec<Rule>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/rules");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// nothing, an empty object or no `params` at all: the rules are the server's, whatever the
/// project
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "the brackets make it the empty object a client sends, which a unit struct is not"
)]
pub(crate) struct RulesParams {}

/// a rule, as a diagnostic reported under it names it
#[derive(Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Rule {
    /// what a diagnostic reported under the rule carries as its `code`: `unresolved-reference`
    name: String,
    /// a one-line summary
    summary: String,
    /// its level when the configuration names no other
    default_level: RuleLevel,
    /// whether the rule is stable, in preview or deprecated
    status: RuleStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RuleLevel {
    /// not reported
    Ignore,
    /// reported as a warning
    Warn,
    /// reported as an error
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RuleStatus {
    Stable,
    /// its behaviour may still change
    Preview,
    /// no longer recommended; its summary says what to use instead
    Deprecated,
}

pub(crate) struct RulesHandler;

impl RequestHandler for RulesHandler {
    type RequestType = RulesRequest;
}

impl BackgroundRequestHandler for RulesHandler {
    fn run(
        _snapshot: &SessionSnapshot,
        _client: &Client,
        _params: Option<RulesParams>,
    ) -> crate::server::Result<Vec<Rule>> {
        Ok(rules())
    }
}

/// every rule a diagnostic can be reported under, by name. a removed rule reports nothing, and
/// is left out
fn rules() -> Vec<Rule> {
    let mut rules: Vec<Rule> = default_lint_registry()
        .lints()
        .iter()
        .filter_map(|lint| {
            let status = match lint.status {
                LintStatus::Stable { .. } => RuleStatus::Stable,
                LintStatus::Preview { .. } => RuleStatus::Preview,
                LintStatus::Deprecated { .. } => RuleStatus::Deprecated,
                LintStatus::Removed { .. } => return None,
            };
            Some(Rule {
                name: lint.name().as_str().to_string(),
                summary: lint.summary().to_string(),
                default_level: match lint.default_level() {
                    Level::Ignore => RuleLevel::Ignore,
                    Level::Warn => RuleLevel::Warn,
                    Level::Error => RuleLevel::Error,
                },
                status,
            })
        })
        .collect();
    rules.sort_by(|left, right| left.name.cmp(&right.name));
    rules
}

impl RetriableRequestHandler for RulesHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    /// the wire form, exactly as a client sends it: no parameters at all, or an empty object
    #[test]
    fn the_params_a_client_sends_parse() {
        let empty: Option<RulesParams> = serde_json::from_str("{}").expect("an empty object");
        assert!(empty.is_some());
        let absent: Option<RulesParams> =
            serde_json::from_value(serde_json::Value::Null).expect("no params");
        assert!(absent.is_none());
    }

    #[test]
    fn every_rule_is_listed_once_by_its_name() {
        let rules = rules();
        let unresolved = rules
            .iter()
            .find(|rule| rule.name == "unresolved-reference")
            .expect("a rule of the type checker");
        assert_eq!(unresolved.default_level, RuleLevel::Error);
        assert_eq!(unresolved.status, RuleStatus::Stable);
        assert!(!unresolved.summary.is_empty());
        assert!(
            rules.windows(2).all(|pair| pair[0].name < pair[1].name),
            "sorted and without duplicates"
        );
        // the linter's are not this server's
        assert!(rules.iter().all(|rule| rule.name != "F401"));
    }

    #[test]
    fn a_rule_serializes_as_a_client_reads_it() {
        let rule = rules()
            .into_iter()
            .find(|rule| rule.name == "unresolved-reference")
            .expect("a rule");
        let json = serde_json::to_value(&rule).expect("json");
        assert_eq!(json["name"], "unresolved-reference");
        assert_eq!(json["defaultLevel"], "error");
        assert_eq!(json["status"], "stable");
    }
}

//! `by/dataFlowAt` — what a stopped program's own state says about the code below it
//!
//! a custom request rather than an `executeCommand`: this is a question with a typed answer, and
//! `executeCommand` is for things that have an effect. it is also not an `inlayHint` variant,
//! because an inlay hint request carries a range and nothing else — there is nowhere in it to put
//! what a debugger saw, and the answer depends entirely on that
//!
//! the client is the one holding a debug session, so it is the client that sends the observations.
//! the server never learns what a debugger is: it is handed facts and reads source under them

use std::borrow::Cow;

use lsp_types::{LspRequestMethod, MessageDirection, Request, TextDocumentIdentifier, Uri};
use ruff_source_file::OneIndexed;
use ty_ide::{FindingKind, data_flow_at};
use ty_project::{ProjectDatabase, SemanticDb as _};
use ty_python_core::assumptions::{ClassName, Observation, Observed};

use crate::document::ToRangeExt;
use crate::server::api::traits::{
    BackgroundDocumentRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::DocumentSnapshot;
use crate::session::client::Client;

/// the request a client sends while its debuggee is stopped
pub(crate) enum DataFlowRequest {}

impl Request for DataFlowRequest {
    type Params = DataFlowParams;
    type Result = Option<Vec<DataFlowFinding>>;
    // not a method LSP defines, so it goes across as a custom one. the `by/` prefix is what keeps
    // it from ever colliding with something the protocol grows later
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/dataFlowAt");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// what the client knows: where the program is, and what it was holding there
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DataFlowParams {
    /// the file the program is stopped in
    pub(crate) text_document: TextDocumentIdentifier,

    /// the one-based line it is stopped on
    pub(crate) line: u32,

    /// what the debugger observed, one entry per name
    ///
    /// only observations the client is willing to stand behind belong here. a debugger that
    /// reports how long a reading stays true — as `bpd` does — is the thing that decides which
    /// ones those are; the server takes what it is given
    pub(crate) observations: Vec<WireObservation>,
}

/// one observation, in the shape a client sends it
///
/// deliberately its own type rather than `serde` on [`Observed`]: this is a wire format that a
/// plugin written in another language has to produce, and it should be able to change on its own
/// schedule without an internal enum's representation deciding it
///
/// no `deny_unknown_fields` here, and it is not an oversight: serde does not support that together
/// with `flatten`, because the flattened variant's own keys are exactly the unknown fields it would
/// then reject. asking for both makes every observation fail to parse
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireObservation {
    /// the name, or a dotted path such as `self.limit`
    pub(crate) name: String,

    /// what was seen. exactly one of these is set; anything else is refused
    #[serde(flatten)]
    pub(crate) observed: WireObserved,
}

/// what was read off the value
// the shared `Is` prefix is the wire contract: `rename_all` derives the `observed` tag from the
// variant name, so `isInt` is spelled here and nowhere else. renaming the variants to satisfy the
// lint would mean pinning each tag with its own `serde(rename)`, which is the same names written
// twice and one more place they can drift apart
#[expect(clippy::enum_variant_names)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "observed", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum WireObserved {
    /// the value is `None`
    IsNone,
    /// the value is exactly this `bool`
    IsBool { value: bool },
    /// the value is exactly this integer, in decimal
    IsInt { text: String },
    /// the value is exactly this float, as `float.__repr__` writes it
    ///
    /// text rather than a json number, for the reason an integer is text: a reader that went
    /// through json's number would lose `inf` and `nan`, which have no json spelling at all
    IsFloat { text: String },
    /// the value is exactly this string
    IsStr { text: String },
    /// the value is exactly these bytes
    ///
    /// an array of numbers rather than a base64 string, because json has no byte string and every
    /// encoding of one is a decoder this has to get right. serde refuses a number outside a byte
    /// on its own, so a malformed reading is rejected at the edge instead of somewhere inside
    IsBytes { bytes: Vec<u8> },
    /// `type(value)` is exactly this class
    IsExactly { module: String, qualname: String },
    /// the value is this member of this enum
    IsEnumMember {
        module: String,
        qualname: String,
        member: String,
    },
}

impl WireObservation {
    fn into_observation(self) -> Observation {
        let observed = match self.observed {
            WireObserved::IsNone => Observed::IsNone,
            WireObserved::IsBool { value } => Observed::IsBool(value),
            WireObserved::IsInt { text } => Observed::IsInt(text),
            WireObserved::IsFloat { text } => Observed::IsFloat(text),
            WireObserved::IsStr { text } => Observed::IsStr(text),
            WireObserved::IsBytes { bytes } => Observed::IsBytes(bytes.into_boxed_slice()),
            WireObserved::IsExactly { module, qualname } => {
                Observed::IsExactly(ClassName { module, qualname })
            }
            WireObserved::IsEnumMember {
                module,
                qualname,
                member,
            } => Observed::IsEnumMember {
                class: ClassName { module, qualname },
                member: ruff_python_ast::name::Name::new(member),
            },
        };
        Observation {
            name: ruff_python_ast::name::Name::new(self.name),
            observed,
        }
    }
}

/// one thing the state settles, positioned for the editor
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DataFlowFinding {
    /// where in the document
    pub(crate) range: lsp_types::Range,
    /// what kind of finding: `condition`, `unreachable` or `value`
    pub(crate) kind: String,
    /// which way a condition goes. absent for anything else
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) taken: Option<bool>,
    /// what a decided read will find, written the way a source writes it. absent for anything else
    ///
    /// carried beside [`label`](Self::label), which already spells it, because a client that wants
    /// to do anything but draw the label — colour by value, offer it for a copy — should not have
    /// to take a string written for a human back apart
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) value: Option<String>,
    /// what to draw beside the source
    pub(crate) label: String,
}

pub(crate) struct DataFlowRequestHandler;

impl RequestHandler for DataFlowRequestHandler {
    type RequestType = DataFlowRequest;
}

impl BackgroundDocumentRequestHandler for DataFlowRequestHandler {
    fn document_uri(params: &DataFlowParams) -> Cow<'_, Uri> {
        Cow::Borrowed(&params.text_document.uri)
    }

    fn run_with_snapshot(
        db: &ProjectDatabase,
        snapshot: &DocumentSnapshot,
        _client: &Client,
        params: DataFlowParams,
    ) -> crate::server::Result<Option<Vec<DataFlowFinding>>> {
        if snapshot
            .workspace_settings()
            .is_language_services_disabled()
        {
            return Ok(None);
        }
        let Some(file) = snapshot.to_notebook_or_file(db) else {
            return Ok(None);
        };
        let document = snapshot.uri();
        // a one-based line of zero is not a line. answering nothing is right: there is no
        // statement above the first one for a program to be stopped after
        let Some(line) = OneIndexed::new(params.line as usize) else {
            return Ok(None);
        };

        let observations = params
            .observations
            .into_iter()
            .map(WireObservation::into_observation)
            .collect();

        let findings = data_flow_at(db, db.program_file(file), line, observations)
            .into_iter()
            // two ways a finding is dropped rather than reported, and both are the same
            // judgement: a position that is not certainly in the document the client asked
            // about is worse than no position. the reply may have raced an edit, and a
            // notebook maps ranges per cell — so the location is required to name the very
            // document the request did
            .filter_map(|finding| {
                let location = finding
                    .range
                    .to_lsp_range(db, file, snapshot.encoding())?
                    .to_location()?;
                if &location.uri != document {
                    return None;
                }
                let label = finding.label();
                let (kind, taken, value) = match finding.kind {
                    FindingKind::Condition { taken } => ("condition", Some(taken), None),
                    FindingKind::Unreachable => ("unreachable", None, None),
                    FindingKind::Value { value, .. } => ("value", None, Some(value)),
                };
                Some(DataFlowFinding {
                    range: location.range,
                    kind: kind.to_string(),
                    taken,
                    value,
                    label,
                })
            })
            .collect();

        Ok(Some(findings))
    }
}

impl RetriableRequestHandler for DataFlowRequestHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    /// the params exactly as a client sends them
    ///
    /// this is the whole contract with a plugin written in another language, and nothing else in
    /// the crate exercises it — the handler is reached through a `Request` the tests construct
    /// from typed values, which is precisely the path that cannot notice a serde attribute that
    /// makes the typed value unreachable from json
    fn params(observation: &str) -> Result<DataFlowParams, serde_json::Error> {
        serde_json::from_str(&format!(
            r#"{{"textDocument":{{"uri":"file:///a.py"}},"line":3,"observations":[{observation}]}}"#
        ))
    }

    #[test]
    fn every_observation_a_client_can_send_parses() {
        for observation in [
            r#"{"name":"value","observed":"isNone"}"#,
            r#"{"name":"flag","observed":"isBool","value":true}"#,
            r#"{"name":"limit","observed":"isInt","text":"5"}"#,
            r#"{"name":"label","observed":"isStr","text":"hi"}"#,
            r#"{"name":"raw","observed":"isBytes","bytes":[104,105]}"#,
            r#"{"name":"self.thing","observed":"isExactly","module":"main","qualname":"Runner"}"#,
            r#"{"name":"c","observed":"isEnumMember","module":"main","qualname":"Color","member":"RED"}"#,
        ] {
            assert!(
                params(observation).is_ok(),
                "a client sending {observation} would have got an error back: {:?}",
                params(observation).unwrap_err().to_string()
            );
        }
    }

    #[test]
    fn an_observation_of_a_kind_the_server_does_not_know_is_refused() {
        // the wire format is closed for the same reason `Observed` is: an unrecognised reading
        // swept into a catch-all would be a reading nothing understands, treated as one that is
        assert!(params(r#"{"name":"x","observed":"isImaginary","value":1}"#).is_err());
    }

    /// `float.__repr__`'s text, and the two spellings json has no number for
    #[test]
    fn a_float_observation_survives_the_crossing_including_its_infinities() {
        for text in ["0.25", "-0.0", "inf", "-inf", "nan"] {
            let parsed = params(&format!(
                r#"{{"name":"ratio","observed":"isFloat","text":"{text}"}}"#
            ))
            .expect("a float observation is one of the wire forms");
            let observation = parsed
                .observations
                .into_iter()
                .next()
                .expect("one observation was sent")
                .into_observation();
            assert_eq!(observation.observed, Observed::IsFloat(text.to_string()));
        }
    }

    #[test]
    fn a_bytes_observation_survives_the_crossing_to_an_observed() {
        let parsed = params(r#"{"name":"raw","observed":"isBytes","bytes":[104,105]}"#)
            .expect("the wire form is one of the seven above");
        let observation = parsed
            .observations
            .into_iter()
            .next()
            .expect("one observation was sent")
            .into_observation();
        assert_eq!(
            observation.observed,
            Observed::IsBytes(Box::from(&b"hi"[..]))
        );
    }
}

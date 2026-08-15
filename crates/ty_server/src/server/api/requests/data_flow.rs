//! `by/dataFlowAt` — what a stopped program's own state says about the code below it.
//!
//! A custom request rather than an `executeCommand`: this is a question with a typed answer, and
//! `executeCommand` is for things that have an effect. It is also not an `inlayHint` variant,
//! because an inlay hint request carries a range and nothing else — there is nowhere in it to put
//! what a debugger saw, and the answer depends entirely on that.
//!
//! The client is the one holding a debug session, so it is the client that sends the observations.
//! The server never learns what a debugger is: it is handed facts and reads source under them.

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

/// The request a client sends while its debuggee is stopped.
pub(crate) enum DataFlowRequest {}

impl Request for DataFlowRequest {
    type Params = DataFlowParams;
    type Result = Option<Vec<DataFlowFinding>>;
    // Not a method LSP defines, so it goes across as a custom one. The `by/` prefix is what keeps
    // it from ever colliding with something the protocol grows later
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/dataFlowAt");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// What the client knows: where the program is, and what it was holding there.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DataFlowParams {
    /// The file the program is stopped in.
    pub(crate) text_document: TextDocumentIdentifier,

    /// The one-based line it is stopped on.
    pub(crate) line: u32,

    /// What the debugger observed, one entry per name.
    ///
    /// Only observations the client is willing to stand behind belong here. A debugger that
    /// reports how long a reading stays true — as `bpd` does — is the thing that decides which
    /// ones those are; the server takes what it is given.
    pub(crate) observations: Vec<WireObservation>,
}

/// One observation, in the shape a client sends it.
///
/// Deliberately its own type rather than `serde` on [`Observed`]: this is a wire format that a
/// plugin written in another language has to produce, and it should be able to change on its own
/// schedule without an internal enum's representation deciding it.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WireObservation {
    /// The name, or a dotted path such as `self.limit`.
    pub(crate) name: String,

    /// What was seen. Exactly one of these is set; anything else is refused.
    #[serde(flatten)]
    pub(crate) observed: WireObserved,
}

/// What was read off the value.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "observed", rename_all = "camelCase")]
pub(crate) enum WireObserved {
    /// The value is `None`.
    IsNone,
    /// The value is exactly this `bool`.
    IsBool { value: bool },
    /// The value is exactly this integer, in decimal.
    IsInt { text: String },
    /// The value is exactly this string.
    IsStr { text: String },
    /// `type(value)` is exactly this class.
    IsExactly { module: String, qualname: String },
    /// The value is this member of this enum.
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
            WireObserved::IsStr { text } => Observed::IsStr(text),
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

/// One thing the state settles, positioned for the editor.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DataFlowFinding {
    /// Where in the document.
    pub(crate) range: lsp_types::Range,
    /// What kind of finding: `condition` or `unreachable`.
    pub(crate) kind: String,
    /// Which way a condition goes. Absent for an unreachable range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) taken: Option<bool>,
    /// What to draw beside the source.
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
        // A one-based line of zero is not a line. Answering nothing is right: there is no
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
            // Two ways a finding is dropped rather than reported, and both are the same
            // judgement: a position that is not certainly in the document the client asked
            // about is worse than no position. The reply may have raced an edit, and a
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
                Some(DataFlowFinding {
                    range: location.range,
                    kind: match finding.kind {
                        FindingKind::Condition { .. } => "condition",
                        FindingKind::Unreachable => "unreachable",
                    }
                    .to_string(),
                    taken: match finding.kind {
                        FindingKind::Condition { taken } => Some(taken),
                        FindingKind::Unreachable => None,
                    },
                    label: finding.label().to_string(),
                })
            })
            .collect();

        Ok(Some(findings))
    }
}

impl RetriableRequestHandler for DataFlowRequestHandler {}

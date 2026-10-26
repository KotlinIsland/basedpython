//! `by/inputStatus` — whether the text typed into a `by repl` console is a whole input yet
//!
//! an editor's console decides at each enter whether to run what has been typed or to start a
//! new line of it, which is a question about basedpython's grammar: a compound statement goes on
//! until a blank line, an open bracket until it closes. `by repl` answers it the same way for
//! the lines it reads itself, from [`by_stage::session::input_status`], so the two agree. the text
//! is all there is to it: no document, and no project

use by_stage::session::{InputStatus, input_status};
use lsp_types::{LspRequestMethod, MessageDirection, Request};

use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

pub(crate) enum InputStatusRequest {}

impl Request for InputStatusRequest {
    type Params = InputStatusParams;
    type Result = InputStatusResult;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/inputStatus");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InputStatusParams {
    /// every line typed so far, as the console holds it
    text: String,
}

#[derive(Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InputStatusResult {
    status: InputStatus,
}

pub(crate) struct InputStatusHandler;

impl RequestHandler for InputStatusHandler {
    type RequestType = InputStatusRequest;
}

impl BackgroundRequestHandler for InputStatusHandler {
    fn run(
        _snapshot: &SessionSnapshot,
        _client: &Client,
        params: InputStatusParams,
    ) -> crate::server::Result<InputStatusResult> {
        Ok(InputStatusResult {
            status: input_status(&params.text),
        })
    }
}

impl RetriableRequestHandler for InputStatusHandler {}

//! `by/commandLine` — `by`'s command line, for an editor to complete it in a terminal
//!
//! the answer is described from the parser `by` parses its own arguments with, so it is the
//! command line of the `by` this server is part of: the one an editor that started the server
//! also puts on its terminal's `PATH`. it takes no parameters, and is the same for every project

use lsp_types::{LspRequestMethod, MessageDirection, Request};

use crate::command_line::CommandLine;
use crate::server::api::traits::{RequestHandler, SyncRequestHandler};
use crate::session::Session;
use crate::session::client::Client;

pub(crate) enum CommandLineRequest {}

impl Request for CommandLineRequest {
    type Params = Option<CommandLineParams>;
    type Result = CommandLine;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/commandLine");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// nothing, an empty object or no `params` at all
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "the brackets make it the empty object a client sends, which a unit struct is not"
)]
pub(crate) struct CommandLineParams {}

pub(crate) struct CommandLineHandler;

impl RequestHandler for CommandLineHandler {
    type RequestType = CommandLineRequest;
}

impl SyncRequestHandler for CommandLineHandler {
    fn run(
        session: &mut Session,
        _client: &Client,
        _params: Option<CommandLineParams>,
    ) -> crate::server::Result<CommandLine> {
        Ok(session.command_line().clone())
    }
}

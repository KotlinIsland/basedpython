//! `by/entryPoint` — how a module runs as a program.
//!
//! basedpython turns a module's top-level `main` into its entry point: it appends the `__main__`
//! guard that calls it and generates an argument parser from its signature. Which `main` counts,
//! whether the module already invokes it, which parameters the command line can fill and how each
//! is spelled are all decisions of that transpiler pass — so an editor offering to run a module, or
//! to fill in its arguments, asks the pass's own reading ([`by_transforms::entry_point`]) rather
//! than reading the signature a second way that drifts from it.
//!
//! Asked by URI, not by open document: a run configuration names a module, and the file behind it
//! is usually not open when the run starts.

use lsp_types::{LspRequestMethod, MessageDirection, Range, Request, Uri};
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::Db as _;

use crate::document::ToRangeExt;
use crate::server::api::program_files::project_file;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

pub(crate) enum EntryPointRequest {}

impl Request for EntryPointRequest {
    type Params = EntryPointParams;
    type Result = Option<EntryPointResponse>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/entryPoint");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// Which file to read.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EntryPointParams {
    /// The file — the editor's buffer when it is open, the file on disk when it is not.
    uri: Uri,
}

/// How the module runs.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EntryPointResponse {
    /// The module's top-level `main`, when it is a module basedpython generates an entry point
    /// for — a `.by` that is not a stub. Present whether or not it ends up the entry point; see
    /// [`MainFunction::entry_point`].
    main: Option<MainFunction>,
    /// The module's own `if __name__ == "__main__":` headers, in source order: each runs the module
    /// as a program, in a `.py` as much as in a `.by`.
    guards: Vec<Range>,
}

/// The last top-level `def main` / `async def main`.
// a wire format: each flag is a separate fact a client reads, not states of one enum
#[expect(clippy::struct_excessive_bools)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MainFunction {
    /// The function's name.
    name_range: Range,
    is_async: bool,
    /// Whether the transpiler appends the guard that calls it, which is the conjunction of the
    /// three below: it is not `private`, the module does not invoke `main` itself, and nothing
    /// blocks it.
    entry_point: bool,
    /// `private def main`, which is renamed.
    is_private: bool,
    /// The module calls `main` itself — a hand-written guard or a bare top-level `main()` — so it
    /// keeps that entry point and no argument parser is generated.
    module_invokes_main: bool,
    /// The first required parameter the command line cannot supply, which makes calling `main`
    /// raise `TypeError`, so no guard is generated for it.
    blocked_by: Option<String>,
    /// Every parameter but the variadics, in declared order.
    parameters: Vec<MainParameter>,
    /// The converter the arguments nobody else claims are passed through, when a leading `*rest`
    /// asks for them.
    extra_arguments: Option<String>,
    /// The docstring, which is the generated parser's `--help` description.
    docstring: Option<String>,
}

/// One parameter of `main`.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MainParameter {
    name: String,
    /// `positional` (positional-only), `keyword` (keyword-only) or `any`.
    kind: String,
    required: bool,
    /// The annotation as written.
    annotation: Option<String>,
    /// The default as written.
    default: Option<String>,
    /// How the command line spells it; absent when it does not, and the parameter keeps its
    /// default.
    cli: Option<CliParameter>,
}

/// A parameter as the generated argument parser registers it.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CliParameter {
    /// The callable that converts the argument, as the source names it — `int`, `Path`,
    /// `pathlib.Path`. Absent for a flag pair, which takes no value.
    converter: Option<String>,
    /// The command-line values a literal union admits.
    choices: Option<Vec<String>>,
    /// The option spellings, the one to write first.
    flags: Vec<String>,
    /// The `--no-…` spellings of a flag; empty for a value.
    negative_flags: Vec<String>,
}

pub(crate) struct EntryPointHandler;

impl RequestHandler for EntryPointHandler {
    type RequestType = EntryPointRequest;
}

impl BackgroundRequestHandler for EntryPointHandler {
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: EntryPointParams,
    ) -> crate::server::Result<Option<EntryPointResponse>> {
        let Some((db, file)) = project_file(snapshot, &params.uri) else {
            return Ok(None);
        };
        let encoding = snapshot.position_encoding();
        let source = source_text(db, file);
        let parsed = parsed_module(db, db.program_file(file).python_file(db)).load(db);
        let body = &parsed.suite();
        let range = |range: TextRange| {
            range
                .to_lsp_range(db, file, encoding)
                .map(|range| range.local_range())
        };
        let text = |range: TextRange| source.as_str()[range].to_owned();

        let guards = by_transforms::entry_point::main_guards(body)
            .filter_map(|guard| range(TextRange::new(guard.start(), guard.test.end())))
            .collect();

        let source_type = file.source_type(db);
        let generates_entry_points = source_type.is_basedpython() && !source_type.is_stub();
        let main = generates_entry_points
            .then(|| by_transforms::entry_point::entry_point(body, source.as_str()))
            .flatten()
            .and_then(|entry| {
                Some(MainFunction {
                    name_range: range(entry.function.name.range())?,
                    is_async: entry.function.is_async,
                    entry_point: entry.generates_guard(),
                    is_private: entry.is_private,
                    module_invokes_main: entry.module_invokes_main,
                    blocked_by: entry.blocked_by().map(|param| param.name().to_owned()),
                    parameters: entry
                        .parameters
                        .iter()
                        .map(|param| MainParameter {
                            name: param.name().to_owned(),
                            kind: param.kind.as_str().to_owned(),
                            required: param.is_required(),
                            annotation: param
                                .parameter
                                .parameter
                                .annotation
                                .as_deref()
                                .map(|annotation| text(annotation.range())),
                            default: param
                                .parameter
                                .default
                                .as_deref()
                                .map(|default| text(default.range())),
                            cli: param.spelling.as_ref().map(|spelling| CliParameter {
                                converter: spelling.converter.map(str::to_owned),
                                choices: spelling.choices.as_ref().map(|choices| {
                                    choices.iter().map(|choice| choice.value.clone()).collect()
                                }),
                                flags: param.flags(),
                                negative_flags: param.negative_flags(),
                            }),
                        })
                        .collect(),
                    extra_arguments: entry.extra_arguments.map(str::to_owned),
                    docstring: entry.docstring().map(str::to_owned),
                })
            });

        Ok(Some(EntryPointResponse { main, guards }))
    }
}

impl RetriableRequestHandler for EntryPointHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form a client sends — the contract with a plugin written in another language.
    #[test]
    fn the_params_a_client_sends_parse() {
        let parsed: EntryPointParams =
            serde_json::from_str(r#"{"uri":"file:///p/main.by"}"#).expect("a client sends a uri");
        assert_eq!(parsed.uri.as_str(), "file:///p/main.by");
    }
}

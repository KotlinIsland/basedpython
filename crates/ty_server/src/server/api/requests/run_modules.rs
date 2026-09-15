//! `by/runModules` — the names `by run` runs a project's files under.
//!
//! `by run <module>` stages the project's module roots into a tree shaped like the module tree and
//! hands the name to `runpy`, and with no name it runs the project's configured `run.main` — read
//! through [`by_stage::run_module::configured_main`], which `by run` shares. Which file a name means
//! is the project resolver's answer: search paths in order, only the module roots the build stages,
//! a package running as its `__main__`, and one file per name — where two files would build to one
//! module, `by run` refuses to stage either, and the resolver's choice is the one reported. An
//! editor building a run configuration asks that here, rather than walking directories in an order
//! that agrees with the resolver only until a project has two roots.

use lsp_types::{LspRequestMethod, MessageDirection, Request, Uri};
use ruff_db::system::SystemPath;
use ty_project::Db as _;

use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;
use crate::system::file_to_uri;

pub(crate) enum RunModulesRequest {}

impl Request for RunModulesRequest {
    type Params = RunModulesParams;
    type Result = Option<RunModulesResponse>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/runModules");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// What to resolve beyond every project file.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunModulesParams {
    /// A module name as a run configuration holds it, resolved in each project as
    /// [`RunModulesProject::requested`]. Not every name `by run` accepts names a file of its own —
    /// a package runs as its `__main__` — so a name is resolved rather than looked up.
    #[serde(default)]
    module: Option<String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunModulesResponse {
    projects: Vec<RunModulesProject>,
}

/// One project's answer.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunModulesProject {
    /// The project root, which is where `by run` resolves its configuration from.
    root: Uri,
    /// The configured `run.main`, which `by run` runs when it is given no module.
    main: Option<RunModule>,
    /// The requested module, resolved.
    requested: Option<RunModule>,
    /// Every project file `by run` runs by a name of its own, with that name.
    modules: Vec<RunModule>,
}

/// A module name and the file it runs.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunModule {
    module: String,
    /// Absent when the name resolves to nothing `by run` stages, which is how a mistyped
    /// `run.main` reads.
    uri: Option<Uri>,
}

pub(crate) struct RunModulesHandler;

impl RequestHandler for RunModulesHandler {
    type RequestType = RunModulesRequest;
}

impl BackgroundRequestHandler for RunModulesHandler {
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: RunModulesParams,
    ) -> crate::server::Result<Option<RunModulesResponse>> {
        let projects = snapshot
            .projects()
            .iter()
            .filter_map(|db| {
                let resolve = |module: String| RunModule {
                    uri: by_stage::run_module::module_file(db, &module)
                        .and_then(|file| file_to_uri(db, file)),
                    module,
                };
                let root: &SystemPath = db.project().root(db);
                let mut modules: Vec<RunModule> = db
                    .project()
                    .files(db)
                    .into_iter()
                    .filter_map(|file| {
                        Some(RunModule {
                            module: by_stage::run_module::module_name(db, file)?,
                            uri: Some(file_to_uri(db, file)?),
                        })
                    })
                    .collect();
                modules.sort_by(|a, b| a.module.cmp(&b.module));
                Some(RunModulesProject {
                    root: Uri::from_file_path(root.as_std_path()).ok()?,
                    main: by_stage::run_module::configured_main(db).map(resolve),
                    requested: params.module.clone().map(resolve),
                    modules,
                })
            })
            .collect();
        Ok(Some(RunModulesResponse { projects }))
    }
}

impl RetriableRequestHandler for RunModulesHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_params_a_client_sends_parse() {
        let named: RunModulesParams =
            serde_json::from_str(r#"{"module":"app.cli"}"#).expect("a client names a module");
        assert_eq!(named.module.as_deref(), Some("app.cli"));
        let bare: RunModulesParams = serde_json::from_str("{}").expect("a client names nothing");
        assert!(bare.module.is_none());
    }
}

//! `by/moduleName`: the name a file or a directory is imported by
//!
//! what an editor's *copy reference* puts on the clipboard for a module or a package, and what
//! anything that names a module after its file needs. the name is the module resolver's: the search
//! path the file is under and the path from there, so a src-layout project's `src/shapes/circle.by`
//! is `shapes.circle`, its `src/shapes/__init__.by` and its `src/shapes` are both `shapes`, and a
//! file that resolution would never reach — shadowed by another file under the same name earlier
//! on the search path — has no name, rather than one that imports something else

use lsp_types::{LspRequestMethod, MessageDirection, Request, Uri};
use ruff_db::Db as _;
use ruff_db::files::system_path_to_file;
use ruff_db::system::SystemPathBuf;
use ty_module_resolver::{file_to_module, path_to_module_name};
use ty_project::{Db as _, SemanticDb as _};

use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

pub(crate) enum ModuleNameRequest {}

impl Request for ModuleNameRequest {
    type Params = ModuleNameParams;
    type Result = Option<String>;
    const METHOD: LspRequestMethod<'static> = LspRequestMethod::Custom("by/moduleName");
    const MESSAGE_DIRECTION: MessageDirection = MessageDirection::ClientToServer;
}

/// the file or directory to name
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ModuleNameParams {
    uri: Uri,
}

pub(crate) struct ModuleNameHandler;

impl RequestHandler for ModuleNameHandler {
    type RequestType = ModuleNameRequest;
}

impl BackgroundRequestHandler for ModuleNameHandler {
    /// `null` for anything outside every project, and for a path no module is imported from: one
    /// on no search path, a directory or a file whose path is not a dotted name, or a file that
    /// resolution does not reach
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: ModuleNameParams,
    ) -> crate::server::Result<Option<String>> {
        let Some(path) = params
            .uri
            .to_file_path()
            .ok()
            .and_then(|path| SystemPathBuf::from_path_buf(path).ok())
        else {
            return Ok(None);
        };
        let Some(db) = snapshot
            .projects()
            .iter()
            .filter(|db| path.starts_with(db.project().root(*db)))
            .max_by_key(|db| db.project().root(*db).components().count())
        else {
            return Ok(None);
        };
        let name = if db.system().is_directory(&path) {
            let environment = db.project().program(db).resolver_environment(db);
            path_to_module_name(db, environment, &path).map(|name| name.to_string())
        } else {
            system_path_to_file(db, &path).ok().and_then(|file| {
                let module = file_to_module(db, db.program_file(file).resolver_file(db))?;
                Some(module.name(db).to_string())
            })
        };
        Ok(name)
    }
}

impl RetriableRequestHandler for ModuleNameHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_params_a_client_sends_parse() {
        let params: ModuleNameParams = serde_json::from_str(r#"{"uri":"file:///p/src/shapes"}"#)
            .expect("a client names a path");
        assert_eq!(params.uri.as_str(), "file:///p/src/shapes");
    }
}

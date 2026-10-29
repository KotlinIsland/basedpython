//! Data model, state management, and configuration resolution.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::{Deref, DerefMut};
use std::panic::RefUnwindSafe;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use lsp_server::{Message, RequestId};
use lsp_types::{
    ClientInfo, DiagnosticProvider, DiagnosticRegistrationOptions, Registration,
    RegistrationParams, TextDocumentContentChangeEvent, Unregistration, UnregistrationParams, Uri,
    WorkDoneProgressBegin,
};
use lsp_types::{
    DocumentDiagnosticRequest, RegistrationRequest, Request, ShutdownRequest, UnregistrationRequest,
};
use lsp_types::{ExitNotification, Notification};
use ruff_db::Db;
use ruff_db::files::{File, system_path_to_file, vendored_path_to_file};
use ruff_db::system::{System, SystemPath, SystemPathBuf, SystemVirtualPath};
use ruff_python_ast::PySourceType;
use ty_combine::Combine;
use ty_project::metadata::Options;
use ty_project::watch::{ChangeEvent, DeletedKind, ProjectWatcher};
use ty_project::{
    ChangeResult, Db as _, ProjectDatabase, ProjectMetadata, ProjectReloadResult,
    ScriptEnvironmentAvailability, UseUv, UvSyncChanges, UvWorkspace,
};

use index::DocumentError;
use ty_python_core::program::UseDefaultStrategy;

pub(crate) use self::options::InitializationOptions;
pub use self::options::{
    ClientOptions, DiagnosticMode, GlobalOptions, ImportStyleOption, WorkspaceOptions,
};
pub(crate) use self::settings::{GlobalSettings, WorkspaceSettings};
use crate::capabilities::{ResolvedClientCapabilities, server_diagnostic_options};
use crate::command_line::CommandLine;
use crate::db::Db as _;
use crate::document::{DocumentKey, DocumentVersion, LanguageId, NotebookDocument};
use crate::server::{
    Action, LazyWorkDoneProgress, ScriptProgress, SuspendedWorkspaceRequestKind,
    publish_all_document_diagnostics, publish_diagnostics_if_needed, publish_settings_diagnostics,
};
use crate::session::client::Client;
use crate::session::file_watcher::LspFileWatcher;
use crate::session::index::Document;
use crate::session::request_queue::RequestQueue;
use crate::system::{AnySystemPath, LSPSystem};
use crate::{PositionEncoding, TextDocument};
use index::Index;

pub(crate) mod client;
mod file_watcher;
pub(crate) mod index;
mod options;
mod request_queue;
mod settings;

/// The global state for the LSP
pub(crate) struct Session {
    /// A native system to use with the [`LSPSystem`].
    native_system: Arc<dyn System + 'static + Send + Sync + RefUnwindSafe>,

    /// Used to retrieve information about open documents and settings.
    ///
    /// This will be [`None`] when a mutable reference is held to the index via [`index_mut`]
    /// to prevent the index from being accessed while it is being modified. It will be restored
    /// when the mutable reference ([`MutIndexGuard`]) is dropped.
    ///
    /// [`index_mut`]: Session::index_mut
    index: Option<Arc<Index>>,

    /// Maps workspace folders to their respective workspace.
    workspaces: Workspaces,

    /// Whether the initial workspace configuration has been applied.
    workspace_configuration_initialized: bool,

    /// All projects across all workspaces.
    ///
    /// Each initialized workspace currently has one project database; this map
    /// contains the root of that workspace as a key and the state for the
    /// associated project as a value.
    ///
    /// Note that the workspace root used as a key can differ from the project
    /// root of the associated project (because the project root is determined
    /// by a configuration discovery process that might settle on an ancestor
    /// of the workspace).
    projects: BTreeMap<SystemPathBuf, ProjectState>,

    /// Initialization options that were provided by the client during server initialization.
    initialization_options: InitializationOptions,

    /// The uv integrations enabled for the lifetime of this server.
    use_uv: UseUv,

    /// Shares one progress indicator while script synchronization requests are outstanding.
    script_progress: ScriptProgress,

    /// Resolved global settings that are shared across all workspaces.
    global_settings: Arc<GlobalSettings>,

    /// The global position encoding, negotiated during LSP initialization.
    position_encoding: PositionEncoding,

    /// Tracks what LSP features the client supports and doesn't support.
    resolved_client_capabilities: ResolvedClientCapabilities,

    /// Tracks the pending requests between client and server.
    request_queue: RequestQueue,

    /// Has the client requested the server to shutdown.
    shutdown_requested: bool,

    /// Whether the server has dynamically registered the diagnostic capability with the client.
    /// Is the connected client a `TestServer` instance.
    in_test: bool,

    deferred_messages: VecDeque<Message>,

    /// A revision counter. It gets incremented on every change to `Session` that
    /// could result in different workspace diagnostics.
    revision: u64,

    /// the workspace diagnostics requests held open, see [`SuspendedWorkspaceDiagnosticRequest`].
    /// each is run again after every change to `Session` (see `revision`), and answered once
    /// it has something to say
    ///
    /// more than one, because a client can have a `workspace/diagnostic` request held open and
    /// send a `by/checkWorkspace` beside it, and neither may be dropped: a request that is never
    /// answered is one the client waits on until it gives up
    ///
    /// each is run again on its own, so a change costs one workspace check per request held. the
    /// checks share the per-file results salsa caches, so the second and later ones mostly redo
    /// the walk over the project's files and the building of each report. they are not merged
    /// into one check, because each request compares against its own previous result ids and
    /// reports its own progress, and is re-dispatched like any other request so that a
    /// cancellation or a retry reaches it alone
    suspended_workspace_diagnostics_requests: Vec<SuspendedWorkspaceDiagnosticRequest>,

    /// Document requests about a text the session does not hold yet, each waiting for the session
    /// to change. See [`HeldRequest`].
    held_requests: Vec<HeldRequest>,

    /// When to ask the client to pull every document's diagnostics again, after an edit that can
    /// change what another document says: see [`Session::diagnostics_may_have_changed`]
    diagnostic_refresh: DiagnosticRefresh,

    /// Registrations is a set of LSP methods that have been dynamically registered with the
    /// client.
    registrations: HashSet<String>,

    file_watcher: Option<LspFileWatcher>,

    /// The name of the client (editor) that connected to this server.
    client_name: ClientName,

    /// `by`'s command line, as the `by` that started this server describes it
    command_line: Arc<CommandLine>,

    /// The session's own file system watcher, over every project's paths.
    ///
    /// The client's watcher, [`Self::file_watcher`], is registered only where this one cannot
    /// watch.
    own_watcher: OwnWatcher,
}

/// Whether a [`Session`] learns of file system changes by watching the file system itself.
enum OwnWatcher {
    /// It does not: it was told not to, or its watcher could not be started, which leaves the
    /// client's watcher as the only source of changes.
    Off,
    /// It does, but there is no project to watch yet.
    Idle,
    /// It is watching every project's paths.
    Running(Box<ProjectWatcher>),
}

/// LSP State for a Project
pub(crate) struct ProjectState {
    /// Files that we have outstanding otherwise-untracked pushed diagnostics for.
    ///
    /// In `CheckMode::OpenFiles` we still read some files that the client hasn't
    /// told us to open. Notably settings files like `pyproject.toml`. In this
    /// mode the client will never pull diagnostics for that file, and because
    /// the file isn't formally "open" we also don't have a reliable signal to
    /// refresh diagnostics for it either.
    ///
    /// However diagnostics for those files include things like "you typo'd your
    /// configuration for the LSP itself", so it's really important that we tell
    /// the user about them! So we remember which ones we have emitted diagnostics
    /// for so that we can clear the diagnostics for all of them before we go
    /// to update any of them.
    pub(crate) untracked_files_with_pushed_diagnostics: Vec<Uri>,

    // Note: This field should be last to ensure the `db` gets dropped last.
    // The db drop order matters because we call `Arc::into_inner` on some Arc's
    // and we use Salsa's cancellation to guarantee that there's only a single reference to the `Arc`.
    // However, this requires that the db drops last.
    // This shouldn't matter here because the db's stored in the session are the
    // only reference we want to hold on, but better be safe than sorry ;).
    pub(crate) db: ProjectDatabase,
}

impl Session {
    #[expect(
        clippy::too_many_arguments,
        reason = "one caller, which has each of these to hand"
    )]
    pub(crate) fn new(
        resolved_client_capabilities: ResolvedClientCapabilities,
        position_encoding: PositionEncoding,
        workspace_uris: Vec<Uri>,
        initialization_options: InitializationOptions,
        native_system: Arc<dyn System + 'static + Send + Sync + RefUnwindSafe>,
        client_name: ClientName,
        in_test: bool,
        watch_file_system: bool,
        command_line: Arc<CommandLine>,
    ) -> crate::Result<Self> {
        let index = Arc::new(Index::new());

        let mut workspaces = Workspaces::default();
        // Register workspaces with default settings - they'll be initialized with real settings
        // when workspace/configuration response is received
        for uri in workspace_uris {
            workspaces.register(uri)?;
        }

        let use_uv = initialization_options.use_uv(&*native_system);

        Ok(Self {
            native_system,
            position_encoding,
            workspaces,
            workspace_configuration_initialized: false,
            deferred_messages: VecDeque::new(),
            index: Some(index),
            initialization_options,
            use_uv,
            script_progress: ScriptProgress::default(),
            global_settings: Arc::new(GlobalSettings::default()),
            projects: BTreeMap::new(),
            resolved_client_capabilities,
            request_queue: RequestQueue::new(),
            shutdown_requested: false,
            in_test,
            suspended_workspace_diagnostics_requests: Vec::new(),
            held_requests: Vec::new(),
            diagnostic_refresh: DiagnosticRefresh::default(),
            revision: 0,
            registrations: HashSet::new(),
            file_watcher: LspFileWatcher::new(resolved_client_capabilities),
            client_name,
            command_line,
            own_watcher: if watch_file_system {
                OwnWatcher::Idle
            } else {
                OwnWatcher::Off
            },
        })
    }

    pub(crate) fn system(&self) -> &dyn System {
        &*self.native_system
    }

    /// `by`'s command line, as the `by` that started this server describes it
    pub(crate) fn command_line(&self) -> &CommandLine {
        &self.command_line
    }

    pub(crate) fn request_queue(&self) -> &RequestQueue {
        &self.request_queue
    }

    pub(crate) fn request_queue_mut(&mut self) -> &mut RequestQueue {
        &mut self.request_queue
    }

    fn initialization_options(&self) -> &InitializationOptions {
        &self.initialization_options
    }

    /// The session's revision: how many times it has changed in a way an answer could see.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn is_shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    pub(crate) fn set_shutdown_requested(&mut self, requested: bool) {
        self.shutdown_requested = requested;
    }

    /// holds `request` open beside any others already held
    ///
    /// once the server is shutting down nothing resumes a held request, so one whose check was
    /// still running when the `shutdown` request came is answered now, as `shutdown` answered
    /// the ones held before it
    pub(crate) fn add_suspended_workspace_diagnostics_request(
        &mut self,
        request: SuspendedWorkspaceDiagnosticRequest,
        client: &Client,
    ) {
        if self.shutdown_requested {
            request.answer_at_shutdown(client);
            return;
        }
        self.suspended_workspace_diagnostics_requests.push(request);
        // Run the suspended workspace diagnostic request immediately in case there
        // were changes since the workspace diagnostics background thread queued
        // the action to suspend the workspace diagnostic request.
        self.resume_suspended_workspace_diagnostic_requests(client);
    }

    pub(crate) fn take_suspended_workspace_diagnostic_requests(
        &mut self,
    ) -> Vec<SuspendedWorkspaceDiagnosticRequest> {
        std::mem::take(&mut self.suspended_workspace_diagnostics_requests)
    }

    /// runs each held workspace diagnostics request again if the [`Session`] changed (the
    /// revision got bumped) since it last ran
    ///
    /// a request the client cancelled in the meantime is dropped
    pub(crate) fn resume_suspended_workspace_diagnostic_requests(&mut self, client: &Client) {
        let suspended = std::mem::take(&mut self.suspended_workspace_diagnostics_requests);
        self.suspended_workspace_diagnostics_requests = suspended
            .into_iter()
            .filter_map(|request| {
                if !self.request_queue.incoming().is_pending(&request.id) {
                    // Clear out the suspended request if the request has been cancelled.
                    tracing::debug!(
                        "Skipping suspended workspace diagnostics request `{}` \
                        because it was cancelled",
                        request.id
                    );
                    return None;
                }

                request.resume_if_revision_changed(self.revision, client)
            })
            .collect();
    }

    /// Keeps `request` until the session changes, because what it asks about is a text the session
    /// did not hold at `revision`.
    ///
    /// Asked again at once if the session has changed since then: the change may be the one it
    /// waits for, and nothing would wake it otherwise.
    pub(crate) fn hold_request(
        &mut self,
        request: lsp_server::Request,
        revision: u64,
        client: &Client,
    ) {
        if !self.request_queue.incoming().is_pending(&request.id) {
            tracing::debug!("Not holding request {}: it was cancelled", request.id);
            return;
        }
        if revision == self.revision {
            tracing::debug!(
                "Holding request id={} method={} until the session holds the text it asks about",
                request.id,
                request.method
            );
            self.held_requests.push(HeldRequest { request, revision });
        } else {
            client.retry(request);
        }
    }

    /// Asks every held request again whose session has changed since it was held, and answers
    /// those that have waited longer than [`HOLD_LIMIT`]. Forgets the ones the client cancelled.
    pub(crate) fn release_held_requests(&mut self, client: &Client) {
        if self.held_requests.is_empty() {
            return;
        }
        let revision = self.revision;
        let incoming = self.request_queue.incoming();
        let mut expired = Vec::new();
        self.held_requests.retain_mut(|held| {
            let Some(started) = incoming.started(&held.request.id) else {
                // cancelled, and answered as cancelled already
                return false;
            };
            if held.revision != revision {
                client.retry(held.request.clone());
                false
            } else if started.elapsed() >= HOLD_LIMIT {
                expired.push(held.request.id.clone());
                false
            } else {
                true
            }
        });
        for id in expired {
            tracing::debug!(
                "Request {id} waited {HOLD_LIMIT:?} for a text the session never held; giving up"
            );
            client.respond_err(
                id,
                lsp_server::ResponseError {
                    code: lsp_server::ErrorCode::ServerCancelled as i32,
                    message: "the document never held the text the request asked about".to_string(),
                    data: None,
                },
            );
        }
    }

    /// Asks the client, shortly, to pull the diagnostics of every document it shows again: an
    /// edit to one module can change what another module, or the project's `api.lock`, is
    /// reported as, and so can closing a module with unsaved edits, which reads it from disk
    /// again. a client pulls a document's diagnostics when that document changes, not others'.
    ///
    /// `interFileDependencies` says so too, but a client may not act on it, as the one in
    /// `IntelliJ` does not; `workspace/diagnostic/refresh` is what every client that supports it
    /// acts on. The diagnostics of the documents the client does not pull are published to it
    /// again instead. Either goes out [`DiagnosticRefresh::DELAY`] after the last of a run of
    /// changes, so that a burst of typing asks once, and only while a document is open whose
    /// diagnostics the client would not otherwise be given: one other than the document
    /// `changed`, which the client pulls itself, or is published, as it changes.
    pub(crate) fn diagnostics_may_have_changed(&mut self, changed: Changed) {
        let others_open = match changed {
            Changed::Edited => self.file_document_handles().nth(1).is_some(),
            Changed::Closed => self.file_document_handles().next().is_some(),
        };
        if others_open {
            self.diagnostic_refresh.schedule(std::time::Instant::now());
        }
    }

    /// Sees to the diagnostic refresh [`Session::diagnostics_may_have_changed`] scheduled, once
    /// it is due: asks a client that supports it to pull every document's diagnostics again, and
    /// publishes again the diagnostics of each document the client does not pull.
    pub(crate) fn send_due_diagnostic_refresh(&mut self, client: &Client) {
        if self.diagnostic_refresh.take_due(std::time::Instant::now()) {
            if self
                .client_capabilities()
                .supports_workspace_diagnostic_refresh()
            {
                client.send_request::<lsp_types::DiagnosticRefreshRequest>(self, (), |_, ()| {});
            }
            publish_all_document_diagnostics(self, client);
        }
    }

    /// When the main loop next has something to do without a message: answer the longest-held
    /// request, or ask the client to refresh its diagnostics.
    pub(crate) fn next_deadline(&self) -> Option<std::time::Instant> {
        match (
            self.held_requests_deadline(),
            self.diagnostic_refresh.deadline(),
        ) {
            (Some(held), Some(refresh)) => Some(held.min(refresh)),
            (held, refresh) => held.or(refresh),
        }
    }

    /// When the longest-held request is due an answer anyway, if any request is held.
    fn held_requests_deadline(&self) -> Option<std::time::Instant> {
        let incoming = self.request_queue.incoming();
        self.held_requests
            .iter()
            .filter_map(|held| incoming.started(&held.request.id))
            .min()
            .map(|started| started + HOLD_LIMIT)
    }

    /// Returns each project's background uv synchronization wakeups.
    pub(crate) fn uv_sync_wakeups(&self) -> Vec<(SystemPathBuf, crossbeam::channel::Receiver<()>)> {
        self.projects
            .iter()
            .map(|(root, state)| (root.clone(), state.db.uv_environments().sync_wakeups()))
            .collect()
    }

    /// Gives one project's uv environments an opportunity to make progress.
    pub(crate) fn poll_uv_sync(&mut self, client: &Client, project_root: &SystemPath) {
        let Some(project) = self.projects.get_mut(project_root) else {
            tracing::debug!(
                "Ignored uv synchronization wakeup for removed project `{project_root}`"
            );
            return;
        };
        let db = &mut project.db;
        let environments = db.uv_environments().clone();
        let changes = environments.poll_sync(db);
        if matches!(
            changes.project,
            Some(ProjectReloadResult::Changed {
                files_changed: true,
            })
        ) {
            let scripts: Vec<_> = db.project().script_files(db).iter().collect();
            Self::synchronize_closed_scripts(
                db,
                &scripts,
                client,
                self.resolved_client_capabilities,
                &self.script_progress,
            );
        }
        self.uv_environments_changed(client, project_root, changes);
    }

    fn uv_environments_changed(
        &mut self,
        client: &Client,
        project_root: &SystemPath,
        changes: UvSyncChanges,
    ) {
        if changes.is_empty() {
            return;
        }

        if changes.project.is_some() {
            publish_settings_diagnostics(self, client, project_root.to_path_buf());
        }

        self.bump_revision();

        self.resume_suspended_workspace_diagnostic_requests(client);

        let capabilities = self.client_capabilities();
        if capabilities.supports_workspace_diagnostic_refresh() {
            client.send_request::<lsp_types::DiagnosticRefreshRequest>(self, (), |_, ()| {});
        } else if changes.project.is_some() {
            publish_all_document_diagnostics(self, client);
        } else if let Some(project) = self.projects.get(project_root) {
            for file in changes.scripts {
                if let Some(document) = project.db.document(file) {
                    let document = OpenDocumentHandle::from_document(document);
                    publish_diagnostics_if_needed(&document, self, client);
                }
            }
        }

        if capabilities.supports_semantic_tokens_refresh() {
            client.send_request::<lsp_types::SemanticTokensRefreshRequest>(self, (), |_, ()| {});
        }

        if capabilities.supports_inlay_hint_refresh() {
            client.send_request::<lsp_types::InlayHintRefreshRequest>(self, (), |_, ()| {});
        }

        // a synchronized environment is new search paths to watch
        self.update_file_watcher(client);
    }

    /// Requests synchronization using the scripts' saved metadata.
    fn synchronize_closed_scripts(
        db: &mut ProjectDatabase,
        scripts: &[File],
        client: &Client,
        capabilities: ResolvedClientCapabilities,
        progress: &ScriptProgress,
    ) {
        for &file in scripts {
            // Open and save handle editor documents separately. Their metadata may contain
            // unsaved changes, including overlays that are not in the diagnostic open-file set.
            if db.document(file).is_some() {
                continue;
            }
            Self::request_script_sync(
                db,
                file,
                client,
                capabilities,
                ScriptEnvironmentAvailability::Pending,
                progress,
            );
        }
    }

    fn request_script_sync(
        db: &mut ProjectDatabase,
        file: File,
        client: &Client,
        capabilities: ResolvedClientCapabilities,
        availability: ScriptEnvironmentAvailability,
        progress: &ScriptProgress,
    ) {
        let environments = db.uv_environments().clone();
        environments.request_sync(db, file, availability, &|db, file| {
            let file_path = file.path(db);
            let display_path = file_path.as_system_path().map_or_else(
                || file_path.to_string(),
                |path| {
                    path.strip_prefix(db.project().root(db))
                        .unwrap_or(path)
                        .to_string()
                },
            );
            progress.for_script(client, capabilities, display_path)
        });
    }

    /// Bumps the revision.
    ///
    /// The revision is used to track when workspace diagnostics may have changed and need to be re-run.
    /// It's okay if a bump doesn't necessarily result in new workspace diagnostics.
    ///
    /// In general, any change to a project database should bump the revision and so should
    /// any change to the document states (but also when the open workspaces change etc.).
    fn bump_revision(&mut self) {
        self.revision += 1;
    }

    /// The LSP specification doesn't allow configuration requests during initialization,
    /// but we need access to the configuration to resolve the settings in turn to create the
    /// project databases. This will become more important in the future when we support
    /// persistent caching. It's then crucial that we have the correct settings to select the
    /// right cache.
    ///
    /// We work around this by queueing up all messages that arrive between the `initialized` notification
    /// and the completion of workspace initialization (which waits for the client's configuration response).
    ///
    /// This queuing is only necessary when registering *new* workspaces. Changes to configurations
    /// don't need to go through the same process because we can update the existing
    /// database in place.
    ///
    /// See <https://github.com/Microsoft/language-server-protocol/issues/567#issuecomment-2085131917>
    pub(crate) fn should_defer_message(&mut self, message: Message) -> Option<Message> {
        if self.workspaces.all_initialized() {
            Some(message)
        } else {
            match &message {
                Message::Request(request) => {
                    if request.method == ShutdownRequest::METHOD.as_str() {
                        return Some(message);
                    }
                    tracing::debug!(
                        "Deferring `{}` request until all workspaces are initialized",
                        request.method
                    );
                }
                Message::Response(_) => {
                    // We still want to get client responses even during workspace initialization.
                    return Some(message);
                }
                Message::Notification(notification) => {
                    if notification.method == ExitNotification::METHOD.as_str() {
                        return Some(message);
                    }
                    tracing::debug!(
                        "Deferring `{}` notification until all workspaces are initialized",
                        notification.method
                    );
                }
            }

            self.deferred_messages.push_back(message);
            None
        }
    }

    pub(crate) fn workspaces(&self) -> &Workspaces {
        &self.workspaces
    }

    /// Returns a reference to the project's [`ProjectDatabase`] in which the given `path` belongs.
    ///
    /// If the path is a system path, it will return the project database that is closest to the
    /// given path, then one whose search paths contain it, then the first project when sorted by
    /// workspace folder path.
    ///
    /// If the path is a virtual path, it will return the first project database in the session.
    pub(crate) fn project_db(&self, path: &AnySystemPath) -> &ProjectDatabase {
        &self.project_state(path).db
    }

    /// Returns an iterator, in arbitrary order, over all project databases
    /// in this session.
    pub(crate) fn project_dbs(&self) -> impl Iterator<Item = &ProjectDatabase> {
        self.projects
            .values()
            .map(|project_state| &project_state.db)
    }

    /// Returns a mutable reference to the project's [`ProjectDatabase`] in which the given `path`
    /// belongs.
    ///
    /// Refer to [`project_db`] for more details on how the project is selected.
    ///
    /// [`project_db`]: Session::project_db
    fn project_db_mut(&mut self, path: &AnySystemPath) -> &mut ProjectDatabase {
        &mut self.project_state_mut(path).db
    }

    /// Returns a reference to the project's [`ProjectState`] in which the given `path` belongs.
    ///
    /// If the path is a system path, it will return the project database that is closest to the
    /// given path, then one whose search paths contain it, then the first project when sorted by
    /// workspace folder path.
    ///
    /// If the path is a virtual path, it will return the first project database in the session.
    fn project_state(&self, path: &AnySystemPath) -> &ProjectState {
        self.project_state_for_document(path)
            .map(|(_, project)| project)
            .expect("To always have at least one project")
    }

    /// Returns a mutable reference to the project's [`ProjectState`] in which the given `path`
    /// belongs.
    ///
    /// Refer to [`project_db`] for more details on how the project is selected.
    ///
    /// [`project_db`]: Session::project_db
    pub(crate) fn project_state_mut(&mut self, path: &AnySystemPath) -> &mut ProjectState {
        self.project_state_for_document(path)
            .map(|(root, _)| root.to_path_buf())
            .and_then(|root| self.projects.get_mut(&root))
            .expect("To always have at least one project")
    }

    /// Selects a project to use for analysis of the given document (identified
    /// by path): see [`select_project`].
    ///
    /// Returns a tuple where the first element is the workspace root of the
    /// selected project and the second element is the state object for the selected project.
    ///
    /// Returns None when no projects exist.
    fn project_state_for_document(
        &self,
        path: &AnySystemPath,
    ) -> Option<(&SystemPath, &ProjectState)> {
        select_project(
            self.projects.iter(),
            |project| &project.db,
            path.as_system().map(SystemPathBuf::as_path),
        )
        .map(|(workspace_root, project)| (workspace_root.as_path(), project))
    }

    /// applies `changes` to the project holding `path`
    ///
    /// the session's revision moves only if the project's database did: a change that changed
    /// nothing leaves every answer as it was, and must not wake what waits for the session to
    /// change
    pub(crate) fn apply_changes(
        &mut self,
        client: &Client,
        path: &AnySystemPath,
        changes: &[ChangeEvent],
    ) -> ChangeResult {
        let capabilities = self.resolved_client_capabilities;
        let script_progress = self.script_progress.clone();
        let db = self.project_db_mut(path);
        let result = db.apply_changes(changes);
        if let Some(project_path) = result.project_sync_path() {
            db.uv_environments()
                .request_project_sync(db, project_path, &|db, project| {
                    Some(Box::new(LazyWorkDoneProgress::new_on_main_loop(
                        client,
                        WorkDoneProgressBegin {
                            title: format!("Refreshing {} metadata", project.name(db)),
                            cancellable: Some(false),
                            message: None,
                            percentage: None,
                        },
                        capabilities,
                    )))
                });
        }
        let scripts = result.scripts_to_synchronize(db);
        Self::synchronize_closed_scripts(db, &scripts, client, capabilities, &script_progress);
        if result.database_changed() {
            self.bump_revision();
        }
        self.update_file_watcher(client);
        result
    }

    /// Returns a mutable iterator over all project databases.
    pub(crate) fn projects_mut(&mut self) -> impl Iterator<Item = &'_ mut ProjectDatabase> + '_ {
        self.project_states_mut().map(|project| &mut project.db)
    }

    /// Returns a mutable iterator over all projects.
    fn project_states_mut(&mut self) -> impl Iterator<Item = &'_ mut ProjectState> + '_ {
        self.projects.values_mut()
    }

    /// Initializes a sequence of workspace folders identified by URI
    /// along with its corresponding options.
    ///
    /// This is meant to be called when a response from a
    /// `workspace/configuration` request is received. (This is where
    /// the `ClientOptions` comes from.)
    ///
    /// It is legal to call this on URIs corresponding to workspace
    /// folders that are already initialized. When that occurs,
    /// they are skipped by this routine. That is, they are not
    /// re-initialized.
    ///
    /// The client provided is used to show error messages, publish
    /// diagnostics related to configuration and register capabilities.
    ///
    /// This is typically called when a response to a
    /// `workspace/configuration` request is received.
    pub(crate) fn initialize_workspace_folders(
        &mut self,
        client: &Client,
        workspace_folders: Vec<(Uri, ClientOptions)>,
    ) {
        // Every workspace folder can come with its own
        // global options. In theory, these can have different
        // values. At time of writing (2026-01-28), AG has been
        // unable to make VS Code cause this. This is because
        // the ty VS Code extension scopes its settings to the
        // "window":
        // https://github.com/astral-sh/ty-vscode/blob/e68f26549a920926d8a6bced942dfaf32313f851/package.json#L107
        //
        // So at least in theory, there is a semantic mismatch
        // between the LSP protocol and ty's LSP's understanding
        // of what settings are global and which aren't.
        //
        // We used to try and combine these global options across
        // multiple workspace folders. But when we did that, we
        // didn't actually support multiple workspace folders. We
        // always took the first workspace folder and ignored the
        // rest.
        //
        // In a world where we support multiple workspace folders,
        // we also need to support possibly _adding_ (or removing)
        // new workspace folders dynamically (that's the
        // `workspace/didChangeWorkspaceFolders` notification).
        // This in turn means that global options can change based
        // on new workspace folders being added.
        //
        // Should we try to merge the existing global options with
        // the new ones? What if a workspace folder is removed? Should
        // we try to update our global options based on that?
        //
        // It should be clear that there are many different
        // permutations of possibilities here. Perhaps the best
        // choice is to find a way to get rid of global options
        // entirely and make all settings specific to workspace
        // folders.
        //
        // In any case, our current strategy for now is to just use the
        // most recent global options received (after being combined
        // with the global options received at initialization time).
        // Doing anything more complicated seems unwarranted unless
        // real users are having problems as a result of this.
        //
        // Note that this is a divergence from previous behavior:
        // https://github.com/astral-sh/ruff/pull/19614
        let mut global_options: Option<GlobalOptions> = None;

        for (uri, options) in workspace_folders {
            // Last setting wins.
            global_options = Some(
                self.initialization_options
                    .options
                    .global
                    .clone()
                    .combine(options.global),
            );
            if !options.unknown.is_empty() {
                warn_about_unknown_options(client, Some(&uri), &options.unknown);
            }
            self.initialize_workspace_folder(client, &uri, options.workspace);
        }

        if let Some(global_options) = global_options {
            let global_settings = global_options.into_settings();
            self.global_settings = Arc::new(global_settings);
        }
        if let Some(check_mode) = self.global_settings.diagnostic_mode().to_check_mode() {
            for project in self.projects.values_mut() {
                project.db.set_check_mode(check_mode);
            }
        }

        self.watch_projects(client);
        self.register_capabilities(client);

        // New workspace settings can affect diagnostics in existing workspaces. Re-registering
        // diagnostic support does not invalidate the client's cached results. There are no
        // cached results to refresh during the initial workspace configuration.
        if self.workspace_configuration_initialized
            && self
                .client_capabilities()
                .supports_workspace_diagnostic_refresh()
        {
            client.send_request::<lsp_types::DiagnosticRefreshRequest>(self, (), |_, ()| {});
        }
        self.workspace_configuration_initialized = true;
    }

    /// Brings the session's own file system watcher up to date with the projects, and asks
    /// the client to watch instead wherever it cannot.
    ///
    /// Called after anything that can change what there is to watch. Cheap when nothing did.
    pub(crate) fn update_file_watcher(&mut self, client: &Client) {
        self.watch_projects(client);
        if !self.needs_client_file_watcher() {
            return;
        }
        let Some(file_watcher) = &mut self.file_watcher else {
            return;
        };
        if let Some(update) = file_watcher.update(self.projects.values().map(|state| &state.db)) {
            update.apply(self, client);
        }
    }

    /// Watches what every project needs watched: its root, its included paths, the module
    /// search paths outside it, and its configuration files.
    ///
    /// ## why the server watches, when the client offers to
    ///
    /// A client's `workspace/didChangeWatchedFiles` says a file changed, and the only thing
    /// the server can do with that is read the file. That is only right if the change is
    /// on disk by the time the notification is read, and the protocol does not say it has to
    /// be. PyCharm's client sends the notification when its virtual file system takes a save,
    /// while the bytes can still be queued to be written: the server then reads the old text,
    /// nothing tells it again once the write lands, and every open file depending on the saved
    /// one keeps its diagnostics, hover, and types from before the save.
    ///
    /// The operating system reports a write after it happens, so a change reaches the session
    /// no earlier than the bytes do.
    ///
    /// The client is not asked to watch as well while this one works; see
    /// [`Self::needs_client_file_watcher`].
    fn watch_projects(&mut self, client: &Client) {
        if matches!(self.own_watcher, OwnWatcher::Off) {
            return;
        }
        if self.projects.is_empty() {
            // the last workspace folder was removed: nothing left to watch
            self.own_watcher = OwnWatcher::Idle;
            return;
        }

        let dbs = self.projects.values_mut().map(|project| &mut project.db);
        if let OwnWatcher::Running(watcher) = &mut self.own_watcher {
            watcher.update_projects(dbs);
            return;
        }

        match ty_project::watch::directory_watcher(client.file_system_changes()) {
            Ok(watcher) => {
                self.own_watcher =
                    OwnWatcher::Running(Box::new(ProjectWatcher::for_projects(watcher, dbs)));
            }
            Err(error) => {
                tracing::warn!(
                    "Failed to start watching the file system, asking the client to report changes instead: {error}"
                );
                self.own_watcher = OwnWatcher::Off;
            }
        }
    }

    /// Whether the client has to report file changes, because the session's own watcher is not
    /// running or could not watch every path.
    ///
    /// Not both. A client that reports a change before it is on disk makes the session read the
    /// old text, which this watcher then corrects when the write lands: a second pass over the
    /// same change, and a second refresh of everything the editor shows, for every save. A caller
    /// that has to be answered from the new text at once cannot rely on either watcher, and has
    /// to say so itself: write the file, then send `workspace/didChangeWatchedFiles`, then ask.
    ///
    /// A session that watches for itself but has no project yet has nothing for either watcher
    /// to watch, and does not ask the client: a registration made then would outlive the moment
    /// the session's own watcher starts.
    fn needs_client_file_watcher(&self) -> bool {
        match &self.own_watcher {
            OwnWatcher::Running(watcher) => watcher.has_errored_paths(),
            OwnWatcher::Idle => false,
            OwnWatcher::Off => true,
        }
    }

    /// Initializes a single workspace folder with the given URI
    /// and options.
    ///
    /// If this workspace folder has already been initialized, then
    /// this is a no-op.
    ///
    /// The client provided is used to show error messages and publish
    /// diagnostics related to configuration.
    fn initialize_workspace_folder(
        &mut self,
        client: &Client,
        uri: &Uri,
        options: WorkspaceOptions,
    ) {
        let options = self
            .initialization_options
            .options
            .workspace
            .clone()
            .combine(options);

        tracing::debug!("Initializing workspace `{uri}`: {options:#?}");

        let Ok(root) = uri.to_file_path() else {
            tracing::debug!("Ignoring workspace with non-path root: {uri}");
            return;
        };

        // Realistically I don't think this can fail because we got the path from a Uri
        let root = match SystemPathBuf::from_path_buf(root) {
            Ok(root) => root,
            Err(root) => {
                tracing::debug!(
                    "Ignoring workspace with non-UTF8 root: {root}",
                    root = root.display()
                );
                return;
            }
        };

        // Zed sends a single file as a workspace folder. Preserve that path as the
        // workspace's identity, but resolve configuration and imports from its parent directory.
        // https://github.com/zed-industries/zed/issues/40627
        let workspace_directory = if self.native_system.is_file(&root)
            && let Some(parent) = root.parent()
        {
            parent
        } else {
            root.as_path()
        };

        let settings = options.into_settings(workspace_directory, client, &*self.native_system);
        let Some(workspace) = self.workspaces.workspaces.get_mut(&root) else {
            tracing::debug!("Ignoring workspace `{uri}` since it was not registered");
            return;
        };
        if workspace.is_initialized() {
            tracing::debug!(
                "Ignoring workspace initialization for `{uri}` \
                 since it has already been initialized"
            );
            return;
        }
        workspace.initialize(settings);

        // For now, create one project database per workspace. Future support for nested projects
        // may instead manage the project collection inside ProjectDatabase.
        let system = LSPSystem::new(
            self.index.as_ref().unwrap().clone(),
            self.native_system.clone(),
            self.initialization_options.workspace_trust,
        );

        let configuration_file = workspace.settings.configuration_file();

        let metadata = if let Some(configuration_file) = configuration_file {
            ProjectMetadata::from_config_file(
                configuration_file.clone(),
                workspace_directory,
                &system,
            )
        } else {
            ProjectMetadata::discover(workspace_directory, &system)
        }
        .map(|metadata| metadata.with_use_uv(self.use_uv));

        let (mut metadata, discovery_result) =
            match metadata.context("Failed to discover project configuration") {
                Ok(metadata) => (metadata, Ok(())),
                Err(err) => {
                    let Ok(metadata) = ProjectMetadata::from_options(
                        Options::default(),
                        workspace_directory.to_path_buf(),
                        None,
                        &UseDefaultStrategy,
                    );
                    (metadata.with_use_uv(self.use_uv), Err(err))
                }
            };

        if let Some(fallback_options) = workspace.settings.fallback_options() {
            metadata.set_fallback_options(fallback_options.clone());
        }

        if let Some(override_options) = workspace.settings.override_options() {
            metadata.set_override_options(override_options.clone());
        }

        // Load user configuration even if project discovery failed.
        let project = discovery_result
            .and(
                metadata
                    .apply_configuration_files(&system)
                    .context("Failed to apply configuration files"),
            )
            .and_then(|()| {
                if metadata.use_uv().workspace_discovery_enabled() {
                    let workspace = UvWorkspace::discover(workspace_directory, &system);
                    metadata
                        .apply_uv_workspace(&system, workspace)
                        .context("Failed to discover uv workspace")?;
                }
                ProjectDatabase::fallible(metadata.clone(), system.clone())
            });

        let mut db = project.unwrap_or_else(|err| {
            tracing::error!(
                "Failed to load project for workspace `{uri}`: {err:#}. \
                 Continuing with valid settings"
            );
            client.show_error_message(format!(
                "Failed to load project for workspace {uri}. {}",
                self.client_name.log_guidance(),
            ));
            ProjectDatabase::use_defaults(metadata, system)
        });

        // the same checker `by check` registers, so that a python file's django
        // diagnostics reach the editor through `check_file` — one set of rules, one
        // set of suppressions
        db.set_checker(Arc::new(ty_ide::DjangoChecker));

        // Carry forward diagnostic state if any exists
        let previous = self.projects.remove(&root);
        let untracked = previous
            .map(|state| state.untracked_files_with_pushed_diagnostics)
            .unwrap_or_default();
        let scripts: Vec<_> = db.project().script_files(&db).iter().collect();
        Self::synchronize_closed_scripts(
            &mut db,
            &scripts,
            client,
            self.resolved_client_capabilities,
            &self.script_progress,
        );
        self.projects.insert(
            root.clone(),
            ProjectState {
                db,
                untracked_files_with_pushed_diagnostics: untracked,
            },
        );

        publish_settings_diagnostics(self, client, root);
    }

    /// Adds an uninitialized workspace to this session.
    ///
    /// This returns `true` when this workspace is added and `false`
    /// when it has already been added.
    ///
    /// If there was a problem adding the workspace folder (e.g., the
    /// path derived from the given URI is not valid UTF-8), then an
    /// error is returned and no workspace folder is registered.
    ///
    /// To initialize the workspace folder, callers must initiate
    /// a request for workspace folder configuration via
    /// `Session::request_uninitialized_workspace_folder_configuration`.
    pub(crate) fn register_workspace_folder(&mut self, uri: Uri) -> anyhow::Result<bool> {
        self.workspaces.register(uri)
    }

    /// Requests configuration for each registered but uninitialized
    /// workspace folder in this session.
    ///
    /// When all workspace folders in this session are initialized, then
    /// this is a no-op.
    ///
    /// Each uninitialized workspace folder will be fully initialized
    /// once the configuration response is received (asynchronously).
    /// When the client doesn't support requesting workspace
    /// configuration, the workspace folder is initialized immediately
    /// using the options this session was initialized with.
    ///
    /// Adding an uninitialized workspace to this session can be done
    /// with `Session::register_workspace_folder`.
    pub(crate) fn request_uninitialized_workspace_folder_configurations(
        &mut self,
        client: &Client,
    ) {
        // When all workspaces are already initialized, then
        // there's nothing to do.
        if self.workspaces().all_initialized() {
            return;
        }

        let uninit_workspace_uris: Vec<Uri> = self
            .workspaces()
            .into_iter()
            .filter_map(|(_, workspace)| {
                if workspace.is_initialized() {
                    None
                } else {
                    Some(workspace.uri().clone())
                }
            })
            .collect();

        if !self
            .client_capabilities()
            .supports_workspace_configuration()
        {
            tracing::info!(
                "Client does not support workspace configuration, initializing workspaces \
                 using the initialization options"
            );
            self.initialize_workspace_folders(
                client,
                uninit_workspace_uris
                    .into_iter()
                    .map(|uri| (uri, self.initialization_options().options.clone()))
                    .collect::<Vec<_>>(),
            );
            return;
        }

        let items: Vec<lsp_types::ConfigurationItem> = uninit_workspace_uris
            .iter()
            .map(|uri| lsp_types::ConfigurationItem {
                scope_uri: Some(uri.clone()),
                section: Some("ty".to_string()),
            })
            .collect();

        tracing::debug!("Requesting workspace configuration for workspaces");
        client.send_request::<lsp_types::ConfigurationRequest>(
            self,
            lsp_types::ConfigurationParams { items },
            move |client, result: Vec<serde_json::Value>| {
                tracing::debug!("Received workspace configurations, initializing workspaces");

                // This shouldn't fail because, as per the spec, the client needs to provide a
                // `null` value even if it cannot provide a configuration for a workspace.
                assert_eq!(
                    result.len(),
                    uninit_workspace_uris.len(),
                    "Mismatch in number of workspace URIs ({}) and configuration results ({})",
                    uninit_workspace_uris.len(),
                    result.len()
                );

                let workspaces_with_options: Vec<_> = uninit_workspace_uris
                    .into_iter()
                    .zip(result)
                    .map(|(uri, value)| {
                        if value.is_null() {
                            tracing::debug!(
                                "No workspace options provided for {uri}, using default options"
                            );
                            return (uri, ClientOptions::default());
                        }
                        let options: ClientOptions =
                            serde_json::from_value(value).unwrap_or_else(|err| {
                                tracing::error!(
                                    "Failed to deserialize workspace options for {uri}: {err}. \
                                        Using default options"
                                );
                                ClientOptions::default()
                            });
                        (uri, options)
                    })
                    .collect();

                client.queue_action(Action::InitializeWorkspaces(workspaces_with_options));
            },
        );
    }

    /// Removes a workspace folder at the given URI.
    ///
    /// This removes the workspace folder and its associated project database,
    /// and clears diagnostics for any documents that were in the workspace.
    ///
    /// # Errors
    ///
    /// This returns an error if the workspace folder has already been removed
    /// or otherwise could not be found.
    pub(crate) fn remove_workspace_folder(
        &mut self,
        client: &Client,
        uri: &Uri,
    ) -> anyhow::Result<()> {
        tracing::info!("Removing workspace folder: {uri}");

        let path = uri
            .to_file_path()
            .map_err(|()| anyhow!("Workspace URI is not a file path: {uri}"))?;
        let workspace_path = SystemPathBuf::from_path_buf(path)
            .map_err(|path| anyhow!("Workspace path is not valid UTF-8: {}", path.display()))?;

        anyhow::ensure!(
            self.workspaces.unregister(&workspace_path),
            "Workspace not found: {uri}",
        );

        // Note that it is somewhat unclear whether we actually need to
        // clear diagnostics here. It seems that, at least in the case
        // of VS Code, it will auto-clear any diagnostics not found in
        // the workspace diagnostic response. Moreover, VS Code will
        // re-request workspace diagnostics after removing a workspace
        // folder.
        //
        // For now, we keep unconditionally clearing diagnostics on
        // opened text documents for reasons of good sense, but it's
        // possible that we don't even need to do that (when workspace
        // diagnostics are enabled).
        //
        // See: https://github.com/astral-sh/ruff/pull/22953#discussion_r2745255350

        // Remove the associated project database.
        if let Some(project_state) = self.projects.remove(&workspace_path) {
            // Clear diagnostics for any files that had pushed diagnostics in this project.
            for file_uri in project_state.untracked_files_with_pushed_diagnostics {
                self.clear_diagnostics(client, &file_uri);
            }
        }

        // Collect all of the documents to clear upfront to
        // work around borrowck.
        let documents_to_clear: Vec<OpenDocumentHandle> = self
            .text_document_handles()
            .filter_map(|doc| {
                if let AnySystemPath::System(ref path) = *doc.notebook_or_file_path()
                    && path.starts_with(&workspace_path)
                {
                    Some(doc)
                } else {
                    None
                }
            })
            .collect();
        for doc in documents_to_clear {
            self.clear_diagnostics_if_needed(&doc, client);
        }

        self.update_file_watcher(client);
        self.bump_revision();

        Ok(())
    }

    pub(crate) fn clear_diagnostics_if_needed(
        &self,
        document: &OpenDocumentHandle,
        client: &Client,
    ) {
        if self.client_capabilities().supports_pull_diagnostics() && !document.is_cell_or_notebook()
        {
            return;
        }
        self.clear_diagnostics(client, document.uri());
    }

    /// Clears the diagnostics for the document identified by `uri`.
    ///
    /// This is done by notifying the client with an empty list of diagnostics for the document.
    /// For notebook cells, this clears diagnostics for the specific cell.
    /// For other document types, this clears diagnostics for the main document.
    fn clear_diagnostics(&self, client: &Client, uri: &Uri) {
        if self.global_settings().diagnostic_mode().is_off() {
            return;
        }
        client.send_notification::<lsp_types::PublishDiagnosticsNotification>(
            lsp_types::PublishDiagnosticsParams {
                uri: uri.clone(),
                diagnostics: vec![],
                version: None,
            },
        );
    }

    pub(crate) fn take_deferred_messages(&mut self) -> Option<Message> {
        if self.workspaces.all_initialized() {
            self.deferred_messages.pop_front()
        } else {
            None
        }
    }

    /// Registers the dynamic capabilities with the client as per the resolved global settings.
    ///
    /// ## Diagnostic capability
    ///
    /// This capability is used to enable / disable workspace diagnostics as per the
    /// `ty.diagnosticMode` global setting.
    ///
    /// ## Rename capability
    ///
    /// This capability is used to enable / disable rename functionality as per the
    /// `ty.experimental.rename` global setting.
    fn register_capabilities(&mut self, client: &Client) {
        static DIAGNOSTIC_REGISTRATION_ID: &str = "ty/textDocument/diagnostic";

        let mut registrations = vec![];
        let mut unregistrations = vec![];

        if self
            .resolved_client_capabilities
            .supports_diagnostic_dynamic_registration()
        {
            if self
                .registrations
                .contains(DocumentDiagnosticRequest::METHOD.as_str())
            {
                unregistrations.push(Unregistration {
                    id: DIAGNOSTIC_REGISTRATION_ID.into(),
                    method: DocumentDiagnosticRequest::METHOD.into(),
                });
            }

            let diagnostic_mode = self.global_settings.diagnostic_mode;

            match diagnostic_mode {
                DiagnosticMode::Off => {
                    tracing::debug!(
                        "Skipping registration of diagnostic capability \
                        because diagnostics are turned off"
                    );
                }
                DiagnosticMode::OpenFilesOnly | DiagnosticMode::Workspace => {
                    tracing::debug!(
                        "Registering diagnostic capability \
                        with {diagnostic_mode:?} diagnostic mode"
                    );
                    registrations.push(Registration {
                        id: DIAGNOSTIC_REGISTRATION_ID.into(),
                        method: DocumentDiagnosticRequest::METHOD.into(),
                        register_options: Some(
                            serde_json::to_value(
                                DiagnosticProvider::DiagnosticRegistrationOptions(
                                    DiagnosticRegistrationOptions {
                                        diagnostic_options: server_diagnostic_options(
                                            diagnostic_mode.is_workspace(),
                                        ),
                                        ..Default::default()
                                    },
                                ),
                            )
                            .unwrap(),
                        ),
                    });
                }
            }
        }

        // First, unregister any existing capabilities and then register or re-register them.
        self.unregister_dynamic_capability(client, unregistrations);
        self.register_dynamic_capability(client, registrations);
        self.update_file_watcher(client);
    }

    /// Registers a list of dynamic capabilities with the client.
    fn register_dynamic_capability(&mut self, client: &Client, registrations: Vec<Registration>) {
        if registrations.is_empty() {
            return;
        }

        for registration in &registrations {
            self.registrations.insert(registration.method.clone());
        }

        client.send_request::<RegistrationRequest>(
            self,
            RegistrationParams { registrations },
            |_: &Client, ()| {
                tracing::debug!("Registered dynamic capabilities");
            },
        );
    }

    /// Unregisters a list of dynamic capabilities with the client.
    fn unregister_dynamic_capability(
        &mut self,
        client: &Client,
        unregistrations: Vec<Unregistration>,
    ) {
        if unregistrations.is_empty() {
            return;
        }

        for unregistration in &unregistrations {
            if !self.registrations.remove(&unregistration.method) {
                tracing::debug!(
                    "Unregistration for `{}` was requested, but it was not registered",
                    unregistration.method
                );
            }
        }

        client.send_request::<UnregistrationRequest>(
            self,
            UnregistrationParams {
                unregisterations: unregistrations,
            },
            |_: &Client, ()| {
                tracing::debug!("Unregistered dynamic capabilities");
            },
        );
    }

    /// Captures client settings and a target for a document request.
    pub(crate) fn snapshot_document(&self, uri: &Uri) -> Result<DocumentSnapshot, DocumentError> {
        let document = self.document_target(uri)?;

        Ok(DocumentSnapshot {
            resolved_client_capabilities: self.resolved_client_capabilities,
            global_settings: self.global_settings.clone(),
            workspace_settings: self
                .project_state_for_document(document.notebook_or_file_path())
                .and_then(|(workspace_root, _)| self.workspaces.settings_for_path(workspace_root))
                .unwrap_or_else(|| Arc::new(WorkspaceSettings::default())),
            position_encoding: self.position_encoding,
            document,
            client_name: self.client_name,
        })
    }

    /// Selects an open document or a supported closed document for a request.
    fn document_target(&self, uri: &Uri) -> Result<DocumentRequestTarget, DocumentError> {
        match self.index().open_document_handle(uri) {
            Ok(handle) => Ok(DocumentRequestTarget::Open(handle)),
            Err(DocumentError::NotFound(key))
                if let DocumentKey::File(path) = &key
                    && self.is_supported_closed_file(path) =>
            {
                Ok(DocumentRequestTarget::Closed {
                    uri: uri.clone(),
                    path: key.into_file_path(),
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Returns whether requests may target this file while it is closed.
    fn is_supported_closed_file(&self, path: &SystemPath) -> bool {
        // Closed notebooks lack the client's cell URI and position mappings.
        if PySourceType::try_from_path(path) == Some(PySourceType::Ipynb) {
            return false;
        }

        // A request needs a project, but the file need not belong to it:
        // unrelated files use the same fallback project as open files.
        !self.projects.is_empty()
    }

    /// Creates a snapshot of the current state of the [`Session`].
    pub(crate) fn snapshot_session(&self) -> SessionSnapshot {
        SessionSnapshot {
            workspace_roots: self.projects.keys().cloned().collect(),
            projects: self
                .projects
                .values()
                .map(|project| &project.db)
                .cloned()
                .collect(),
            index: self.index.clone().unwrap(),
            global_settings: self.global_settings.clone(),
            position_encoding: self.position_encoding,
            in_test: self.in_test,
            resolved_client_capabilities: self.resolved_client_capabilities,
            revision: self.revision,
            client_name: self.client_name,
            native_system: self.native_system.clone(),
        }
    }

    /// Iterates over handles to all open text documents.
    pub(super) fn text_document_handles(&self) -> impl Iterator<Item = OpenDocumentHandle> + '_ {
        self.index()
            .text_documents()
            .map(|(_, document)| OpenDocumentHandle::from_text_document(document))
    }

    /// Iterates over all open file-level documents.
    ///
    /// Notebook cells are excluded because their file-level representation is the containing
    /// notebook.
    pub(super) fn file_document_handles(&self) -> impl Iterator<Item = OpenDocumentHandle> + '_ {
        self.index()
            .file_documents()
            .map(OpenDocumentHandle::from_document)
    }

    /// Returns a handle to the open document specified by its URI.
    ///
    /// # Errors
    ///
    /// Returns an error if the document is not open in the session.
    pub(crate) fn open_document_handle(
        &self,
        uri: &lsp_types::Uri,
    ) -> Result<OpenDocumentHandle, DocumentError> {
        self.index().open_document_handle(uri)
    }

    /// Registers a notebook document at the provided `path`.
    /// If a document is already open here, it will be overwritten.
    ///
    /// Returns a handle to the opened document.
    pub(crate) fn open_notebook_document(
        &mut self,
        client: &Client,
        document: NotebookDocument,
    ) -> OpenDocumentHandle {
        let handle = self.index_mut().open_notebook_document(document);
        self.open_document_in_db(client, &handle, None);
        handle
    }

    /// Registers a text document at the provided `path`.
    /// If a document is already open here, it will be overwritten.
    ///
    /// Starts script synchronization from the backing file before installing the editor contents.
    ///
    /// Returns a handle to the opened document.
    pub(crate) fn open_text_document(
        &mut self,
        client: &Client,
        document: TextDocument,
    ) -> OpenDocumentHandle {
        let language_id = document.language_id();

        // Request synchronization before installing the editor contents because uv reads the
        // script from disk. This ensures both use the saved metadata, so saving changed metadata
        // requests another synchronization.
        if self.use_uv != UseUv::Off
            && language_id.is_python()
            && document.notebook().is_none()
            && let DocumentKey::File(system_path) = DocumentKey::from_uri(document.uri())
        {
            let capabilities = self.resolved_client_capabilities;
            let script_progress = self.script_progress.clone();
            let db = self.project_db_mut(&AnySystemPath::System(system_path.clone()));

            // Refresh any cached disk revision before reading the script tag. A filesystem
            // change may not have reached the watcher yet, and the later open event will
            // refresh the file from the editor contents instead.
            File::sync_path(db, &system_path);
            if let Ok(file) = system_path_to_file(db, &system_path) {
                Self::request_script_sync(
                    db,
                    file,
                    client,
                    capabilities,
                    ScriptEnvironmentAvailability::Pending,
                    &script_progress,
                );
            }
        }
        let handle = self.index_mut().open_text_document(document);
        self.open_document_in_db(client, &handle, Some(language_id));
        handle
    }

    fn open_document_in_db(
        &mut self,
        client: &Client,
        document: &OpenDocumentHandle,
        language_id: Option<LanguageId>,
    ) {
        let path = document.notebook_or_file_path();

        // This is a "maybe" because the `File` might've not been interned yet i.e., the
        // `try_system` call will return `None` which doesn't mean that the file is new, it's just
        // that the server didn't need the file yet.
        let _is_maybe_new_system_file = path.as_system().is_some_and(|system_path| {
            let db = self.project_db(path);
            db.files()
                .try_system(db, system_path)
                .is_none_or(|file| !file.exists(db))
        });

        // A document in a language the server has no services at all for is
        // tracked by the index but never enters the project.
        let is_unsupported = matches!(language_id, Some(LanguageId::Other));

        // A django template does have language services, so its `File` has to be
        // interned for its source to be readable — but it is not python, so it is
        // never added to the project's open-file set. That is what keeps the type
        // checker from ever being handed a `.html` file.
        let is_template = language_id.is_some_and(LanguageId::is_django_template);

        match path {
            AnySystemPath::System(system_path) => {
                self.apply_changes(client, path, &[ChangeEvent::Opened(system_path.clone())]);

                if is_unsupported {
                    return;
                }

                let db = self.project_db_mut(path);
                match system_path_to_file(db, system_path) {
                    Ok(file) => {
                        let project = db.project();

                        // Only mark this file as open if it's part of the project.
                        // This ensures that we don't show diagnostics for files outside the project.
                        if !is_template && project.is_file_included(db, system_path).is_included() {
                            project.open_file(db, file);
                        }
                    }
                    Err(err) => tracing::warn!("Failed to open file {system_path}: {err}"),
                }
            }
            AnySystemPath::SystemVirtual(virtual_path) => {
                if is_unsupported {
                    return;
                }

                let db = self.project_db_mut(path);
                let virtual_file = db.files().virtual_file(db, virtual_path);
                if !is_template {
                    db.project().open_file(db, virtual_file.file());
                }
            }
        }

        self.bump_revision();
        self.update_file_watcher(client);
    }

    /// Returns a reference to the index.
    ///
    /// # Panics
    ///
    /// Panics if there's a mutable reference to the index via [`index_mut`].
    ///
    /// [`index_mut`]: Session::index_mut
    fn index(&self) -> &Index {
        self.index.as_ref().unwrap()
    }

    /// Returns a mutable reference to the index.
    ///
    /// This method drops all references to the index and returns a guard that will restore the
    /// references when dropped. This guard holds the only reference to the index and allows
    /// modifying it.
    fn index_mut(&mut self) -> MutIndexGuard<'_> {
        let index = self.index.take().unwrap();

        for db in self.projects_mut() {
            // Remove the `index` from each database. This drops the count of `Arc<Index>` down to 1
            db.system_mut()
                .as_any_mut()
                .downcast_mut::<LSPSystem>()
                .unwrap()
                .take_index();
        }

        // There should now be exactly one reference to index which is self.index.
        let index = Arc::into_inner(index).unwrap();

        MutIndexGuard {
            session: self,
            index: Some(index),
        }
    }

    pub(crate) fn client_capabilities(&self) -> ResolvedClientCapabilities {
        self.resolved_client_capabilities
    }

    pub(crate) fn global_settings(&self) -> &GlobalSettings {
        &self.global_settings
    }

    pub(crate) fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    pub(crate) fn client_name(&self) -> ClientName {
        self.client_name
    }
}

/// A guard that holds the only reference to the index and allows modifying it.
///
/// When dropped, this guard restores all references to the index.
struct MutIndexGuard<'a> {
    session: &'a mut Session,
    index: Option<Index>,
}

impl Deref for MutIndexGuard<'_> {
    type Target = Index;

    fn deref(&self) -> &Self::Target {
        self.index.as_ref().unwrap()
    }
}

impl DerefMut for MutIndexGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.index.as_mut().unwrap()
    }
}

impl Drop for MutIndexGuard<'_> {
    fn drop(&mut self) {
        if let Some(index) = self.index.take() {
            let index = Arc::new(index);
            for db in self.session.projects_mut() {
                db.system_mut()
                    .as_any_mut()
                    .downcast_mut::<LSPSystem>()
                    .unwrap()
                    .set_index(index.clone());
            }

            self.session.index = Some(index);
        }
    }
}

/// An immutable snapshot of [`Session`] that references a specific document.
///
/// The document may be open or closed. Creating the snapshot does not open it.
#[derive(Debug)]
pub(crate) struct DocumentSnapshot {
    resolved_client_capabilities: ResolvedClientCapabilities,
    global_settings: Arc<GlobalSettings>,
    workspace_settings: Arc<WorkspaceSettings>,
    position_encoding: PositionEncoding,
    document: DocumentRequestTarget,
    client_name: ClientName,
}

impl DocumentSnapshot {
    /// Returns the resolved client capabilities that were captured during initialization.
    pub(crate) fn resolved_client_capabilities(&self) -> ResolvedClientCapabilities {
        self.resolved_client_capabilities
    }

    /// Returns the position encoding that was negotiated during initialization.
    pub(crate) fn encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    /// Returns the client settings for all workspaces.
    pub(crate) fn global_settings(&self) -> &GlobalSettings {
        &self.global_settings
    }

    /// Returns the client settings for the workspace that this document belongs to.
    pub(crate) fn workspace_settings(&self) -> &WorkspaceSettings {
        &self.workspace_settings
    }

    /// Returns the document targeted by this snapshot.
    pub(crate) fn document(&self) -> &DocumentRequestTarget {
        &self.document
    }

    pub(crate) fn client_name(&self) -> ClientName {
        self.client_name
    }

    /// Whether this document is a django template rather than python.
    pub(crate) fn is_django_template(&self) -> bool {
        self.document.is_django_template()
    }
}

/// A document targeted by a request, whether open or closed.
///
/// Request handlers use this type to resolve a database file without requiring the client to
/// open the document. Updating or closing a client document requires an [`OpenDocumentHandle`].
#[derive(Debug)]
pub(crate) enum DocumentRequestTarget {
    /// A document opened by the client.
    Open(OpenDocumentHandle),
    /// A closed document identified by its URI and path.
    Closed {
        /// The URI supplied by the client.
        uri: Uri,
        /// The path used to resolve the document in the database.
        path: AnySystemPath,
    },
}

impl DocumentRequestTarget {
    /// Returns the database file for this document, or its containing notebook for a cell.
    ///
    /// Returns [`None`] if the file cannot be resolved.
    pub(crate) fn to_notebook_or_file(&self, db: &ProjectDatabase) -> Option<File> {
        let file = match self {
            Self::Open(handle) => handle.notebook_or_file(db),
            Self::Closed { path, .. } => {
                let path = path.as_system()?;
                if let Some(root) = ty_ide::cached_vendored_root(db)
                    && let Some(vendored) = ty_ide::map_system_to_vendored(&root, path)
                {
                    // Reuse the bundled file's database identity instead of creating
                    // a separate file for its cached copy.
                    vendored_path_to_file(db, vendored).ok()
                } else {
                    system_path_to_file(db, path).ok()
                }
            }
        };
        if file.is_none() {
            tracing::debug!(
                "Failed to resolve file: file not found for `{}`",
                self.uri()
            );
        }
        file
    }

    /// Returns whether the document is a cell in a notebook opened by the client.
    pub(crate) fn is_cell(&self) -> bool {
        matches!(self, Self::Open(handle) if handle.is_cell())
    }

    /// Returns whether the document is a notebook, or a cell in one, opened by the client.
    ///
    /// A closed notebook is never a request's target: it lacks the client's cell URIs.
    pub(crate) fn is_cell_or_notebook(&self) -> bool {
        matches!(self, Self::Open(handle) if handle.is_cell_or_notebook())
    }

    /// Returns the document URI supplied by the client.
    pub(crate) fn uri(&self) -> &Uri {
        match self {
            Self::Open(handle) => handle.uri(),
            Self::Closed { uri, .. } => uri,
        }
    }

    /// The client's version of an open document. a closed document has no version of its own,
    /// so every request against one sees the same version
    pub(crate) fn version(&self) -> DocumentVersion {
        match self {
            Self::Open(handle) => handle.version(),
            Self::Closed { .. } => 0,
        }
    }

    /// Returns the document's path, or its containing notebook's path for a cell.
    pub(crate) fn notebook_or_file_path(&self) -> &AnySystemPath {
        match self {
            Self::Open(handle) => handle.notebook_or_file_path(),
            Self::Closed { path, .. } => path,
        }
    }

    /// Whether this document is a django template rather than python.
    ///
    /// A closed document has no language the client gave it, so it is what its path says.
    fn is_django_template(&self) -> bool {
        match self {
            Self::Open(handle) => handle.is_django_template(),
            Self::Closed { path, .. } => LanguageId::of_path(path).is_django_template(),
        }
    }
}

/// Selects, of `projects` keyed and sorted by workspace root, the project to use for analysis of
/// the file at `path`, or of a file the server is asked about at no path.
///
/// When `path` is given, we select the project registered under the closest containing workspace
/// folder. Otherwise, we select the first project by workspace-root path whose import search paths
/// contain the file. If no project matches, or there is no path, we select the project associated
/// with the workspace root path that sorts first lexicographically.
///
/// Returns None when no projects exist.
fn select_project<'a, P>(
    mut projects: impl DoubleEndedIterator<Item = (&'a SystemPathBuf, &'a P)> + Clone,
    db: impl Fn(&P) -> &ProjectDatabase,
    path: Option<&SystemPath>,
) -> Option<(&'a SystemPathBuf, &'a P)> {
    path.and_then(|path| {
        // a root above `path` sorts before it, and the closest such root last
        projects
            .clone()
            .rev()
            .find(|(root, _)| path.starts_with(root))
            .or_else(|| {
                projects
                    .clone()
                    .find(|(_, project)| db(project).program_for_dependency(path).is_some())
            })
    })
    .or_else(move || projects.next())
}

/// An immutable snapshot of the current state of [`Session`].
pub(crate) struct SessionSnapshot {
    index: Arc<Index>,
    global_settings: Arc<GlobalSettings>,
    position_encoding: PositionEncoding,
    resolved_client_capabilities: ResolvedClientCapabilities,
    in_test: bool,
    revision: u64,
    client_name: ClientName,

    /// The file system underneath the editor's buffers.
    ///
    /// Held so that a snapshot can ask what a file says on disk, which is a different
    /// question from what [`Self::open_documents`] answers.
    native_system: Arc<dyn System + 'static + Send + Sync + RefUnwindSafe>,

    /// The workspace root each of [`Self::projects`] is registered under, in the same order.
    workspace_roots: Vec<SystemPathBuf>,

    /// IMPORTANT: It's important that the databases come last, or at least,
    /// after any `Arc` that we try to extract or mutate in-place using `Arc::into_inner`
    /// and that relies on Salsa's cancellation to guarantee that there's now only a
    /// single reference to it (e.g. see [`Session::index_mut`]).
    ///
    /// Making this field come last guarantees that the db's `Drop` handler is
    /// dropped after all other fields, which ensures that
    /// Salsa's cancellation blocks until all fields are dropped (and not only
    /// waits for the db to be dropped while we still hold on to the `Index`).
    projects: Vec<ProjectDatabase>,
}

impl SessionSnapshot {
    pub(crate) fn projects(&self) -> &[ProjectDatabase] {
        &self.projects
    }

    /// The project the session analyses the file at `path` in, as it would were the file open,
    /// or the first project for a request that names no file: see [`select_project`].
    ///
    /// Returns None when no projects exist.
    pub(crate) fn project_for_path(&self, path: Option<&SystemPath>) -> Option<&ProjectDatabase> {
        select_project(
            self.workspace_roots.iter().zip(&self.projects),
            |db| db,
            path,
        )
        .map(|(_, db)| db)
    }

    pub(crate) fn index(&self) -> &Index {
        &self.index
    }

    /// Every file-backed document the editor has open, with the path it stands for.
    ///
    /// Notebooks are included, because the whole point of asking is whether the editor is
    /// holding something the file system is not. Notebook *cells* are not: a cell's
    /// file-level representation is the notebook that contains it, which is already here.
    ///
    /// A document behind a non-`file` URI has no path on disk at all, and is left out — there
    /// is nothing to compare it to, and a check that reads it is reading something a caller on
    /// the command line could never have seen either.
    pub(crate) fn open_documents(&self) -> impl Iterator<Item = (&SystemPath, OpenDocument<'_>)> {
        self.index
            .keyed_file_documents()
            .filter_map(|(key, document)| {
                let document = match document {
                    Document::Text(text) => OpenDocument::Text(text.contents()),
                    Document::Notebook(_) => OpenDocument::Notebook,
                };
                Some((key.file_path()?.as_path(), document))
            })
    }

    /// What `path` says on disk, past whatever the editor is holding for it.
    pub(crate) fn read_from_disk(&self, path: &SystemPath) -> Option<String> {
        self.native_system.read_to_string(path).ok()
    }

    pub(crate) fn global_settings(&self) -> &GlobalSettings {
        &self.global_settings
    }

    pub(crate) fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    pub(crate) fn resolved_client_capabilities(&self) -> ResolvedClientCapabilities {
        self.resolved_client_capabilities
    }

    pub(crate) const fn in_test(&self) -> bool {
        self.in_test
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn client_name(&self) -> ClientName {
        self.client_name
    }
}

/// What the editor is holding for one file.
///
/// A notebook carries no text to compare: the editor holds it as cells, and what is on disk
/// is a serialization of them whose formatting is the writer's choice. So the two are only
/// ever known to be the same when neither has been touched, and this says which case it is
/// rather than pretending to answer.
#[derive(Debug, Clone, Copy)]
pub(crate) enum OpenDocument<'a> {
    Text(&'a str),
    Notebook,
}

/// Represents the client (editor) that's connected to the language server.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ClientName {
    Zed,
    Other,
}

impl From<Option<ClientInfo>> for ClientName {
    fn from(info: Option<ClientInfo>) -> Self {
        match info {
            Some(info) if matches!(info.name.as_str(), "Zed") => ClientName::Zed,
            _ => ClientName::Other,
        }
    }
}

impl ClientName {
    /// Returns editor-specific guidance for finding logs.
    ///
    /// Different editors have different ways to access language server logs, so we provide tailored
    /// instructions based on the connected client.
    pub(crate) fn log_guidance(self) -> &'static str {
        match self {
            ClientName::Zed => {
                "Please refer to the logs for more details \
                    (command palette: `dev: open language server logs`)."
            }
            ClientName::Other => "Please refer to the logs for more details.",
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Workspaces {
    workspaces: BTreeMap<SystemPathBuf, Workspace>,
}

impl Workspaces {
    /// Registers a new workspace with the given URI and default settings for the workspace.
    ///
    /// This returns `true` when this workspace is added and `false`
    /// when it has already been added.
    ///
    /// It's the caller's responsibility to later call
    /// [`Session::request_uninitialized_workspace_folder_configurations`] with
    /// the resolved settings for this workspace. Registering and initializing
    /// a workspace is a two-step process because the workspace are announced
    /// to the server during the `initialize` request, but the resolved
    /// settings are only available after the client has responded to the
    /// `workspace/configuration` request.
    fn register(&mut self, uri: Uri) -> anyhow::Result<bool> {
        let path = uri
            .to_file_path()
            .map_err(|()| anyhow!("Workspace URI is not a file or directory: {uri:?}"))?;

        // Realistically I don't think this can fail because we got the path from a Uri
        let system_path = SystemPathBuf::from_path_buf(path)
            .map_err(|_| anyhow!("Workspace URI is not valid UTF8"))?;

        if self.workspaces.contains_key(&system_path) {
            return Ok(false);
        }

        self.workspaces.insert(
            system_path,
            Workspace {
                uri,
                settings: Arc::new(WorkspaceSettings::default()),
                initialized: false,
            },
        );
        Ok(true)
    }

    /// Unregisters a workspace folder at the given path.
    ///
    /// Returns `true` if the workspace was removed, `false` if it wasn't found.
    fn unregister(&mut self, path: &SystemPath) -> bool {
        self.workspaces.remove(path).is_some()
    }

    /// Returns a reference to the workspace for the given path, [`None`] if there's no workspace
    /// registered for the path.
    fn for_path(&self, path: impl AsRef<SystemPath>) -> Option<&Workspace> {
        let path = path.as_ref();
        self.workspaces
            .range(..=path.to_path_buf())
            .rfind(|(workspace_root, _)| path.starts_with(workspace_root))
            .map(|(_, db)| db)
    }

    /// Returns the client settings for the workspace at the given path, [`None`] if there's no
    /// workspace registered for the path.
    fn settings_for_path(&self, path: impl AsRef<SystemPath>) -> Option<Arc<WorkspaceSettings>> {
        self.for_path(path).map(Workspace::settings_arc)
    }

    /// Returns `true` if all workspaces have been [initialized].
    ///
    /// [initialized]: Workspaces::initialize
    fn all_initialized(&self) -> bool {
        self.workspaces.values().all(Workspace::is_initialized)
    }
}

impl<'a> IntoIterator for &'a Workspaces {
    type Item = (&'a SystemPathBuf, &'a Workspace);
    type IntoIter = std::collections::btree_map::Iter<'a, SystemPathBuf, Workspace>;

    fn into_iter(self) -> Self::IntoIter {
        self.workspaces.iter()
    }
}

#[derive(Debug)]
pub(crate) struct Workspace {
    /// The workspace root URI as sent by the client during initialization.
    uri: Uri,
    /// The settings for this workspace.
    ///
    /// The settings here have already been "combined" with the initialization
    /// settings for the LSP.
    settings: Arc<WorkspaceSettings>,
    /// Whether this workspace has been initialized or not.
    ///
    /// If a workspace hasn't been initialized, then it is still
    /// generally usable. It just means that its settings may not
    /// be correct. That is, a workspace is considered initialized
    /// when the configuration from a `workspace/configuration`
    /// request has been received and set on this workspace.
    initialized: bool,
}

impl Workspace {
    pub(crate) fn uri(&self) -> &Uri {
        &self.uri
    }

    pub(crate) fn settings(&self) -> &WorkspaceSettings {
        &self.settings
    }

    fn settings_arc(&self) -> Arc<WorkspaceSettings> {
        self.settings.clone()
    }

    fn is_initialized(&self) -> bool {
        self.initialized
    }

    fn initialize(&mut self, settings: WorkspaceSettings) {
        self.settings = Arc::new(settings);
        self.initialized = true;
    }
}

/// a workspace diagnostics request held open until the session changes: a `workspace/diagnostic`
/// request whose last run found nothing that differs from what the client has, or either kind of
/// request when the workspace was not ready to be checked
#[derive(Debug)]
pub(crate) struct SuspendedWorkspaceDiagnosticRequest {
    /// The LSP request id
    pub(crate) id: RequestId,

    /// which request this is
    pub(crate) kind: SuspendedWorkspaceRequestKind,

    /// the params the request was sent with
    pub(crate) params: serde_json::Value,

    /// The session's revision when the request ran the last time.
    ///
    /// This is to prevent races between:
    /// * The background thread completes
    /// * A did change notification coming in
    /// * storing this struct on `Session`
    ///
    /// The revision helps us detect that a did change notification
    /// happened in the meantime, so that we can reschedule the
    /// workspace diagnostic request immediately.
    pub(crate) revision: u64,
}

impl SuspendedWorkspaceDiagnosticRequest {
    fn resume_if_revision_changed(self, current_revision: u64, client: &Client) -> Option<Self> {
        if self.revision == current_revision {
            return Some(self);
        }

        tracing::debug!("Resuming workspace diagnostics request after revision bump");
        client.queue_action(Action::RetryRequest(lsp_server::Request {
            id: self.id,
            method: self.kind.method(),
            params: self.params,
        }));

        None
    }

    /// answers the request when the server shuts down before it could be run again
    pub(crate) fn answer_at_shutdown(self, client: &Client) {
        self.kind.answer_at_shutdown(&self.id, client);
    }
}

/// How long a request may wait for its text before it is answered with `ServerCancelled`.
///
/// A backstop, not a timing anyone should meet. What a request waits for is a notification the
/// client has already decided to send — a `didOpen` behind the edit that made the request, or the
/// file system reporting a write — and it arrives in milliseconds. A client that gives up sooner
/// cancels the request, as it cancels any other.
const HOLD_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// what happened to a python document that can change what other documents say
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Changed {
    /// the client changed its text, and still has it open
    Edited,
    /// the client closed it, and the server reads it from disk again
    Closed,
}

/// When to ask the client to refresh its diagnostics: once for a run of changes, [`Self::DELAY`]
/// after the last of them.
#[derive(Debug, Default)]
struct DiagnosticRefresh {
    due: Option<std::time::Instant>,
}

impl DiagnosticRefresh {
    /// How long after the last of a run of changes the client is asked to refresh its
    /// diagnostics.
    const DELAY: std::time::Duration = std::time::Duration::from_millis(200);

    /// A change at `now`, which puts the refresh off until [`Self::DELAY`] after it.
    fn schedule(&mut self, now: std::time::Instant) {
        self.due = Some(now + Self::DELAY);
    }

    /// Whether the refresh is due at `now`. Once it is, it is not due again until another change
    /// schedules it.
    fn take_due(&mut self, now: std::time::Instant) -> bool {
        let due = self.due.is_some_and(|due| due <= now);
        if due {
            self.due = None;
        }
        due
    }

    /// When the refresh is due, if a change scheduled it.
    fn deadline(&self) -> Option<std::time::Instant> {
        self.due
    }
}

/// A document request that named a text the session did not hold, waiting for it.
///
/// Asked again, whole, each time the session changes, rather than told what changed: the check
/// that holds it is the same one that lets it through, and it runs against a fresh snapshot.
#[derive(Debug)]
pub(crate) struct HeldRequest {
    /// The request as the client sent it, the named text included.
    request: lsp_server::Request,

    /// The session's revision when the request found its text missing.
    revision: u64,
}

/// A handle to a document opened by the client and stored in [`Index`].
///
/// Carries the client's URI and document version and supports updating or closing the document.
///
/// Requests that also accept closed files use [`DocumentRequestTarget`].
#[derive(Clone, Debug)]
pub(crate) enum OpenDocumentHandle {
    Text {
        uri: lsp_types::Uri,
        path: AnySystemPath,
        version: DocumentVersion,
        /// What the document is written in, which decides whether a request for
        /// it is answered by the python services or the django template ones.
        language_id: LanguageId,
    },
    Notebook {
        uri: lsp_types::Uri,
        path: AnySystemPath,
        version: DocumentVersion,
    },
    Cell {
        uri: lsp_types::Uri,
        version: DocumentVersion,
        notebook_path: AnySystemPath,
    },
}

impl OpenDocumentHandle {
    fn from_text_document(document: &TextDocument) -> Self {
        match document.notebook() {
            None => Self::Text {
                version: document.version(),
                uri: document.uri().clone(),
                path: DocumentKey::from_uri(document.uri()).into_file_path(),
                language_id: document.language_id(),
            },
            Some(notebook) => Self::Cell {
                notebook_path: notebook.clone(),
                version: document.version(),
                uri: document.uri().clone(),
            },
        }
    }

    fn from_notebook_document(document: &NotebookDocument) -> Self {
        Self::Notebook {
            path: DocumentKey::from_uri(document.uri()).into_file_path(),
            uri: document.uri().clone(),
            version: document.version(),
        }
    }

    fn from_document(document: &Document) -> Self {
        match document {
            Document::Text(text) => Self::from_text_document(text),
            Document::Notebook(notebook) => Self::from_notebook_document(notebook),
        }
    }

    fn key(&self) -> DocumentKey {
        DocumentKey::from_uri(self.uri())
    }

    pub(crate) const fn version(&self) -> DocumentVersion {
        match self {
            Self::Text { version, .. }
            | Self::Notebook { version, .. }
            | Self::Cell { version, .. } => *version,
        }
    }

    /// The URI as used by the client to reference this document.
    pub(crate) fn uri(&self) -> &lsp_types::Uri {
        match self {
            Self::Text { uri, .. } | Self::Notebook { uri, .. } | Self::Cell { uri, .. } => uri,
        }
    }

    /// The path to the enclosing file for this document.
    ///
    /// This is the path corresponding to the URI, except for notebook cells where the
    /// path corresponds to the notebook file.
    pub(crate) fn notebook_or_file_path(&self) -> &AnySystemPath {
        match self {
            Self::Text { path, .. } | Self::Notebook { path, .. } => path,
            Self::Cell { notebook_path, .. } => notebook_path,
        }
    }

    fn file_path(&self) -> Option<&AnySystemPath> {
        match self {
            Self::Text { path, .. } | Self::Notebook { path, .. } => Some(path),
            Self::Cell { .. } => None,
        }
    }

    #[expect(unused)]
    fn notebook_path(&self) -> Option<&AnySystemPath> {
        match self {
            OpenDocumentHandle::Notebook { path, .. } => Some(path),
            OpenDocumentHandle::Cell { notebook_path, .. } => Some(notebook_path),
            OpenDocumentHandle::Text { .. } => None,
        }
    }

    /// Returns the salsa interned [`File`] for the document selected by this query.
    ///
    /// It returns [`None`] for the following cases:
    /// - For virtual file, if it's not yet opened
    /// - For regular file, if it does not exists or is a directory
    pub(crate) fn notebook_or_file(&self, db: &dyn Db) -> Option<File> {
        match &self.notebook_or_file_path() {
            AnySystemPath::System(path) => system_path_to_file(db, path).ok(),
            AnySystemPath::SystemVirtual(virtual_path) => db
                .files()
                .try_virtual_file(virtual_path)
                .map(|virtual_file| virtual_file.file()),
        }
    }

    /// What this document is written in.
    ///
    /// A notebook and its cells are always python; only a plain text document
    /// can be anything else.
    fn language_id(&self) -> LanguageId {
        match self {
            Self::Text { language_id, .. } => *language_id,
            Self::Notebook { .. } | Self::Cell { .. } => LanguageId::Python,
        }
    }

    pub(crate) fn is_django_template(&self) -> bool {
        self.language_id().is_django_template()
    }

    /// whether the document is python of either dialect, a change to which can change what
    /// another document says
    pub(crate) fn is_python(&self) -> bool {
        self.language_id().is_python()
    }

    fn is_cell(&self) -> bool {
        matches!(self, Self::Cell { .. })
    }

    pub(crate) fn is_cell_or_notebook(&self) -> bool {
        matches!(self, Self::Cell { .. } | Self::Notebook { .. })
    }

    /// Synchronizes an open script, using `availability` until its first synchronization completes.
    pub(crate) fn synchronize_script(
        &self,
        session: &mut Session,
        client: &Client,
        availability: ScriptEnvironmentAvailability,
    ) {
        let path = self.notebook_or_file_path();
        let Some(system_path) = path.as_system() else {
            return;
        };
        let capabilities = session.resolved_client_capabilities;
        let script_progress = session.script_progress.clone();
        let db = session.project_db_mut(path);
        let Some(file) = db.files().try_system(db, system_path) else {
            return;
        };
        Session::request_script_sync(
            db,
            file,
            client,
            capabilities,
            availability,
            &script_progress,
        );
    }

    pub(crate) fn update_text_document(
        &mut self,
        session: &mut Session,
        client: &Client,
        content_changes: Vec<TextDocumentContentChangeEvent>,
        new_version: DocumentVersion,
    ) -> crate::Result<()> {
        let position_encoding = session.position_encoding();
        {
            let mut index = session.index_mut();

            let document_mut = index.document_mut(&self.key())?;

            let Some(document) = document_mut.as_text_mut() else {
                anyhow::bail!("Text document path does not point to a text document");
            };

            if content_changes.is_empty() {
                document.update_version(new_version);
            } else {
                document.apply_changes(content_changes, new_version, position_encoding);
            }

            self.set_version(document.version());
        }

        self.update_in_databases(session, client);

        Ok(())
    }

    pub(crate) fn update_notebook_document(
        &mut self,
        session: &mut Session,
        client: &Client,
        cells: Option<lsp_types::NotebookDocumentCellChanges>,
        metadata: Option<lsp_types::LspObject>,
        new_version: DocumentVersion,
    ) -> crate::Result<()> {
        let position_encoding = session.position_encoding();
        {
            let mut index = session.index_mut();

            index.update_notebook_document(
                &self.key(),
                cells,
                metadata,
                new_version,
                position_encoding,
            )?;

            self.set_version(new_version);
        }

        self.update_in_databases(session, client);
        Ok(())
    }

    fn update_in_databases(&self, session: &mut Session, client: &Client) {
        let change = match self.notebook_or_file_path() {
            AnySystemPath::System(system_path) => {
                ChangeEvent::file_content_changed(system_path.clone())
            }
            AnySystemPath::SystemVirtual(virtual_path) => {
                ChangeEvent::ChangedVirtual(virtual_path.clone())
            }
        };
        self.apply_change_in_databases(session, client, change);
        // the document's text changed, whether or not its file in the database did
        session.bump_revision();
    }

    fn apply_change_in_databases(
        &self,
        session: &mut Session,
        client: &Client,
        change: ChangeEvent,
    ) {
        let path = self.notebook_or_file_path();
        let changes = [change];
        let is_external = matches!(path, AnySystemPath::System(system_path)
            if session.workspaces().for_path(system_path).is_none());

        if !is_external {
            // A containing workspace determines the project for a system file, while virtual
            // documents select a single, arbitrary project. Neither selection depends on import
            // search paths, so update only the selected database.
            session.apply_changes(client, path, &changes);
        } else {
            // This is both a system file and an external file (it has no containing workspace
            // that defines how to select a single project for it). For external files, a change in
            // import search paths can change the selected project, so here we update all databases
            // to keep previously selected projects' cached contents fresh.
            let roots: Vec<_> = session.projects.keys().cloned().collect();
            for root in roots {
                session.apply_changes(client, &AnySystemPath::System(root), &changes);
            }
        }
    }

    fn set_version(&mut self, version: DocumentVersion) {
        let self_version = match self {
            OpenDocumentHandle::Text { version, .. }
            | OpenDocumentHandle::Notebook { version, .. }
            | OpenDocumentHandle::Cell { version, .. } => version,
        };

        *self_version = version;
    }

    /// De-registers a document, specified by its key.
    /// Calling this multiple times for the same document is a logic error.
    ///
    /// Returns `true` if the client needs to clear the diagnostics for this document.
    ///
    /// # Errors
    ///
    /// This can return an error when the document does not exist in the
    /// session index.
    pub(crate) fn close(&self, session: &mut Session, client: &Client) -> crate::Result<bool> {
        let removed_document = session.index_mut().close_document(&self.key())?;

        let should_clear_diagnostics = match self.file_path() {
            // Closing a notebook cell clears its diagnostics but leaves its notebook open.
            None => true,
            Some(AnySystemPath::System(path)) => {
                let is_deleted = self.close_system_file(session, client, path, &removed_document);

                // For non-virtual files, we clear diagnostics if:
                //
                // 1. The file was deleted before it was closed
                // 2. The file does not belong to any workspace e.g., opening a random file from
                //    outside the workspace because closing it acts like the file doesn't exist
                // 3. The diagnostic mode is set to open-files only
                is_deleted
                    || session.workspaces().for_path(path).is_none()
                    || session
                        .global_settings()
                        .diagnostic_mode()
                        .is_open_files_only()
            }
            Some(AnySystemPath::SystemVirtual(path)) => {
                self.close_virtual_file(session, path);

                // Always clear diagnostics for virtual files, as they don't really exist on disk
                // which means closing them is like deleting the file.
                true
            }
        };

        session.bump_revision();

        Ok(should_clear_diagnostics)
    }

    /// Closes a system file and returns whether it was deleted from disk.
    fn close_system_file(
        &self,
        session: &mut Session,
        client: &Client,
        system_path: &SystemPath,
        removed_document: &Document,
    ) -> bool {
        let db = session.project_db_mut(self.notebook_or_file_path());

        // The editor can delete the file before closing it and before its watcher
        // reports the deletion. Check disk state after removing the editor overlay.
        let is_deleted = db
            .system()
            .path_metadata(system_path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);

        if let Some(file) = db.files().try_system(db, system_path) {
            db.project().close_file(db, file);

            // In case we preferred the language given by the Client
            // over the one detected by the file extension, remove the file
            // from the project to handle cases where a user changes the language
            // of a file (which results in a didClose and didOpen for the same path but with different languages).
            if removed_document.language_id().is_some()
                && system_path
                    .extension()
                    .and_then(PySourceType::try_from_extension)
                    .is_none()
            {
                db.project().remove_file(db, file);
            }

            if is_deleted {
                // A content change leaves missing files in the project index, allowing
                // workspace diagnostics to report an I/O error until the watcher fires.
                self.apply_change_in_databases(
                    session,
                    client,
                    ChangeEvent::Deleted {
                        path: system_path.to_path_buf(),
                        kind: DeletedKind::File,
                    },
                );
            } else {
                // Restore file and script membership from the saved contents. Discarding
                // unsaved script metadata can bring a file back into the project when
                // `exclude-scripts` is enabled. Also request synchronization for saved
                // metadata changes that were skipped while the editor overlay was present.
                self.update_in_databases(session, client);
            }
        } else {
            // This can only fail when the path is a directory or it doesn't exists but the
            // file should exists for this handler in this branch. This is because every
            // close call is preceded by an open call, which ensures that the file is
            // interned in the lookup table (`Files`).
            tracing::warn!("Salsa file does not exists for {}", system_path);
        }

        is_deleted
    }

    fn close_virtual_file(&self, session: &mut Session, virtual_path: &SystemVirtualPath) {
        let db = session.project_db_mut(self.notebook_or_file_path());

        if let Some(virtual_file) = db.files().try_virtual_file(virtual_path) {
            db.project().close_file(db, virtual_file.file());
            virtual_file.close(db);
            // Bump the file's revision back to using the file system's revision.
            virtual_file.sync(db);
        } else {
            tracing::warn!("Salsa virtual file does not exists for {}", virtual_path);
        }
    }
}

/// Warns about unknown options received by the server.
///
/// If `workspace_uri` is `Some`, it indicates that the unknown options were received during a
/// workspace initialization, otherwise they were received during the server initialization.
pub(super) fn warn_about_unknown_options(
    client: &Client,
    workspace_uri: Option<&Uri>,
    unknown_options: &HashMap<String, serde_json::Value>,
) {
    let message = if let Some(workspace_uri) = workspace_uri {
        format!(
            "Received unknown options for workspace `{workspace_uri}`: {}",
            serde_json::to_string_pretty(unknown_options)
                .unwrap_or_else(|_| format!("{unknown_options:?}"))
        )
    } else {
        format!(
            "Received unknown options during initialization: {}",
            serde_json::to_string_pretty(unknown_options)
                .unwrap_or_else(|_| format!("{unknown_options:?}"))
        )
    };
    tracing::warn!("{message}");
    client.show_warning_message(message);
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::sync::Arc;

    use anyhow::Context;
    use lsp_types::Uri;
    use ruff_db::files::vendored_path_to_file;
    use ruff_db::system::{Command, CommandExecutor, OsSystem, System as _, SystemPath};
    use ruff_db::vendored::VendoredPath;
    use ty_project::metadata::Options;
    use ty_project::{ProjectDatabase, ProjectMetadata};
    use ty_python_core::program::UseDefaultStrategy;

    use super::{DiagnosticRefresh, DocumentRequestTarget, Index};
    use crate::system::{AnySystemPath, LSPSystem, WorkspaceTrust};

    /// a run of changes, each sooner after the last than the delay, asks once, after the last
    #[test]
    fn a_run_of_changes_asks_for_one_diagnostic_refresh_after_the_last() {
        let start = std::time::Instant::now();
        let at = |millis| start + std::time::Duration::from_millis(millis);
        let mut refresh = DiagnosticRefresh::default();
        assert!(!refresh.take_due(at(0)));

        refresh.schedule(at(0));
        refresh.schedule(at(150));
        refresh.schedule(at(300));
        assert!(!refresh.take_due(at(450)), "a change puts the refresh off");
        assert_eq!(refresh.deadline(), Some(at(500)));
        assert!(refresh.take_due(at(500)));
        assert!(!refresh.take_due(at(1000)), "the run asks once");
        assert_eq!(refresh.deadline(), None);

        refresh.schedule(at(1000));
        assert!(refresh.take_due(at(1200)), "the next run asks again");
    }

    #[test]
    fn closed_cached_stub_resolves_to_vendored_file() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = SystemPath::from_std_path(directory.path()).context("UTF-8 project path")?;
        let Ok(metadata) = ProjectMetadata::from_options(
            Options::default(),
            root.to_path_buf(),
            None,
            &UseDefaultStrategy,
        );
        let db = ProjectDatabase::use_defaults(metadata, OsSystem::new(root));
        let vendored_path = VendoredPath::new("stdlib/builtins.byi");
        let cached_path = ty_ide::cached_vendored_root(&db)
            .context("typeshed cache root")?
            .join(vendored_path.as_str());
        let target = DocumentRequestTarget::Closed {
            uri: Uri::from_file_path(cached_path.as_std_path())
                .ok()
                .context("cached stub URI")?,
            path: AnySystemPath::System(cached_path),
        };

        assert_eq!(
            target.to_notebook_or_file(&db),
            Some(vendored_path_to_file(&db, vendored_path)?),
        );
        Ok(())
    }

    /// Mutating the document index requires exclusive ownership after Salsa cancels the current
    /// database snapshots. A background command executor must not retain an `LSPSystem`, because
    /// that would keep the index alive and prevent the server from applying document changes.
    #[test]
    fn detached_command_executor_does_not_retain_document_index() {
        let index = Arc::new(Index::new());
        let system = LSPSystem::new(
            index.clone(),
            Arc::new(OsSystem::default()),
            WorkspaceTrust::default(),
        );
        let executor = system.command_executor().map(CommandExecutor::dyn_clone);
        assert!(executor.is_some());
        drop(system);

        assert_eq!(Arc::strong_count(&index), 1);

        drop(executor);
    }

    #[test]
    fn detached_untrusted_executor_rejects_commands() -> anyhow::Result<()> {
        let system = LSPSystem::new(
            Arc::new(Index::new()),
            Arc::new(OsSystem::default()),
            WorkspaceTrust::Untrusted,
        )
        .dyn_clone();
        let executor = system
            .command_executor()
            .context("Expected an executor for the untrusted workspace")?
            .dyn_clone();
        drop(system);

        let error = executor.execute(Command::new("must-not-run")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert_eq!(
            error.to_string(),
            "external commands are disabled in an untrusted workspace",
        );
        Ok(())
    }
}

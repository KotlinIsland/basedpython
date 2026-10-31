use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::panic::AssertUnwindSafe;
use std::sync::{Mutex, MutexGuard, PoisonError};

use by_stage::emit::{project_config, transpile_failure_diagnostic};
use lsp_types::{Code, PublishDiagnosticsNotification};
use lsp_types::{
    CodeDescription, Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticTag,
    Message, PublishDiagnosticsParams, Uri,
};
use ruff_diagnostics::Applicability;
use ruff_text_size::Ranged;
use rustc_hash::{FxHashMap, FxHashSet};
use ty_ide::{Hint, django_template_diagnostics, fix_edits, hints};

use ruff_db::diagnostic::{
    Annotation, DisplayDiagnosticConfig, HyperlinkMode, Severity, SubDiagnostic,
};
use ruff_db::files::{File, FileRange};
use ruff_db::panic::PanicActivity;
use ruff_db::source::source_text;
use ruff_db::system::SystemPathBuf;
use serde::{Deserialize, Serialize};
use ty_project::{Db as _, ProjectDatabase};

use crate::capabilities::ResolvedClientCapabilities;
use crate::document::{DocumentVersion, FileRangeExt, ToRangeExt};
use crate::server::api::requests::api_lock::api_lock_document_diagnostics;
use crate::server::api::requests::detached_transpile::transpile_detached;
use crate::server::main_loop::Action;
use crate::server::schedule::{BackgroundSchedule, Task};
use crate::session::client::Client;
use crate::session::{DocumentRequestTarget, GlobalSettings, OpenDocumentHandle};
use crate::system::{AnySystemPath, file_to_uri};
use crate::{DIAGNOSTIC_NAME, Db, DiagnosticMode};
use crate::{PositionEncoding, Session};

#[derive(Debug)]
pub(super) struct Diagnostics {
    items: Vec<ruff_db::diagnostic::Diagnostic>,
    unnecessary_hints: Vec<Hint>,
    encoding: PositionEncoding,
    file_or_notebook: File,
}

impl Diagnostics {
    /// Computes the result ID for the filtered `diagnostics`.
    ///
    /// The result ID is `None` if there are no diagnostics or hints.
    pub(super) fn result_id_from_hash<'a>(
        db: &dyn Db,
        diagnostics: impl Iterator<Item = &'a ruff_db::diagnostic::Diagnostic> + Clone,
        unnecessary_hints: &[Hint],
        client_capabilities: ResolvedClientCapabilities,
    ) -> Option<String> {
        let diagnostic_count = diagnostics.clone().count();
        if diagnostic_count == 0 && unnecessary_hints.is_empty() {
            return None;
        }

        // Generate the base result ID from filtered diagnostic content.
        let mut hasher = DefaultHasher::new();

        diagnostic_count.hash(&mut hasher);
        for diagnostic in diagnostics.clone() {
            diagnostic.hash(&mut hasher);
        }
        unnecessary_hints.hash(&mut hasher);

        if client_capabilities.supports_full_diagnostic_output() {
            // The rendered output includes source snippets that aren't part of the raw diagnostic.
            // Hash each referenced file's source once so that source-only changes invalidate the
            // result without eagerly rendering every diagnostic.
            // TODO: Hash only the source snippets used by the rendered output. Hashing the entire
            // file is deliberately conservative: an edit outside the rendered context can cause a
            // full report, but an edit inside it can never leave stale rendered output on the client.
            let mut hashed_files = FxHashSet::default();

            for diagnostic in diagnostics {
                let annotations = diagnostic
                    .sub_diagnostics()
                    .iter()
                    .flat_map(SubDiagnostic::annotations)
                    .chain(diagnostic.annotations());

                for annotation in annotations {
                    let file = annotation.get_span().expect_ty_file();
                    if hashed_files.insert(file) {
                        source_text(db, file).as_str().hash(&mut hasher);
                    }
                }
            }
        }

        Some(format!("{:016x}", hasher.finish()))
    }

    /// Computes the result ID for the diagnostics.
    ///
    /// The result ID is `None` if there are no diagnostics or hints.
    pub(super) fn result_id(
        &self,
        db: &dyn Db,
        client_capabilities: ResolvedClientCapabilities,
    ) -> Option<String> {
        Self::result_id_from_hash(
            db,
            self.items.iter(),
            &self.unnecessary_hints,
            client_capabilities,
        )
    }

    pub(super) fn to_lsp_diagnostics(
        &self,
        db: &ProjectDatabase,
        client_capabilities: ResolvedClientCapabilities,
    ) -> LspDiagnostics {
        if let Some(notebook_document) = db.notebook_document(self.file_or_notebook) {
            let mut cell_diagnostics: FxHashMap<Uri, Vec<Diagnostic>> = FxHashMap::default();

            // Populates all relevant URIs with an empty diagnostic list. This ensures that documents
            // without diagnostics still get updated.
            for cell_uri in notebook_document.cell_uris() {
                cell_diagnostics.entry(cell_uri.clone()).or_default();
            }

            for diagnostic in &self.items {
                let (uri, lsp_diagnostic) =
                    to_lsp_diagnostic(db, diagnostic, self.encoding, client_capabilities);

                let Some(uri) = uri else {
                    tracing::warn!("Unable to find notebook cell");
                    continue;
                };

                cell_diagnostics
                    .entry(uri)
                    .or_default()
                    .push(lsp_diagnostic);
            }

            for hint in &self.unnecessary_hints {
                let Some((uri, lsp_diagnostic)) = unnecessary_hint_to_lsp_diagnostic(
                    db,
                    self.file_or_notebook,
                    self.encoding,
                    hint,
                ) else {
                    continue;
                };

                let Some(uri) = uri else {
                    tracing::warn!("Unable to find notebook cell");
                    continue;
                };

                cell_diagnostics
                    .entry(uri)
                    .or_default()
                    .push(lsp_diagnostic);
            }

            LspDiagnostics::NotebookDocument(cell_diagnostics)
        } else {
            let mut diagnostics = self
                .items
                .iter()
                .map(|diagnostic| {
                    to_lsp_diagnostic(db, diagnostic, self.encoding, client_capabilities).1
                })
                .collect::<Vec<_>>();
            diagnostics.extend(unnecessary_hints_to_lsp_diagnostics(
                db,
                self.file_or_notebook,
                self.encoding,
                &self.unnecessary_hints,
            ));
            LspDiagnostics::TextDocument(diagnostics)
        }
    }
}

/// Represents the diagnostics for a text document or a notebook document.
pub(super) enum LspDiagnostics {
    TextDocument(Vec<Diagnostic>),

    /// A map of cell URIs to the diagnostics for that cell.
    NotebookDocument(FxHashMap<Uri, Vec<Diagnostic>>),
}

impl LspDiagnostics {
    /// Returns the diagnostics for the text document or notebook cell at `uri`.
    pub(super) fn into_document_diagnostics(self, uri: &Uri) -> Vec<Diagnostic> {
        match self {
            LspDiagnostics::TextDocument(diagnostics) => diagnostics,
            LspDiagnostics::NotebookDocument(mut diagnostics) => {
                diagnostics.remove(uri).unwrap_or_default()
            }
        }
    }
}

/// Publishes diagnostics for all open files that need push diagnostics.
pub(crate) fn publish_all_document_diagnostics(session: &Session, client: &Client) {
    for document in session.file_document_handles() {
        publish_diagnostics_if_needed(&document, session, client);
    }
}

/// Publishes the diagnostics for the given document snapshot using the [publish diagnostics
/// notification] .
///
/// Unlike [`publish_diagnostics`], this function only publishes diagnostics if a client doesn't support
/// pull diagnostics and `document` is not a notebook or cell (VS Code
/// does not support pull diagnostics for notebooks or cells (as of 2025-11-12).
///
/// [publish diagnostics notification]: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_publishDiagnostics
pub(crate) fn publish_diagnostics_if_needed(
    document: &OpenDocumentHandle,
    session: &Session,
    client: &Client,
) {
    if !document.is_cell_or_notebook() && session.client_capabilities().supports_pull_diagnostics()
    {
        return;
    }

    publish_diagnostics(document, session, client);
}

/// Publishes the diagnostics for the given document snapshot using the [publish diagnostics
/// notification].
pub(super) fn publish_diagnostics(
    document: &OpenDocumentHandle,
    session: &Session,
    client: &Client,
) {
    if session.global_settings().diagnostic_mode().is_off() {
        return;
    }

    let db = session.project_db(document.notebook_or_file_path());

    if let Some(diagnostics) = api_lock_document_diagnostics(
        db,
        document.notebook_or_file_path(),
        session.position_encoding(),
    ) {
        client.send_notification::<PublishDiagnosticsNotification>(PublishDiagnosticsParams {
            uri: document.uri().clone(),
            diagnostics,
            version: Some(document.version()),
        });
        return;
    }

    let Some(file) = document.notebook_or_file(db) else {
        tracing::info!(
            "No file found for snapshot for `{}`",
            document.notebook_or_file_path()
        );
        return;
    };
    let transpile_verdicts = session.transpile_verdicts(document.notebook_or_file_path());
    // a transpile takes about as long as the check before it, and this is the thread the next
    // edit is waiting on, so what needs one is published from a worker
    if !document.is_django_template() && transpile_verdicts.awaits_transpile(db, file) {
        client.queue_action(Action::PublishAwaitingTranspile {
            uri: document.uri().clone(),
            version: document.version(),
        });
        return;
    }
    let Some(diagnostics) = compute_diagnostics(
        db,
        file,
        session.position_encoding(),
        session.global_settings(),
        document.is_django_template(),
        transpile_verdicts,
    ) else {
        return;
    };

    // Sends a notification to the client with the diagnostics for the document.
    let publish_diagnostics_notification =
        |uri: Uri, version: Option<i32>, diagnostics: Vec<Diagnostic>| {
            client.send_notification::<PublishDiagnosticsNotification>(PublishDiagnosticsParams {
                uri,
                diagnostics,
                version,
            });
        };

    match diagnostics.to_lsp_diagnostics(db, session.client_capabilities()) {
        LspDiagnostics::TextDocument(diagnostics) => {
            publish_diagnostics_notification(
                document.uri().clone(),
                Some(document.version()),
                diagnostics,
            );
        }
        LspDiagnostics::NotebookDocument(cell_diagnostics) => {
            #[expect(
                clippy::iter_over_hash_type,
                reason = "diagnostic notifications for distinct cell URIs are independent"
            )]
            for (cell_uri, diagnostics) in cell_diagnostics {
                let version = session
                    .open_document_handle(&cell_uri)
                    .map(|document| document.version())
                    .ok();
                publish_diagnostics_notification(cell_uri, version, diagnostics);
            }
        }
    }
}

/// publishes what [`publish_diagnostics`] handed over for the document `uri` at `version`
///
/// nothing is published for a document that has moved on, since the edit that moved it asked for
/// its own, nor once a change to the database cancels this, since each change to the database is
/// followed by a publish. the database is held until the notification is sent, so that one for an
/// older version can't overtake the next edit's
pub(in crate::server) fn publish_awaiting_transpile(uri: Uri, version: DocumentVersion) -> Task {
    Task::background(BackgroundSchedule::Worker, move |session: &Session| {
        let snapshot = match session.snapshot_document(&uri) {
            Ok(snapshot)
                if matches!(
                    snapshot.document(),
                    DocumentRequestTarget::Open(document) if document.version() == version
                ) =>
            {
                snapshot
            }
            _ => return Box::new(|_| {}),
        };
        let db = session
            .project_db(snapshot.document().notebook_or_file_path())
            .clone();

        Box::new(move |client| {
            let published = ruff_db::panic::catch_unwind(|| {
                salsa::attach(&db, || {
                    let diagnostics =
                        snapshot
                            .document()
                            .to_notebook_or_file(&db)
                            .and_then(|file| {
                                compute_diagnostics(
                                    &db,
                                    file,
                                    snapshot.encoding(),
                                    snapshot.global_settings(),
                                    snapshot.is_django_template(),
                                    snapshot.transpile_verdicts(),
                                )
                            });
                    let Some(diagnostics) = diagnostics else {
                        return;
                    };
                    let diagnostics = diagnostics
                        .to_lsp_diagnostics(&db, snapshot.resolved_client_capabilities())
                        .into_document_diagnostics(&uri);
                    client.send_notification::<PublishDiagnosticsNotification>(
                        PublishDiagnosticsParams {
                            uri: uri.clone(),
                            diagnostics,
                            version: Some(version),
                        },
                    );
                });
            });
            if let Err(error) = published
                && error.payload.downcast_ref::<salsa::Cancelled>().is_none()
            {
                tracing::error!("Failed to publish the diagnostics of {uri}: {error}");
            }
        })
    })
}

/// Publishes settings diagnostics for all the project at the given path
/// using the [publish diagnostics notification].
///
/// [publish diagnostics notification]: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_publishDiagnostics
pub(crate) fn publish_settings_diagnostics(
    session: &mut Session,
    client: &Client,
    path: SystemPathBuf,
) {
    // Don't publish settings diagnostics for workspace that are already doing full diagnostics.
    //
    // Note we DO NOT respect the fact that clients support pulls because these are
    // files they *specifically* won't pull diagnostics from us for, because we don't
    // claim to be an LSP for them.
    match session.global_settings().diagnostic_mode() {
        DiagnosticMode::Workspace | DiagnosticMode::Off => {
            return;
        }
        DiagnosticMode::OpenFilesOnly => {}
    }

    let session_encoding = session.position_encoding();
    let client_capabilities = session.client_capabilities();

    let project_path = AnySystemPath::System(path);

    let (mut diagnostics_by_uri, old_untracked) = {
        let state = session.project_state_mut(&project_path);
        let db = &state.db;
        let project = db.project();
        let settings_diagnostics = project.check_settings(db);

        // We need to send diagnostics if we have non-empty ones, or we have ones to clear.
        // These will both almost always be empty so this function will almost always be a no-op.
        if settings_diagnostics.is_empty()
            && state.untracked_files_with_pushed_diagnostics.is_empty()
        {
            return;
        }

        // Group diagnostics by URI
        let mut diagnostics_by_uri: FxHashMap<Uri, Vec<_>> = FxHashMap::default();
        for diagnostic in settings_diagnostics {
            if let Some(span) = diagnostic.primary_span() {
                let file = span.expect_ty_file();
                let Some(uri) = file_to_uri(db, file) else {
                    tracing::debug!("Failed to convert file to URI at {}", file.path(db));
                    continue;
                };
                diagnostics_by_uri.entry(uri).or_default().push(diagnostic);
            }
        }

        // Record the URIs we're sending non-empty diagnostics for, so we know to clear them
        // the next time we publish settings diagnostics!
        let old_untracked = std::mem::replace(
            &mut state.untracked_files_with_pushed_diagnostics,
            diagnostics_by_uri.keys().cloned().collect(),
        );

        (diagnostics_by_uri, old_untracked)
    };

    // Add empty diagnostics for any files that had diagnostics before but don't now.
    // This will clear them (either the file is no longer relevant to us or fixed!)
    for uri in old_untracked {
        diagnostics_by_uri.entry(uri).or_default();
    }

    let db = session.project_db(&project_path);
    let global_settings = session.global_settings();

    // Send the settings diagnostics!
    #[expect(
        clippy::iter_over_hash_type,
        reason = "diagnostic notifications for distinct document URIs are independent"
    )]
    for (uri, file_diagnostics) in diagnostics_by_uri {
        // Convert diagnostics to LSP format
        let lsp_diagnostics = file_diagnostics
            .into_iter()
            .filter(|diagnostic| global_settings.should_show_diagnostic(diagnostic))
            .map(|diagnostic| {
                to_lsp_diagnostic(db, &diagnostic, session_encoding, client_capabilities).1
            })
            .collect::<Vec<_>>();

        client.send_notification::<PublishDiagnosticsNotification>(PublishDiagnosticsParams {
            uri,
            diagnostics: lsp_diagnostics,
            version: None,
        });
    }
}

/// The diagnostics of `file`.
///
/// `template` says the file is a django template rather than python, which only
/// the document the client opened knows: the language a client gives a document
/// is not something the file's path settles.
pub(super) fn compute_diagnostics(
    db: &ProjectDatabase,
    file: File,
    encoding: PositionEncoding,
    global_settings: &GlobalSettings,
    template: bool,
    transpile_verdicts: &TranspileVerdicts,
) -> Option<Diagnostics> {
    // The first uv result supplies the module paths needed for correct diagnostics. Do not analyze
    // the script until that result is available. Waiting would not help: publishing the environment
    // advances the database revision and cancels this snapshot, so the request must retry anyway.
    if db.uv_environments().is_initialization_pending(db, file) {
        return None;
    }

    // A django template is not python. It is never handed to the type checker or
    // to the hints — either would parse an html file as python — and what it does
    // report is the template language's own.
    if template {
        let mut diagnostics = django_template_diagnostics(db, file);
        diagnostics.retain(|diagnostic| global_settings.should_show_diagnostic(diagnostic));
        return Some(Diagnostics {
            items: diagnostics,
            unnecessary_hints: Vec::new(),
            encoding,
            file_or_notebook: file,
        });
    }

    // what django's routes and views get wrong is read from the project's url tree
    // rather than from this file alone, so it is not part of the type checker's own
    // pass. it arrives here through the checker the session registers, which is the
    // one `by check` registers too
    let mut diagnostics = db.check_file(file);
    if let Some(failure) = transpile_verdicts.failure(db, file, &diagnostics) {
        diagnostics.push(failure);
    }
    diagnostics.retain(|diagnostic| global_settings.should_show_diagnostic(diagnostic));

    let unnecessary_hints = hints(db, file);

    Some(Diagnostics {
        items: diagnostics,
        unnecessary_hints,
        encoding,
        file_or_notebook: file,
    })
}

/// basedpython: what stops a `.by` module that checks clean from being transpiled — the
/// transpiler refusing it, the python it wrote not being python the target version runs, or the
/// transpiler breaking — as the diagnostic `by build` and `by run` stop on, one answer per file
/// and revision of the project's database
///
/// without it the first anyone hears of such a module is from a run or a build, since a refusal
/// is about how the module lowers rather than what it means, and the check has nothing to say
/// about it: `from typing import Optional` beside `x = Some(3)` checks clean, and takes over the
/// name the runtime's `Some` helper is provided under
///
/// a transpile is not a query of the database — it builds databases of its own — so salsa does
/// not remember its answer the way it remembers the check's. this does, for as long as nothing in
/// the database changes: a client asks for a document's diagnostics again whenever the server
/// says they may have changed, which is far more often than the document does
#[derive(Debug, Default)]
pub(crate) struct TranspileVerdicts {
    verdicts: Mutex<FxHashMap<File, (salsa::Revision, Option<ruff_db::diagnostic::Diagnostic>)>>,
}

impl TranspileVerdicts {
    /// why `file` does not transpile, when it is a `.by` module whose check reported no error,
    /// `checked`
    ///
    /// a module the check found an error in is not asked. its transpile fails as often as not on
    /// the very thing the check reported — a capture pattern that makes the rest unreachable is
    /// `invalid-match-pattern` to the checker and `invalid-syntax` in the python it lowers to —
    /// and `by run` refuses such a module before it transpiles anything. a module that does not
    /// parse has nothing to transpile at all
    fn failure(
        &self,
        db: &ProjectDatabase,
        file: File,
        checked: &[ruff_db::diagnostic::Diagnostic],
    ) -> Option<ruff_db::diagnostic::Diagnostic> {
        if !file.source_type(db).is_basedpython() || checked_has_error(checked) {
            return None;
        }

        let revision = salsa::plumbing::current_revision(db);
        if let Some((known_at, failure)) = self.lock().get(&file)
            && *known_at == revision
        {
            return failure.clone();
        }

        let failure = transpile_failure(db, file).ok()?;
        self.lock().insert(file, (revision, failure.clone()));
        failure
    }

    /// whether [`Self::failure`] would have to transpile `file` to answer now
    fn awaits_transpile(&self, db: &ProjectDatabase, file: File) -> bool {
        file.source_type(db).is_basedpython()
            && !db.uv_environments().is_initialization_pending(db, file)
            && !checked_has_error(&db.check_file(file))
            && self
                .lock()
                .get(&file)
                .is_none_or(|(known_at, _)| *known_at != salsa::plumbing::current_revision(db))
    }

    fn lock(
        &self,
    ) -> MutexGuard<'_, FxHashMap<File, (salsa::Revision, Option<ruff_db::diagnostic::Diagnostic>)>>
    {
        // a panic while the lock was held left nothing half-written: each entry is replaced whole
        self.verdicts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn checked_has_error(checked: &[ruff_db::diagnostic::Diagnostic]) -> bool {
    checked
        .iter()
        .any(|diagnostic| diagnostic.severity() >= Severity::Error)
}

/// transpile `file` as `by build` does, and say what stopped it
///
/// `Ok(None)` when it transpiles, and `Err` when there is no verdict to give, because the thread
/// the transpile ran on failed outright, which `transpile_detached` has logged
fn transpile_failure(
    db: &ProjectDatabase,
    file: File,
) -> Result<Option<ruff_db::diagnostic::Diagnostic>, ()> {
    // a build settles the stamps, and a stamp nobody has settled yet isn't what stops the module
    let config = by_transforms::Config {
        stamps_undetermined: true,
        ..project_config(db)
    };
    let owned = db.dyn_clone();
    // the transpiler builds databases of its own, which a thread with the session's attached
    // cannot; see `transpile_detached`. a panic in it is reported the way a build reports one
    let transpiled = transpile_detached(move || {
        ty_project::catch_panic(
            &*owned,
            file,
            PanicActivity::Transpiling,
            AssertUnwindSafe(|| by_transforms::transpile_typed(&*owned, file, &config, None)),
        )
    })?;
    Ok(match transpiled {
        Ok(Ok(_)) => None,
        Ok(Err(error)) => Some(transpile_failure_diagnostic(file, &error)),
        Err(panic) => Some(panic),
    })
}

pub(super) fn unnecessary_hints_to_lsp_diagnostics(
    db: &ProjectDatabase,
    file: File,
    encoding: PositionEncoding,
    hints: &[Hint],
) -> Vec<Diagnostic> {
    hints
        .iter()
        .filter_map(|hint| unnecessary_hint_to_lsp_diagnostic(db, file, encoding, hint))
        .map(|(_, diagnostic)| diagnostic)
        .collect()
}

fn unnecessary_hint_to_lsp_diagnostic(
    db: &ProjectDatabase,
    file: File,
    encoding: PositionEncoding,
    hint: &Hint,
) -> Option<(Option<Uri>, Diagnostic)> {
    let range = hint.range.to_lsp_range(db, file, encoding)?;
    let uri = range.to_location().map(|location| location.uri);

    Some((
        uri,
        Diagnostic {
            range: range.local_range(),
            severity: Some(DiagnosticSeverity::Hint),
            code: hint
                .kind
                .code()
                .map(|code| lsp_types::Code::String(code.to_owned())),
            code_description: None,
            source: Some(DIAGNOSTIC_NAME.into()),
            message: Message::String(hint.message()),
            related_information: None,
            tags: Some(vec![DiagnosticTag::Unnecessary]),
            data: None,
        },
    ))
}

/// Converts the tool specific [`Diagnostic`][ruff_db::diagnostic::Diagnostic] to an LSP
/// [`Diagnostic`].
pub(super) fn to_lsp_diagnostic(
    db: &dyn Db,
    diagnostic: &ruff_db::diagnostic::Diagnostic,
    encoding: PositionEncoding,
    client_capabilities: ResolvedClientCapabilities,
) -> (Option<lsp_types::Uri>, Diagnostic) {
    let supports_related_information =
        client_capabilities.supports_diagnostic_related_information();

    let location = diagnostic.primary_span().and_then(|span| {
        let file = span.expect_ty_file();
        span.range()?
            .to_lsp_range(db, file, encoding)
            .unwrap_or_default()
            .to_location()
    });

    let (range, uri) = match location {
        Some(location) => (location.range, Some(location.uri)),
        None => (lsp_types::Range::default(), None),
    };

    let severity = match diagnostic.severity() {
        Severity::Info => DiagnosticSeverity::Information,
        Severity::Warning => DiagnosticSeverity::Warning,
        Severity::Error | Severity::Fatal => DiagnosticSeverity::Error,
    };

    let tags = diagnostic
        .primary_tags()
        .map(|tags| {
            tags.iter()
                .map(|tag| match tag {
                    ruff_db::diagnostic::DiagnosticTag::Unnecessary => DiagnosticTag::Unnecessary,
                    ruff_db::diagnostic::DiagnosticTag::Deprecated => DiagnosticTag::Deprecated,
                })
                .collect::<Vec<DiagnosticTag>>()
        })
        .filter(|mapped_tags| !mapped_tags.is_empty());

    let code_description = diagnostic.documentation_url().and_then(|url| {
        let href = Uri::parse(url).ok()?;

        Some(CodeDescription { href })
    });

    let related_information =
        if supports_related_information {
            let mut related_information = Vec::new();
            related_information.extend(diagnostic.secondary_annotations().filter_map(
                |annotation| annotation_to_related_information(db, annotation, encoding),
            ));

            for sub_diagnostic in diagnostic.sub_diagnostics() {
                related_information.extend(sub_diagnostic_to_related_information(
                    db,
                    sub_diagnostic,
                    encoding,
                ));

                related_information.extend(sub_diagnostic.secondary_annotations().filter_map(
                    |annotation| annotation_to_related_information(db, annotation, encoding),
                ));
            }

            Some(related_information)
        } else {
            None
        };

    let data = DiagnosticData::try_from_diagnostic(
        db,
        diagnostic,
        encoding,
        FullDiagnosticOutput::from_client_capabilities(client_capabilities),
    );

    let mut message = if supports_related_information {
        // Show both the primary and annotation messages if available,
        // because we don't create a related information for the primary message.
        if let Some(annotation_message) = diagnostic
            .primary_annotation()
            .and_then(|annotation| annotation.get_message())
        {
            format!("{}: {annotation_message}", diagnostic.headline_message())
        } else {
            diagnostic.headline_message().to_string()
        }
    } else {
        diagnostic.concise_message().to_string()
    };

    // Append info sub-diagnostics that have no location (and thus
    // can't be shown as "related information") to the message.
    let mut first = true;
    for sub_diagnostic in diagnostic.sub_diagnostics() {
        if sub_diagnostic.primary_annotation().is_none() {
            if first {
                message.push('\n');
                first = false;
            }
            write!(
                message,
                "\n{severity}: {hint}",
                hint = sub_diagnostic.concise_message(),
                severity = sub_diagnostic.severity()
            )
            .ok();
        }
    }

    (
        uri,
        Diagnostic {
            range,
            severity: Some(severity),
            tags,
            code: Some(Code::String(diagnostic.id().to_string())),
            code_description,
            source: Some(DIAGNOSTIC_NAME.into()),
            message: Message::String(message),
            related_information,
            data: serde_json::to_value(data).ok(),
        },
    )
}

/// Converts an [`Annotation`] to a [`DiagnosticRelatedInformation`].
fn annotation_to_related_information(
    db: &dyn Db,
    annotation: &Annotation,
    encoding: PositionEncoding,
) -> Option<DiagnosticRelatedInformation> {
    let span = annotation.get_span();

    let annotation_message = annotation.get_message()?;
    let range = FileRange::try_from(span).ok()?;
    let location = range.to_lsp_range(db, encoding)?.into_location()?;

    Some(DiagnosticRelatedInformation {
        location,
        message: annotation_message.to_string(),
    })
}

/// Converts a [`SubDiagnostic`] to a [`DiagnosticRelatedInformation`].
fn sub_diagnostic_to_related_information(
    db: &dyn Db,
    diagnostic: &SubDiagnostic,
    encoding: PositionEncoding,
) -> Option<DiagnosticRelatedInformation> {
    let primary_annotation = diagnostic.primary_annotation()?;

    let span = primary_annotation.get_span();
    let range = FileRange::try_from(span).ok()?;
    let location = range.to_lsp_range(db, encoding)?.into_location()?;

    Some(DiagnosticRelatedInformation {
        location,
        message: diagnostic.concise_message().to_string(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FullDiagnosticOutput {
    Enabled,
    Disabled,
}

impl FullDiagnosticOutput {
    fn from_client_capabilities(client_capabilities: ResolvedClientCapabilities) -> Self {
        if client_capabilities.supports_full_diagnostic_output() {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct FullDiagnosticData {
    rendered: String,
    pub(crate) diagnostic_id: String,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub(crate) fix: Option<DiagnosticFixData>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct DiagnosticFixData {
    pub(crate) fix_title: String,
    pub(crate) edits: HashMap<Uri, Vec<lsp_types::TextEdit>>,
    pub(crate) preferred: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum DiagnosticData {
    Full(FullDiagnosticData),
    Fix(DiagnosticFixData),
}

impl DiagnosticData {
    fn try_from_diagnostic(
        db: &dyn Db,
        diagnostic: &ruff_db::diagnostic::Diagnostic,
        encoding: PositionEncoding,
        full_diagnostic_output: FullDiagnosticOutput,
    ) -> Option<Self> {
        let fix = Self::try_fix_from_diagnostic(db, diagnostic, encoding);

        match full_diagnostic_output {
            FullDiagnosticOutput::Enabled => Some(Self::Full(FullDiagnosticData {
                rendered: diagnostic
                    .display(
                        &(db as &dyn ruff_db::Db),
                        &DisplayDiagnosticConfig::new("ty")
                            .color(true)
                            // The styled renderer can enable OSC-8 hyperlinks based on the process
                            // environment, even though this output is sent over LSP rather than to a
                            // terminal. The ANSI parser used by ty-vscode does not strip OSC-8
                            // sequences, so their escape codes would appear in the virtual diagnostic
                            // document.
                            .hyperlinks(HyperlinkMode::Never),
                    )
                    .to_string(),
                diagnostic_id: diagnostic.id().to_string(),
                fix,
            })),
            FullDiagnosticOutput::Disabled => fix.map(Self::Fix),
        }
    }

    fn try_fix_from_diagnostic(
        db: &dyn Db,
        diagnostic: &ruff_db::diagnostic::Diagnostic,
        encoding: PositionEncoding,
    ) -> Option<DiagnosticFixData> {
        let fix = diagnostic
            .fix()
            .filter(|fix| fix.applies(Applicability::Unsafe))?;

        let primary_span = diagnostic.primary_span()?;
        let file = primary_span.expect_ty_file();

        let mut lsp_edits: HashMap<Uri, Vec<lsp_types::TextEdit>> = HashMap::new();

        for edit in fix_edits(db, db.program_file(file), diagnostic)? {
            let location = edit
                .range()
                .to_lsp_range(db, file, encoding)?
                .to_location()?;

            lsp_edits
                .entry(location.uri)
                .or_default()
                .push(lsp_types::TextEdit {
                    range: location.range,
                    new_text: edit.content().unwrap_or_default().to_string(),
                });
        }

        Some(DiagnosticFixData {
            fix_title: diagnostic
                .first_help_text()
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("Fix {}", diagnostic.id())),
            edits: lsp_edits,
            preferred: fix.applies(Applicability::Safe),
        })
    }
}

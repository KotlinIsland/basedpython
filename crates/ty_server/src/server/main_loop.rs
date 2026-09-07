use crate::Session;
use crate::project_server;
use crate::server::schedule::{BackgroundSchedule, Scheduler, Task};
use crate::server::{Server, api};
use crate::session::client::{Client, ClientResponseHandler};
use crate::session::{ClientOptions, SuspendedWorkspaceDiagnosticRequest};
use anyhow::anyhow;
use lsp_server::Message;
use lsp_types::Notification;
use lsp_types::Uri;
use ruff_db::system::SystemPathBuf;
use std::panic::AssertUnwindSafe;
use ty_project::watch::ChangeEvent;

/// How many fresh snapshots a command-line request gets before it is told to check for
/// itself.
///
/// Every retry is a check that ran and was cancelled, so this is bounded by patience rather
/// than by correctness: a session being typed into cancels checks faster than they finish,
/// and the caller has a cold path that always works.
const RETRY_LIMIT: u8 = 3;

pub(crate) type ConnectionSender = crossbeam::channel::Sender<Message>;
pub(crate) type MainLoopSender = crossbeam::channel::Sender<Event>;
pub(crate) type MainLoopReceiver = crossbeam::channel::Receiver<Event>;

impl Server {
    pub(super) fn main_loop(&mut self) -> crate::Result<()> {
        self.initialize(&Client::new(
            self.main_loop_sender.clone(),
            self.connection.sender.clone(),
        ));

        let mut scheduler = Scheduler::new(self.worker_threads);

        while let Ok(next_event) = self.next_event() {
            let Some(next_event) = next_event else {
                anyhow::bail!("client exited without proper shutdown sequence");
            };

            let client = Client::new(
                self.main_loop_sender.clone(),
                self.connection.sender.clone(),
            );

            match next_event {
                Event::Message(msg) => {
                    let Some(msg) = self.session.should_defer_message(msg) else {
                        continue;
                    };

                    let task = match msg {
                        Message::Request(req) => {
                            self.session
                                .request_queue_mut()
                                .incoming_mut()
                                .register(req.id.clone(), req.method.clone());

                            if self.session.is_shutdown_requested() {
                                tracing::warn!(
                                    "Received request `{}` after server shutdown was requested, discarding",
                                    &req.method
                                );
                                client.respond_err(
                                    req.id,
                                    lsp_server::ResponseError {
                                        code: lsp_server::ErrorCode::InvalidRequest as i32,
                                        message: "Shutdown already requested".to_owned(),
                                        data: None,
                                    },
                                );
                                continue;
                            }

                            api::request(req)
                        }
                        Message::Notification(notification) => {
                            if notification.method == lsp_types::ExitNotification::METHOD.as_str() {
                                if !self.session.is_shutdown_requested() {
                                    return Err(anyhow!(
                                        "Received exit notification before a shutdown request"
                                    ));
                                }

                                tracing::debug!("Received exit notification, exiting");
                                return Ok(());
                            }

                            api::notification(notification)
                        }

                        // Handle the response from the client to a server request
                        Message::Response(response) => {
                            if let Some(handler) = self
                                .session
                                .request_queue_mut()
                                .outgoing_mut()
                                .complete(&response.id)
                            {
                                handler.handle_response(&client, response);
                            } else {
                                tracing::error!(
                                    "Received a response with ID {}, which was not expected",
                                    response.id
                                );
                            }

                            continue;
                        }
                    };

                    scheduler.dispatch(task, &mut self.session, client);
                }
                Event::Action(action) => match action {
                    Action::SendResponse(response) => {
                        // Filter out responses for already canceled requests.
                        if let Some((start_time, method)) = self
                            .session
                            .request_queue_mut()
                            .incoming_mut()
                            .complete(&response.id)
                        {
                            let duration = start_time.elapsed();
                            tracing::debug!(name: "message response", method, %response.id, duration = format_args!("{:0.2?}", duration));

                            self.connection.sender.send(Message::Response(response))?;
                        } else {
                            tracing::debug!(
                                "Ignoring response for canceled request id={}",
                                response.id
                            );
                        }
                    }

                    Action::RetryRequest(request) => {
                        // Never retry canceled requests.
                        if self
                            .session
                            .request_queue()
                            .incoming()
                            .is_pending(&request.id)
                        {
                            let task = api::request(request);
                            scheduler.dispatch(task, &mut self.session, client);
                        } else {
                            tracing::debug!(
                                "Request {}/{} was cancelled, not retrying",
                                request.method,
                                request.id
                            );
                        }
                    }

                    Action::SendRequest(request) => client.send_request_raw(&self.session, request),

                    Action::RescanProjects => {
                        api::changes::apply(
                            &mut self.session,
                            &client,
                            &[ty_project::watch::ChangeEvent::Rescan],
                        );
                    }

                    Action::SuspendWorkspaceDiagnostics(suspended_request) => {
                        self.session.set_suspended_workspace_diagnostics_request(
                            *suspended_request,
                            &client,
                        );
                    }

                    Action::InitializeWorkspaces(workspaces_with_options) => {
                        self.session
                            .initialize_workspace_folders(&client, workspaces_with_options);
                        // We do this here after workspaces have been initialized
                        // so that the file watcher globs can take project search
                        // paths into account.
                        // self.try_register_file_watcher(&client);
                    }
                },
                Event::ProjectServer(incoming) => {
                    self.answer_project_server_request(incoming, &mut scheduler, client);
                }
                Event::PollUvEnvironments { project_root } => {
                    self.session.poll_uv_sync(&client, &project_root);
                }
            }
        }

        Ok(())
    }

    /// Waits for the next message from the client or action.
    ///
    /// Returns `Ok(None)` if the client connection is closed.
    fn next_event(&mut self) -> Result<Option<Event>, crossbeam::channel::RecvError> {
        // We can't queue those into the main loop because that could result in reordering if
        // the `select` below picks a client message first.
        if let Some(deferred) = self.session.take_deferred_messages() {
            match &deferred {
                Message::Request(req) => {
                    tracing::debug!("Processing deferred request `{}`", req.method);
                }
                Message::Notification(notification) => {
                    tracing::debug!("Processing deferred notification `{}`", notification.method);
                }
                Message::Response(response) => {
                    tracing::debug!("Processing deferred response `{}`", response.id);
                }
            }

            return Ok(Some(Event::Message(deferred)));
        }

        let uv_sync = UvSyncWakeups(self.session.uv_sync_wakeups());
        uv_sync.select(&self.connection.receiver, &self.main_loop_receiver)
    }

    /// Schedules a `by` command line's request against a snapshot of this session.
    ///
    /// Twice, in the ordinary case. The first pass decides only whether this session may
    /// answer at all, and the second produces the answer — with a re-read of the file system
    /// in between, on this thread, because that is what makes the answer the caller's own:
    /// this session's picture of the file system is whatever the editor's watcher reported,
    /// and the caller is a process that can see the disk directly, so anything the watcher
    /// missed — a `git checkout` most of all — would otherwise be answered out of a tree that
    /// is no longer there.
    ///
    /// The order is the point. Re-reading walks every project in the session and makes the
    /// editor re-pull its diagnostics, which is far too much to spend on a request that was
    /// going to be refused for a configuration the two never shared.
    fn answer_project_server_request(
        &mut self,
        mut incoming: Box<project_server::Incoming>,
        scheduler: &mut Scheduler,
        client: Client,
    ) {
        if incoming.rescanned {
            api::changes::apply(&mut self.session, &client, &[ChangeEvent::Rescan]);
        }

        let sender = self.main_loop_sender.clone();
        let task = Task::background(BackgroundSchedule::Worker, move |session: &Session| {
            // the snapshot is not read after an unwind: `project_server::run` catches the
            // panic and builds its answer without it
            let snapshot = AssertUnwindSafe(session.snapshot_session());

            Box::new(move |_client: &Client| {
                let _span = tracing::debug_span!("project server request").entered();
                let snapshot = snapshot.0;
                let outcome = project_server::run(&snapshot, &incoming.payload, incoming.rescanned);

                let response = match outcome {
                    // back to the main loop to be re-read and then answered. the flag is set
                    // here rather than there so that the pass which set it is the pass that
                    // agreed the request was worth it
                    project_server::Outcome::NeedsRescan => {
                        incoming.rescanned = true;
                        requeue(&sender, incoming);
                        return;
                    }
                    project_server::Outcome::Answered(response) => response,
                };

                if project_server::is_retryable(&response) && incoming.attempts < RETRY_LIMIT {
                    incoming.attempts += 1;
                    requeue(&sender, incoming);
                    return;
                }

                let token = incoming.token.clone();
                if let Err(error) =
                    project_server::respond(&mut incoming.connection, &token, &response)
                {
                    tracing::debug!("Failed to answer a command-line request: {error}");
                }
            })
        });

        scheduler.dispatch(task, &mut self.session, client);
    }

    fn initialize(&mut self, client: &Client) {
        self.session
            .request_uninitialized_workspace_folder_configurations(client);
    }
}

/// Sends a request back to the main loop for another pass.
///
/// A failure means the main loop is gone and the process is on its way out; the caller sees
/// the connection close and checks for itself, which is the same thing every other failure
/// here leads to.
fn requeue(sender: &MainLoopSender, incoming: Box<project_server::Incoming>) {
    if sender.send(Event::ProjectServer(incoming)).is_err() {
        tracing::debug!("Dropping a command-line request: the main loop is gone");
    }
}

/// An action that should be performed on the main loop.
#[derive(Debug)]
pub(crate) enum Action {
    /// Send a response to the client
    SendResponse(lsp_server::Response),

    /// Retry a request that previously failed due to a salsa cancellation.
    RetryRequest(lsp_server::Request),

    /// Send a request from the server to the client.
    SendRequest(SendRequest),

    SuspendWorkspaceDiagnostics(Box<SuspendedWorkspaceDiagnosticRequest>),

    /// Re-read the file system, after the server itself changed something in it.
    RescanProjects,

    /// Initialize the workspace after the server received
    /// the options from the client.
    InitializeWorkspaces(Vec<(Uri, ClientOptions)>),
}

#[derive(Debug)]
pub(crate) enum Event {
    /// An incoming message from the LSP client.
    Message(lsp_server::Message),

    Action(Action),

    /// A request from a `by` command line, over the server's side channel.
    ProjectServer(Box<project_server::Incoming>),

    PollUvEnvironments {
        project_root: SystemPathBuf,
    },
}

pub(crate) struct SendRequest {
    pub(crate) method: String,
    pub(crate) params: serde_json::Value,
    pub(crate) response_handler: ClientResponseHandler,
}

impl std::fmt::Debug for SendRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendRequest")
            .field("method", &self.method)
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

/// Uv-environment wakeups for the currently active project databases.
struct UvSyncWakeups(Vec<(SystemPathBuf, crossbeam::channel::Receiver<()>)>);

impl UvSyncWakeups {
    /// Waits for a project wakeup, client message, or main-loop action.
    fn select(
        &self,
        connection: &crossbeam::channel::Receiver<Message>,
        main_loop: &MainLoopReceiver,
    ) -> Result<Option<Event>, crossbeam::channel::RecvError> {
        let mut select = crossbeam::channel::Select::new_biased();
        for (_, receiver) in &self.0 {
            select.recv(receiver);
        }
        let connection_index = select.recv(connection);
        let main_loop_index = select.recv(main_loop);
        let operation = select.select();
        let index = operation.index();

        if let Some((project_root, receiver)) = self.0.get(index) {
            return operation.recv(receiver).map(|()| {
                Some(Event::PollUvEnvironments {
                    project_root: project_root.clone(),
                })
            });
        }

        if index == connection_index {
            // Ignore disconnect errors, they're handled by the main loop (it will exit).
            return Ok(operation.recv(connection).ok().map(Event::Message));
        }

        debug_assert_eq!(index, main_loop_index);
        operation.recv(main_loop).map(Some)
    }
}

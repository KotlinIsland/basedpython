//! The side channel a `by` command line reaches the server on.
//!
//! The language server's own connection is stdin and stdout, and it belongs to the editor
//! that started it. So requests from a command line arrive somewhere else: a loopback socket,
//! opened once at startup, announced through a [record](super::discovery::Record) only this
//! user can read.
//!
//! Nothing here touches the session. A connection is read, checked against the two things
//! that can be settled without it — the protocol and the build — and then handed to the main
//! loop, which is the only place a database may be looked at.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use ruff_db::system::{SystemPath, SystemPathBuf};

use super::discovery::{self, Record, Registration};
use super::protocol::{Answer, Build, PROTOCOL, Payload, Refusal, Request, Response};
use crate::server::{Event, MainLoopSender};

/// How long a client is given to send its request once it has connected.
///
/// A connection that opens and says nothing holds a thread, and the port is reachable by
/// anything running on this machine.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the server will spend trying to hand an answer back.
///
/// A client that stops reading part way through would otherwise pin a worker thread for as
/// long as it cared to.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// The most a request may be.
///
/// Nothing legitimate comes close: the largest field is a project's merged configuration,
/// which is kilobytes. The cap is here because reading a line from a socket that anyone on
/// this machine can connect to is otherwise an invitation to fill memory, and the token that
/// would stop them is not read until after the line is.
const MAX_REQUEST: u64 = 4 * 1024 * 1024;

/// A request that reached the main loop, and the connection waiting for its answer.
#[derive(Debug)]
pub(crate) struct Incoming {
    pub(crate) payload: Payload,
    pub(crate) connection: TcpStream,

    /// The secret this request arrived with, to be repeated in the answer.
    pub(crate) token: String,

    /// Whether the file system has been re-read for this request yet.
    ///
    /// The re-read is the expensive part and it disrupts the editor, so it happens only once
    /// everything cheap about the request has been agreed — see the main loop.
    pub(crate) rescanned: bool,

    /// How many times this has been through the main loop.
    ///
    /// A check runs on a snapshot, and an edit in the editor cancels it. Re-queueing takes a
    /// fresh snapshot, which is the only way to retry; giving up after a few is what keeps a
    /// caller from waiting on a session that is being typed into.
    pub(crate) attempts: u8,
}

/// Everything the server holds open for as long as it accepts command-line requests.
///
/// Dropping it takes the record off disk. The listener thread ends when the process does: it
/// is blocked in `accept`, and there is nothing worth waking it for.
#[derive(Debug)]
pub(crate) struct Listener {
    _registration: Registration,
}

impl Listener {
    /// Starts accepting command-line requests for `roots`, announcing itself in `directory`.
    ///
    /// `Ok(None)` when there is nothing to announce: a server with no workspace folders holds
    /// no project any caller could ask about, so a record for it could only ever be a port to
    /// connect to and be refused by.
    ///
    /// An `Err` is the listener failing to start, which is not fatal: a server that cannot be
    /// reached this way is a server every command line checks for itself, which is what they
    /// all did before.
    pub(crate) fn spawn(
        sender: MainLoopSender,
        directory: &SystemPath,
        roots: Vec<SystemPathBuf>,
        version: &str,
    ) -> anyhow::Result<Option<Self>> {
        if roots.is_empty() {
            tracing::debug!("Not accepting command-line requests: this server has no workspaces");
            return Ok(None);
        }

        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let port = listener.local_addr()?.port();
        let token = super::token()?;

        let build = Build::current(version);
        let registration = discovery::publish(
            directory,
            &Record {
                protocol: PROTOCOL,
                build: build.clone(),
                port,
                token: token.clone(),
                roots,
            },
        )?;

        std::thread::Builder::new()
            .name("ty:project-server".to_owned())
            .spawn(move || accept_loop(&listener, &sender, &token, &build))?;

        tracing::info!("Accepting command-line requests on 127.0.0.1:{port}");

        Ok(Some(Self {
            _registration: registration,
        }))
    }
}

fn accept_loop(listener: &TcpListener, sender: &MainLoopSender, token: &str, build: &Build) {
    std::thread::scope(|scope| {
        for connection in listener.incoming() {
            let Ok(connection) = connection.inspect_err(|error| {
                tracing::debug!("Failed to accept a command-line connection: {error}");
            }) else {
                continue;
            };

            // one thread per connection, because reading one is the only part of this that
            // waits on somebody else. a single slow caller — or a local process that connects
            // and says nothing until its five seconds are up — would otherwise be enough to
            // stop every other caller being heard
            let spawned = std::thread::Builder::new()
                .name("ty:project-server-request".to_owned())
                .spawn_scoped(scope, || {
                    if let Err(error) = accept(connection, sender, token, build) {
                        tracing::debug!("Failed to read a command-line request: {error}");
                    }
                });

            if let Err(error) = spawned {
                tracing::debug!("Failed to start a thread for a command-line request: {error}");
            }
        }
    });
}

fn accept(
    mut connection: TcpStream,
    sender: &MainLoopSender,
    token: &str,
    build: &Build,
) -> anyhow::Result<()> {
    connection.set_read_timeout(Some(READ_TIMEOUT))?;
    connection.set_write_timeout(Some(WRITE_TIMEOUT))?;

    let mut line = String::new();
    BufReader::new(connection.try_clone()?)
        .take(MAX_REQUEST)
        .read_line(&mut line)?;
    let request: Request = serde_json::from_str(&line)?;

    // an unreadable request and a wrong token get the same treatment, which is none: the
    // caller learns whether it guessed the token by whether it is answered at all
    if !constant_time_eq(&request.token, token) {
        tracing::warn!("Rejecting a command-line request with the wrong token");
        return Ok(());
    }

    if request.protocol != PROTOCOL {
        return respond(
            &mut connection,
            token,
            &Response::Refused {
                reason: Refusal::Protocol { server: PROTOCOL },
            },
        );
    }

    // same protocol is not the same checker. two builds of `by` disagree about what this
    // project's diagnostics are as readily as they disagree about anything else, and the
    // caller asked for its own answer
    if !build.is(&request.client) {
        return respond(
            &mut connection,
            token,
            &Response::Refused {
                reason: Refusal::Version {
                    server: Box::new(build.clone()),
                },
            },
        );
    }

    // the payload is only parsed once the two agree on how to read it. a newer client's
    // request would fail here, having already been told that its build is not this one
    let payload: Payload = serde_json::from_value(request.payload)?;

    sender
        .send(Event::ProjectServer(Box::new(Incoming {
            payload,
            connection,
            token: request.token,
            rescanned: false,
            attempts: 0,
        })))
        .map_err(|_| anyhow::anyhow!("the main loop is gone"))
}

pub(crate) fn respond(
    connection: &mut TcpStream,
    token: &str,
    response: &Response,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(&Answer {
        token: token.to_owned(),
        response: response.clone(),
    })?;
    line.push(b'\n');
    connection.write_all(&line)?;
    connection.flush()?;
    Ok(())
}

/// Whether two secrets are equal, in time that does not depend on where they first differ.
///
/// The channel is loopback and the attack is not a practical one, but a comparison that
/// leaks is not cheaper to write than one that does not.
fn constant_time_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }

    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

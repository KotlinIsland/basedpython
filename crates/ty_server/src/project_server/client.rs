//! Asking a running server for an answer, from the `by` command line.
//!
//! Everything in here fails quietly. A caller uses this the way it would use a cache: it
//! asks, and if it does not get an answer it does the work. So there is no error type — a
//! failure is `None` and a line in the log, and the caller carries on into the check it was
//! always able to run.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use ruff_db::system::SystemPath;

use super::discovery;
use super::protocol::{
    Answer, Build, CheckRequest, CheckResponse, PROTOCOL, Payload, Refusal, Request, Response,
};

/// How long the server has to accept a connection.
///
/// It is on this machine, and it is either listening or it is a record left behind by a
/// process that has died. There is no third case worth waiting on.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// How long the server has to answer once it has accepted.
///
/// Long, because the answer is a real check: the usual case is a database that has the result
/// already, but a server that has just started, or one whose project changed under it, does
/// the same work the caller would have done. Not unbounded, because a server whose main loop
/// has wedged would otherwise leave the caller waiting for something that is never coming,
/// with a check it could have run itself sitting there the whole time.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// The most an answer may be.
///
/// A project's rendered diagnostics can genuinely be megabytes, so this is generous — but the
/// port a record names is free for anything to bind once its server dies, and a reply from
/// something that is not a server should not be able to fill memory before the token that
/// would have caught it is read.
const MAX_RESPONSE: u64 = 256 * 1024 * 1024;

/// Asks a server holding `request.project_root` to check it.
///
/// `None` when there is no server to ask, when the ones there are will not answer for this
/// caller, or when anything at all goes wrong on the way.
pub fn check(
    directory: &SystemPath,
    request: CheckRequest,
    build: &Build,
) -> Option<CheckResponse> {
    let project_root = request.project_root.clone();
    let payload = Payload::Check(request);

    // two editors can hold the same project, and one of them may be in a state that stops it
    // answering while the other is not — so a refusal moves on to the next rather than ending
    // the search. every refusal is a server that did nothing, so the request is still ours to
    // send again
    for candidate in discovery::candidates(directory, &project_root) {
        match ask(&candidate, &payload, build) {
            Some(Response::Check(response)) => return Some(response),
            Some(Response::Refused { reason }) => {
                tracing::debug!("A project server did not answer: {reason}");
                if let Refusal::Options { server } = &reason {
                    tracing::debug!("It resolved: {server}");
                }
            }
            None => {}
        }
    }

    tracing::debug!("No project server answered for `{project_root}`");
    None
}

/// One round trip, or `None` if there wasn't one.
///
/// `payload` is borrowed rather than moved because every outcome here leaves the request
/// still ours to make: a server that refuses did nothing, and a server that could not be
/// reached never saw it.
fn ask(candidate: &discovery::Candidate, payload: &Payload, build: &Build) -> Option<Response> {
    let record = &candidate.record;

    // both of these are in the record, so they are settled before a connection is opened
    // rather than after a round trip
    if record.protocol != PROTOCOL {
        tracing::debug!(
            "Ignoring a project server speaking protocol {} (this build speaks {PROTOCOL})",
            record.protocol
        );
        return None;
    }
    if !build.is(&record.build) {
        tracing::debug!(
            "Ignoring a project server from a different build ({})",
            record.build.version
        );
        return None;
    }

    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, record.port));
    let Ok(mut connection) = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) else {
        // a listening server always accepts, so this is a record whose server is gone
        tracing::debug!("No project server is listening on {address}; forgetting its record");
        discovery::forget(candidate);
        return None;
    };

    let request = Request {
        protocol: PROTOCOL,
        token: record.token.clone(),
        client: build.clone(),
        payload: serde_json::to_value(payload)
            .inspect_err(|error| tracing::debug!("Failed to serialize the request: {error}"))
            .ok()?,
    };

    match round_trip(&mut connection, &request, &record.token) {
        Ok(response) => Some(response),
        Err(error) => {
            tracing::debug!("The project server on {address} did not answer: {error}");
            None
        }
    }
}

fn round_trip(
    connection: &mut TcpStream,
    request: &Request,
    token: &str,
) -> anyhow::Result<Response> {
    connection.set_read_timeout(Some(RESPONSE_TIMEOUT))?;
    connection.set_write_timeout(Some(SEND_TIMEOUT))?;

    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    connection.write_all(&line)?;
    connection.flush()?;

    let mut response = String::new();
    BufReader::new(connection)
        .take(MAX_RESPONSE)
        .read_line(&mut response)?;
    if response.is_empty() {
        anyhow::bail!("the server closed the connection without answering");
    }

    let answer: Answer = serde_json::from_str(&response)?;

    // the record's port was free to be taken by anything once the server that published it
    // died, so an answer only counts if it came from something that had read the record
    if answer.token != token {
        anyhow::bail!("something other than the project server answered on its port");
    }

    Ok(answer.response)
}

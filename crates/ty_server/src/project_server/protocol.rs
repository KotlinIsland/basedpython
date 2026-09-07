//! What a `by` command line and a running server say to each other.
//!
//! One JSON object per message, terminated by a newline. `serde_json` escapes newlines
//! inside strings, so a rendered diagnostic — which is full of them — still arrives as a
//! single line.

use ruff_db::system::SystemPathBuf;

/// Bumped whenever the shape below changes.
///
/// A client and a server that disagree on it stop talking, which matters because the two
/// are separate builds of separate binaries: the server in the editor may be months older
/// than the `by` on the `PATH`.
pub const PROTOCOL: u32 = 1;

/// A project's merged configuration, in the one shape both sides will produce for it.
///
/// Serializing the options is not enough on its own. A command line builds its layer of them
/// out of its arguments, and does so by filling in every group whether or not anything went
/// into it — so a `by check` with no flags at all resolves `{"environment": {}, "terminal":
/// {}, …}` where a server resolves `{}`. Those say the same thing, and a comparison that
/// called them different would refuse every invocation there is.
///
/// So a group with nothing in it is removed, recursively. An empty *list* is left alone: it
/// is a real setting, and `exclude = []` does not mean the same as no `exclude` at all.
pub fn configuration<T: serde::Serialize>(
    options: &T,
) -> Result<serde_json::Value, serde_json::Error> {
    fn prune(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(fields) => serde_json::Value::Object(
                fields
                    .into_iter()
                    .map(|(name, value)| (name, prune(value)))
                    .filter(|(_, value)| match value {
                        serde_json::Value::Null => false,
                        serde_json::Value::Object(fields) => !fields.is_empty(),
                        _ => true,
                    })
                    .collect(),
            ),
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.into_iter().map(prune).collect())
            }
            value => value,
        }
    }

    serde_json::to_value(options).map(prune)
}

/// The part of a request that is read before anything is decided.
///
/// Deliberately lenient about its payload, which stays JSON until the protocol and the build
/// have been agreed. A stricter envelope would fail to parse a newer client's request and
/// close the connection without saying why, which is the one case the version fields exist to
/// explain.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub protocol: u32,

    /// The secret from the server's own discovery record.
    ///
    /// The listener is on loopback, so anything running as any user on this machine can
    /// reach the port. The record file it has to read first is not readable by them.
    pub token: String,

    pub client: Build,

    pub payload: serde_json::Value,
}

/// Enough about a build of `by` to tell it apart from another one.
///
/// The version alone is not enough, and the case it misses is the one that matters most: two
/// builds of the same commit with different uncommitted changes report the same version, and
/// that is the ordinary state of an editor holding a server while its `by` is rebuilt. So the
/// executable is identified as well — the same file, unmodified, is the same build, which is
/// the only direction this needs to be sure about.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Build {
    pub version: String,

    /// The executable, and what the file system last said about it.
    ///
    /// `None` where the executable cannot be identified at all, which never compares equal to
    /// anything — including another `None`.
    pub executable: Option<Executable>,
}

impl Build {
    /// This process's build.
    pub fn current(version: &str) -> Self {
        Self {
            version: version.to_owned(),
            executable: Executable::current(),
        }
    }

    /// Whether `other` is known to be the same build as this one.
    ///
    /// Never assumes: two builds it cannot tell apart are reported as different, because the
    /// cost of that is a check the caller was always able to run.
    pub(crate) fn is(&self, other: &Self) -> bool {
        self.version == other.version
            && match (&self.executable, &other.executable) {
                (Some(left), Some(right)) => left == right,
                _ => false,
            }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Executable {
    pub path: String,
    pub modified: u64,
    pub size: u64,
}

impl Executable {
    fn current() -> Option<Self> {
        let path = std::env::current_exe().ok()?;
        let metadata = std::fs::metadata(&path).ok()?;
        Some(Self {
            path: path.to_str()?.to_owned(),
            modified: metadata
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs(),
            size: metadata.len(),
        })
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Payload {
    Check(CheckRequest),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckRequest {
    /// The project the command line resolved for itself.
    ///
    /// A server holds a database per workspace, and a workspace is not always a project —
    /// so the root is matched against the databases rather than assumed.
    pub project_root: SystemPathBuf,

    /// The command line's configuration, merged across every layer, as JSON.
    ///
    /// The two sides must agree about what checking this project *means* before one can
    /// answer for the other. Merged, because the layers are where the disagreements are: the
    /// caller's flags and the editor's own settings both arrive as layers over the same
    /// configuration file, and comparing the file alone compares the one thing that could
    /// never have differed.
    ///
    /// Sending the whole thing rather than a hash of it costs a few kilobytes once and buys a
    /// diffable explanation when the two disagree.
    pub options: serde_json::Value,

    /// The python version, platform and search paths the command line resolved.
    ///
    /// Not derivable from the options: an environment is discovered as much as configured,
    /// out of `VIRTUAL_ENV`, `CONDA_PREFIX`, a `.venv` beside the project, uv's answer, or the
    /// interpreter the running executable sits next to — and a server started by an editor
    /// discovers it from the editor's environment, not from the caller's shell. Two processes
    /// that agree about every option can still be checking against different site-packages.
    ///
    /// A rendering rather than the settings themselves, because what has to cross is an
    /// identity to compare and a difference to print.
    pub environment: String,

    /// Whether exclusions apply to paths named on the command line.
    ///
    /// A database input rather than an option, so it is not in the merged configuration and
    /// has to travel on its own.
    pub force_exclude: bool,

    /// Whether every diagnostic should say where its rule was turned on.
    ///
    /// Also a database input, and one a server never sets: an editor has its own way of
    /// showing where a rule came from. So a verbose check is always refused — which is the
    /// point of comparing it rather than declining to ask, because a refusal says so in the
    /// log where a decision not to ask would say nothing.
    pub verbose: bool,

    /// The directory the command was run from.
    ///
    /// Diagnostics name their file relative to where the reader is standing, and the server
    /// is standing wherever the editor started it. Without this every path in the answer
    /// would be absolute, which is a visible difference from the same check run cold.
    pub working_directory: SystemPathBuf,

    /// Whether the command line's output is going somewhere that renders ANSI colour.
    ///
    /// The server cannot see the caller's terminal, and its own stdout is the LSP
    /// connection.
    pub color: bool,
}

/// A response, and the proof that it came from the server the caller meant.
///
/// The token travels back because a record outlives the process that wrote it: a server that
/// was killed leaves its port behind, and any local process that binds that port next
/// receives the request. Without this, such a process could answer "All checks passed!" and
/// the caller would exit zero on a project it never looked at.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Answer {
    pub token: String,
    pub response: Response,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Response {
    Check(CheckResponse),

    /// The server will not answer, and the caller should check for itself.
    ///
    /// Never an error: every refusal is a case where the hot answer might not have been
    /// the answer a cold run would give.
    Refused {
        reason: Refusal,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
// four flags, because a rendering is the one thing that crosses and everything the caller
// still has to decide has to cross beside it. they are not a state machine; they are four
// independent facts about one check
#[expect(clippy::struct_excessive_bools)]
pub struct CheckResponse {
    /// The diagnostics, already rendered in the project's configured output format.
    ///
    /// Rendering happens on the server because it needs the database — the source text
    /// behind every span, and the roots that make a path relative. Sending the diagnostics
    /// themselves would mean the caller reopening every file they point into.
    pub rendered: String,

    pub diagnostics: usize,

    /// Whether the configured output format is one a summary line belongs under.
    pub human_readable: bool,

    /// The worst severity reported, or `None` when nothing was.
    ///
    /// The exit status is the caller's to decide — it is the process that carries it — so
    /// what crosses is what the decision reads rather than the decision.
    pub max_severity: Option<SeverityLevel>,

    pub io_error: bool,
    pub error_on_warning: bool,

    /// Whether the project turned out to hold no python at all.
    ///
    /// The cold path warns about this, and about [`Self::fatal`], on stderr. Neither is a
    /// diagnostic, so neither is in the rendering, and a caller that did not hear them would
    /// be quietly getting less than the command normally gives it.
    pub empty_project: bool,

    /// Whether anything failed so badly that the check is incomplete.
    pub fatal: bool,
}

/// [`ruff_db::diagnostic::Severity`], in a shape that survives the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SeverityLevel {
    Info,
    Warning,
    Error,
    Fatal,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Refusal {
    Protocol {
        server: u32,
    },

    /// The two are different builds of `by`.
    ///
    /// Same protocol, different checker: the answers could differ for any reason at all.
    Version {
        server: Box<Build>,
    },

    UnknownProject,

    /// The server resolved a different configuration for this project than the caller did.
    Options {
        server: Box<serde_json::Value>,
    },

    /// The server resolved a different environment for this project than the caller did.
    Environment {
        server: String,
    },

    /// The two disagree about whether exclusions apply to paths named on the command line.
    ForceExclude {
        server: bool,
    },

    /// The caller asked for diagnostics that say where each rule was turned on.
    Verbose,

    /// The server is only checking the files the editor has open.
    ///
    /// Its answer would be the diagnostics for those files, and a `by check` asked for the
    /// project's.
    CheckMode,

    /// A file open in the editor does not match what is on disk.
    ///
    /// The server's view of an open file is the editor's buffer, so answering from it would
    /// report on source the caller cannot see.
    Unsaved {
        files: Vec<String>,
    },

    /// The database changed under the check often enough that it never finished.
    Cancelled,

    /// Already logged on the server.
    Panicked,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Protocol { server } => {
                write!(
                    f,
                    "the server speaks protocol {server}, this build speaks {PROTOCOL}"
                )
            }
            Refusal::Version { server } => {
                write!(f, "the server is a different build ({})", server.version)
            }
            Refusal::UnknownProject => f.write_str("the server has no database for this project"),
            Refusal::CheckMode => f.write_str(
                "the server is only checking open files — set the editor's diagnostic mode to \
                 `workspace` for it to answer for the whole project",
            ),
            Refusal::Options { .. } => {
                f.write_str("the server resolved a different configuration for this project")
            }
            Refusal::Environment { .. } => {
                f.write_str("the server resolved a different environment for this project")
            }
            Refusal::Verbose => f.write_str(
                "a verbose check explains where each rule was turned on, and the server's \
                 diagnostics do not",
            ),
            Refusal::ForceExclude { server } => write!(
                f,
                "the server checks with `force-exclude` {}",
                if *server { "on" } else { "off" }
            ),
            Refusal::Unsaved { files } => {
                write!(f, "unsaved editor changes in {}", files.join(", "))
            }
            Refusal::Cancelled => f.write_str("the server was too busy to finish the check"),
            Refusal::Panicked => f.write_str("the check panicked on the server"),
        }
    }
}

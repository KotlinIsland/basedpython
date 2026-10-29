//! `by repl`: basedpython typed an input at a time, run in one python process
//!
//! the project is staged as `by run` stages it, and the interpreter runs `_by_repl.py` in that
//! tree, which reads the inputs, runs them and prints what python's repl prints. each input is
//! a question for this process first: whether it is whole yet, and what python it runs as. the
//! session is one module whose source is the inputs that ran (see
//! `docs/basedpython/features/repl.md`), held here as the text of a file the project's db checks
//! and transpiles like any other, and [`by_stage::session`] picks the input's statements out of
//! what the module comes to. the file is one of its own outside the project, or, for a session
//! started in a module of the project with `--module`, that module's: its source is the
//! session's first, and the inputs follow it
//!
//! the two halves talk over a socket on the loopback interface rather than over the
//! interpreter's standard streams, because those are the program's: `input()` reads standard
//! input, and the program's output is its own. the interpreter is told the port and a token in
//! its environment, and the first thing it sends is the token, so nothing else on the machine
//! that finds the port is answered
//!
//! for the same reason an editor that asks with `--events` is told what standard input is
//! being read for — an input, or the program — on a connection of its own: the interpreter says
//! so to this process, which passes it on

use std::fs;
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Context;
use by_stage::project::Rebuilder;
use by_stage::session::{Refusal, Session, input_status};
use by_transforms::config::Config;
use ruff_db::diagnostic::render::{FullRenderer, Renderer};
use ruff_db::diagnostic::{
    Diagnostic, DisplayDiagnosticConfig, FileResolver, Input, Severity, Span, UnifiedFile,
};
use ruff_db::files::{File, system_path_to_file};
use ruff_db::panic::PanicActivity;
use ruff_db::source::source_text;
use ruff_db::system::SystemPath;
use ruff_diagnostics::SourceMap;
use ruff_notebook::NotebookIndex;
use ruff_python_ast::PySourceType;
use ruff_python_parser::parse_unchecked_source;
use ruff_source_file::{SourceFile, SourceFileBuilder};
use ruff_text_size::{TextRange, TextSize};
use salsa::Setter as _;
use serde_json::{Value, json};
use ty_project::{Db as _, ProjectDatabase};

use ty_static::EnvVars;

use crate::ExitStatus;
use crate::args::LoweringArgs;
use crate::by_commands::{BY_REPL_FILENAME, RunTarget, StagedRun, stage_run};

/// how long the interpreter has to connect and send its token once it has started: it does both
/// before it runs anything of the session's
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// what `by repl` is given beyond what it stages the project with
pub(crate) struct ReplFlags<'a> {
    /// `--python`: the interpreter, or the environment holding it
    pub(crate) python: Option<&'a Path>,
    /// `--session-file`: where an editor reads the session's source
    pub(crate) session_file: Option<&'a Path>,
    /// `--events`: the port on the loopback interface an editor listens for the session's
    /// events on
    pub(crate) events: Option<u16>,
    /// `--module`: the module of the project the session starts in
    pub(crate) module: Option<&'a str>,
}

#[expect(clippy::exit, clippy::print_stderr)]
pub(crate) fn cmd_repl(
    min_version: Option<&str>,
    lowering: &LoweringArgs,
    flags: &ReplFlags<'_>,
) -> anyhow::Result<ExitStatus> {
    let staged = match stage_run(
        min_version,
        lowering,
        flags.python,
        false,
        RunTarget::Session,
    )? {
        Ok(staged) => staged,
        Err(status) => return Ok(status),
    };
    let StagedRun {
        cwd,
        interpreter,
        config,
        tree,
        rebuilder,
        ..
    } = staged;
    let session_file = flags.session_file.map(|path| cwd.join(path));
    if let Some(path) = &session_file {
        fs::write(path, "").with_context(|| format!("could not write {}", path.display()))?;
    }
    let events = flags.events.map(connect_events).transpose()?;
    let mut repl = Repl::new(rebuilder, config, flags.module, session_file, events)?;
    // a session that cannot start in the module it was asked to has said why, and does not start
    if !repl.prepare_start()? {
        return Ok(ExitStatus::Failure);
    }

    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("could not listen for the session")?;
    let port = listener.local_addr()?.port();
    let token = token();
    let mut command = Command::new(&interpreter.path);
    command
        .arg(tree.path().join(BY_REPL_FILENAME))
        .env("BY_REPL_PORT", port.to_string())
        .env("BY_REPL_TOKEN", &token)
        // set either way, so a value this process was started with is never the session's
        .env(
            "BY_REPL_EVENTS",
            if flags.events.is_some() { "1" } else { "0" },
        )
        // the editor's, not the program's
        .env_remove(EnvVars::BY_REPL_EVENTS_TOKEN)
        .current_dir(&cwd);
    // ctrl-c is the interpreter's to answer, with a `KeyboardInterrupt` in the input it is
    // running; this process lives as long as the session does, from before the interpreter
    // can be sent one
    ctrlc::set_handler(|| {}).context("could not hand ctrl-c to the session")?;
    let mut child = command.spawn().with_context(|| {
        format!(
            "could not run `{}`, the interpreter from {}",
            interpreter.path, interpreter.origin
        )
    })?;

    let served = accept(&listener, &token, &mut child).and_then(|reader| match reader {
        Some(reader) => repl.serve(reader),
        None => Ok(()),
    });
    if let Err(error) = &served {
        eprintln!("error: the session ended: {error:#}");
        // the interpreter is left with no one to answer it
        let _ = child.kill();
    }
    let status = child
        .wait()
        .context("the session's interpreter could not be waited for")?;
    drop(repl);
    // `process::exit` skips destructors, and the tree is a temporary directory
    drop(tree);
    std::process::exit(crate::by_commands::exit_code(status));
}

/// the editor's connection for the session's events, to `port` on the loopback interface, with
/// the token it set in `BY_REPL_EVENTS_TOKEN` sent on it
fn connect_events(port: u16) -> anyhow::Result<TcpStream> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("could not connect to the editor's events port, {port}"))?;
    let token = std::env::var(EnvVars::BY_REPL_EVENTS_TOKEN).ok();
    writeln!(stream, "{}", json!({ "token": token }))?;
    stream.flush()?;
    Ok(stream)
}

/// a token nothing else can guess, from the process's own random hasher keys
fn token() -> String {
    let state = RandomState::new();
    format!(
        "{:016x}{:016x}",
        state.hash_one(std::process::id()),
        state.hash_one(Instant::now())
    )
}

/// the interpreter's connection, read through the one buffer every message after the token
/// is read through, once it has sent `token`. `None` when the interpreter ended without
/// making one
///
/// anything on the machine can connect to the port, so no connection holds up another: each
/// is read as its bytes arrive, only as far as the token goes, and one that sends anything but
/// the token is dropped
fn accept(
    listener: &TcpListener,
    token: &str,
    child: &mut std::process::Child,
) -> anyhow::Result<Option<BufReader<TcpStream>>> {
    listener.set_nonblocking(true)?;
    let expected = format!("{token}\n").into_bytes();
    // each connection that has yet to send the whole token, with what it has sent of it
    let mut connections: Vec<(TcpStream, Vec<u8>)> = Vec::new();
    let started = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(true)?;
                connections.push((stream, Vec::new()));
                continue;
            }
            Err(error) if is_transient(&error) => {}
            Err(error) => return Err(error).context("could not accept the session's interpreter"),
        }
        let mut index = 0;
        while index < connections.len() {
            let (stream, sent) = &mut connections[index];
            // never past the token, so what the interpreter sends after it stays for the reader
            let mut buffer = vec![0; expected.len() - sent.len()];
            match stream.read(&mut buffer) {
                Ok(read)
                    if read > 0 && expected.starts_with(&[&sent[..], &buffer[..read]].concat()) =>
                {
                    sent.extend_from_slice(&buffer[..read]);
                    if sent.len() == expected.len() {
                        let (stream, _) = connections.swap_remove(index);
                        stream.set_nonblocking(false)?;
                        return Ok(Some(BufReader::new(stream)));
                    }
                }
                Err(error) if is_transient(&error) => {}
                // closed, or sent something that is not the token
                _ => {
                    connections.swap_remove(index);
                    continue;
                }
            }
            index += 1;
        }
        if child.try_wait()?.is_some() {
            return Ok(None);
        }
        anyhow::ensure!(
            started.elapsed() < CONNECT_TIMEOUT,
            "the interpreter did not connect to the session"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// whether `error`, from a socket that does not block, only says to try again
fn is_transient(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}

/// the session as this process holds it: the module, the db that checks it, and what each
/// input that ran was
struct Repl {
    db: ProjectDatabase,
    /// the project, with the module's directory one of its first-party roots when it is a file
    /// of its own
    rebuilder: Rebuilder,
    /// what the module is transpiled for: the project's run configuration, as a session
    config: Config,
    session: Session,
    /// the file the module is checked as, whose text the db holds as the session's source: one
    /// of its own outside the project, or the module of the project the session started in
    module: File,
    /// keeps the directory of a module of its own as long as the session
    _module_dir: Option<tempfile::TempDir>,
    /// `--module`: the module of the project the session started in
    started_in: Option<StartModule>,
    /// how much of the session's source is the module it started in, which a diagnostic points
    /// into as the module's file rather than as an input
    prelude: TextSize,
    /// what the interpreter runs before the first input: the module the session starts in, as
    /// `run` answers for an input
    start: Option<Value>,
    /// each input that ran, for a diagnostic of a later one to point into
    inputs: Vec<RanInput>,
    /// the input handed over to run, which is part of the session once the interpreter says it
    /// has started running it
    pending: Option<Pending>,
    /// the number the next input is named by
    next: u32,
    /// `--session-file`: where an editor reads the session's source
    session_file: Option<PathBuf>,
    /// `--events`: the editor's connection, which each event the interpreter reports is
    /// passed on to
    events: Option<TcpStream>,
}

/// the module of the project a session starts in
struct StartModule {
    /// the name it was given, as `by run` takes one
    name: String,
    /// its file
    path: PathBuf,
    /// its source, as the session starts with it
    text: String,
}

struct RanInput {
    /// where in the session's source the input starts
    start: TextSize,
    file: SourceFile,
}

struct Pending {
    number: u32,
    text: String,
    selection: by_stage::session::Selection,
}

impl Repl {
    fn new(
        rebuilder: Rebuilder,
        config: Config,
        start_in: Option<&str>,
        session_file: Option<PathBuf>,
        events: Option<TcpStream>,
    ) -> anyhow::Result<Self> {
        let (rebuilder, mut db, module, module_dir, started_in) = match start_in {
            None => {
                let module_dir =
                    tempfile::TempDir::new().context("failed to create temp directory")?;
                // a module name nothing a project imports is called, since the directory is
                // searched for first-party modules after the project's own roots
                let module_path = module_dir.path().join("__by_session__.by");
                fs::write(&module_path, "")?;
                let directory = SystemPath::from_std_path(module_dir.path())
                    .with_context(|| format!("non-utf8 path: {}", module_dir.path().display()))?;
                let rebuilder = rebuilder.with_first_party_root(directory);
                let db = rebuilder.project_database();
                let system_path = SystemPath::from_std_path(&module_path)
                    .with_context(|| format!("non-utf8 path: {}", module_path.display()))?;
                let module = system_path_to_file(&db, system_path)
                    .with_context(|| format!("could not read {}", module_path.display()))?;
                (rebuilder, db, module, Some(module_dir), None)
            }
            Some(name) => {
                let db = rebuilder.project_database();
                // the module `by run` would run: the session is its continuation, checked as
                // the module itself, so a relative import resolves in it and in an input alike
                let module = by_stage::run_module::module_file(&db, name)
                    .with_context(|| format!("no module named `{name}` in the project"))?;
                let path = module
                    .path(&db)
                    .as_system_path()
                    .with_context(|| format!("module `{name}` is no file"))?
                    .as_std_path()
                    .to_path_buf();
                let text = source_text(&db, module).as_str().to_owned();
                let started_in = StartModule {
                    name: name.to_owned(),
                    path,
                    text,
                };
                (rebuilder, db, module, None, Some(started_in))
            }
        };
        // one outside the project is checked only as a file the project has open
        db.project().open_file(&mut db, module);
        Ok(Self {
            db,
            rebuilder,
            config: Config {
                session: true,
                // a `from` import binds its names when it is entered, so a name that cannot be
                // imported says so then, as it does in python's repl. deferred, it would be
                // bound through a holder that each later input's transpile writes afresh
                defer_from_imports: false,
                runtime_module: None,
                ..config
            },
            session: Session::default(),
            module,
            _module_dir: module_dir,
            started_in,
            prelude: TextSize::default(),
            start: None,
            inputs: Vec::new(),
            pending: None,
            next: 1,
            session_file,
            events,
        })
    }

    /// answer the interpreter until it hangs up
    fn serve(&mut self, mut reader: BufReader<TcpStream>) -> anyhow::Result<()> {
        let mut writer = reader.get_ref().try_clone()?;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let message: Value = serde_json::from_str(&line).context("an unreadable message")?;
            let reply = if let Some(text) = message["status"].as_str() {
                Some(json!({ "status": input_status(text) }))
            } else if message.get("start").is_some() {
                Some(self.start.take().unwrap_or_else(|| json!({})))
            } else if let Some(text) = message["run"].as_str() {
                Some(self.run(text)?)
            } else if let Some(number) = message["ran"].as_u64() {
                self.ran(number)?;
                None
            } else if let Some(event) = message.get("event") {
                self.event(event)?;
                None
            } else {
                anyhow::bail!("an unknown message: {line}");
            };
            if let Some(reply) = reply {
                writeln!(writer, "{reply}")?;
                writer.flush()?;
            }
        }
    }

    /// make ready what the interpreter runs before the first input: the module the session
    /// starts in, if it starts in one. `false`, having said why on standard error, when the
    /// module cannot run as the session's start
    fn prepare_start(&mut self) -> anyhow::Result<bool> {
        let Some(StartModule { name, path, text }) = &self.started_in else {
            return Ok(true);
        };
        let (name, path, text) = (name.clone(), path.clone(), text.clone());
        self.prelude = TextSize::try_from(text.len())?;
        let file = SourceFileBuilder::new(path.display().to_string(), &*text).finish();
        let Some(mut reply) = self.prepare(0, &text, &file)? else {
            return Ok(false);
        };
        // a module prints no values, as a program does not
        reply["display"] = json!(false);
        reply["module"] = json!(name);
        reply["file"] = json!(path);
        reply["text"] = json!(text);
        self.start = Some(reply);
        Ok(true)
    }

    /// what `text` runs as, or what is in the way of it running, said on standard error
    fn run(&mut self, text: &str) -> anyhow::Result<Value> {
        let number = self.next;
        self.next += 1;
        let input_file = SourceFileBuilder::new(format!("<by-input-{number}>"), text).finish();
        Ok(self
            .prepare(number, text, &input_file)?
            .unwrap_or_else(|| json!({ "input": number })))
    }

    /// what `text`, input `number`, runs as, and the run handed over for the interpreter to
    /// start; `None` when something is in the way of it running, having said what on standard
    /// error. `input_file` is the input as a diagnostic quotes it
    fn prepare(
        &mut self,
        number: u32,
        text: &str,
        input_file: &SourceFile,
    ) -> anyhow::Result<Option<Value>> {
        self.pending = None;

        // the input's own grammar first: a line that continues an earlier input, `else:` after
        // an `if`, would otherwise parse as part of it
        let parsed = parse_unchecked_source(text, PySourceType::BasedPython);
        if !parsed.errors().is_empty() {
            let diagnostics: Vec<Diagnostic> = parsed
                .errors()
                .iter()
                .map(|error| {
                    Diagnostic::invalid_syntax(input_file.clone(), &error.error, error.location)
                })
                .collect();
            self.render(&diagnostics);
            return Ok(None);
        }

        let (source, lines) = self.session.with_input(text);
        // the module's text is the session's, whatever its file holds
        let source = source_text(&self.db, self.module).with_text(source, &SourceMap::default());
        self.module
            .set_source_text_override(&mut self.db)
            .to(Some(source));
        let start = TextSize::try_from(self.session.source().len())?;
        let errors: Vec<Diagnostic> =
            ty_project::check_file_ignoring_check_mode(&self.db, self.module)
                .into_iter()
                .filter(|diagnostic| diagnostic.severity() >= Severity::Error)
                .map(|diagnostic| self.in_inputs(diagnostic, start, input_file))
                .collect();
        if !errors.is_empty() {
            self.render(&errors);
            return Ok(None);
        }

        let rebuild = || Some(self.rebuilder.rebuild());
        let transpiled = ty_project::catch_panic(
            &self.db,
            self.module,
            PanicActivity::Transpiling,
            AssertUnwindSafe(|| {
                by_transforms::transpile_typed_with_report(
                    &self.db,
                    self.module,
                    &self.config,
                    Some(&rebuild),
                )
            }),
        );
        let (python, line_map) = match transpiled {
            Ok(Ok((python, line_map, _, _))) => (python, line_map),
            Ok(Err(error)) => {
                let diagnostic = by_stage::emit::transpile_failure_diagnostic(self.module, &error);
                let diagnostic = self.in_inputs(diagnostic, start, input_file);
                self.render(&[diagnostic]);
                return Ok(None);
            }
            Err(panic) => {
                self.render(&[panic]);
                return Ok(None);
            }
        };
        let selection = match self.session.select(&python, &line_map, lines) {
            Ok(selection) => selection,
            Err(refusal) => {
                self.report_refusal(&refusal);
                return Ok(None);
            }
        };
        let reply = json!({
            "input": number,
            "python": python,
            "statements": selection.statements,
            "lines": selection.lines,
            // as python's repl runs an input, printing the value of each expression statement
            "display": true,
        });
        self.pending = Some(Pending {
            number,
            text: text.to_owned(),
            selection,
        });
        Ok(Some(reply))
    }

    /// the interpreter has started running input `number`, which is part of the session from
    /// now on, whatever it raises
    fn ran(&mut self, number: u64) -> anyhow::Result<()> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        if u64::from(pending.number) != number {
            return Ok(());
        }
        // the module the session started in is its own file to a diagnostic, not an input
        if pending.number != 0 {
            let start = TextSize::try_from(self.session.source().len())?;
            let file =
                SourceFileBuilder::new(format!("<by-input-{}>", pending.number), &*pending.text)
                    .finish();
            self.inputs.push(RanInput { start, file });
        }
        self.session
            .commit(pending.number, &pending.text, pending.selection);
        if let Some(path) = &self.session_file {
            fs::write(path, self.session.source())
                .with_context(|| format!("could not write {}", path.display()))?;
        }
        Ok(())
    }

    /// pass `event`, which the interpreter reported, on to the editor listening for it, if one is
    fn event(&mut self, event: &Value) -> anyhow::Result<()> {
        if let Some(events) = &mut self.events {
            writeln!(events, "{event}")
                .and_then(|()| events.flush())
                .context("the editor stopped listening for the session's events")?;
        }
        Ok(())
    }

    /// `diagnostic` with every place it points to in the session's module moved to the input
    /// that place is in, `input` being the one at `start` that has not run yet. a place in the
    /// module the session started in stays where it is, in that module's file
    fn in_inputs(
        &self,
        mut diagnostic: Diagnostic,
        start: TextSize,
        input: &SourceFile,
    ) -> Diagnostic {
        let module = self.module;
        let place = |range: TextRange| -> (SourceFile, TextRange) {
            let (file, at) = if range.start() >= start {
                (input.clone(), start)
            } else {
                let ran = self
                    .inputs
                    .iter()
                    .rev()
                    .find(|ran| ran.start <= range.start())
                    .or_else(|| self.inputs.first());
                match ran {
                    Some(ran) => (ran.file.clone(), ran.start),
                    None => (input.clone(), start),
                }
            };
            let length = TextSize::try_from(file.source_text().len()).unwrap_or_default();
            let moved = TextRange::new(
                (range.start() - at.min(range.start())).min(length),
                (range.end().max(at) - at).min(length),
            );
            (file, moved)
        };
        let move_span = |span: &Span| -> Option<Span> {
            match span.file() {
                UnifiedFile::Ty(file) if *file == module => {
                    let range = span.range().unwrap_or_default();
                    if range.start() < self.prelude {
                        return None;
                    }
                    let (file, range) = place(range);
                    Some(Span::from(file).with_range(range))
                }
                _ => None,
            }
        };
        for annotation in diagnostic.annotations_mut() {
            if let Some(span) = move_span(annotation.get_span()) {
                annotation.set_span(span);
            }
        }
        for sub in diagnostic.sub_diagnostics_mut() {
            for annotation in sub.annotations_mut() {
                if let Some(span) = move_span(annotation.get_span()) {
                    annotation.set_span(span);
                }
            }
        }
        diagnostic
    }

    /// say why an input's python cannot run
    #[expect(clippy::print_stderr)]
    fn report_refusal(&self, refusal: &Refusal) {
        match refusal {
            Refusal::ChangesAnEarlierInput { input, line } => eprintln!(
                "error: this input changes the python that {}, line {line} was transpiled to, \
                 which has run already and cannot run again differently",
                self.input_name(*input)
            ),
            Refusal::JoinsAnEarlierInput { input } => eprintln!(
                "error: this input is transpiled into one statement with {}, which has run \
                 already",
                self.input_name(*input)
            ),
            Refusal::NotPython(error) => {
                eprintln!("error: the python this input was transpiled to does not parse: {error}");
            }
        }
    }

    /// what input `number` is called: `<by-input-N>`, or the file of the module the session
    /// started in, which is input 0
    fn input_name(&self, number: u32) -> String {
        match &self.started_in {
            Some(started_in) if number == 0 => started_in.path.display().to_string(),
            _ => format!("<by-input-{number}>"),
        }
    }

    /// print `diagnostics` on standard error, as `by check` prints them
    #[expect(clippy::print_stderr)]
    fn render(&self, diagnostics: &[Diagnostic]) {
        let config = DisplayDiagnosticConfig::new("ty")
            .color(colored::control::SHOULD_COLORIZE.should_colorize())
            .context(0);
        let resolver = Inputs(&self.db);
        eprint!(
            "{}",
            FullRenderer::new(&resolver, &config).display(diagnostics)
        );
    }
}

/// the project's db as the diagnostics of a session read their files: the project's files
/// through it, and the inputs, which are no file of its, as the text each diagnostic carries
struct Inputs<'db>(&'db ProjectDatabase);

impl FileResolver for Inputs<'_> {
    fn path(&self, file: ruff_db::files::File) -> &str {
        FileResolver::path(self.0, file)
    }

    fn input(&self, file: ruff_db::files::File) -> Input {
        FileResolver::input(self.0, file)
    }

    fn notebook_index(&self, file: &UnifiedFile) -> Option<NotebookIndex> {
        match file {
            UnifiedFile::Ty(_) => FileResolver::notebook_index(self.0, file),
            UnifiedFile::Ruff(_) => None,
        }
    }

    fn is_notebook(&self, file: &UnifiedFile) -> bool {
        match file {
            UnifiedFile::Ty(_) => FileResolver::is_notebook(self.0, file),
            UnifiedFile::Ruff(_) => false,
        }
    }

    fn current_directory(&self) -> &Path {
        FileResolver::current_directory(self.0)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    use super::accept;

    /// a process that outlives the test, standing in for the interpreter
    fn interpreter() -> Child {
        Command::new("sleep").arg("30").spawn().expect("sleep runs")
    }

    /// a connection that sends nothing, or something other than the token, does not hold up
    /// the interpreter's, which is taken as soon as its token arrives
    #[test]
    fn accept_takes_the_interpreter_s_connection_whatever_else_connects() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listens");
        let port = listener.local_addr().expect("a port").port();
        let mut child = interpreter();

        let _silent = TcpStream::connect(("127.0.0.1", port)).expect("connects");
        let mut endless = TcpStream::connect(("127.0.0.1", port)).expect("connects");
        endless.write_all(&[b'0'; 4096]).expect("sends");
        let mut interpreter_s = TcpStream::connect(("127.0.0.1", port)).expect("connects");
        interpreter_s
            .write_all(b"secret\n{\"start\": true}\n")
            .expect("sends");

        let started = Instant::now();
        let reader = accept(&listener, "secret", &mut child)
            .expect("accepts")
            .expect("a connection");
        assert!(started.elapsed() < Duration::from_secs(5));
        // what follows the token is left for the session to read
        let mut rest = String::new();
        reader
            .into_inner()
            .take(16)
            .read_to_string(&mut rest)
            .expect("reads");
        assert_eq!(rest, "{\"start\": true}\n");
        child.kill().expect("ends");
        child.wait().expect("ends");
    }
}

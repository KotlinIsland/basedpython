//! `by repl`: basedpython typed an input at a time, run in one python process.
//!
//! The project is staged as `by run` stages it, and the interpreter runs `_by_repl.py` in that
//! tree, which reads the inputs, runs them and prints what python's repl prints. Each input is
//! a question for this process first: whether it is whole yet, and what python it runs as. The
//! session is one module whose source is the inputs that ran (see
//! `docs/basedpython/features/repl.md`), held here as a file the project's db checks and
//! transpiles like any other, and [`by_stage::session`] picks the input's statements out of what
//! the module comes to.
//!
//! The two halves talk over a socket on the loopback interface rather than over the
//! interpreter's standard streams, because those are the program's: `input()` reads standard
//! input, and the program's output is its own. The interpreter is told the port and a token in
//! its environment, and the first thing it sends is the token, so nothing else on the machine
//! that finds the port is answered.

use std::fs;
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader, Write};
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
use ruff_db::system::SystemPath;
use ruff_notebook::NotebookIndex;
use ruff_python_ast::{PySourceType, Stmt};
use ruff_python_parser::parse_unchecked_source;
use ruff_source_file::{SourceFile, SourceFileBuilder};
use ruff_text_size::{TextRange, TextSize};
use serde_json::{Value, json};
use ty_project::{Db as _, ProjectDatabase};

use crate::ExitStatus;
use crate::args::LoweringArgs;
use crate::by_commands::{BY_REPL_FILENAME, RunTarget, StagedRun, stage_run};

/// How long the interpreter has to connect once it has started, and to send its token once it
/// has connected: it does both before it runs anything of the session's.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

#[allow(clippy::exit, clippy::print_stderr)]
pub(crate) fn cmd_repl(
    min_version: Option<&str>,
    lowering: &LoweringArgs,
    python_flag: Option<&Path>,
    session_file: Option<&Path>,
) -> anyhow::Result<ExitStatus> {
    let staged = match stage_run(
        min_version,
        lowering,
        python_flag,
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
    let session_file = session_file.map(|path| cwd.join(path));
    if let Some(path) = &session_file {
        fs::write(path, "").with_context(|| format!("could not write {}", path.display()))?;
    }
    let mut repl = Repl::new(&rebuilder, config, session_file)?;

    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("could not listen for the session")?;
    let port = listener.local_addr()?.port();
    let token = token();
    let mut child = Command::new(&interpreter.path)
        .arg(tree.path().join(BY_REPL_FILENAME))
        .env("BY_REPL_PORT", port.to_string())
        .env("BY_REPL_TOKEN", &token)
        .current_dir(&cwd)
        .spawn()
        .with_context(|| {
            format!(
                "could not run `{}`, the interpreter from {}",
                interpreter.path, interpreter.origin
            )
        })?;
    // ctrl-c is the interpreter's to answer, with a `KeyboardInterrupt` in the input it is
    // running; this process lives as long as the session does
    ctrlc::set_handler(|| {}).context("could not hand ctrl-c to the session")?;

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
    std::process::exit(status.code().unwrap_or(1));
}

/// A token nothing else can guess, from the process's own random hasher keys.
fn token() -> String {
    let state = RandomState::new();
    format!(
        "{:016x}{:016x}",
        state.hash_one(std::process::id()),
        state.hash_one(Instant::now())
    )
}

/// The interpreter's connection, read through the one buffer every message after the token
/// is read through, once it has sent `token`. `None` when the interpreter ended without
/// making one.
fn accept(
    listener: &TcpListener,
    token: &str,
    child: &mut std::process::Child,
) -> anyhow::Result<Option<BufReader<TcpStream>>> {
    listener.set_nonblocking(true)?;
    let started = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
                let mut reader = BufReader::new(stream);
                let mut first = String::new();
                // a connection that is not the interpreter's is dropped, and the wait goes on
                if reader.read_line(&mut first).is_ok()
                    && serde_json::from_str::<Value>(&first)
                        .is_ok_and(|message| message["token"] == token)
                {
                    reader.get_ref().set_read_timeout(None)?;
                    return Ok(Some(reader));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if child.try_wait()?.is_some() {
                    return Ok(None);
                }
                anyhow::ensure!(
                    started.elapsed() < CONNECT_TIMEOUT,
                    "the interpreter did not connect to the session"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("could not accept the session's interpreter"),
        }
    }
}

/// The session as this process holds it: the module, the db that checks it, and what each
/// input that ran was.
struct Repl<'a> {
    db: ProjectDatabase,
    rebuilder: &'a Rebuilder,
    /// what the module is transpiled for: the project's run configuration, as a session
    config: Config,
    session: Session,
    /// the file the module is checked as, outside the project
    module: File,
    module_path: PathBuf,
    /// keeps the module's directory as long as the session
    _module_dir: tempfile::TempDir,
    /// each input that ran, for a diagnostic of a later one to point into
    inputs: Vec<RanInput>,
    /// the input handed over to run, which is part of the session once the interpreter says it
    /// has started running it
    pending: Option<Pending>,
    /// the number the next input is named by
    next: u32,
    /// `--session-file`: where an editor reads the session's source
    session_file: Option<PathBuf>,
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

impl<'a> Repl<'a> {
    fn new(
        rebuilder: &'a Rebuilder,
        config: Config,
        session_file: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let mut db = rebuilder.project_database();
        let module_dir = tempfile::TempDir::new().context("failed to create temp directory")?;
        // named for what it is in a message that cannot help naming it
        let module_path = module_dir.path().join("session.by");
        fs::write(&module_path, "")?;
        let system_path = SystemPath::from_std_path(&module_path)
            .with_context(|| format!("non-utf8 path: {}", module_path.display()))?;
        let module = system_path_to_file(&db, system_path)
            .with_context(|| format!("could not read {}", module_path.display()))?;
        // outside the project, so checked only as a file the project has open
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
            module_path,
            _module_dir: module_dir,
            inputs: Vec::new(),
            pending: None,
            next: 1,
            session_file,
        })
    }

    /// Answer the interpreter until it hangs up.
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
            } else if let Some(text) = message["run"].as_str() {
                Some(self.run(text)?)
            } else if let Some(number) = message["ran"].as_u64() {
                self.ran(number)?;
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

    /// What `text` runs as, or what is in the way of it running, said on standard error.
    fn run(&mut self, text: &str) -> anyhow::Result<Value> {
        let number = self.next;
        self.next += 1;
        self.pending = None;
        let refused = json!({ "input": number });
        let input_file = SourceFileBuilder::new(format!("<by-input-{number}>"), text).finish();

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
            return Ok(refused);
        }
        let display = matches!(parsed.suite().last(), Some(Stmt::Expr(_)));

        let (source, lines) = self.session.with_input(text);
        fs::write(&self.module_path, &source)
            .with_context(|| format!("could not write {}", self.module_path.display()))?;
        File::sync_path(
            &mut self.db,
            SystemPath::from_std_path(&self.module_path).context("non-utf8 path")?,
        );
        let start = TextSize::try_from(self.session.source().len())?;
        let errors: Vec<Diagnostic> =
            ty_project::check_file_ignoring_check_mode(&self.db, self.module)
                .into_iter()
                .filter(|diagnostic| diagnostic.severity() >= Severity::Error)
                .map(|diagnostic| self.in_inputs(diagnostic, start, &input_file))
                .collect();
        if !errors.is_empty() {
            self.render(&errors);
            return Ok(refused);
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
            Ok(Ok((python, line_map, _))) => (python, line_map),
            Ok(Err(error)) => {
                let diagnostic = by_stage::emit::transpile_failure_diagnostic(self.module, &error);
                let diagnostic = self.in_inputs(diagnostic, start, &input_file);
                self.render(&[diagnostic]);
                return Ok(refused);
            }
            Err(panic) => {
                self.render(&[panic]);
                return Ok(refused);
            }
        };
        let selection = match self.session.select(&python, &line_map, lines) {
            Ok(selection) => selection,
            Err(refusal) => {
                report_refusal(&refusal);
                return Ok(refused);
            }
        };
        let reply = json!({
            "input": number,
            "python": python,
            "statements": selection.statements,
            "lines": selection.lines,
            "display": display,
        });
        self.pending = Some(Pending {
            number,
            text: text.to_owned(),
            selection,
        });
        Ok(reply)
    }

    /// The interpreter has started running input `number`, which is part of the session from
    /// now on, whatever it raises.
    fn ran(&mut self, number: u64) -> anyhow::Result<()> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        if u64::from(pending.number) != number {
            return Ok(());
        }
        let start = TextSize::try_from(self.session.source().len())?;
        let file = SourceFileBuilder::new(format!("<by-input-{}>", pending.number), &*pending.text)
            .finish();
        self.inputs.push(RanInput { start, file });
        self.session
            .commit(pending.number, &pending.text, pending.selection);
        if let Some(path) = &self.session_file {
            fs::write(path, self.session.source())
                .with_context(|| format!("could not write {}", path.display()))?;
        }
        Ok(())
    }

    /// `diagnostic` with every place it points to in the session's module moved to the input
    /// that place is in, `input` being the one at `start` that has not run yet.
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

    /// Print `diagnostics` on standard error, as `by check` prints them.
    #[allow(clippy::print_stderr)]
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

/// Say why an input's python cannot run.
#[allow(clippy::print_stderr)]
fn report_refusal(refusal: &Refusal) {
    match refusal {
        Refusal::ChangesAnEarlierInput { input, line } => eprintln!(
            "error: this input changes the python that <by-input-{input}>, line {line} was \
             transpiled to, which has run already and cannot run again differently"
        ),
        Refusal::JoinsAnEarlierInput { input } => eprintln!(
            "error: this input is transpiled into one statement with <by-input-{input}>, \
             which has run already"
        ),
        Refusal::NotPython(error) => {
            eprintln!("error: the python this input was transpiled to does not parse: {error}");
        }
    }
}

/// The project's db as the diagnostics of a session read their files: the project's files
/// through it, and the inputs, which are no file of its, as the text each diagnostic carries.
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

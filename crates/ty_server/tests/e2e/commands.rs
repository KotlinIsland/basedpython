use anyhow::{Context, Result};
use lsp_types::{
    ExecuteCommandParams, ExecuteCommandRequest, LogMessageNotification,
    PublishDiagnosticsNotification, WorkDoneProgressParams,
};
use ruff_db::system::SystemPath;
use std::time::Duration;

use crate::{TestServer, TestServerBuilder};

// Sends an executeCommand request to the TestServer
fn execute_command(
    server: &mut TestServer,
    command: String,
    arguments: Vec<serde_json::Value>,
) -> Option<serde_json::Value> {
    let params = ExecuteCommandParams {
        command,
        arguments: Some(arguments),
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    let id = server.send_request::<ExecuteCommandRequest>(params);
    server.await_response::<ExecuteCommandRequest>(&id)
}

#[test]
fn debug_command() -> Result<()> {
    let workspace_root = SystemPath::new("src");
    let foo = SystemPath::new("src/foo.py");
    let ty_toml = SystemPath::new("ty.toml");
    let foo_content = "\
def foo() -> str:
return 42
";
    let ty_toml_content = "\
[environment]
python-version = \"3.10\"
python-platform = \"linux\"
";

    let mut server = TestServerBuilder::new()?
        .with_workspace(workspace_root, None)?
        .with_file(foo, foo_content)?
        .with_file(ty_toml, ty_toml_content)?
        .enable_pull_diagnostics(false)
        .build()
        .wait_until_workspaces_are_initialized();

    let response = execute_command(&mut server, "ty.printDebugInformation".to_string(), vec![]);
    let response = response.expect("expect server response");

    let response = response
        .as_str()
        .expect("debug command to return a string response");

    let (before_structs, salsa_structs) = response
        .split_once("=======SALSA STRUCTS=======\n")
        .context("debug response missing Salsa structs section")?;
    let (salsa_structs, salsa_queries) = salsa_structs
        .split_once("=======SALSA QUERIES=======\n")
        .context("debug response missing Salsa queries section")?;
    let (salsa_queries, summary) = salsa_queries
        .split_once("=======SALSA SUMMARY=======\n")
        .context("debug response missing Salsa summary section")?;

    // Memory usage varies between platforms and build profiles. Sort entries by name instead.
    let mut salsa_structs = salsa_structs.lines().collect::<Vec<_>>();
    salsa_structs.sort_unstable();
    let query_lines = salsa_queries.lines().collect::<Vec<_>>();
    let mut salsa_queries = query_lines
        .chunks(2)
        .map(|query| query.join("\n"))
        .collect::<Vec<_>>();
    salsa_queries.sort_unstable();
    let response = format!(
        "{before_structs}=======SALSA STRUCTS=======\n{}\n=======SALSA QUERIES=======\n{}\n=======SALSA SUMMARY=======\n{summary}",
        salsa_structs.join("\n"),
        salsa_queries.join("\n")
    );

    let mut settings = insta::Settings::clone_current();
    settings.add_filter(r"\b[0-9]+.[0-9]+MB\b", "[X.XXMB]");
    settings.add_filter(r"Workspace .+\)", "Workspace XXX");
    settings.add_filter(r"Project at .+", "Project at XXX");
    settings.add_filter(r"(?m)^(\s+).*/site-packages,$", "$1<site-packages>,");
    settings.add_filter(r"rules: \{(.|\n)+?\}\,", "rules: <RULES>,");
    let _settings = settings.bind_to_scope();

    insta::assert_snapshot!(response);

    Ok(())
}

/// What the server says next about a command it is running.
///
/// The wait is generous because spawning a process from a server this size can
/// take seconds on a loaded machine: what is under test is what uv was told to
/// do, not how quickly the machine got round to doing it.
#[cfg(unix)]
fn awaited_report(server: &mut TestServer) -> String {
    server
        .try_await_notification::<LogMessageNotification>(Some(Duration::from_mins(1)))
        .expect("the server to report on the command it is running")
        .message
}

/// A stand-in for uv that records how it was called.
///
/// What matters about the command is the command line and the directory it runs
/// in, and those are what this writes down. `pwd -P` rather than `$PWD`, which a
/// child inherits from whoever spawned it and so says nothing about where the
/// child actually is.
#[cfg(unix)]
const UV_STUB: &str = "\
#!/bin/sh
{ pwd -P; printf '%s\\n' \"$@\"; } > \"$(dirname \"$0\")/recorded\"
";

#[cfg(unix)]
#[test]
fn adding_a_dependency_runs_uv_in_the_project() -> Result<()> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let builder = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(
            SystemPath::new("src/pyproject.toml"),
            "[project]\nname = \"mine\"\ndependencies = []\n",
        )?
        .with_file(SystemPath::new("uv"), UV_STUB)?;

    let uv = builder.file_path(SystemPath::new("uv"));
    fs::set_permissions(uv.as_std_path(), fs::Permissions::from_mode(0o755))?;

    let root = builder.file_path(SystemPath::new("src"));
    let recorded = builder.file_path(SystemPath::new("recorded"));

    let mut server = builder
        .with_env_var("UV", uv.to_string())
        .build()
        .wait_until_workspaces_are_initialized();

    execute_command(
        &mut server,
        "ty.addDependency".to_string(),
        vec![serde_json::json!({
            "root": root.to_string(),
            "distribution": "numpy",
            "group": "dev",
        })],
    );

    // uv is run on a thread of its own, so that a long install doesn't stop the
    // server answering anything else in the meantime. what it is doing is
    // reported as it goes, and the second of those reports is it finishing
    assert_eq!(
        awaited_report(&mut server),
        "running uv add --group dev numpy"
    );
    assert_eq!(
        awaited_report(&mut server),
        "uv add --group dev numpy succeeded"
    );
    assert_eq!(
        fs::read_to_string(recorded.as_std_path())?,
        format!("{root}\nadd\n--group\ndev\nnumpy\n")
    );

    Ok(())
}

#[test]
fn adding_a_dependency_to_a_directory_that_is_no_project_is_refused() -> Result<()> {
    let mut server = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(SystemPath::new("src/foo.py"), "")?
        .with_env_var("UV", "/uv")
        .build()
        .wait_until_workspaces_are_initialized();

    let id = server.send_request::<ExecuteCommandRequest>(ExecuteCommandParams {
        command: "ty.addDependency".to_string(),
        arguments: Some(vec![serde_json::json!({
            "root": "/somewhere/else",
            "distribution": "numpy",
        })]),
        work_done_progress_params: WorkDoneProgressParams::default(),
    });

    let failure = server
        .try_await_response::<ExecuteCommandRequest>(&id, None)
        .expect_err("a refusal rather than uv run wherever the client asked");

    assert!(
        format!("{failure}").contains("is not a project of ty's"),
        "got {failure}"
    );

    Ok(())
}

/// A stand-in for uv that installs something rather than recording anything.
///
/// A module written where the project can import it is what an install comes down
/// to here, and it is written outside the editor, which is the point: nothing told
/// the server it appeared.
#[cfg(unix)]
const UV_INSTALLING_STUB: &str = "\
#!/bin/sh
echo 'x: int = 1' > \"$(dirname \"$0\")/src/numpy.py\"
";

#[cfg(unix)]
#[test]
fn what_uv_installed_is_picked_up_when_it_finishes() -> Result<()> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let foo = SystemPath::new("src/foo.py");
    let foo_content = "import numpy\n";

    let builder = TestServerBuilder::new()?
        .with_workspace(SystemPath::new("src"), None)?
        .with_file(
            SystemPath::new("src/pyproject.toml"),
            "[project]\nname = \"mine\"\ndependencies = []\n",
        )?
        .with_file(foo, foo_content)?
        .with_file(SystemPath::new("uv"), UV_INSTALLING_STUB)?;

    let uv = builder.file_path(SystemPath::new("uv"));
    fs::set_permissions(uv.as_std_path(), fs::Permissions::from_mode(0o755))?;

    let root = builder.file_path(SystemPath::new("src"));

    // the server pushes diagnostics rather than waiting to be asked for them, so
    // what it publishes after the install is the answer to whether it noticed —
    // asking for them instead would be a question racing the rescan
    let mut server = builder
        .with_env_var("UV", uv.to_string())
        .enable_pull_diagnostics(false)
        .build()
        .wait_until_workspaces_are_initialized();

    server.open_text_document(foo, foo_content, 1);

    let before = server.await_notification::<PublishDiagnosticsNotification>();
    assert_eq!(before.diagnostics.len(), 1, "got {before:#?}");

    execute_command(
        &mut server,
        "ty.addDependency".to_string(),
        vec![serde_json::json!({
            "root": root.to_string(),
            "distribution": "numpy",
        })],
    );

    awaited_report(&mut server);
    awaited_report(&mut server);

    let after = server
        .try_await_notification::<PublishDiagnosticsNotification>(Some(Duration::from_mins(1)))
        .expect("the server to publish what it makes of the file now");
    assert!(after.diagnostics.is_empty(), "got {after:#?}");

    Ok(())
}

/// A stand-in for `by`, and for python, that records how it was called and then says what
/// `manage.py makemigrations` says.
///
/// As `by`, it writes the notes `by run --notes` writes: what it wrote back to the project, and
/// one file it did not.
#[cfg(unix)]
const MANAGE_STUB: &str = r#"#!/bin/sh
{ pwd -P; printf '%s\n' "$@"; } > "$(dirname "$0")/recorded"
notes=
while [ $# -gt 0 ]; do
    if [ "$1" = --notes ]; then notes=$2; fi
    shift
done
echo "Migrations for 'app':"
echo "  app/migrations/0001_initial.py"
if [ -n "$notes" ]; then
    printf '%s\n' \
        '{"level":"note","message":"wrote back app/migrations/0001_initial.py"}' \
        '{"level":"warning","message":"not written back: app/admin.py was changed in the project while the program ran"}' \
        > "$notes"
fi
"#;

/// A django project whose models module is `models`, with an interpreter the editor chose, as
/// the python extension chooses one: an environment the server finds only because it was told.
/// `by` and the interpreter are both [`MANAGE_STUB`], recording into `env/bin/recorded` and
/// `recorded` beside `by`
#[cfg(unix)]
fn manage_project(models: &str) -> Result<TestServerBuilder> {
    manage_project_in(SystemPath::new("src"), models)
}

/// [`manage_project`], with `manage.py` and the django project in `site`, a directory of the
/// workspace `src`
#[cfg(unix)]
fn manage_project_in(site: &SystemPath, models: &str) -> Result<TestServerBuilder> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let builder = TestServerBuilder::new()?;
    let python = builder.file_path(SystemPath::new("env/bin/python3"));
    let options: ty_server::ClientOptions = serde_json::from_value(serde_json::json!({
        "pythonExtension": {
            "activeEnvironment": {
                "executable": {
                    "uri": builder.file_uri(SystemPath::new("env/bin/python3")),
                    "sysPrefix": builder.file_path(SystemPath::new("env")),
                }
            }
        }
    }))?;

    let builder = builder
        .with_workspace(SystemPath::new("src"), Some(options))?
        .with_file(
            site.join("manage.py"),
            "import os\n\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.settings')\n",
        )?
        .with_file(site.join("project/__init__.py"), "")?
        .with_file(
            site.join("project/settings.py"),
            "INSTALLED_APPS = ['app']\n",
        )?
        .with_file(site.join("app/__init__.py"), "")?
        .with_file(site.join("app").join(models), "class Widget:\n    pass\n")?
        .with_file(SystemPath::new("env/bin/python3"), MANAGE_STUB)?
        .with_file(
            SystemPath::new("env/lib/python3.13/site-packages/installed.py"),
            "",
        )?
        .with_file(SystemPath::new("by/by"), MANAGE_STUB)?;

    let by = builder.file_path(SystemPath::new("by/by"));
    for executable in [&python, &by] {
        fs::set_permissions(executable.as_std_path(), fs::Permissions::from_mode(0o755))?;
    }

    Ok(builder.with_by_executable(&by))
}

/// what the server showed once the command it ran had finished, with everything it logged
#[cfg(unix)]
fn shown_outcome(server: &mut TestServer) -> (lsp_types::ShowMessageParams, Vec<String>) {
    let shown = server
        .try_await_notification::<lsp_types::ShowMessageNotification>(Some(Duration::from_mins(1)))
        .expect("the server to show what came of the command");
    let mut logged = Vec::new();
    while let Ok(log) =
        server.try_await_notification::<LogMessageNotification>(Some(Duration::from_millis(10)))
    {
        logged.push(log.message);
    }
    (shown, logged)
}

#[cfg(unix)]
fn run_manage_command(server: &mut TestServer, arguments: &[&str]) -> lsp_server::RequestId {
    server.send_request::<ExecuteCommandRequest>(ExecuteCommandParams {
        command: "ty.runManageCommand".to_string(),
        arguments: Some(vec![serde_json::json!({ "arguments": arguments })]),
        work_done_progress_params: WorkDoneProgressParams::default(),
    })
}

/// python never imports a `.by` module, so in a project with a `.by` model a management
/// command runs through `by run`, on the interpreter the project is checked against, from the
/// directory `manage.py` is in. what `by run` wrote back is logged, and the outcome shown is
/// the command's own last line, with what was not written back after it
#[cfg(unix)]
#[test]
fn a_management_command_of_a_project_with_by_sources_runs_through_by_run() -> Result<()> {
    let builder = manage_project("models.by")?;
    let root = builder.file_path(SystemPath::new("src"));
    let python = builder.file_path(SystemPath::new("env/bin/python3"));
    let recorded = builder.file_path(SystemPath::new("by/recorded"));
    let mut server = builder.build().wait_until_workspaces_are_initialized();

    let id = run_manage_command(&mut server, &["makemigrations", "app"]);
    assert_eq!(server.await_response::<ExecuteCommandRequest>(&id), None);

    let (shown, logged) = shown_outcome(&mut server);
    assert_eq!(
        shown.message,
        "by run manage.py makemigrations app succeeded: app/migrations/0001_initial.py; \
         warning: not written back: app/admin.py was changed in the project while the program ran"
    );
    assert_eq!(shown.kind, lsp_types::MessageType::Warning);
    assert_eq!(
        logged,
        [
            "running by run manage.py makemigrations app",
            "Migrations for 'app':\n  app/migrations/0001_initial.py",
            "note: wrote back app/migrations/0001_initial.py",
            "warning: not written back: app/admin.py was changed in the project while the program ran",
            "by run manage.py makemigrations app succeeded",
        ]
    );

    let recorded = std::fs::read_to_string(recorded.as_std_path())?;
    let recorded: Vec<&str> = recorded.lines().collect();
    let [
        directory,
        "run",
        "--python",
        interpreter,
        "--notes",
        notes,
        manage,
        arguments @ ..,
    ] = recorded.as_slice()
    else {
        panic!("not a `by run` of the interpreter: {recorded:?}");
    };
    assert_eq!(*directory, root.as_str());
    assert_eq!(*interpreter, python.as_str());
    assert!(notes.ends_with("notes.jsonl"), "{notes}");
    assert_eq!(*manage, root.join("manage.py").as_str());
    assert_eq!(arguments, ["makemigrations", "app"]);

    Ok(())
}

/// `by run` refuses a project with no `.by` source, which python runs as it is
#[cfg(unix)]
#[test]
fn a_management_command_of_a_project_without_by_sources_runs_on_its_interpreter() -> Result<()> {
    let builder = manage_project("models.py")?;
    let root = builder.file_path(SystemPath::new("src"));
    let recorded = builder.file_path(SystemPath::new("env/bin/recorded"));
    let mut server = builder.build().wait_until_workspaces_are_initialized();

    let id = run_manage_command(&mut server, &["makemigrations", "app"]);
    assert_eq!(server.await_response::<ExecuteCommandRequest>(&id), None);

    let (shown, _) = shown_outcome(&mut server);
    assert_eq!(
        shown.message,
        "manage.py makemigrations app succeeded: app/migrations/0001_initial.py"
    );
    assert_eq!(shown.kind, lsp_types::MessageType::Info);
    assert_eq!(
        std::fs::read_to_string(recorded.as_std_path())?,
        format!("{root}\n{root}/manage.py\nmakemigrations\napp\n")
    );

    Ok(())
}

/// `by run` finds its project from the directory it is started in, `manage.py`'s, so a
/// `pyproject.toml` there makes the django project one of its own, whatever the workspace
/// around it holds: with no `.by` source of its own, `by run` would refuse it, and python runs it
#[cfg(unix)]
#[test]
fn a_management_command_of_a_nested_project_without_by_sources_runs_on_its_interpreter()
-> Result<()> {
    let builder = manage_project_in(SystemPath::new("src/site"), "models.py")?
        .with_file(
            SystemPath::new("src/pyproject.toml"),
            "[project]\nname = \"outer\"\n",
        )?
        .with_file(SystemPath::new("src/outer.by"), "x = 1\n")?
        .with_file(
            SystemPath::new("src/site/pyproject.toml"),
            "[project]\nname = \"site\"\n",
        )?;
    let site = builder.file_path(SystemPath::new("src/site"));
    let recorded = builder.file_path(SystemPath::new("env/bin/recorded"));
    let mut server = builder.build().wait_until_workspaces_are_initialized();

    let id = run_manage_command(&mut server, &["makemigrations", "app"]);
    assert_eq!(server.await_response::<ExecuteCommandRequest>(&id), None);

    let (shown, _) = shown_outcome(&mut server);
    assert_eq!(
        shown.message,
        "manage.py makemigrations app succeeded: app/migrations/0001_initial.py"
    );
    assert_eq!(
        std::fs::read_to_string(recorded.as_std_path())?,
        format!("{site}\n{site}/manage.py\nmakemigrations\napp\n")
    );

    Ok(())
}

/// a refusal hands back the command that runs it, which in a project with a `.by` model is
/// `by run`'s
#[cfg(unix)]
#[test]
fn a_refused_management_command_of_a_project_with_by_sources_hands_back_by_run() -> Result<()> {
    let mut server = manage_project("models.by")?
        .build()
        .wait_until_workspaces_are_initialized();

    let id = run_manage_command(&mut server, &["runserver", "8001"]);
    let failure = server
        .try_await_response::<ExecuteCommandRequest>(&id, None)
        .expect_err("a refusal rather than a process nothing could stop");

    assert!(
        format!("{failure}").contains("Run it yourself: `by run manage.py runserver 8001`"),
        "got {failure}"
    );
    let shown = server.await_notification::<lsp_types::ShowMessageNotification>();
    assert!(
        shown
            .message
            .contains("Run it yourself: `by run manage.py runserver 8001`"),
        "got {shown:?}"
    );

    Ok(())
}

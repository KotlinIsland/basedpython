//! `by check`'s side of the [project server](../../../src/by_project_server.rs).
//!
//! What a server would say, and whether it is asked at all, is covered against a real server
//! in `ty_server`'s end-to-end tests. What is left here is the command line's own half: that
//! the switch exists, that it does not change the answer, and that these tests are not
//! quietly being answered by whatever server the person running them has open.

use anyhow::Result;
use insta_cmd::assert_cmd_snapshot;

use crate::CliTest;

/// The answer is the answer either way. `--no-server` chooses how it is arrived at, and a
/// flag that changed what came out would be a different command rather than a faster one.
#[test]
fn no_server_does_not_change_the_answer() -> Result<()> {
    let case = CliTest::with_file(
        "main.py",
        r#"
def f() -> str:
    return 42
"#,
    )?;

    let with_server = case.command().output()?;
    assert_cmd_snapshot!(case.command().arg("--no-server"), @r"
    success: false
    exit_code: 1
    ----- stdout -----
    error[invalid-return-type]: Return type does not match returned value
     --> main.py:3:12
      |
    2 | def f() -> str:
      |            --- Expected `str` because of return type
    3 |     return 42
      |            ^^ expected `str`, found `Literal[42]`

    Found 1 diagnostic

    ----- stderr -----
    ");

    assert_eq!(
        String::from_utf8(with_server.stdout)?,
        String::from_utf8(case.command().arg("--no-server").output()?.stdout)?
    );

    Ok(())
}

/// These tests assert on exact output, so none of them may be answered out of a language
/// server that happens to be running on this machine. `CliTest` sets the kill switch for
/// every command it builds; this is the assertion that it still does.
#[test]
fn the_tests_never_ask_a_server() -> Result<()> {
    let case = CliTest::with_file("main.py", "x: int = 1\n")?;

    let output = case.command().output()?;
    assert!(
        !String::from_utf8(output.stderr)?.contains("using project server information"),
        "a cli test was answered by a project server"
    );

    Ok(())
}

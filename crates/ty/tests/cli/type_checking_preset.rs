use insta_cmd::assert_cmd_snapshot;

use crate::CliTest;

/// A basedpython-only rule is enabled by default, and off under `ty-compatible`
#[test]
fn ty_compatible_disables_basedpython_rules() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "test.py",
        r#"
            a: int = True
            "#,
    )?;

    assert_cmd_snapshot!(case.command(), @"
    success: false
    exit_code: 1
    ----- stdout -----
    warning[bool-as-int]: `Literal[True]` is implicitly used as `int`
     --> test.py:2:10
      |
    2 | a: int = True
      |          ^^^^
    help: Write `int(...)` if the number is meant, or annotate `bool` if the flag is

    Found 1 diagnostic

    ----- stderr -----
    ");

    assert_cmd_snapshot!(case.command().arg("--type-checking-preset").arg("ty-compatible"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    All checks passed!

    ----- stderr -----
    ");

    Ok(())
}

/// A basedpython analysis option is on by default, and off under `ty-compatible`
#[test]
fn ty_compatible_disables_basedpython_analysis() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "test.py",
        r#"
            from typing import reveal_type

            def f[T]() -> T:
                raise NotImplementedError

            reveal_type(f())
            "#,
    )?;

    assert_cmd_snapshot!(case.command(), @"
    success: true
    exit_code: 0
    ----- stdout -----
    info[revealed-type]: Revealed type
     --> test.py:7:13
      |
    7 | reveal_type(f())
      |             ^^^ `Never`

    Found 1 diagnostic

    ----- stderr -----
    ");

    assert_cmd_snapshot!(case.command().arg("--type-checking-preset").arg("ty-compatible"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    info[revealed-type]: Revealed type
     --> test.py:7:13
      |
    7 | reveal_type(f())
      |             ^^^ `Unknown`

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

/// The `analysis` table still wins over the preset it started from
#[test]
fn analysis_beats_the_preset() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r#"
            [tool.ty]
            type-checking-preset = "ty-compatible"

            [tool.ty.analysis]
            precise-unsolved-typevars = true
            "#,
        ),
        (
            "test.py",
            r#"
            from typing import reveal_type

            def f[T]() -> T:
                raise NotImplementedError

            reveal_type(f())
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: true
    exit_code: 0
    ----- stdout -----
    info[revealed-type]: Revealed type
     --> test.py:7:13
      |
    7 | reveal_type(f())
      |             ^^^ `Never`

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

/// A rule the preset leaves out can't be enabled, and naming it is reported
#[test]
fn ty_compatible_rejects_a_basedpython_rule() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r#"
            [tool.ty]
            type-checking-preset = "ty-compatible"

            [tool.ty.rules]
            bool-as-int = "error"
            "#,
        ),
        (
            "test.py",
            r#"
            a: int = True
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: false
    exit_code: 1
    ----- stdout -----
    warning[unknown-rule]: Rule `bool-as-int` is a basedpython rule, which the `ty-compatible` type checking preset does not include
     --> pyproject.toml:6:1
      |
    6 | bool-as-int = "error"
      | ^^^^^^^^^^^

    Found 1 diagnostic

    ----- stderr -----
    "#);

    Ok(())
}

/// `rules = { all = ... }` does not resurrect a rule the preset leaves out
#[test]
fn ty_compatible_ignores_all_selector_for_basedpython_rules() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r#"
            [tool.ty]
            type-checking-preset = "ty-compatible"

            [tool.ty.rules]
            all = "error"
            "#,
        ),
        (
            "test.py",
            r#"
            a: int = True
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: true
    exit_code: 0
    ----- stdout -----
    All checks passed!

    ----- stderr -----
    ");

    Ok(())
}

/// A diagnostic ty ships disabled is on under the default preset, and off again under
/// `ty-compatible`
#[test]
fn ty_compatible_restores_tys_disabled_rules() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "test.py",
        // no `-> None`: a `def` that leaves it out already returns `None`, and writing it
        // would draw `redundant-return-annotation` into a snapshot about another rule
        r#"
            def f(flag: bool):
                if flag:
                    x = 1
                print(x)
            "#,
    )?;

    assert_cmd_snapshot!(case.command(), @"
    success: false
    exit_code: 1
    ----- stdout -----
    error[possibly-unresolved-reference]: Name `x` used when possibly not defined
     --> test.py:5:11
      |
    5 |     print(x)
      |           ^

    Found 1 diagnostic

    ----- stderr -----
    ");

    assert_cmd_snapshot!(case.command().arg("--type-checking-preset").arg("ty-compatible"), @"
    success: true
    exit_code: 0
    ----- stdout -----
    All checks passed!

    ----- stderr -----
    ");

    Ok(())
}

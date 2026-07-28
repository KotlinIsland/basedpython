use insta_cmd::assert_cmd_snapshot;

use crate::CliTest;

/// ty ignores `type: ignore` comments when setting `respect-type-ignore-comments=false`
#[test]
fn respect_type_ignore_comments_is_turned_off() -> anyhow::Result<()> {
    let case = CliTest::with_file(
        "test.py",
        r#"
            y = a + 5  # type: ignore
            "#,
    )?;

    // Assert that there's an `unresolved-reference` diagnostic (error).
    assert_cmd_snapshot!(case.command(), @"
    success: true
    exit_code: 0
    ----- stdout -----
    All checks passed!

    ----- stderr -----
    ");

    assert_cmd_snapshot!(case.command().arg("--config").arg("analysis.respect-type-ignore-comments=false"), @"
    success: false
    exit_code: 1
    ----- stdout -----
    error[unresolved-reference]: Name `a` used when not defined
     --> test.py:2:5
      |
    2 | y = a + 5  # type: ignore
      |     ^
      |

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

/// Basic override functionality: override analysis options for a specific file
#[test]
fn overrides_basic() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r#"
            [tool.ty.analysis]
            respect-type-ignore-comments = true

            [[tool.ty.overrides]]
            include = ["tests/**"]

            [tool.ty.overrides.analysis]
            respect-type-ignore-comments = false
            "#,
        ),
        (
            "main.py",
            r#"
            print(x)  # type: ignore  # ignore respected (global)
            "#,
        ),
        (
            "tests/test_main.py",
            r#"
            print(x)  # type: ignore  # ignore not-respected (override)
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: false
    exit_code: 1
    ----- stdout -----
    error[unresolved-reference]: Name `x` used when not defined
     --> tests/test_main.py:2:7
      |
    2 | print(x)  # type: ignore  # ignore not-respected (override)
      |       ^
      |

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

/// Multiple overrides: later overrides take precedence
#[test]
fn overrides_precedence() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r#"
            [tool.ty.analysis]
            respect-type-ignore-comments = true

            # First override: all test files
            [[tool.ty.overrides]]
            include = ["tests/**"]
            [tool.ty.overrides.analysis]
            respect-type-ignore-comments = false

            # Second override: specific test file (takes precedence)
            [[tool.ty.overrides]]
            include = ["tests/important.py"]
            [tool.ty.overrides.analysis]
            respect-type-ignore-comments = true
            "#,
        ),
        (
            "tests/test_main.py",
            r#"
            print(y)  # type: ignore (should be an error, because type ignores are disabled)
            "#,
        ),
        (
            "tests/important.py",
            r#"
            print(y)  # type: ignore (no error, because type ignores are enabled)
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: false
    exit_code: 1
    ----- stdout -----
    error[unresolved-reference]: Name `y` used when not defined
     --> tests/test_main.py:2:7
      |
    2 | print(y)  # type: ignore (should be an error, because type ignores are disabled)
      |       ^
      |

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

/// Override without analysis options inherit the global analysis options
#[test]
fn overrides_inherit_global() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r#"
            [tool.ty.analysis]
            respect-type-ignore-comments = false

            [[tool.ty.overrides]]
            include = ["tests/**"]

            [tool.ty.overrides.rules]
            division-by-zero = "warn"

            [tool.ty.overrides.analysis]
            "#,
        ),
        (
            "main.py",
            r#"
            print(y)  # type: ignore ignore not-respected (global)
            "#,
        ),
        (
            "tests/test_main.py",
            r#"
            print(y)  # type: ignore ignore respected (inherited from global)
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: false
    exit_code: 1
    ----- stdout -----
    error[unresolved-reference]: Name `y` used when not defined
     --> main.py:2:7
      |
    2 | print(y)  # type: ignore ignore not-respected (global)
      |       ^
      |

    error[unresolved-reference]: Name `y` used when not defined
     --> tests/test_main.py:2:7
      |
    2 | print(y)  # type: ignore ignore respected (inherited from global)
      |       ^
      |

    Found 2 diagnostics

    ----- stderr -----
    ");

    Ok(())
}

/// `sound-types` is resolved per module: the module that *declares* a construct governs how its
/// types are inferred, and consumers see the result regardless of their own setting.
#[test]
fn sound_types_is_per_module() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r#"
            [tool.ty.environment]
            python-version = "3.13"

            [[tool.ty.overrides]]
            include = ["sound/**"]

            [tool.ty.overrides.analysis]
            sound-types = true
            "#,
        ),
        (
            "sound/lib.py",
            r#"
            def f(a=1) -> None: ...
            "#,
        ),
        (
            "gradual/lib.py",
            r#"
            def g(a=1) -> None: ...
            "#,
        ),
        (
            "gradual/main.py",
            r#"
            from sound.lib import f

            # `f` is declared in a sound module, so its signature is precise even here
            f("wrong")
            "#,
        ),
        (
            "sound/main.py",
            r#"
            from gradual.lib import g

            # `g` is declared in a gradual module, so its parameter stays gradual even here
            g("fine")
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: false
    exit_code: 1
    ----- stdout -----
    error[invalid-argument-type]: Argument to function `f` is incorrect
     --> gradual/main.py:5:3
      |
    5 | f("wrong")
      |   ^^^^^^^ Expected `int`, found `Literal["wrong"]`
      |
    info: Function defined here
     --> sound/lib.py:2:5
      |
    2 | def f(a=1) -> None: ...
      |     ^ --- Parameter declared here
      |

    Found 1 diagnostic

    ----- stderr -----
    "#);

    Ok(())
}

/// `bivariant-private-attributes` is resolved per module: the module that *declares* a class
/// governs how its variance is inferred, and consumers see the result regardless of their own
/// setting.
#[test]
fn bivariant_private_attributes_is_per_module() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r#"
            [tool.ty.environment]
            python-version = "3.13"

            [tool.ty.analysis]
            bivariant-private-attributes = false

            [[tool.ty.overrides]]
            include = ["bivariant/**"]

            [tool.ty.overrides.analysis]
            bivariant-private-attributes = true
            "#,
        ),
        (
            "bivariant/lib.py",
            r#"
            class Bivariant[T]:
                _x: T
            "#,
        ),
        (
            "covariant/lib.py",
            r#"
            class Covariant[T]:
                _x: T
            "#,
        ),
        (
            "covariant/main.py",
            r#"
            from bivariant.lib import Bivariant

            class A: ...
            class B(A): ...

            # `Bivariant` is declared in a bivariant module, so it is bivariant even here
            widened: Bivariant[A] = Bivariant[B]()
            narrowed: Bivariant[B] = Bivariant[A]()
            "#,
        ),
        (
            "bivariant/main.py",
            r#"
            from covariant.lib import Covariant

            class A: ...
            class B(A): ...

            # `Covariant` is declared in a covariant module, so it stays covariant even here
            widened: Covariant[A] = Covariant[B]()
            narrowed: Covariant[B] = Covariant[A]()
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: false
    exit_code: 1
    ----- stdout -----
    error[invalid-assignment]: Object of type `Covariant[A]` is not assignable to `Covariant[B]`
     --> bivariant/main.py:9:11
      |
    9 | narrowed: Covariant[B] = Covariant[A]()
      |           ------------   ^^^^^^^^^^^^^^ Incompatible value of type `Covariant[A]`
      |           |
      |           Declared type
      |

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

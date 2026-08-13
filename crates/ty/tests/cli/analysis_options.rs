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

    error[unresolved-reference]: Name `y` used when not defined
     --> tests/test_main.py:2:7
      |
    2 | print(y)  # type: ignore ignore respected (inherited from global)
      |       ^

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

            # a gradual module has to opt out of signature recovery too, or its own unannotated
            # parameters would be precise whatever `sound-types` says
            [tool.ty.analysis]
            infer-unannotated-signatures = false

            [[tool.ty.overrides]]
            include = ["sound/**"]

            [tool.ty.overrides.analysis]
            sound-types = true
            "#,
        ),
        (
            // no `-> None`: a sound module already returns `None` without it, and writing it
            // would draw `redundant-return-annotation` into a snapshot about parameters
            "sound/lib.py",
            r#"
            def f(a=1): ...
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
      |   ^^^^^^^ Argument type `Literal["wrong"]` does not satisfy `int`, inferred for parameter `a`
    info: Parameter declared here
     --> sound/lib.py:2:7
      |
    2 | def f(a=1): ...
      |       ^

    Found 1 diagnostic

    ----- stderr -----
    "#);

    Ok(())
}

/// `precise-unsolved-typevars` is resolved per module: the module that *declares* a function
/// governs how a call leaving its type variables unsolved is solved, and callers see the result
/// regardless of their own setting.
#[test]
fn precise_unsolved_typevars_is_per_module() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r#"
            [tool.ty.environment]
            python-version = "3.13"

            [[tool.ty.overrides]]
            include = ["gradual/**"]

            [tool.ty.overrides.analysis]
            precise-unsolved-typevars = false
            "#,
        ),
        (
            "precise/lib.py",
            r#"
            def f[T]() -> T:
                raise NotImplementedError
            "#,
        ),
        (
            "gradual/lib.py",
            r#"
            def g[T]() -> T:
                raise NotImplementedError
            "#,
        ),
        (
            "gradual/main.py",
            r#"
            from precise.lib import f
            from typing_extensions import reveal_type

            # `f` is declared in a precise module, so its unsolved type variable is `Never` here too
            reveal_type(f())
            "#,
        ),
        (
            "precise/main.py",
            r#"
            from gradual.lib import g
            from typing_extensions import reveal_type

            # `g` is declared in a gradual module, so its unsolved type variable stays `Unknown`
            reveal_type(g())
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: true
    exit_code: 0
    ----- stdout -----
    info[revealed-type]: Revealed type
     --> gradual/main.py:6:13
      |
    6 | reveal_type(f())
      |             ^^^ `Never`

    info[revealed-type]: Revealed type
     --> precise/main.py:6:13
      |
    6 | reveal_type(g())
      |             ^^^ `Unknown`

    Found 2 diagnostics

    ----- stderr -----
    ");

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
     --> bivariant/main.py:9:26
      |
    9 | narrowed: Covariant[B] = Covariant[A]()
      |           ------------   ^^^^^^^^^^^^^^ Incompatible value of type `Covariant[A]`
      |           |
      |           Declared type

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

/// A class name that could not name a class is a configuration error, not a silent no-op.
#[test]
fn overlapping_condition_exempt_types_rejects_a_malformed_name() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [analysis]
            overlapping-condition-exempt-types = ["int", "list[int]"]
            "#,
        ),
        (
            "test.py",
            r#"
            def f(a: str | None):
                if not a:
                    ...
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: false
    exit_code: 2
    ----- stdout -----

    ----- stderr -----
    by failed
      Cause: error[invalid-class-name]: Invalid class name
     --> ty.toml:3:46
      |
    2 | [analysis]
    3 | overlapping-condition-exempt-types = ["int", "list[int]"]
      |                                              ^^^^^^^^^^^ Expected a bare or qualified class name, such as `int` or `decimal.Decimal`
    "#);

    Ok(())
}

/// A well-formed name that resolves to nothing is not an error; it just never matches — which the
/// surviving `overlapping-condition` warning is the proof of.
#[test]
fn overlapping_condition_exempt_types_accepts_an_unresolvable_name() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [analysis]
            overlapping-condition-exempt-types = ["nowhere.Nothing"]
            "#,
        ),
        (
            "test.py",
            r#"
            def f(a: str | None):
                if not a:
                    ...
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: false
    exit_code: 1
    ----- stdout -----
    warning[overlapping-condition]: This condition does not distinguish between `str & ~AlwaysTruthy` and `None`
     --> test.py:3:8
      |
    3 |     if not a:
      |        ^^^^^
    info: `str | None` is tested for falsiness
    help: Compare against the specific value instead of testing truthiness

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

/// The option name reaches the diagnostic, so the report names the setting that was wrong.
#[test]
fn implicit_object_repr_report_types_rejects_a_malformed_name() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [analysis]
            implicit-object-repr-report-types = ["types.FunctionType", "not a class"]
            "#,
        ),
        ("test.py", "print(1)\n"),
    ])?;

    assert_cmd_snapshot!(case.command(), @r#"
    success: false
    exit_code: 2
    ----- stdout -----

    ----- stderr -----
    by failed
      Cause: error[invalid-class-name]: Invalid class name
     --> ty.toml:3:60
      |
    2 | [analysis]
    3 | implicit-object-repr-report-types = ["types.FunctionType", "not a class"]
      |                                                            ^^^^^^^^^^^^^ Expected a bare or qualified class name, such as `int` or `decimal.Decimal`
    "#);

    Ok(())
}

/// Exempting a class silences the report the defaults would otherwise produce.
#[test]
fn implicit_object_repr_exempt_types_silences_a_default() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "ty.toml",
            r#"
            [analysis]
            implicit-object-repr-exempt-types = ["types.FunctionType"]
            "#,
        ),
        (
            // no `-> None`: a bodyless `def` already returns `None` without it, and writing it
            // would draw `redundant-return-annotation` into a snapshot about rendering
            "test.py",
            r#"
            def f(): ...

            print(f)
            print(int)
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: false
    exit_code: 1
    ----- stdout -----
    warning[implicit-object-repr]: `type` has no `__str__` or `__repr__` of its own
     --> test.py:5:7
      |
    5 | print(int)
      |       ^^^
    info: nothing in its hierarchy defines one, so the output is the interpreter's default, which identifies the class rather than the value

    Found 1 diagnostic

    ----- stderr -----
    ");

    Ok(())
}

/// `redundant-return-annotation` is on by default, but only reports where dropping the annotation
/// would have left the return type alone.
#[test]
fn redundant_return_annotation_is_gated_on_infer_unannotated_signatures() -> anyhow::Result<()> {
    let case = CliTest::with_files([
        (
            "pyproject.toml",
            r#"
            [tool.ty.environment]
            python-version = "3.13"

            # recovery is on by default, so the gradual half has to say it is off
            [tool.ty.analysis]
            infer-unannotated-signatures = false

            [[tool.ty.overrides]]
            include = ["inferred/**"]

            [tool.ty.overrides.analysis]
            infer-unannotated-signatures = true
            "#,
        ),
        (
            "inferred/lib.py",
            r#"
            def f() -> None:
                print("hi")

            def raises() -> None:
                raise ValueError
            "#,
        ),
        // a first-party stub in a module that recovers signatures is reported too: a bodyless
        // `def` would return `None` on its own
        (
            "inferred/stub.pyi",
            r#"
            def s() -> None: ...
            "#,
        ),
        (
            "gradual/lib.py",
            r#"
            def g() -> None:
                print("hi")
            "#,
        ),
    ])?;

    assert_cmd_snapshot!(case.command(), @"
    success: false
    exit_code: 1
    ----- stdout -----
    warning[redundant-return-annotation]: Redundant `-> None` return annotation
     --> inferred/lib.py:2:12
      |
    2 | def f() -> None:
      |            ^^^^
    info: a `def` that leaves out its return type already returns `None`
    help: Remove the annotation

    warning[redundant-return-annotation]: Redundant `-> None` return annotation
     --> inferred/stub.pyi:2:12
      |
    2 | def s() -> None: ...
      |            ^^^^
    info: a `def` that leaves out its return type already returns `None`
    help: Remove the annotation

    Found 2 diagnostics

    ----- stderr -----
    ");

    Ok(())
}

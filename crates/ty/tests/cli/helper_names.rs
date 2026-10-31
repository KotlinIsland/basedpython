//! `by check` reports a binding that takes over the name of a runtime helper the file's lowering
//! needs, which the transpiler refuses, rather than leaving it to be found at `by run`.

use anyhow::Result;

use crate::CliTest;

/// everything printed by a check of the one file `source`
fn check(source: &str) -> Result<String> {
    let case = CliTest::with_file("main.by", source)?;
    let output = case
        .command()
        .arg("--output-format")
        .arg("concise")
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// a checked call writes `_soundness_check`, which this module binds
#[test]
fn a_binding_the_lowering_needs_is_reported_where_it_is_bound() -> Result<()> {
    let printed = check(concat!(
        "def pick[T](items: list[T], i: int) -> T:\n",
        "    return items[i]\n",
        "\n",
        "xs: list[str] = [\"a\"]\n",
        "_soundness_check = 3\n",
        "print(pick(xs, 0))\n",
    ))?;
    assert!(
        printed.contains("main.by:5:1: error[transpile-refused]")
            && printed.contains("`_soundness_check`"),
        "{printed}"
    );
    Ok(())
}

/// `Some(...)` writes `Optional`
#[test]
fn a_typing_import_the_lowering_needs_is_reported() -> Result<()> {
    let printed = check("from typing import Optional\n\nx = Some(3)\nprint(x!)\n")?;
    assert!(
        printed.contains("main.by:1:20: error[transpile-refused]")
            && printed.contains("`Optional`"),
        "{printed}"
    );
    Ok(())
}

/// a scope of the module's own takes the name over for the lowered lines in it
#[test]
fn a_local_binding_the_lowering_needs_is_reported() -> Result<()> {
    let printed = check(concat!(
        "def pick[T](items: list[T], i: int) -> T:\n",
        "    return items[i]\n",
        "\n",
        "def go() -> str:\n",
        "    _soundness_check = \"mine\"\n",
        "    xs: list[str] = [_soundness_check]\n",
        "    return pick(xs, 0)\n",
        "\n",
        "print(go())\n",
    ))?;
    assert!(
        printed.contains("main.by:5:5: error[transpile-refused]")
            && printed.contains("`_soundness_check`"),
        "{printed}"
    );
    Ok(())
}

/// a helper's name is the module's to use where the lowering writes nothing that reads it
#[test]
fn a_binding_the_lowering_does_not_need_is_left_alone() -> Result<()> {
    for source in [
        "_soundness_check = 3\nprint(_soundness_check)\n",
        "from typing import Optional\n\nx: Optional[int] = None\nprint(x is None)\n",
        "def generic(x: int) -> int:\n    return x\n\nprint(generic(1))\n",
    ] {
        let printed = check(source)?;
        assert!(
            !printed.contains("transpile-refused"),
            "{source}\n{printed}"
        );
    }
    Ok(())
}

/// these are written under a name of the file's own wherever the file binds them
#[test]
fn a_name_the_lowering_writes_around_is_left_alone() -> Result<()> {
    let printed = check(concat!(
        "_by_lit = 1\n",
        "_by_variance = 2\n",
        "_by_out = 3\n",
        "_by_in = 4\n",
        "_by_specialization = 5\n",
        "\n",
        "def show[T](x: T) -> object:\n",
        "    return T\n",
        "\n",
        "class Out[out T]: ...\n",
        "\n",
        "print(show(1), _by_lit, _by_variance, _by_out, _by_in, _by_specialization)\n",
        "keep: Out[object] = Out[int]()\n",
    ))?;
    assert!(!printed.contains("transpile-refused"), "{printed}");
    Ok(())
}
